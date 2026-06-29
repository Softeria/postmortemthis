mod agents;
mod gg;
mod login;
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
#[command(name = "postmortemthis", version = VERSION, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Connect an OpenRouter account via OAuth and save the key locally, so
    /// runs need no OPENROUTER_API_KEY env var or --key flag.
    Login,
    /// Show which agent CLIs are installed and authenticated.
    Doctor,
}

#[derive(clap::Args, Default)]
struct RunArgs {
    /// Prompt sent to every agent. If omitted, it is read from stdin.
    prompt: Option<String>,

    /// Comma-separated agents to run (default: all available).
    #[arg(long, value_delimiter = ',')]
    agents: Vec<String>,

    /// Comma-separated agents to send straight to OpenRouter, skipping their
    /// native login attempt. Use when a native login is known-broken and you
    /// want to avoid the wasted retry (see the run notes after a fallback).
    #[arg(long, value_delimiter = ',')]
    skip_native: Vec<String>,

    /// Skip refreshing the agent CLIs before running. By default each selected
    /// agent is updated (gg update <tool> -u, in parallel) so they stay current.
    #[arg(long = "no-update")]
    no_update: bool,

    /// Per-agent timeout in seconds.
    #[arg(long, default_value_t = 600)]
    timeout: u64,

    /// Also write each agent's output to <DIR>/<agent>.md (untruncated) and
    /// print the paths. Useful for large panels where the combined stdout can
    /// exceed the caller's output limit. Default is stdout only.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,

    /// OpenRouter API key for agents you have no native login for. Also read
    /// from OPENROUTER_API_KEY or ~/.config/postmortemthis/key.
    #[arg(long)]
    key: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::Login) => login::run(),
        Some(Cmd::Doctor) => doctor(),
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
    let skip_native = parse_agents(&args.skip_native)?;
    let cwd = std::env::current_dir()?;
    let timeout = Duration::from_secs(args.timeout);

    if !args.no_update
        && let Some(gg) = gg::locate()
    {
        // Scoped: update only the agents this run uses, in parallel - not the
        // user's whole gg toolchain, and never postmortemthis itself.
        let tools: Vec<&str> = selected
            .iter()
            .filter(|a| a.via() == Some(Via::Gg))
            .map(|a| a.gg_tool())
            .collect();
        if !tools.is_empty() {
            eprintln!("postmortemthis: updating {} ...", tools.join(", "));
            let children: Vec<_> = tools
                .iter()
                .filter_map(|t| {
                    gg.update_tool(t)
                        .current_dir(&cwd)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .ok()
                })
                .collect();
            for mut c in children {
                let _ = c.wait();
            }
        }
    }

    let _vibe = start_vibe_home(&selected);

    eprintln!(
        "postmortemthis: running {} agent(s) in parallel: {}",
        selected.len(),
        selected.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
    );

    prewarm(&selected, &cwd);

    let reports = runner::run_all(&selected, &prompt, &cwd, timeout, &skip_native)?;

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

    let notes = run_notes(&reports, &selected);
    if !notes.is_empty() {
        print!(
            "\n\n---\n\n# postmortemthis run notes (operational; not part of the review)\n\n{}\n",
            notes.join("\n")
        );
    }

    if reports.iter().all(|r| r.outcome != Outcome::Ok) {
        bail!("all agents failed");
    }
    Ok(())
}

/// One agent's section: a `# name (provenance)` header and its output (or a
/// failure note with stderr). The provenance tells the synthesizing agent
/// which model actually answered, so it can weight opinions. The same text is
/// printed to stdout and, with --out, a file.
fn report_section(r: &Report) -> String {
    let provenance = if r.used_openrouter {
        format!("via OpenRouter: {}", r.agent.openrouter_model())
    } else {
        "native login".to_string()
    };
    let body = match &r.outcome {
        Outcome::Ok => r.output.trim().to_string(),
        Outcome::TimedOut => "_timed out_".to_string(),
        Outcome::Failed(why) => format!("_failed: {why}_\n\n```\n{}\n```", r.stderr.trim()),
    };
    format!("# {} ({provenance})\n\n{body}\n", r.agent.name())
}

/// Operational notes for the calling agent: what it can fix or change on a
/// later run. Empty when there is nothing worth saying. Kept terse and
/// imperative because the consumer is an LLM composing the next command.
fn run_notes(reports: &[Report], selected: &[Agent]) -> Vec<String> {
    let mut notes = Vec::new();

    for r in reports {
        let name = r.agent.name();
        if r.fell_back {
            notes.push(format!(
                "- {name}: native login failed, so it ran on OpenRouter ({}). To restore the native login, {}. To skip the wasted retry on later runs, add `--skip-native {name}`.",
                r.agent.openrouter_model(),
                r.agent.native_fix_hint(),
            ));
        }
        if r.outcome == Outcome::TimedOut {
            notes.push(format!(
                "- {name}: timed out. Raise --timeout or shorten the prompt."
            ));
        }
    }

    // Agents that exist but were not run because they have no login and no key.
    let has_key = openrouter::key().is_some();
    if !has_key {
        for agent in agents::ALL {
            if agent.via().is_some() && !selected.contains(&agent) && !agent.authed() {
                notes.push(format!(
                    "- {}: skipped (no native login, no OpenRouter key). Run `postmortemthis login` or set OPENROUTER_API_KEY to include it.",
                    agent.name()
                ));
            }
        }
    }

    notes
}

fn doctor() -> Result<()> {
    openrouter::init(None);
    println!("postmortemthis doctor\n");
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
        println!("\nNo agent CLIs found. Install one of claude, codex, antigravity, or run");
        println!("postmortemthis through postmortemthis.cmd, which bootstraps them itself.");
    }
    Ok(())
}

/// Resolve agent names to `Agent`s, erroring on an unknown name.
fn parse_agents(names: &[String]) -> Result<Vec<Agent>> {
    names
        .iter()
        .map(|s| {
            Agent::from_name(s).ok_or_else(|| {
                anyhow::anyhow!("unknown agent '{s}' (known: claude, codex, antigravity, qwen, vibe)")
            })
        })
        .collect()
}

fn select_agents(requested: &[String]) -> Result<Vec<Agent>> {
    let explicit = !requested.is_empty();
    let candidates: Vec<Agent> = if requested.is_empty() {
        agents::ALL.to_vec()
    } else {
        parse_agents(requested)?
    };

    let mut selected: Vec<Agent> = Vec::new();
    for agent in candidates {
        if selected.contains(&agent) {
            continue;
        }
        match agent.via() {
            Some(Via::Native) => selected.push(agent),
            // Auto-pick a gg-bootstrapped agent only if it can actually run:
            // own login, or an OpenRouter key it can actually use. The key
            // check is gated on supports_openrouter() so a native-only agent
            // (antigravity) is not auto-selected on key-presence alone, only to
            // fail with an empty attempt plan. An explicit --agents overrides.
            Some(Via::Gg)
                if explicit
                    || agent.authed()
                    || (openrouter::key().is_some() && agent.supports_openrouter()) =>
            {
                selected.push(agent)
            }
            Some(Via::Gg) => eprintln!(
                "postmortemthis: skipping {} (no credentials - log in once, or pass --key)",
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
        bail!("no agent CLIs available - run `postmortemthis doctor`");
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
    eprintln!("postmortemthis: bootstrapping {} (first run may download)", tools.join(", "));
    match gg
        .tool(&tools.join(":"))
        .arg("--version")
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("postmortemthis: gg prewarm exited with {s}; continuing"),
        Err(e) => eprintln!("postmortemthis: gg prewarm failed: {e}; continuing"),
    }
}

/// Write the scratch VIBE_HOME when the Vibe leg will run on OpenRouter (Vibe
/// selected and an OpenRouter key is present). Held for the run, removed on
/// drop.
fn start_vibe_home(selected: &[Agent]) -> Option<vibe::Home> {
    if !(selected.contains(&Agent::Vibe) && openrouter::key().is_some()) {
        return None;
    }
    match vibe::Home::create(Agent::Vibe.openrouter_model()) {
        Ok(home) => Some(home),
        Err(e) => {
            eprintln!("postmortemthis: could not prepare vibe home ({e}); the vibe leg will fail");
            None
        }
    }
}

