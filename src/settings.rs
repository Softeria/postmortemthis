//! Per-agent preferences chosen in `setup`, persisted next to the OpenRouter
//! key in `~/.config/postmortemthis/agents.json`. Only non-default modes are
//! written, so a fresh user has no file and every agent is `Auto`.

use crate::agents::Agent;
use crate::openrouter;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How the user wants an agent handled on a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Default: native login if usable, else OpenRouter - whatever works.
    Auto,
    /// Always run on OpenRouter, skipping the native login (a persistent
    /// `--skip-native`). Only meaningful for agents that support OpenRouter.
    Openrouter,
    /// Never run this agent on a default run.
    Disabled,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Openrouter => "openrouter",
            Mode::Disabled => "disabled",
        }
    }

    fn parse(s: &str) -> Option<Mode> {
        match s {
            "auto" => Some(Mode::Auto),
            "openrouter" => Some(Mode::Openrouter),
            "disabled" => Some(Mode::Disabled),
            _ => None,
        }
    }

    /// Human label for doctor / setup summaries.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Openrouter => "forced OpenRouter",
            Mode::Disabled => "disabled",
        }
    }
}

/// Agent modes keyed by `Agent::name()`. A missing agent is `Auto`.
pub struct Settings {
    modes: BTreeMap<String, Mode>,
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        Some(openrouter::config_dir()?.join("agents.json"))
    }

    /// Load saved modes. Any missing file or read/parse error yields empty
    /// (all-`Auto`) settings, so a corrupt file never blocks a run.
    pub fn load() -> Settings {
        let mut modes = BTreeMap::new();
        if let Some(path) = Self::path()
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(serde_json::Value::Object(obj)) =
                serde_json::from_str::<serde_json::Value>(&text)
        {
            for (name, val) in obj {
                if let Some(m) = val.as_str().and_then(Mode::parse) {
                    modes.insert(name, m);
                }
            }
        }
        Settings { modes }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("cannot locate a home directory")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Persist only non-default modes: keeps the file minimal, and an agent
        // the user never touched stays on whatever `Auto` means in the future.
        let obj: serde_json::Map<String, serde_json::Value> = self
            .modes
            .iter()
            .filter(|(_, m)| **m != Mode::Auto)
            .map(|(k, m)| (k.clone(), serde_json::Value::String(m.as_str().into())))
            .collect();
        let text = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    pub fn mode(&self, agent: Agent) -> Mode {
        self.modes.get(agent.name()).copied().unwrap_or(Mode::Auto)
    }

    pub fn set(&mut self, agent: Agent, mode: Mode) {
        self.modes.insert(agent.name().to_string(), mode);
    }
}
