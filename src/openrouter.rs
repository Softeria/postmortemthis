use std::sync::OnceLock;

/// An OpenRouter API key. Agents the user has no native login for get pointed
/// at OpenRouter on this key, so one key fills in the whole panel without the
/// user holding a separate account at every provider. OpenRouter handles
/// pricing, billing, and provider routing - postmortemthis holds no keys and runs
/// no infrastructure.
///
/// There is no region concept: OpenRouter routes globally, so unlike a
/// self-hosted proxy there is no jurisdiction for us to select.
static KEY: OnceLock<Option<String>> = OnceLock::new();

/// Resolve the key once per process. `flag_key` comes from `--key`; otherwise
/// `OPENROUTER_API_KEY`, then `~/.config/postmortemthis/key`.
pub fn init(flag_key: Option<&str>) -> Option<&'static str> {
    KEY.get_or_init(|| {
        // Filter each source before falling through: an empty OPENROUTER_API_KEY
        // must NOT mask a real key in the file (it would otherwise win the
        // or_else chain as Some("") and stop the fallback).
        let clean = |s: String| {
            let t = s.trim().to_string();
            (!t.is_empty()).then_some(t)
        };
        flag_key
            .map(str::to_string)
            .and_then(clean)
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok().and_then(clean))
            .or_else(|| read_key_file().and_then(clean))
    })
    .as_deref()
}

/// The already-resolved key (None until/unless `init` found one).
pub fn key() -> Option<&'static str> {
    KEY.get().and_then(Option::as_deref)
}

/// The postmortemthis config directory, `~/.config/postmortemthis`. Holds the
/// OpenRouter key and the setup wizard's per-agent preferences. Deliberately
/// under config, not cache: a wiped cache must not reset the user's choices.
pub fn config_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        std::path::PathBuf::from(home)
            .join(".config")
            .join("postmortemthis"),
    )
}

pub fn key_file_path() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("key"))
}

fn read_key_file() -> Option<String> {
    std::fs::read_to_string(key_file_path()?).ok()
}
