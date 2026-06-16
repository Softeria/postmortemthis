//! Throwaway VIBE_HOME for running Mistral's `vibe` CLI on OpenRouter.
//!
//! `vibe` has no native login most users hold, and it reads its model and
//! provider list from `$VIBE_HOME/config.toml` (array-of-table entries that
//! env overrides can't reach). So when the vibe leg needs OpenRouter we write
//! a disposable home with a single OpenRouter provider and a read-only default
//! agent, point the CLI at it, and remove it afterwards - the same
//! disposable-scratch pattern as the gemini bridge, minus the server.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

/// A live scratch home. Dropping it removes the directory.
pub struct Home {
    dir: PathBuf,
}

/// Set once the home is written, so `agents::command` can read it without
/// threading it through every call.
static HOME: OnceLock<PathBuf> = OnceLock::new();

/// The scratch VIBE_HOME, once it has been created.
pub fn home() -> Option<PathBuf> {
    HOME.get().cloned()
}

impl Home {
    /// Write a disposable VIBE_HOME whose only model routes to `model` on
    /// OpenRouter (reading the key from `OPENROUTER_API_KEY`). The caller
    /// holds the returned guard for the lifetime of the review.
    pub fn create(model: &str) -> Result<Home> {
        let dir = std::env::temp_dir().join(format!("postmortem-vibe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).context("creating scratch VIBE_HOME")?;
        let config = config_toml(model);
        std::fs::File::create(dir.join("config.toml"))
            .context("creating vibe config.toml")?
            .write_all(config.as_bytes())
            .context("writing vibe config.toml")?;
        let _ = HOME.set(dir.clone());
        Ok(Home { dir })
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Minimal vibe config: one OpenAI-style provider pointed at OpenRouter, one
/// model on it, and a read-only default agent (`plan`). Telemetry and update
/// checks are off so a one-shot stays quiet.
fn config_toml(model: &str) -> String {
    format!(
        r#"active_model = "{model}"
default_agent = "plan"
enable_telemetry = false
enable_update_checks = false
enable_notifications = false

[[providers]]
name = "openrouter"
api_base = "https://openrouter.ai/api/v1"
api_key_env_var = "OPENROUTER_API_KEY"
api_style = "openai"
backend = "generic"

[[models]]
name = "{model}"
provider = "openrouter"
alias = "{model}"
"#
    )
}
