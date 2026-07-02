//! `postmortemthis setup`: an interactive wizard that pokes each agent (like
//! doctor) and lets the user keep it, log in, force OpenRouter, disable, or
//! reset, then optionally fires a test prompt. Choices persist in agents.json
//! and are honoured by every later run (see `settings` and `plan_run`).
//!
//! The menus use dialoguer (arrow keys + Enter, colour theme) rather than
//! read-a-line prompts. A single AGENT can be passed to configure just that one.

use crate::agents::{self, Agent};
use crate::runner::Outcome;
use crate::settings::{Mode, Settings};
use crate::{login, openrouter};
use anyhow::{Result, bail};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Select};
use std::io::IsTerminal;
use std::time::Duration;

/// What a menu choice does to an agent's saved mode.
#[derive(Clone, Copy)]
enum Action {
    Keep,
    Login,
    ForceOpenrouter,
    Disable,
    Reset,
}

pub fn run(target: Option<Agent>) -> Result<()> {
    // The wizard reads keys from the terminal and hands the terminal to each
    // agent's login flow, so it is useless without a real TTY. dialoguer renders
    // on and reads from stderr, so require that stream too, or `setup 2>file`
    // slips past this guard and dies with a raw "not a terminal" mid-prompt.
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        bail!("setup is interactive - run it directly in a terminal, not piped");
    }

    // dialoguer hides the cursor during each prompt and only restores it on a
    // normal exit; on Ctrl-C, console re-raises SIGINT and the process dies with
    // the cursor still hidden. Restore it (and exit cleanly) ourselves.
    let _ = ctrlc::set_handler(|| {
        use std::io::Write;
        let _ = std::io::stderr().write_all(b"\x1b[?25h");
        std::process::exit(130);
    });

    let agents: Vec<Agent> = match target {
        Some(a) => vec![a],
        None => agents::ALL.to_vec(),
    };
    let theme = ColorfulTheme::default();

    println!("postmortemthis setup\n");
    if target.is_none() {
        println!("Configure each agent CLI. Many have a free or already-paid-for tier,");
        println!("so logging in is often worth it.\n");
    }

    // Offer to connect an OpenRouter key up front, but only if it could help an
    // agent in scope (skip it for a lone native-only agent). Done BEFORE
    // openrouter::init so a freshly-saved key is seen by the pokes and test run;
    // check env/file directly with init's non-empty semantics since init caches.
    let had_key = std::env::var("OPENROUTER_API_KEY").is_ok_and(|v| !v.trim().is_empty())
        || openrouter::key_file_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|s| !s.trim().is_empty());
    let key_helps = agents.iter().any(|a| a.supports_openrouter());
    if !had_key
        && key_helps
        && Confirm::with_theme(&theme)
            .with_prompt("No OpenRouter key found (lets agents with no native login run, and adds a fallback). Connect one now?")
            .default(false)
            .interact()?
    {
        // Optional - a network/OAuth hiccup must not throw the user out of setup.
        if let Err(e) = login::run() {
            println!("Couldn't connect a key ({e}); continuing without one.");
        }
        println!();
    }
    openrouter::init(None);
    let has_key = openrouter::key().is_some();

    let mut settings = Settings::load();
    for &agent in &agents {
        configure_agent(agent, &mut settings, has_key, &theme)?;
        // Persist after each choice: on Ctrl-C, console re-raises SIGINT and the
        // process can die mid-loop, so saving only at the end would discard
        // every decision already made.
        settings.save()?;
    }

    println!(
        "\nSaved to {}.\n",
        Settings::path().map(|p| p.display().to_string()).unwrap_or_default()
    );
    print_summary(&agents, &settings, has_key);

    // Blank line as a real println, not baked into the prompt: dialoguer counts
    // its own render lines to clear on redraw, so an embedded \n garbles it.
    println!();
    if Confirm::with_theme(&theme)
        .with_prompt("Run a quick test prompt now?")
        .default(false)
        .interact()?
    {
        test_run(&settings, target)?;
    }
    Ok(())
}

/// Poke, present a capability-aware menu, and apply the choice for one agent.
fn configure_agent(
    agent: Agent,
    settings: &mut Settings,
    has_key: bool,
    theme: &ColorfulTheme,
) -> Result<()> {
    let current = settings.mode(agent);
    println!();
    println!("  {}", poke(agent, has_key));
    if current != Mode::Auto {
        println!("  current setting: {}", current.label());
    }

    // The menu adapts to capability: 'log in' only for agents with a native
    // login (not qwen/vibe), 'force OpenRouter' only for those that have BOTH a
    // login and a route (claude/codex), 'reset' only when a non-default is set.
    let can_login = agent.has_native_login();
    let mut items: Vec<(&str, Action)> = vec![("keep as-is", Action::Keep)];
    if can_login {
        items.push(("log in now", Action::Login));
    }
    if can_login && agent.supports_openrouter() {
        items.push(("force OpenRouter", Action::ForceOpenrouter));
    }
    items.push(("disable", Action::Disable));
    if current != Mode::Auto {
        items.push(("reset to auto", Action::Reset));
    }

    let labels: Vec<&str> = items.iter().map(|(l, _)| *l).collect();
    let idx = Select::with_theme(theme)
        .with_prompt(format!("{} ({})", agent.name(), agent.vendor()))
        .items(&labels)
        .default(0)
        .interact()?;

    match items[idx].1 {
        Action::Keep => {}
        Action::Login => {
            // Set Auto before launching the login so the intent (enable +
            // native path) survives even if the login is interrupted.
            settings.set(agent, Mode::Auto);
            do_login(agent)?;
        }
        Action::ForceOpenrouter => {
            settings.set(agent, Mode::Openrouter);
            if !has_key {
                println!("  note: forced OpenRouter needs a key - none set yet.");
            }
        }
        Action::Disable => settings.set(agent, Mode::Disabled),
        Action::Reset => settings.set(agent, Mode::Auto),
    }
    Ok(())
}

/// One-line prediction of what a run will do with this agent right now, mirroring
/// the select/attempt logic so the wizard tells the truth.
fn poke(agent: Agent, has_key: bool) -> String {
    if agent.via().is_none() {
        return "not installed, and no gg to bootstrap it".into();
    }
    if agent.authed() {
        return format!("{} - native login will be used", agent.auth_hint());
    }
    if has_key && let Some(model) = agent.openrouter_model() {
        return format!("no login - will run on OpenRouter ({model})");
    }
    if agent.has_native_login() {
        "not logged in - won't run until you log in".into()
    } else {
        "no OpenRouter key - won't run until one is set".into()
    }
}

/// What a run will do given the saved mode (folds disabled / forced-OpenRouter
/// over the live poke). Used for the closing summary.
fn effective(agent: Agent, settings: &Settings, has_key: bool) -> String {
    match settings.mode(agent) {
        Mode::Disabled => "disabled".into(),
        // Forced OpenRouter only takes effect with a usable route; plan_run falls
        // back to native otherwise, so the summary must say so, not overstate it.
        Mode::Openrouter if has_key && agent.supports_openrouter() => {
            let model = agent.openrouter_model().expect("supports_openrouter, so has a model");
            format!("forced OpenRouter ({model})")
        }
        Mode::Openrouter => format!("forced OpenRouter (inactive, no key) - {}", poke(agent, has_key)),
        Mode::Auto => poke(agent, has_key),
    }
}

/// Launch the agent's login interactively, then re-poke so the user sees whether
/// it took. The login inherits this terminal; we wait for it to exit.
fn do_login(agent: Agent) -> Result<()> {
    let Some(mut cmd) = agent.login_command() else {
        println!("  ({} has no native login)", agent.name());
        return Ok(());
    };
    println!("  launching {} - complete the login, then quit it to continue...", agent.name());
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(_) => println!("  (login exited non-zero; it may not have completed)"),
        Err(e) => println!("  (couldn't launch {}: {e})", agent.name()),
    }
    if agent.authed() {
        println!("  logged in.");
    } else {
        println!("  not detected as logged in (headless auth may need an API key env var).");
    }
    Ok(())
}

fn print_summary(agents: &[Agent], settings: &Settings, has_key: bool) {
    println!("Configuration:");
    for &agent in agents {
        println!("  {:<12} {}", agent.name(), effective(agent, settings, has_key));
    }
}

/// Fire a trivial prompt across the agents a run would use (all enabled, or just
/// `target`) and report pass/fail per agent - a quick confidence check.
fn test_run(settings: &Settings, target: Option<Agent>) -> Result<()> {
    let requested: Vec<String> = target.map(|a| vec![a.name().to_string()]).unwrap_or_default();
    // plan_run bails when nothing is selectable; with no name to mis-parse that
    // is the only error here, so treat it as "nothing to test" rather than
    // letting the generic doctor bail escape the wizard.
    // plan_run's bail carries the real reason (all disabled, nothing installed,
    // no usable credentials) - print it verbatim rather than guessing.
    let (selected, skip) = match crate::plan_run(&requested, &[], settings) {
        Ok(v) => v,
        Err(e) => {
            println!("\nNothing to test - {e}");
            return Ok(());
        }
    };
    let names = selected.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ");
    println!("\nTesting: {names}\n");
    let cwd = std::env::current_dir()?;
    let reports = crate::execute(
        &selected,
        &skip,
        "Reply with exactly one word: OK",
        &cwd,
        Duration::from_secs(120),
    )?;
    println!("\nResult:");
    for r in &reports {
        match &r.outcome {
            Outcome::Ok => println!("  {:<12} ok", r.agent.name()),
            Outcome::TimedOut => println!("  {:<12} timed out", r.agent.name()),
            Outcome::Failed(_) => {
                // Surface the first non-empty line of output so a billing/auth
                // wall (e.g. a Grok 403) is visible, not just "failed".
                let reason = r
                    .stderr
                    .lines()
                    .chain(r.output.lines())
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("failed");
                let reason: String = reason.chars().take(90).collect();
                println!("  {:<12} failed - {reason}", r.agent.name());
            }
        }
    }
    Ok(())
}
