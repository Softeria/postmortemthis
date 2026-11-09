//! Launcher side of the Antigravity-on-OpenRouter bridge.
//!
//! Antigravity has a Gemini API key mode (`modelProvider: "gemini"` plus
//! `GEMINI_API_KEY`) and honours `GOOGLE_GEMINI_BASE_URL`, but it speaks
//! Google's protocol, which OpenRouter doesn't expose. `gemshim` translates
//! Gemini <-> OpenAI, so the same Antigravity harness runs on an OpenRouter
//! key. We spawn one gemshim per run, point the CLI at it, and tear it down
//! when the run ends.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

/// Live bridge process. Dropping it closes the bridge's stdin (which makes it
/// exit on its own) and kills it to be sure.
pub struct Bridge {
    child: Child,
    // Held only so it closes on drop; also closes if this process dies.
    _stdin: ChildStdin,
}

/// The bridge's loopback port and per-run secret, set once it is up, so
/// `agents::command` can read them without threading them through every call.
static ENDPOINT: OnceLock<(u16, String)> = OnceLock::new();

/// (port, secret) once the bridge is up. The secret goes to Antigravity as its
/// GEMINI_API_KEY; gemshim refuses any request without it.
pub fn endpoint() -> Option<(u16, &'static str)> {
    ENDPOINT.get().map(|(p, s)| (*p, s.as_str()))
}

impl Bridge {
    /// Spawn gemshim against `or_key`, forwarding every request to `model`
    /// (an OpenRouter slug). The caller holds the bridge for the whole run.
    pub fn start(or_key: &str, model: &str) -> Result<Bridge> {
        let exe = std::env::current_exe().context("cannot locate own executable for the gemshim bridge")?;
        let secret = secret()?;

        // The bridge is this same binary's hidden `__gemshim` subcommand, so
        // it can never version-mismatch the launcher. It binds its own port and
        // reports it, so there is no window where another process can take it.
        let mut child = std::process::Command::new(&exe)
            .arg("__gemshim")
            .env("OPENROUTER_API_KEY", or_key)
            .env("GEMSHIM_MODEL", model)
            .env("GEMSHIM_SECRET", &secret)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn gemshim bridge ({})", exe.display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let port = rx
            .recv_timeout(Duration::from_secs(10))
            .ok()
            .and_then(|l| l.trim().parse::<u16>().ok());
        let Some(port) = port else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("gemshim bridge did not report a port");
        };
        let _ = ENDPOINT.set((port, secret));
        Ok(Bridge { child, _stdin: stdin })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn secret() -> Result<String> {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow::anyhow!("no randomness for the gemshim secret: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}
