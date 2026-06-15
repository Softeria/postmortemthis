use crate::agents::Agent;
use anyhow::Result;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

pub struct Report {
    pub agent: Agent,
    pub output: String,
    pub stderr: String,
    pub elapsed: Duration,
    pub outcome: Outcome,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed(String),
    TimedOut,
}

/// How long to wait for the output pipes to reach EOF after the agent
/// process is gone. A leaked grandchild holding the pipe open must not
/// turn the timeout into an indefinite hang.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

/// Kill the agent and everything it spawned. Each agent runs in its own
/// process group (see `run_one`), so on Unix the whole tree goes down -
/// otherwise orphaned subprocesses (shells, MCP servers) keep running and
/// keep the stdout/stderr pipes open. The group is signalled at most once
/// per run: after the child is reaped its PID can be recycled, and a
/// second raw `kill(-pid)` could land on an unrelated process group.
/// On Windows this is best-effort: only the direct child dies (a full fix
/// needs Job Objects).
#[cfg_attr(not(unix), allow(unused_variables))]
fn kill_tree(child: &mut Child, group_killed: &mut bool) {
    #[cfg(unix)]
    if !*group_killed {
        *group_killed = true;
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Read a pipe to EOF on a thread, delivering the result over a channel so
/// the caller can bound the wait.
fn drain<R: Read + Send + 'static>(mut pipe: R) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    rx
}

/// Receive a drained pipe with a deadline; on stall, kill the tree (some
/// straggler is holding the pipe) and try once more. Output that never
/// arrived is lost, but by then the outcome is already decided.
fn recv_drained(rx: &mpsc::Receiver<String>, child: &mut Child, group_killed: &mut bool) -> String {
    match rx.recv_timeout(DRAIN_GRACE) {
        Ok(s) => s,
        Err(_) => {
            kill_tree(child, group_killed);
            rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
        }
    }
}

/// Run one agent to completion with a timeout. The prompt goes in on stdin.
fn run_one(agent: Agent, prompt: &str, repo: &Path, timeout: Duration) -> Report {
    let started = Instant::now();
    let mut cmd = agent.command(repo);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group, so a timeout can kill the whole tree.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Report {
                agent,
                output: String::new(),
                stderr: String::new(),
                elapsed: started.elapsed(),
                outcome: Outcome::Failed(format!("failed to spawn: {e}")),
            };
        }
    };

    // Feed the prompt on a thread; a write error just means the agent died
    // before reading it, which the exit status will report. Dropping the
    // handle closes the pipe so the agent sees EOF.
    let mut stdin_pipe = child.stdin.take().expect("stdin piped");
    let prompt_owned = prompt.to_string();
    std::thread::spawn(move || {
        let _ = stdin_pipe.write_all(prompt_owned.as_bytes());
    });

    let out_rx = drain(child.stdout.take().expect("stdout piped"));
    let err_rx = drain(child.stderr.take().expect("stderr piped"));
    let mut group_killed = false;

    let status = match child.wait_timeout(timeout) {
        Ok(status) => status,
        Err(e) => {
            kill_tree(&mut child, &mut group_killed);
            return Report {
                agent,
                output: recv_drained(&out_rx, &mut child, &mut group_killed),
                stderr: recv_drained(&err_rx, &mut child, &mut group_killed),
                elapsed: started.elapsed(),
                outcome: Outcome::Failed(format!("wait failed: {e}")),
            };
        }
    };
    if status.is_none() {
        kill_tree(&mut child, &mut group_killed);
    }

    let output = recv_drained(&out_rx, &mut child, &mut group_killed);
    let stderr = recv_drained(&err_rx, &mut child, &mut group_killed);
    let outcome = match status {
        None => Outcome::TimedOut,
        Some(s) if s.success() => Outcome::Ok,
        Some(s) => Outcome::Failed(s.to_string()),
    };

    Report {
        agent,
        output,
        stderr,
        elapsed: started.elapsed(),
        outcome,
    }
}

/// Fan out to all agents in parallel; report each as it finishes.
pub fn run_all(
    agents: &[Agent],
    prompt: &str,
    repo: &Path,
    timeout: Duration,
) -> Result<Vec<Report>> {
    let (tx, rx) = mpsc::channel::<Report>();
    std::thread::scope(|scope| {
        for &agent in agents {
            let tx = tx.clone();
            scope.spawn(move || {
                let report = run_one(agent, prompt, repo, timeout);
                let _ = tx.send(report);
            });
        }
        drop(tx);

        let mut reports = Vec::with_capacity(agents.len());
        for report in rx {
            let badge = match report.outcome {
                Outcome::Ok => "done",
                Outcome::Failed(_) => "FAILED",
                Outcome::TimedOut => "TIMED OUT",
            };
            eprintln!(
                "  [{}] {} in {:.0?}",
                report.agent.name(),
                badge,
                report.elapsed
            );
            reports.push(report);
        }
        // Keep stable order: same as requested agent list.
        reports.sort_by_key(|r| agents.iter().position(|a| *a == r.agent));
        Ok(reports)
    })
}
