mod agents;
mod gemshim;
mod gemshim_server;
mod gg;
mod openrouter;
mod runner;
mod vibe;

use agents::{Agent, Via};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use runner::{Outcome, Report};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

/// Version reported by `--version`: the git tag baked in at release build
/// time (POSTMORTEM_VERSION), or the Cargo.toml placeholder for dev builds.
const VERSION: &str = match option_env!("POSTMORTEM_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Run the AI agent CLIs you have, in parallel, on one prompt, and print each
/// one's output. The prompt is read from stdin; the caller decides what it says.
#[derive(Parser)]
#[command(name = "postmortem", version = VERSION, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show which agent CLIs are installed and authenticated.
    Doctor,
    /// Print instructions for an AI agent to build a Claude Code skill that
    /// drives this tool. Meant to be read by the agent, not the user.
    Skill,
    /// Internal: run the Gemini->OpenRouter bridge server. Spawned by the tool
    /// itself; not for direct use.
    #[command(name = "__gemshim", hide = true)]
    Gemshim,
}

#[derive(clap::Args, Default)]
struct RunArgs {
    /// Prompt sent to every agent. If omitted, it is read from stdin.
    prompt: Option<String>,

    /// Comma-separated agents to run (default: all available).
    #[arg(long, value_delimiter = ',')]
    agents: Vec<String>,

    /// Update the agent CLIs to their latest versions (gg update -u) before
    /// running. gg does this in parallel; a no-op if no gg is present.
    #[arg(long)]
    update: bool,

    /// Per-agent timeout in seconds.
    #[arg(long, default_value_t = 600)]
    timeout: u64,

    /// Also write each agent's output to <DIR>/<agent>.md (untruncated) and
    /// print the paths. Useful for large panels where the combined stdout can
    /// exceed the caller's output limit. Default is stdout only.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,

    /// OpenRouter API key for agents you have no native login for. Also read
    /// from OPENROUTER_API_KEY or ~/.config/postmortem/key.
    #[arg(long)]
    key: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::Doctor) => doctor(),
        Some(Cmd::Skill) => {
            print!("{SKILL_INSTRUCTIONS}");
            Ok(())
        }
        Some(Cmd::Gemshim) => {
            gemshim_server::run();
            Ok(())
        }
        None => run(cli.run),
    }
}

fn run(args: RunArgs) -> Result<()> {
    openrouter::init(args.key.as_deref());

    let prompt = match args.prompt {
        Some(p) => p,
        None => {
            let mut s = String::new();
            let _ = std::io::stdin().read_to_string(&mut s);
            s
        }
    };
    if prompt.trim().is_empty() {
        bail!("no prompt (pass it as an argument or pipe it on stdin)");
    }

    let selected = select_agents(&args.agents)?;
    let cwd = std::env::current_dir()?;
    let timeout = Duration::from_secs(args.timeout);

    if args.update
        && let Some(gg) = gg::locate()
    {
        eprintln!("postmortem: updating agent tools (gg update -u)...");
        let _ = gg
            .update_all()
            .current_dir(&cwd)
            .stdout(std::process::Stdio::null())
            .status();
    }

    let _bridge = start_gemini_bridge(&selected);
    let _vibe = start_vibe_home(&selected);

    eprintln!(
        "postmortem: running {} agent(s) in parallel: {}",
        selected.len(),
        selected.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
    );

    prewarm(&selected, &cwd);

    let reports = runner::run_all(&selected, &prompt, &cwd, timeout)?;

    // When --out is set, write each agent's output to a file and print the
    // paths first, so they survive even if the stdout body is later truncated
    // by the caller. stdout still carries the full sections by default.
    if let Some(dir) = &args.out {
        std::fs::create_dir_all(dir)?;
        println!("Per-agent outputs written to:");
        for r in &reports {
            let path = dir.join(format!("{}.md", r.agent.name()));
            let _ = std::fs::write(&path, report_section(r));
            println!("  {}", path.display());
        }
    }

    for r in &reports {
        print!("\n\n{}", report_section(r));
    }

    if reports.iter().all(|r| r.outcome != Outcome::Ok) {
        bail!("all agents failed");
    }
    Ok(())
}

/// One agent's section: a `# name` header and its output (or a failure note
/// with stderr). The same text is printed to stdout and, with --out, a file.
fn report_section(r: &Report) -> String {
    let body = match &r.outcome {
        Outcome::Ok => r.output.trim().to_string(),
        Outcome::TimedOut => "_timed out_".to_string(),
        Outcome::Failed(why) => format!("_failed: {why}_\n\n```\n{}\n```", r.stderr.trim()),
    };
    format!("# {}\n\n{body}\n", r.agent.name())
}

fn doctor() -> Result<()> {
    openrouter::init(None);
    println!("postmortem doctor\n");
    match gg::locate() {
        Some(gg) => println!("  bootstrap: {}", gg.path().display()),
        None => println!("  bootstrap: none - agent CLIs must already be on PATH"),
    }
    match openrouter::key() {
        Some(k) => println!(
            "  OpenRouter: {}... (fills in agents you're not logged into)\n",
            &k[..k.len().min(12)]
        ),
        None => println!("  OpenRouter: no key - agents need your own logins\n"),
    }
    let mut any = false;
    for agent in agents::ALL {
        let or = openrouter::key().is_some() && agent.supports_openrouter();
        let auth = match (agent.authed(), or) {
            (true, true) => format!("{} (OpenRouter fallback if it fails)", agent.auth_hint()),
            (true, false) => agent.auth_hint(),
            (false, true) => "via OpenRouter key".to_string(),
            (false, false) => agent.auth_hint(),
        };
        match agent.via() {
            Some(Via::Native) => {
                any = true;
                println!("  + {:<8} {}", agent.name(), agent.native_version().unwrap_or_default());
                println!("    auth: {auth}");
            }
            Some(Via::Gg) => {
                any = true;
                println!("  + {:<8} bootstrapped on first run", agent.name());
                println!("    auth: {auth}");
            }
            None => println!("  x {:<8} not found", agent.name()),
        }
    }
    if !any {
        println!("\nNo agent CLIs found. Install one of claude, codex, gemini, or run");
        println!("postmortem through postmortemthis.cmd, which bootstraps them itself.");
    }
    Ok(())
}

fn select_agents(requested: &[String]) -> Result<Vec<Agent>> {
    let explicit = !requested.is_empty();
    let candidates: Vec<Agent> = if requested.is_empty() {
        agents::ALL.to_vec()
    } else {
        requested
            .iter()
            .map(|s| {
                Agent::from_name(s)
                    .ok_or_else(|| anyhow::anyhow!("unknown agent '{s}' (known: claude, codex, gemini)"))
            })
            .collect::<Result<_>>()?
    };

    let mut selected: Vec<Agent> = Vec::new();
    for agent in candidates {
        if selected.contains(&agent) {
            continue;
        }
        match agent.via() {
            Some(Via::Native) => selected.push(agent),
            // Auto-pick a gg-bootstrapped agent only if it can actually run:
            // own login, or an OpenRouter key. An explicit --agents overrides.
            Some(Via::Gg) if explicit || agent.authed() || openrouter::key().is_some() => {
                selected.push(agent)
            }
            Some(Via::Gg) => eprintln!(
                "postmortem: skipping {} (no credentials - log in once, or pass --key)",
                agent.name()
            ),
            None if explicit => bail!(
                "agent '{}' was requested but is not installed and no gg.cmd is available",
                agent.name()
            ),
            None => {}
        }
    }
    if selected.is_empty() {
        bail!("no agent CLIs available - run `postmortem doctor`");
    }
    Ok(selected)
}

/// One chained gg invocation prepares every needed tool in parallel before the
/// fan-out, so the per-agent timeout is spent running, not bootstrapping.
fn prewarm(selected: &[Agent], dir: &std::path::Path) {
    let tools: Vec<&str> = selected
        .iter()
        .filter(|a| a.via() == Some(Via::Gg))
        .map(|a| a.gg_tool())
        .collect();
    let Some(gg) = gg::locate() else { return };
    if tools.is_empty() {
        return;
    }
    eprintln!("postmortem: bootstrapping {} (first run may download)", tools.join(", "));
    match gg
        .tool(&tools.join(":"))
        .arg("--version")
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("postmortem: gg prewarm exited with {s}; continuing"),
        Err(e) => eprintln!("postmortem: gg prewarm failed: {e}; continuing"),
    }
}

/// OpenRouter slug the Gemini leg forwards to when it has no native login.
const GEMINI_OPENROUTER_MODEL: &str = "google/gemini-3.1-pro-preview";

/// Bring up the gemshim bridge when the Gemini leg will run on OpenRouter
/// (Gemini selected and an OpenRouter key is present). Held for the run.
fn start_gemini_bridge(selected: &[Agent]) -> Option<gemshim::Bridge> {
    let needs_bridge = selected.contains(&Agent::Gemini) && openrouter::key().is_some();
    if !needs_bridge {
        return None;
    }
    let key = openrouter::key()?;
    match gemshim::Bridge::start(key, GEMINI_OPENROUTER_MODEL) {
        Ok(bridge) => {
            eprintln!(
                "postmortem: gemini -> OpenRouter via local gemshim bridge (127.0.0.1:{})",
                bridge.port()
            );
            Some(bridge)
        }
        Err(e) => {
            eprintln!("postmortem: could not start gemini bridge ({e}); the gemini leg will fail");
            None
        }
    }
}

/// Write the scratch VIBE_HOME when the Vibe leg will run on OpenRouter (Vibe
/// selected and an OpenRouter key is present). Held for the run, removed on
/// drop.
fn start_vibe_home(selected: &[Agent]) -> Option<vibe::Home> {
    if !(selected.contains(&Agent::Vibe) && openrouter::key().is_some()) {
        return None;
    }
    match vibe::Home::create(VIBE_OPENROUTER_MODEL) {
        Ok(home) => Some(home),
        Err(e) => {
            eprintln!("postmortem: could not prepare vibe home ({e}); the vibe leg will fail");
            None
        }
    }
}

/// OpenRouter slug the Vibe leg (Mistral's CLI) runs on; written into the
/// scratch VIBE_HOME config (see vibe.rs).
const VIBE_OPENROUTER_MODEL: &str = "mistralai/mistral-medium-3.1";

/// Read by an AI agent (via `postmortem skill`) to author a Claude Code skill.
/// This is the whole user-facing setup surface, reached by a one-line prompt
/// that downloads the tool and runs `postmortemthis.cmd skill`.
const SKILL_INSTRUCTIONS: &str = r#"You are setting up a Claude Code skill that runs a multi-model review with
`postmortem`. postmortem runs every agent CLI the user has (Claude, Codex,
Gemini) in parallel on a prompt piped to its stdin, read-only, and prints each
one's output. It does nothing else: you compose the prompt, and you synthesize
the outputs.

Create a skill that fires when the user asks for a review, a second opinion, a
panel review, or says "postmortem this". It should:
1. Compose a review prompt describing what to look at and what to report. For
   pending changes, tell the agents to inspect the diff with their own tools
   (each agent runs read-only).
2. Pipe that prompt to the tool, in the repo, and always pass --update (the
   agents update in parallel and postmortem runs rarely, so keep them current):
       echo "<your prompt>" | postmortem --update --timeout 600
   (Use `./postmortemthis.cmd` instead if `postmortem` is not on PATH; it
   bootstraps the binary and any missing agent CLIs on first run.) Each agent
   runs on the user's own login first; if that login fails or is missing and
   OPENROUTER_API_KEY is set, it falls back to OpenRouter. An agent with no
   working login and no key is skipped.
3. Read the per-agent outputs from stdout and synthesize one verdict: merge and
   deduplicate findings, weight by cross-agent consensus, drop false positives,
   rank by severity with file:line, and end with a clear ship / don't-ship call.
   If the panel is large and stdout looks truncated, rerun with `--out <dir>`
   (writes each agent's full output to `<dir>/<agent>.md`) and read those files.

Place it at `.claude/skills/postmortem/SKILL.md` for this repo, or
`~/.claude/skills/postmortem/SKILL.md` for all repos. Use the current skill
frontmatter format.
"#;
