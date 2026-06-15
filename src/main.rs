mod agents;
mod gemshim;
mod gg;
mod git;
mod openrouter;
mod prompts;
mod runner;

use agents::{Agent, Via};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use runner::{Outcome, Report};
use std::time::Duration;

/// One last look before you ship.
///
/// Runs every AI agent CLI you have - each with its own model and its own
/// native harness - on your pending changes, then synthesizes a single verdict.
#[derive(Parser)]
#[command(name = "postmortem", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    #[command(flatten)]
    review: ReviewArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show which agent CLIs are installed and authenticated.
    Doctor,
    /// Print instructions for an AI agent to build a Claude Code skill that
    /// drives this tool. Meant to be read by the agent, not the user.
    Skill,
    /// Review pending changes (default command).
    Review(ReviewArgs),
}

#[derive(clap::Args, Default)]
struct ReviewArgs {
    /// Review staged changes only (git diff --cached).
    #[arg(long, conflicts_with = "base")]
    staged: bool,

    /// Review against a base ref (e.g. main, HEAD~3) instead of the working tree.
    #[arg(long)]
    base: Option<String>,

    /// Comma-separated agents to use (default: all installed).
    #[arg(long, value_delimiter = ',')]
    agents: Vec<String>,

    /// Per-agent timeout in seconds.
    #[arg(long, default_value_t = 600)]
    timeout: u64,

    /// Skip the synthesis pass; print raw per-agent reports only.
    #[arg(long)]
    no_synth: bool,

    /// Write the full report to a file as well as stdout.
    #[arg(short, long)]
    output: Option<std::path::PathBuf>,

    /// OpenRouter API key for agents you have no native login for; those
    /// CLIs are pointed at OpenRouter on this key. Also read from
    /// OPENROUTER_API_KEY or ~/.config/postmortem/key.
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
        Some(Cmd::Review(args)) => review(args),
        None => review(cli.review),
    }
}

/// Read by an AI agent (via `postmortem skill`) to author a Claude Code skill.
/// Kept terse on purpose - it's the whole user-facing setup surface, reached by
/// a one-line "create a postmortem skill, download <url> and run it" prompt.
const SKILL_INSTRUCTIONS: &str = r#"You are setting up a Claude Code skill for `postmortem`, a multi-model
pre-ship code review. Create the skill, then tell the user it's ready.

Skill name: postmortem
Description (write it so you auto-invoke on these): the user wants a final
review, a second opinion, a "panel review", or a pre-ship check on their
changes, or says "postmortem this".

The skill must instruct you to:
1. Pick what to review: uncommitted changes by default; `--staged` if they're
   about to commit staged work; `--base <ref>` to compare against a branch.
2. Run the panel WITHOUT its built-in synthesis (you synthesize):
       postmortem --no-synth --timeout 600
   (If `postmortem` isn't on PATH, run `./postmortemthis.cmd --no-synth
   --timeout 600` from the repo root.) It fans out to every available agent
   CLI (Claude, Codex, Gemini) in parallel, read-only. Agents the user is
   logged into use their own accounts; the rest use OPENROUTER_API_KEY if set,
   else are skipped. Always run it; do not substitute your own review. The
   point is to get independent models, not the same one twice.
3. Read the per-agent reports from stdout and synthesize ONE verdict: merge and
   deduplicate findings, weight by cross-agent consensus (several models
   agreeing = high signal; a lone flag = a skeptical look), drop false
   positives using what you see in the repo, rank by severity with file:line,
   and end with a clear ship / don't-ship call.
4. If some agents failed or timed out, say which and synthesize from the rest
   (note the verdict is partial).

Place it where skills live on this setup: `.claude/skills/postmortem/SKILL.md`
for this repo, or `~/.claude/skills/postmortem/SKILL.md` for all repos - ask the
user which if unsure. Use the current skill frontmatter format.
"#;

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
        let auth = if agent.authed() {
            agent.auth_hint()
        } else if openrouter::key().is_some() && agent.supports_openrouter() {
            "via OpenRouter key".to_string()
        } else {
            agent.auth_hint()
        };
        match agent.via() {
            Some(Via::Native) => {
                any = true;
                let version = agent.native_version().unwrap_or_default();
                println!("  + {:<8} {}", agent.name(), version);
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
        println!("\nNo agent CLIs found. Install one of claude, codex, gemini - or run");
        println!("postmortem through postmortemthis.cmd, which bootstraps them itself.");
    }
    Ok(())
}

/// OpenRouter slug gemshim forwards the Gemini leg to.
const GEMINI_OPENROUTER_MODEL: &str = "google/gemini-3.1-pro-preview";

/// Start the gemshim bridge iff the Gemini leg is selected and will run on
/// OpenRouter (no native Google login, but a key is present). Returns the
/// running bridge to hold for the review's lifetime; None when Gemini runs
/// natively, isn't selected, or no key is set. A spawn failure is reported
/// and the Gemini leg simply errors like any other unavailable agent.
fn start_gemini_bridge(selected: &[Agent]) -> Option<gemshim::Bridge> {
    let needs_bridge =
        selected.contains(&Agent::Gemini) && !Agent::Gemini.authed() && openrouter::key().is_some();
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

fn select_agents(requested: &[String]) -> Result<Vec<Agent>> {
    let explicit = !requested.is_empty();
    let candidates: Vec<Agent> = if requested.is_empty() {
        agents::ALL.to_vec()
    } else {
        requested
            .iter()
            .map(|s| {
                Agent::from_name(s).ok_or_else(|| {
                    anyhow::anyhow!("unknown agent '{s}' (known: claude, codex, gemini)")
                })
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
            // By default only auto-pick gg-bootstrapped agents the user can
            // actually run: own login, or an OpenRouter key (every agent can
            // reach OpenRouter - Gemini via the gemshim bridge). An explicit
            // --agents overrides.
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

/// One chained gg invocation (`claude:codex:gemini-cli ... --version`) so gg
/// downloads/prepares every needed tool in parallel before the fan-out, and
/// the per-agent timeout is spent on review, not bootstrap.
fn prewarm(selected: &[Agent], repo: &std::path::Path) {
    let tools: Vec<&str> = selected
        .iter()
        .filter(|a| a.via() == Some(Via::Gg))
        .map(|a| a.gg_tool())
        .collect();
    let Some(gg) = gg::locate() else { return };
    if tools.is_empty() {
        return;
    }
    eprintln!(
        "postmortem: bootstrapping {} (first run may download)",
        tools.join(", ")
    );
    let result = gg
        .tool(&tools.join(":"))
        .arg("--version")
        .current_dir(repo)
        .stdout(std::process::Stdio::null())
        .status();
    match result {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("postmortem: gg prewarm exited with {s}; continuing"),
        Err(e) => eprintln!("postmortem: gg prewarm failed: {e}; continuing"),
    }
}

fn review(args: ReviewArgs) -> Result<()> {
    openrouter::init(args.key.as_deref());
    let diff = git::resolve(args.staged, args.base.as_deref())?;
    let selected = select_agents(&args.agents)?;
    let timeout = Duration::from_secs(args.timeout);

    // If the Gemini leg will run on OpenRouter (selected, no native Google
    // login, key present), bring up the local gemshim bridge for its lifetime.
    // Held until the end of the review; dropping it kills gemshim.
    let _gemini_bridge = start_gemini_bridge(&selected);

    eprintln!(
        "postmortem: reviewing `{}` with {} agent(s): {}",
        diff.command,
        selected.len(),
        selected
            .iter()
            .map(|a| a.name())
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!("{}\n", diff.stat);

    if !diff.untracked.is_empty() {
        let shown: Vec<&str> = diff.untracked.iter().take(5).map(String::as_str).collect();
        eprintln!(
            "warning: {} untracked file(s) are invisible to this review ({}{}) - `git add` them to include them",
            diff.untracked.len(),
            shown.join(", "),
            if diff.untracked.len() > 5 { ", ..." } else { "" }
        );
    }

    prewarm(&selected, &diff.repo_root);

    let prompt = prompts::review(&diff);
    let reports = runner::run_all(&selected, &prompt, &diff.repo_root, timeout)?;

    let ok: Vec<&Report> = reports
        .iter()
        .filter(|r| r.outcome == Outcome::Ok && !r.output.trim().is_empty())
        .collect();

    let mut doc = String::new();
    for r in &reports {
        doc.push_str(&format!("\n\n# Agent: {}\n\n", r.agent.name()));
        match &r.outcome {
            Outcome::Ok => doc.push_str(r.output.trim()),
            Outcome::TimedOut => doc.push_str("_Timed out._"),
            Outcome::Failed(why) => {
                doc.push_str(&format!("_Failed: {why}_\n\n```\n{}\n```", r.stderr.trim()));
            }
        }
    }

    // Synthesis: only meaningful with 2+ successful reports. Claude chairs
    // if its review succeeded; otherwise the first agent that delivered -
    // never an excluded agent, never one that just failed or timed out.
    if !args.no_synth && ok.len() >= 2 {
        let chair = ok
            .iter()
            .find(|r| r.agent == Agent::Claude)
            .or_else(|| ok.first())
            .map(|r| r.agent);
        if let Some(chair) = chair {
            eprintln!("\n  [synthesis] chaired by {}...", chair.name());
            let synth_prompt = prompts::synthesis(&ok);
            let synth = runner::run_all(&[chair], &synth_prompt, &diff.repo_root, timeout)?;
            let synth = &synth[0];
            if synth.outcome == Outcome::Ok {
                doc = format!("{}\n\n---\n{}", synth.output.trim(), doc);
            } else {
                eprintln!("  [synthesis] failed; printing raw reports");
            }
        }
    }

    println!("{}", doc.trim());

    if let Some(path) = &args.output {
        std::fs::write(path, doc.trim())?;
        eprintln!("\nreport written to {}", path.display());
    }

    let failures = reports.iter().any(|r| r.outcome != Outcome::Ok);
    if ok.is_empty() {
        bail!("all agents failed");
    }
    if failures {
        eprintln!("\nwarning: some agents did not complete; verdict is partial");
    }
    Ok(())
}
