//! Scratch files that keep Moonshot's `kimi` CLI (kimi-code, the Node rewrite
//! of the old Python kimi-cli) read-only, and a home for it on OpenRouter when
//! it has no login of its own.
//!
//! Kimi has no read-only switch. `-p` runs under the `auto` permission policy,
//! so every tool call goes through, and `--plan` cannot be combined with it.
//! What does hold is an agent file that names the tools it may have, and only
//! those: read, search and a todo list. kimi enforces that list twice, in what
//! the model is shown and again before a call runs. An allowlist rather than
//! excluding the writers, so a write tool added or renamed upstream (the leg
//! auto-updates) stays out. Not Bash, not Write/Edit, not Agent (its `coder`
//! subagent writes), and not the web tools either - a prompt injection in a
//! diff should have nowhere to send anything. MCP tools are `mcp__*` names, so
//! the allowlist keeps those out too.
//!
//! The allowlist is about what the model can call, not what kimi starts. The
//! native leg runs on the user's own `~/.kimi-code`, since that holds the
//! login, so their hooks, plugins and MCP servers load as they would in their
//! own session - the same as the claude leg. So does a repo's own
//! `.kimi-code/mcp.json` when they have trusted that repo or set
//! `require_trust = false`; `command` only drops KIMI_CODE_TRUST_WORKSPACE so
//! this run never grants that trust itself. The OpenRouter leg has a home of
//! its own and loads none of it.
//!
//! No Bash also means no `git diff`, so `agents::decorate_prompt` hands Kimi
//! the working-tree diff as a file in here, and a long prompt goes the same
//! way. Not inline: `-p` takes the prompt as an argument, not on stdin, and one
//! argument is capped at 128 KiB on Linux.

use anyhow::{Context, Result};
use std::fs::DirBuilder;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
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
    pub fn create() -> Result<Home> {
        let dir = private_dir().context("creating kimi scratch dir")?;
        std::fs::write(dir.join(AGENT_FILE), AGENT_MD).context("writing kimi agent file")?;
        let _ = HOME.set(dir.clone());
        Ok(Home { dir })
    }
}

pub const AGENT_FILE: &str = "agent.md";

/// KIMI_CODE_HOME on the OpenRouter leg. A home of its own keeps the user's
/// MCP servers, hooks, plugins and skills out of that leg, and its session out
/// of their history; the model comes in by `KIMI_MODEL_*` env. The native leg
/// needs the real home, since that is where `kimi login` keeps the token.
pub fn openrouter_home(scratch: &Path) -> PathBuf {
    scratch.join("home")
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

/// `subagents: []` on top of leaving `Agent` out of `tools`: no delegation,
/// whatever the built-in sub-agents could do.
const AGENT_MD: &str = r#"---
name: postmortemthis-reviewer
description: Read-only reviewer for postmortemthis
tools:
  - Read
  - ReadMediaFile
  - Glob
  - Grep
  - TodoList
subagents: []
---

You are a code reviewer in a read-only session. You can read and search files, nothing else: no shell, no edits, no web. Answer the request you are given, and make your last message the complete answer.
"#;

/// Where kimi keeps its config: `~/.kimi-code`, unless KIMI_CODE_HOME moves it.
/// Not `~/.kimi`: that is the old Python kimi-cli, whose login kimi-code does
/// not carry over (`kimi migrate` copies the config, not the token).
pub fn share_dir(home: &Path) -> PathBuf {
    match std::env::var_os("KIMI_CODE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.join(".kimi-code"),
    }
}

/// Does the user's own Kimi config name a model? A credentials file alone is
/// not enough: without `default_model` kimi exits with "LLM not set".
pub fn native_configured(share: &Path) -> bool {
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
/// git repo. Capped, but well past what a review reads: kimi pages through the
/// file with Read, so size no longer crowds the request out of the prompt.
fn diff_for_prompt(repo: &Path) -> Option<String> {
    const MAX: usize = 4_000_000;
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

/// Write the working-tree diff into the scratch dir for kimi to read, and
/// return its path. None outside a git repo, or when it could not be written.
pub fn diff_file(repo: &Path) -> Option<PathBuf> {
    let path = home()?.join("diff.txt");
    std::fs::write(&path, diff_for_prompt(repo)?).ok()?;
    Some(path)
}

/// Prompts longer than this go to kimi as a file (see `decorate_prompt`).
const INLINE_PROMPT_MAX: usize = 8 * 1024;

/// Write a prompt argv cannot carry into the scratch dir and return its path:
/// a long one, and on Windows any with a newline, which a .cmd shim (gg's
/// among them) cuts off. None when it fits inline, or when it could not be
/// written (then it goes inline and the spawn reports the failure).
pub fn request_file(prompt: &str) -> Option<PathBuf> {
    if prompt.len() <= INLINE_PROMPT_MAX && !(cfg!(windows) && prompt.contains('\n')) {
        return None;
    }
    let path = home()?.join("request.txt");
    std::fs::write(&path, prompt).ok()?;
    Some(path)
}

/// The answer out of `--output-format stream-json`: one JSON object per line,
/// an assistant message per step, the steps that call a tool carrying
/// `tool_calls`. The last assistant message without them is the final answer -
/// what `--final-message-only` gave on the old CLI. Text mode has no such cut:
/// it prints every step, and the final one tends to restate the earlier ones.
///
/// A run that ends on a tool call has no such message, so then the last
/// assistant message is the best there is. Hook output arrives as assistant
/// messages too and is skipped (see `is_hook_result`). Output that is not
/// JSON lines at all comes back as it was; JSON with no answer in it comes
/// back empty, not as meta lines posing as a review.
pub fn final_message(out: &str) -> String {
    let mut any_json = false;
    let mut last = None;
    let mut last_final = None;
    for line in out.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        any_json = true;
        if msg["role"] != "assistant" {
            continue;
        }
        let text = content_text(&msg["content"]);
        if text.trim().is_empty() || is_hook_result(&text) {
            continue;
        }
        let calls_tool = msg["tool_calls"].as_array().is_some_and(|c| !c.is_empty());
        if !calls_tool {
            last_final = Some(text.clone());
        }
        last = Some(text);
    }
    match last_final.or(last) {
        Some(text) => text,
        None if any_json => String::new(),
        None => out.to_string(),
    }
}

/// kimi-code writes a hook's output as an assistant message titled
/// "<Event> hook" (or "<Event> hook blocked") on its own line, then the body.
/// The native leg runs the user's hooks, and a Stop hook would otherwise land
/// after the answer and replace it.
fn is_hook_result(text: &str) -> bool {
    let title = text.split("\n\n").next().unwrap_or("");
    let event = title
        .strip_suffix(" hook")
        .or_else(|| title.strip_suffix(" hook blocked"));
    event.is_some_and(|e| {
        e.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && e.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// A message's content: a plain string, or a list of parts whose text parts
/// are joined.
fn content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::final_message;

    // Captured from kimi-code 2.6.0, session id shortened.
    const RUN: &str = r#"{"role":"meta","type":"system.version","version":"2.6.0"}
{"role":"assistant","content":"checking","tool_calls":[{"type":"function","id":"t1","function":{"name":"Read","arguments":"{\"path\":\"a.txt\"}"}}]}
{"role":"tool","tool_call_id":"t1","content":"1\thello"}
{"role":"assistant","content":"checking\n```text\nhello\n```\nThe file's exact content is `hello`."}
{"role":"meta","type":"session.resume_hint","session_id":"s1","command":"kimi -r s1","content":"To resume this session: kimi -r s1"}"#;

    #[test]
    fn keeps_only_the_final_answer() {
        assert_eq!(
            final_message(RUN),
            "checking\n```text\nhello\n```\nThe file's exact content is `hello`."
        );
    }

    #[test]
    fn cut_off_mid_tool_call_keeps_the_last_step() {
        let cut = RUN.lines().take(3).collect::<Vec<_>>().join("\n");
        assert_eq!(final_message(&cut), "checking");
    }

    #[test]
    fn content_parts_are_joined() {
        let out = r#"{"role":"assistant","content":[{"type":"text","text":"a"},{"type":"image_url"},{"type":"text","text":"b"}]}"#;
        assert_eq!(final_message(out), "ab");
    }

    #[test]
    fn hook_output_does_not_replace_the_answer() {
        let out = format!("{RUN}\n{}", r#"{"role":"assistant","content":"Stop hook\n\nformatted 3 files"}"#);
        assert_eq!(
            final_message(&out),
            "checking\n```text\nhello\n```\nThe file's exact content is `hello`."
        );
    }

    #[test]
    fn meta_lines_alone_are_no_answer() {
        let out = RUN.lines().filter(|l| l.contains(r#""role":"meta""#)).collect::<Vec<_>>().join("\n");
        assert_eq!(final_message(&out), "");
    }

    #[test]
    fn non_json_output_passes_through() {
        let out = "Error code: 402 - not enough credits";
        assert_eq!(final_message(out), out);
    }
}
