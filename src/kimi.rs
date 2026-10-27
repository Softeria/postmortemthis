//! Scratch files that keep Moonshot's `kimi` CLI read-only, and point it at
//! OpenRouter when it has no login of its own.
//!
//! Kimi has no read-only switch. `--print` auto-approves every tool call, plan
//! mode included (print mode approves the switch out of it), and hooks are no
//! help: SessionStart, PreToolUse and Stop hooks in the real config location
//! all stayed silent in print mode when tried. What does hold is an agent file
//! that names the tools it may have, and only those: read, search, think, and
//! a todo list. An allowlist rather than excluding the writers, so a write
//! tool added or renamed upstream (the leg auto-updates) stays out. Not Shell,
//! not WriteFile/StrReplaceFile, not Agent (its `coder` subagent writes), and
//! not the web tools either - a prompt injection in a diff should have nowhere
//! to send anything. MCP servers are tools too, and the user's `~/.kimi/mcp.json`
//! loads unless `--mcp-config-file` replaces it, so that gets an empty one.
//!
//! No Shell also means no `git diff`, so `agents::decorate_prompt` hands Kimi
//! the working-tree diff in its prompt.

use anyhow::{Context, Result};
use std::fs::DirBuilder;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::OnceLock;

/// A live scratch dir. Dropping it removes the directory.
pub struct Home {
    dir: PathBuf,
}

static HOME: OnceLock<PathBuf> = OnceLock::new();

/// The scratch dir, once it has been created. Without it the kimi leg has no
/// agent file and must not run (see `Agent::attempt_plan`).
pub fn home() -> Option<PathBuf> {
    HOME.get().cloned()
}

impl Home {
    /// Write the agent file, the empty MCP config, and an OpenRouter config on
    /// `model` (key from OPENAI_API_KEY, so it never touches disk).
    pub fn create(model: &str) -> Result<Home> {
        let dir = private_dir().context("creating kimi scratch dir")?;
        for (name, body) in [
            ("agent.yaml", AGENT_YAML.to_string()),
            ("mcp.json", r#"{"mcpServers":{}}"#.to_string()),
            ("config.toml", openrouter_config(model)),
        ] {
            std::fs::write(dir.join(name), body).with_context(|| format!("writing kimi {name}"))?;
        }
        let _ = HOME.set(dir.clone());
        Ok(Home { dir })
    }
}

/// A fresh directory under the temp dir that nobody else could have planted:
/// random name, must not already exist, and owner-only on unix. The agent file
/// in it is what keeps kimi read-only, so a predictable path in a shared /tmp
/// would let another local user pre-create it and swap the file.
fn private_dir() -> Result<PathBuf> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    let mut last = None;
    for _ in 0..8 {
        let mut rand = [0u8; 8];
        getrandom::getrandom(&mut rand).map_err(|e| anyhow::anyhow!("random dir name: {e}"))?;
        let name: String = rand.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("postmortemthis-kimi-{name}"));
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("eight attempts").into())
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `tools` given while extending replaces the default list, it does not add
/// to it (kimi logs "Loaded tools: [...]" with exactly these).
const AGENT_YAML: &str = r#"version: 1
agent:
  extend: default
  tools:
    - "kimi_cli.tools.file:ReadFile"
    - "kimi_cli.tools.file:ReadMediaFile"
    - "kimi_cli.tools.file:Glob"
    - "kimi_cli.tools.file:Grep"
    - "kimi_cli.tools.think:Think"
    - "kimi_cli.tools.todo:SetTodoList"
"#;

/// Replaces the user's `~/.kimi/config.toml` on the OpenRouter leg only; the
/// native leg keeps theirs, since that is where `kimi login` puts the model.
fn openrouter_config(model: &str) -> String {
    format!(
        r#"default_model = "openrouter"
telemetry = false

[providers.openrouter]
type = "openai_legacy"
base_url = "https://openrouter.ai/api/v1"
api_key = ""

[models.openrouter]
provider = "openrouter"
model = "{model}"
max_context_size = 262144
"#
    )
}

/// Where kimi keeps its config: `~/.kimi`, unless KIMI_SHARE_DIR moves it.
pub fn share_dir(home: &std::path::Path) -> PathBuf {
    match std::env::var_os("KIMI_SHARE_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.join(".kimi"),
    }
}

/// Does the user's own Kimi config name a model? A credentials file alone is
/// not enough: without `default_model` kimi exits with "LLM not set".
pub fn native_configured(share: &std::path::Path) -> bool {
    let Ok(config) = std::fs::read_to_string(share.join("config.toml")) else {
        return false;
    };
    config.lines().any(|line| {
        line.split_once('=').is_some_and(|(k, v)| {
            k.trim() == "default_model" && !v.trim().trim_matches('"').trim().is_empty()
        })
    })
}

/// The working-tree diff, for a leg with no shell to fetch it. None outside a
/// git repo. Capped: a huge diff would crowd out the request itself.
pub fn diff_for_prompt(repo: &std::path::Path) -> Option<String> {
    const MAX: usize = 200_000;
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let status = git(&["status", "--short"])?;
    // No external diff driver and no textconv: either is a user-configured
    // program, run here outside the agent's controls and before its timeout.
    // HEAD does not exist in a repo with no commits yet; git's empty tree does
    // in every repo, and diffing against it shows the whole index.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let diff = git(&["diff", "HEAD", "--no-color", "--no-ext-diff", "--no-textconv"])
        .or_else(|| git(&["diff", EMPTY_TREE, "--no-color", "--no-ext-diff", "--no-textconv"]))
        .unwrap_or_default();
    let mut diff = diff;
    if diff.len() > MAX {
        let mut cut = MAX;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        diff.push_str("\n[diff truncated]\n");
    }
    Some(format!(
        "git status --short:\n{status}\ngit diff HEAD:\n{diff}"
    ))
}
