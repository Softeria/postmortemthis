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
use std::process::Command;
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
    /// Interactively configure agents (keep, log in, force OpenRouter, or
    /// disable) and optionally fire a test prompt. Pass an AGENT to configure
    /// just that one; omit it for all. Saved to agents.json.
    Setup {
        /// Agent to configure (default: all).
        agent: Option<String>,
    },
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
        Some(Cmd::Setup { agent }) => {
            // Resolve an optional agent name via the same parser (and error
            // message) as --agents; None means configure all.
            let target = match &agent {
                Some(name) => Some(parse_agents(std::slice::from_ref(name))?[0]),
                None => None,
            };
            setup::run(target)
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

    let settings = settings::Settings::load();
    let (selected, skip_native) = plan_run(&args.agents, &args.skip_native, &settings)?;
    let cwd = std::env::current_dir()?;
    let timeout = Duration::from_secs(args.timeout);

    if !args.no_update
        && let Some(gg) = gg::locate()
    {
        // Scoped: update only the agents this run uses (plus any borrowed
        // OpenRouter runner, e.g. codex for grok), in parallel - not the user's
        // whole gg toolchain, and never postmortemthis itself.
        let tools = gg_tools_for(&selected);
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

    // Antigravity's leg is held read-only by a persona, not a sandbox (see
    // agents::Agent::args), so "it did not write" is an assumption about a CLI
    // that moves fast. Better to notice than to trust a comment.
    let tree_before = tree_state(&cwd);

    let reports = execute(&selected, &skip_native, &prompt, &cwd, timeout)?;

    let tree_touched = selected.contains(&Agent::Antigravity)
        && tree_before.is_some()
        && tree_state(&cwd) != tree_before;

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

    let notes = run_notes(&reports, &selected, &settings, tree_touched);
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
        let model = r.agent.openrouter_model().expect("ran on OpenRouter, so has a model");
        format!("via OpenRouter: {model}")
    } else {
        "native login".to_string()
    };
    let body = match &r.outcome {
        Outcome::Ok => r.output.trim().to_string(),
        Outcome::TimedOut => "_timed out_".to_string(),
        Outcome::Failed(why) => {
            let detail = condense_failure(&r.stderr);
            if detail.is_empty() {
                format!("_failed: {why}_")
            } else {
                format!("_failed: {why}_\n\n```\n{detail}\n```")
            }
        }
    };
    format!("# {} ({provenance})\n\n{body}\n", r.agent.name())
}

/// Trim a failed agent's stderr down to something readable. Agents often log the
/// same error several times (grok prints a 403 five times over), colour it with
/// ANSI escapes, and pad it with blank lines; strip the colour, drop the blanks,
/// collapse exact-duplicate lines, and cap the total so one agent's wall of text
/// can't bury the others. The synthesizing caller still gets the message - once.
fn condense_failure(stderr: &str) -> String {
    const MAX_LINES: usize = 15;
    let cleaned = strip_ansi(stderr);
    let mut seen = std::collections::HashSet::new();
    let mut lines: Vec<&str> = Vec::new();
    for line in cleaned.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if seen.insert(line) {
            lines.push(line);
        }
    }
    if lines.len() > MAX_LINES {
        let omitted = lines.len() - MAX_LINES;
        lines.truncate(MAX_LINES);
        return format!("{}\n... ({omitted} more line(s) omitted)", lines.join("\n"));
    }
    lines.join("\n")
}

/// Drop ANSI CSI escape sequences (colours, styles) an agent wrote to a pipe.
/// A CSI run is ESC `[` ... up to a letter terminator; other escapes just lose
/// the lone ESC. Kept dependency-free (no regex) for such a small need.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for nc in chars.by_ref() {
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// Under this many characters, an agent that exited cleanly probably answered
/// with a placeholder, not a review. A terse-but-real reply costs one note; a
/// lost leg costs the caller an opinion it thinks it had.
const THIN_OUTPUT: usize = 300;

/// Operational notes for the calling agent: what it can fix or change on a
/// later run. Empty when there is nothing worth saying. Kept terse and
/// imperative because the consumer is an LLM composing the next command.
fn run_notes(
    reports: &[Report],
    selected: &[Agent],
    settings: &settings::Settings,
    tree_touched: bool,
) -> Vec<String> {
    let mut notes = Vec::new();

    if tree_touched {
        notes.push(
            "- the working tree changed while the agents ran. Every leg but antigravity is held read-only by its own CLI; antigravity's plan mode is a persona, not a sandbox, so it is the one to suspect. Run `git status` and `git diff` before trusting the review above.".to_string(),
        );
    }

    for r in reports {
        let name = r.agent.name();
        if r.fell_back {
            notes.push(format!(
                "- {name}: native login failed, so it ran on OpenRouter ({}). To restore the native login, {}. To skip the wasted retry on later runs, add `--skip-native {name}`.",
                r.agent.openrouter_model().expect("fell back to OpenRouter, so has a model"),
                r.agent.native_fix_hint(),
            ));
        }
        if r.outcome == Outcome::TimedOut {
            notes.push(format!(
                "- {name}: timed out. Raise --timeout or shorten the prompt."
            ));
        }
        // Exit status is a poor test of "answered": an agent can exit 0 having
        // printed "I have created a plan, please review it", or nothing at all
        // (claude via OpenRouter, see openrouter_env). Both render as a normal
        // section, so the caller reads a silent leg as one with nothing to add.
        let len = r.output.trim().chars().count();
        if r.outcome == Outcome::Ok && len < THIN_OUTPUT {
            notes.push(format!(
                "- {name}: exited cleanly but returned only {len} characters. That is usually a placeholder ('I have created a plan...'), not an answer - read its section before counting it as an opinion, and re-run that agent alone if you need it."
            ));
        }
    }

    // Agents that exist but were not run for lack of usable credentials. An
    // OpenRouter-capable agent is runnable whenever a key is present (whether or
    // not it was requested), so flag it only when there is no key. A native-only
    // agent (antigravity) can't use the key at all, so flag it whenever it
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
    let has_key = openrouter::key().is_some();
    for &agent in &selected {
        // openrouter_reachable (not supports_openrouter) so a borrowed-harness
        // agent (grok -> codex) isn't forced onto OpenRouter when its runner is
        // missing - that would drop the working native leg for an empty plan.
        if settings.mode(agent) == settings::Mode::Openrouter
            && has_key
            && agent.openrouter_reachable()
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
/// `git status --porcelain` for the review cwd: a cheap tree fingerprint,
/// compared before and after a run. None outside a git repo - postmortemthis
/// runs in plain folders too. Never printed.
fn tree_state(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

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
            // supports_openrouter() so a native-only agent (antigravity) is
            // not auto-selected on key-presence alone only to fail with an empty
            // attempt plan. Native and gg share this gate - otherwise an
            // installed-but-logged-out native-only agent would be selected
            // unconditionally and fail on every default run. --agents overrides.
            Some(_)
                if explicit
                    || agent.authed()
                    || (openrouter::key().is_some() && agent.openrouter_reachable()) =>
            {
                selected.push(agent)
            }
            Some(_) => {
                // Native-only agents (antigravity) have no OpenRouter
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

/// The gg tool names needed to run `selected`: each agent's own tool plus, for
/// an agent that borrows another's OpenRouter harness (grok -> codex), that
/// runner's tool - so the borrowed CLI is prewarmed too, not downloaded inside
/// the per-agent timeout on fallback. Deduped, gg-bootstrappable agents only.
fn gg_tools_for(selected: &[Agent]) -> Vec<&'static str> {
    // Only prewarm a borrowed OpenRouter runner (codex for grok) when a key makes
    // that leg reachable - otherwise it's a wasted download for a keyless run.
    let borrow_runners = openrouter::key().is_some();
    let mut tools: Vec<&str> = selected
        .iter()
        .filter(|a| a.via() == Some(Via::Gg))
        .flat_map(|a| {
            let mut v = vec![a.gg_tool()];
            let runner = a.openrouter_runner();
            if borrow_runners && runner != *a {
                v.push(runner.gg_tool());
            }
            v
        })
        .collect();
    tools.sort_unstable();
    tools.dedup();
    tools
}

/// One chained gg invocation prepares every needed tool in parallel before the
/// fan-out, so the per-agent timeout is spent running, not bootstrapping.
fn prewarm(selected: &[Agent], dir: &std::path::Path) {
    let tools = gg_tools_for(selected);
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
    match vibe::Home::create(Agent::Vibe.openrouter_model().expect("vibe has an OpenRouter model")) {
        Ok(home) => Some(home),
        Err(e) => {
            eprintln!("postmortemthis: could not prepare vibe home ({e}); the vibe leg will fail");
            None
        }
    }
}

