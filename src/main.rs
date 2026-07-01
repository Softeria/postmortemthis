mod agents;
mod gg;
mod login;
mod openrouter;
mod runner;
mod settings;
mod setup;
mod vibe;

use agents::{Agent, Via};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use runner::{Outcome, Report};
use std::io::Read;
use std::path::{Path, PathBuf};
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
    /// Interactively configure each agent (keep, log in, force OpenRouter, or
    /// disable) and optionally fire a test prompt. Saved to agents.json.
    Setup,
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
        Some(Cmd::Setup) => setup::run(),
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

    let settings = settings::Settings::load();
    let (selected, skip_native) = plan_run(&args.agents, &args.skip_native, &settings)?;
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

    eprintln!(
        "postmortemthis: running {} agent(s) in parallel: {}",
        selected.len(),
        selected.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
    );

    let reports = execute(&selected, &skip_native, &prompt, &cwd, timeout)?;

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

    let notes = run_notes(&reports, &selected, &settings);
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
fn run_notes(reports: &[Report], selected: &[Agent], settings: &settings::Settings) -> Vec<String> {
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

    // Agents that exist but were not run for lack of usable credentials. An
    // OpenRouter-capable agent is runnable whenever a key is present (whether or
    // not it was requested), so flag it only when there is no key. A native-only
    // agent (antigravity, grok) can't use the key at all, so flag it whenever it
    // lacks its own login - even with a key set - and point it at that login.
    let has_key = openrouter::key().is_some();
    for agent in agents::ALL {
        let runnable_via_key = has_key && agent.supports_openrouter();
        // Don't nag about an agent the user deliberately disabled in setup.
        if settings.mode(agent) == settings::Mode::Disabled {
            continue;
        }
        if agent.via().is_some()
            && !selected.contains(&agent)
            && !agent.authed()
            && !runnable_via_key
        {
            let fix = if agent.supports_openrouter() {
                "Run `postmortemthis login` or set OPENROUTER_API_KEY to include it.".to_string()
            } else {
                format!("It is native-only: {}.", agent.native_fix_hint())
            };
            notes.push(format!("- {}: skipped (not logged in). {fix}", agent.name()));
        }
    }

    notes
}

fn doctor() -> Result<()> {
    openrouter::init(None);
    let settings = settings::Settings::load();
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
        // Surface a non-default setup choice (disabled / forced OpenRouter) so
        // the user can see their `setup` preferences took effect.
        let mode = settings.mode(agent);
        let tag = match mode {
            settings::Mode::Auto => String::new(),
            m => format!("  [{}]", m.label()),
        };
        match agent.via() {
            Some(Via::Native) => {
                any = true;
                println!("  + {:<8} {}{tag}", agent.name(), agent.native_version().unwrap_or_default());
                println!("    auth: {auth}");
            }
            Some(Via::Gg) => {
                any = true;
                println!("  + {:<8} bootstrapped on first run{tag}", agent.name());
                println!("    auth: {auth}");
            }
            None => println!("  x {:<8} not found", agent.name()),
        }
    }
    if !any {
        let names = agents::ALL.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ");
        println!("\nNo agent CLIs found. Install one of: {names}. Or run postmortemthis");
        println!("through postmortemthis.cmd, which bootstraps them itself.");
    }
    Ok(())
}

/// Resolve agent names to `Agent`s, erroring on an unknown name.
fn parse_agents(names: &[String]) -> Result<Vec<Agent>> {
    names
        .iter()
        .map(|s| {
            Agent::from_name(s).ok_or_else(|| {
                let known = agents::ALL.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ");
                anyhow::anyhow!("unknown agent '{s}' (known: {known})")
            })
        })
        .collect()
}

/// Resolve the agents to run and the effective skip-native set, honouring the
/// saved setup preferences: a forced-OpenRouter agent (Mode::Openrouter) joins
/// the skip-native set so it never tries its native login. Disabling is applied
/// inside `select_agents` (it only affects a default, non-explicit run).
fn plan_run(
    requested: &[String],
    user_skip: &[String],
    settings: &settings::Settings,
) -> Result<(Vec<Agent>, Vec<Agent>)> {
    let selected = select_agents(requested, settings)?;
    let mut skip = parse_agents(user_skip)?;
    // Force-OpenRouter is best-effort: only drop the native login when OpenRouter
    // is actually reachable (a key is present and the agent supports it).
    // Otherwise leave the native login in play - forcing a route that can't run
    // would turn a working agent into a guaranteed empty-plan failure every run.
    let or_reachable = openrouter::key().is_some();
    for &agent in &selected {
        if settings.mode(agent) == settings::Mode::Openrouter
            && or_reachable
            && agent.supports_openrouter()
            && !skip.contains(&agent)
        {
            skip.push(agent);
        }
    }
    Ok((selected, skip))
}

/// Bring up per-run scratch state (Vibe's VIBE_HOME), prewarm the gg tools, and
/// fan out. Shared by `run` and `setup`'s test so both take the same path; the
/// Vibe home guard is held until run_all returns.
fn execute(
    selected: &[Agent],
    skip_native: &[Agent],
    prompt: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<Vec<Report>> {
    let _vibe = start_vibe_home(selected);
    prewarm(selected, cwd);
    runner::run_all(selected, prompt, cwd, timeout, skip_native)
}

fn select_agents(requested: &[String], settings: &settings::Settings) -> Result<Vec<Agent>> {
    let explicit = !requested.is_empty();
    let requested_agents = if explicit {
        parse_agents(requested)?
    } else {
        agents::ALL.to_vec()
    };
    // Drop agents disabled in `setup` - authoritatively, even when named on an
    // explicit --agents list. "Disable" means "never run this"; since the skill
    // passes an explicit list on nearly every run, an override there would make
    // disable a silent no-op. Note it when an explicitly-named agent is dropped.
    let candidates: Vec<Agent> = requested_agents
        .into_iter()
        .filter(|a| {
            if settings.mode(*a) == settings::Mode::Disabled {
                if explicit {
                    eprintln!(
                        "postmortemthis: skipping {} (disabled in setup - run `postmortemthis setup` to re-enable)",
                        a.name()
                    );
                }
                return false;
            }
            true
        })
        .collect();

    let mut selected: Vec<Agent> = Vec::new();
    for agent in candidates {
        if selected.contains(&agent) {
            continue;
        }
        match agent.via() {
            // Auto-pick an available agent (native on PATH or gg-bootstrappable)
            // only if it can actually run: explicitly requested, its own login, or
            // an OpenRouter key it can actually use. The key check is gated on
            // supports_openrouter() so a native-only agent (antigravity, grok) is
            // not auto-selected on key-presence alone only to fail with an empty
            // attempt plan. Native and gg share this gate - otherwise an
            // installed-but-logged-out native-only agent (grok) would be selected
            // unconditionally and fail on every default run. --agents overrides.
            Some(_)
                if explicit
                    || agent.authed()
                    || (openrouter::key().is_some() && agent.supports_openrouter()) =>
            {
                selected.push(agent)
            }
            Some(_) => {
                // Native-only agents (antigravity, grok) have no OpenRouter
                // route, so --key can't help them - don't suggest it.
                let how = if agent.supports_openrouter() {
                    "log in once, or pass --key"
                } else {
                    "log in once (native-only; no --key fallback)"
                };
                eprintln!("postmortemthis: skipping {} (no credentials - {how})", agent.name());
            }
            None if explicit => bail!(
                "agent '{}' was requested but is not installed and no gg.cmd is available",
                agent.name()
            ),
            None => {}
        }
    }
    if selected.is_empty() {
        bail!(
            "no agents to run - none are installed with usable credentials, or all \
             selected agents are disabled in setup (run `postmortemthis doctor` or `setup`)"
        );
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

