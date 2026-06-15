//! Launcher side of the Gemini-on-OpenRouter bridge.
//!
//! The `gemini` CLI speaks only Google's protocol, which OpenRouter doesn't
//! expose. `gemshim` (a sibling binary) translates Gemini <-> OpenAI, so the
//! CLI can run on an OpenRouter key. When the Gemini leg needs OpenRouter we
//! spawn one gemshim on a loopback port, point the CLI at it, and tear it
//! down - same disposable-subprocess pattern as bootstrapping a CLI.

use anyhow::{Context, Result};
use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Child;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Live bridge: the gemshim process plus the throwaway HOME holding the
/// gemini settings. Dropping it kills the process and removes the dir.
pub struct Bridge {
    child: Child,
    home: PathBuf,
    port: u16,
}

/// Set once when the bridge comes up, so `agents::command` can read the
/// endpoint without threading it through every call.
static ENDPOINT: OnceLock<(u16, PathBuf)> = OnceLock::new();

/// gemshim endpoint (loopback port, gemini HOME) once the bridge is up.
pub fn endpoint() -> Option<(u16, PathBuf)> {
    ENDPOINT.get().cloned()
}

impl Bridge {
    /// Spawn gemshim against `or_key` and prepare a gemini HOME. `model` is
    /// the OpenRouter slug gemshim forwards to. Returns the running bridge;
    /// the caller holds it for the lifetime of the review.
    pub fn start(or_key: &str, model: &str) -> Result<Bridge> {
        let bin = locate().context(
            "gemshim binary not found next to postmortem - the Gemini-on-OpenRouter \
             bridge needs it (build the workspace, or install both binaries together)",
        )?;
        let port = free_port().context("no free loopback port for gemshim")?;
        let home = make_gemini_home(port)?;

        let child = std::process::Command::new(&bin)
            .env("OPENROUTER_API_KEY", or_key)
            .env("GEMSHIM_PORT", port.to_string())
            .env("GEMSHIM_MODEL", model)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn gemshim ({})", bin.display()))?;

        wait_ready(port).context("gemshim did not come up on its port")?;
        let _ = ENDPOINT.set((port, home.clone()));
        Ok(Bridge { child, home, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Find the gemshim binary: a sibling of the running executable (the layout
/// for both `cargo build` and a paired install), else on PATH.
fn locate() -> Option<PathBuf> {
    let name = if cfg!(windows) { "gemshim.exe" } else { "gemshim" };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    // PATH fallback: trust the bare name and let the spawn error surface.
    which(name)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Ask the OS for an unused loopback port, then release it for gemshim to
/// bind. The window between release and rebind is a race in theory; on
/// loopback for a just-launched child it is not one in practice.
fn free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    listener.local_addr().ok().map(|a| a.port())
}

/// Poll the port until gemshim is accepting connections.
fn wait_ready(port: u16) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("timed out waiting for gemshim on 127.0.0.1:{port}")
}

/// A throwaway HOME with a `.gemini/settings.json` that selects API-key auth,
/// trusts the workspace, and (Policy Engine) auto-allows only read-only git -
/// the headless config gemini-cli 0.46 needs. Isolated so it never touches a
/// user's real ~/.gemini.
fn make_gemini_home(_port: u16) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("postmortem-gemini-{}", std::process::id()));
    let gem = dir.join(".gemini");
    std::fs::create_dir_all(gem.join("policies"))?;
    std::fs::File::create(gem.join("settings.json"))?.write_all(GEMINI_SETTINGS.as_bytes())?;
    std::fs::File::create(gem.join("policies").join("readonly-git.toml"))?
        .write_all(GEMINI_POLICY.as_bytes())?;
    Ok(dir)
}

/// gemini-cli 0.46 settings: API-key auth (the bridge supplies a dummy key;
/// gemshim swaps in the real OpenRouter key) + folder trust enabled.
const GEMINI_SETTINGS: &str = r#"{
  "security": {
    "auth": { "selectedType": "gemini-api-key" },
    "folderTrust": { "enabled": true }
  }
}"#;

/// Policy Engine rule (0.46 replaced `--allowed-tools` with this). Auto-allows
/// only the read-only git commands the review needs; everything else has no
/// allow rule and falls through to `ask_user`, which headless is treated as
/// deny - so a tool call can never run e.g. `git reset --hard` on the changes
/// under review. Run with `--approval-mode default` for this to hold.
/// `commandPrefix` is start-of-string and chained segments are checked
/// individually, so `git diff && rm -rf x` does not get a blanket allow.
const GEMINI_POLICY: &str = r#"[[rule]]
toolName = "run_shell_command"
commandPrefix = ["git diff", "git log", "git show", "git status"]
decision = "allow"
priority = 100
"#;
