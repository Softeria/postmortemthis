use std::sync::OnceLock;

/// An OpenRouter API key. Agents the user has no native login for get pointed
/// at OpenRouter on this key, so one key fills in the whole panel without the
/// user holding a separate account at every provider. OpenRouter handles
/// pricing, billing, and provider routing - postmortem holds no keys and runs
/// no infrastructure.
///
/// There is no region concept: OpenRouter routes globally, so unlike a
/// self-hosted proxy there is no jurisdiction for us to select.
static KEY: OnceLock<Option<String>> = OnceLock::new();

/// Resolve the key once per process. `flag_key` comes from `--key`; otherwise
/// `OPENROUTER_API_KEY`, then `~/.config/postmortem/key`.
pub fn init(flag_key: Option<&str>) -> Option<&'static str> {
    KEY.get_or_init(|| {
        flag_key
            .map(str::to_string)
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
            .or_else(read_key_file)
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
    })
    .as_deref()
}

/// The already-resolved key (None until/unless `init` found one).
pub fn key() -> Option<&'static str> {
    KEY.get().and_then(Option::as_deref)
}

fn key_file_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        std::path::PathBuf::from(home)
            .join(".config")
            .join("postmortem")
            .join("key"),
    )
}

fn read_key_file() -> Option<String> {
    std::fs::read_to_string(key_file_path()?).ok()
}
