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
        let exe = std::env::current_exe().context("cannot locate own executable for the gemshim bridge")?;
        let port = free_port().context("no free loopback port for gemshim")?;
        let home = make_gemini_home(port)?;

        // The bridge is this same binary's hidden `__gemshim` subcommand, so
        // it can never version-mismatch the launcher.
        let child = std::process::Command::new(&exe)
            .arg("__gemshim")
            .env("OPENROUTER_API_KEY", or_key)
            .env("GEMSHIM_PORT", port.to_string())
            .env("GEMSHIM_MODEL", model)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn gemshim bridge ({})", exe.display()))?;

        wait_ready(port).context("gemshim bridge did not come up on its port")?;
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

/// A throwaway HOME with a `.gemini/settings.json` selecting API-key auth and
/// trusting the workspace, so the gemini CLI runs headless against the bridge
/// without touching the user's real ~/.gemini. Read-only is enforced by the
/// CLI's `--approval-mode default` (see agents.rs), not by a policy file.
fn make_gemini_home(_port: u16) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("postmortemthis-gemini-{}", std::process::id()));
    let gem = dir.join(".gemini");
    std::fs::create_dir_all(&gem)?;
    std::fs::File::create(gem.join("settings.json"))?.write_all(GEMINI_SETTINGS.as_bytes())?;
    Ok(dir)
}

/// gemini-cli settings: API-key auth (the bridge supplies a dummy key; gemshim
/// swaps in the real OpenRouter key) + folder trust enabled.
const GEMINI_SETTINGS: &str = r#"{
  "security": {
    "auth": { "selectedType": "gemini-api-key" },
    "folderTrust": { "enabled": true }
  }
}"#;
