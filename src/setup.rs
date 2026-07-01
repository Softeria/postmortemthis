//! `postmortemthis setup`: an interactive wizard that pokes each agent (like
//! doctor) and lets the user keep it, log in, force OpenRouter, or disable it,
//! then optionally fires a test prompt. Choices persist in agents.json and are
//! honoured by every later run (see `settings` and `plan_run`).

use crate::agents::{self, Agent};
use crate::runner::Outcome;
use crate::settings::{Mode, Settings};
use crate::{login, openrouter};
use anyhow::{Result, bail};
use std::io::{IsTerminal, Write};
use std::time::Duration;

pub fn run() -> Result<()> {
    // The wizard reads choices from the terminal and hands the terminal to each
    // agent's login flow, so it is useless without a real TTY.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("setup is interactive - run it directly in a terminal, not piped");
    }

    println!("postmortemthis setup\n");
    println!("Configure each agent CLI once. Many have a free or already-paid-for tier,");
    println!("so logging in is often worth it. Press Enter to keep the current choice.\n");

    // Offer to connect an OpenRouter key up front - it fills in agents with no
    // native login and enables 'force OpenRouter'. Done BEFORE openrouter::init
    // so a freshly-saved key is picked up for the pokes and the test run (init
    // caches once). Check env/file directly since init hasn't run yet.
    // Match openrouter::init's "non-empty" semantics: an empty env var or key
    // file must not suppress the offer (init would resolve it to no key anyway).
    let had_key = std::env::var("OPENROUTER_API_KEY").is_ok_and(|v| !v.trim().is_empty())
        || openrouter::key_file_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|s| !s.trim().is_empty());
    if !had_key
        && yes_no(
            "No OpenRouter key found (optional - fills in qwen/vibe and adds a\n\
             fallback for the rest). Connect one now?",
            false,
        )
    {
        // Connecting a key is optional - a network/OAuth hiccup must not throw the
        // user out of setup before they've configured their native logins.
        if let Err(e) = login::run() {
            println!("Couldn't connect a key ({e}); continuing without one.");
        }
        println!();
    }
    openrouter::init(None);
    let has_key = openrouter::key().is_some();

    let mut settings = Settings::load();
    for agent in agents::ALL {
        configure_agent(agent, &mut settings, has_key)?;
    }
    settings.save()?;

    println!("Saved to {}.\n", Settings::path().map(|p| p.display().to_string()).unwrap_or_default());
    print_summary(&settings, has_key);

    if yes_no("\nRun a quick test prompt across your enabled agents now?", false) {
        test_run(&settings)?;
    }
    Ok(())
}

/// Poke, present a capability-aware menu, and apply the choice for one agent.
fn configure_agent(agent: Agent, settings: &mut Settings, has_key: bool) -> Result<()> {
    let current = settings.mode(agent);
    println!("--- {} ({}) ---", agent.name(), agent.vendor());
    println!("    now: {}", poke(agent, has_key));
    if current != Mode::Auto {
        println!("    setting: {}", current.label());
    }

    // Stable numbering (1 keep, 2 login, 3 force OpenRouter, 4 disable);
    // inapplicable options are dropped. 'Force OpenRouter' only makes sense for
    // an agent that has BOTH a native login and an OpenRouter route - qwen/vibe
    // are OpenRouter-only already, native-only agents have no route.
    let can_login = agent.has_native_login();
    let can_force_or = agent.has_native_login() && agent.supports_openrouter();
    let mut opts: Vec<(&str, &str)> = vec![("1", "keep as-is")];
    if can_login {
        opts.push(("2", "try login now"));
    }
    if can_force_or {
        opts.push(("3", "force OpenRouter"));
    }
    opts.push(("4", "disable"));
    // Without this, a disabled/forced qwen or vibe (no login/force option to
    // route through) would have no way back to the default in the wizard.
    if current != Mode::Auto {
        opts.push(("5", "reset to auto"));
    }
    let menu = opts
        .iter()
        .map(|(c, l)| format!("[{c}] {l}"))
        .collect::<Vec<_>>()
        .join("   ");
    println!("    {menu}");

    match prompt_line("    choice > ").as_str() {
        "" | "1" => {} // keep current setting
        "2" if can_login => {
            do_login(agent)?;
            settings.set(agent, Mode::Auto);
        }
        "3" if can_force_or => {
            settings.set(agent, Mode::Openrouter);
            if !has_key {
                println!("    note: forced OpenRouter needs a key - none set yet.");
            }
        }
        "4" => settings.set(agent, Mode::Disabled),
        "5" if current != Mode::Auto => settings.set(agent, Mode::Auto),
        other => println!("    (unrecognised '{other}' - keeping current)"),
    }
    println!();
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
    if has_key && agent.supports_openrouter() {
        return format!("no login - will run on OpenRouter ({})", agent.openrouter_model());
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
            format!("forced OpenRouter ({})", agent.openrouter_model())
        }
        Mode::Openrouter => format!("forced OpenRouter (inactive, no key) - {}", poke(agent, has_key)),
        Mode::Auto => poke(agent, has_key),
    }
}

/// Launch the agent's login interactively, then re-poke so the user sees whether
/// it took. The login inherits this terminal; we wait for it to exit.
fn do_login(agent: Agent) -> Result<()> {
    let Some(mut cmd) = agent.login_command() else {
        println!("    ({} has no native login)", agent.name());
        return Ok(());
    };
    println!("    launching {} - complete the login, then quit it to continue...", agent.name());
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(_) => println!("    (login exited non-zero; it may not have completed)"),
        Err(e) => println!("    (couldn't launch {}: {e})", agent.name()),
    }
    if agent.authed() {
        println!("    logged in.");
    } else {
        println!("    not detected as logged in (headless auth may need an API key env var).");
    }
    Ok(())
}

fn print_summary(settings: &Settings, has_key: bool) {
    println!("Configuration:");
    for agent in agents::ALL {
        println!("  {:<12} {}", agent.name(), effective(agent, settings, has_key));
    }
}

/// Fire a trivial prompt across the agents a default run would use, and report
/// pass/fail per agent - a quick confidence check that the setup works.
fn test_run(settings: &Settings) -> Result<()> {
    // plan_run bails when nothing is selectable; with no explicit list there is
    // no name to mis-parse, so that is the only error - treat it as "nothing to
    // test" rather than letting the generic doctor bail escape the wizard.
    let Ok((selected, skip)) = crate::plan_run(&[], &[], settings) else {
        println!("\nNothing runnable yet - log in to an agent, or connect an OpenRouter key.");
        return Ok(());
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
        let status = match &r.outcome {
            Outcome::Ok => "ok",
            Outcome::TimedOut => "timed out",
            Outcome::Failed(_) => "failed",
        };
        println!("  {:<12} {status}", r.agent.name());
    }
    Ok(())
}

fn prompt_line(msg: &str) -> String {
    print!("{msg}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

fn yes_no(msg: &str, default_yes: bool) -> bool {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    match prompt_line(&format!("{msg} {hint} ")).to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    }
}
