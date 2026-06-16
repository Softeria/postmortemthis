use crate::gemshim;
use crate::gg;
use crate::openrouter;
use crate::vibe;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// A supported agent CLI. Each runs headless, read-only, in the repo's cwd,
/// using its own native harness and whatever auth the user already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Gemini,
    Qwen,
    Vibe,
}

pub const ALL: [Agent; 5] = [
    Agent::Claude,
    Agent::Codex,
    Agent::Gemini,
    Agent::Qwen,
    Agent::Vibe,
];

/// Codex `-c` overrides that define OpenRouter as a custom model provider -
/// all compile-time constant. The model (`-m`) is appended separately from
/// `openrouter_model()`. `wire_api` is omitted: codex defaults to the
/// Responses API, which OpenRouter implements. `name` is mandatory (codex
/// errors on an empty provider name) though cosmetic.
const CODEX_OPENROUTER_ARGS: [&str; 8] = [
    "-c",
    "model_provider=\"openrouter\"",
    "-c",
    "model_providers.openrouter.name=\"OpenRouter\"",
    "-c",
    "model_providers.openrouter.base_url=\"https://openrouter.ai/api/v1\"",
    "-c",
    "model_providers.openrouter.env_key=\"OPENROUTER_API_KEY\"",
];

/// How an agent's CLI is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The CLI itself is on PATH.
    Native,
    /// Bootstrapped and run through gg.cmd.
    Gg,
}

impl Agent {
    pub fn name(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini",
            Agent::Qwen => "qwen",
            Agent::Vibe => "vibe",
        }
    }

    /// Does this agent take the prompt on stdin? All do except vibe, which
    /// wants it as a command-line argument (appended by the runner).
    pub fn reads_stdin(&self) -> bool {
        !matches!(self, Agent::Vibe)
    }

    /// The tool name in gg's registry (gemini's binary is `gemini`, but the
    /// gg tool is `gemini-cli`).
    pub fn gg_tool(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini-cli",
            Agent::Qwen => "qwen",
            Agent::Vibe => "vibe",
        }
    }

    /// The OpenRouter model slug this agent runs on when it has no usable
    /// native login (or is forced there). Single source for the env/args and
    /// for the provenance shown to the caller.
    pub fn openrouter_model(&self) -> &'static str {
        match self {
            Agent::Claude => "anthropic/claude-sonnet-4.6",
            Agent::Codex => "openai/gpt-5",
            Agent::Gemini => "google/gemini-3.1-pro-preview",
            Agent::Qwen => "qwen/qwen3-coder",
            Agent::Vibe => "mistralai/mistral-medium-3.1",
        }
    }

    /// One-line hint, shown in the run notes, for restoring an agent's native
    /// login after it failed. Only the agents that have a native path are ever
    /// shown this (qwen/vibe never fall back - they have no native login).
    pub fn native_fix_hint(&self) -> &'static str {
        match self {
            Agent::Claude => "run `claude` once to refresh its login",
            Agent::Codex => "run `codex login` to refresh its login",
            Agent::Gemini => "set GEMINI_API_KEY (gemini's Google OAuth cannot run headless)",
            Agent::Qwen | Agent::Vibe => "no native login; runs on OpenRouter",
        }
    }

    pub fn from_name(s: &str) -> Option<Agent> {
        match s.trim().to_lowercase().as_str() {
            "claude" | "claude-code" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "gemini" | "gemini-cli" => Some(Agent::Gemini),
            "qwen" | "qwen-code" => Some(Agent::Qwen),
            "vibe" | "mistral-vibe" => Some(Agent::Vibe),
            _ => None,
        }
    }

    /// Headless, read-only flags. The prompt itself is delivered on stdin:
    /// it is large and multiline, and Windows .cmd shims reject
    /// newline-containing arguments outright.
    fn args(&self, openrouter: bool) -> Vec<&'static str> {
        match self {
            // -p: headless print mode; plan mode keeps it read-only.
            Agent::Claude => vec!["-p", "--permission-mode", "plan"],
            // `-`: read the prompt from stdin. For OpenRouter, the `-c`
            // overrides defining the provider must sit right after `exec`
            // (codex's built-in openai provider can't be repointed by env
            // alone for the Responses wire API).
            Agent::Codex => {
                // --ignore-user-config: run a clean one-shot. The user's
                // config.toml (MCP servers, custom tools, extra headers) is
                // irrelevant to a read-only review and can inject malformed
                // tool schemas that upstream providers reject. Auth still
                // resolves from CODEX_HOME.
                let mut a = vec!["exec", "--ignore-user-config"];
                if openrouter {
                    a.extend_from_slice(&CODEX_OPENROUTER_ARGS);
                    a.push("-m");
                    a.push(self.openrouter_model());
                }
                a.extend_from_slice(&[
                    "--sandbox",
                    "read-only",
                    // Run in any directory, not just a git repo; the sandbox
                    // already enforces read-only, so the git-trust gate is
                    // redundant here and just blocks non-repo cwds.
                    "--skip-git-repo-check",
                    "-",
                ]);
                a
            }
            // Read-only: default approval denies tools that need it (shell,
            // edits) in a headless session, while file reads still work; also
            // skip the folder-trust prompt.
            Agent::Gemini => vec!["--skip-trust", "--approval-mode", "default"],
            // Qwen Code is a Gemini-CLI fork: same read-only approval model.
            // --auth-type openai pins it to the OpenAI-compatible endpoint
            // (the OPENAI_* env points that at OpenRouter); the prompt is read
            // from stdin like the others.
            Agent::Qwen => vec!["--approval-mode", "default", "--auth-type", "openai"],
            // -p: programmatic mode (print, exit). Unlike the others, vibe does
            // not take the prompt on stdin - it wants it as -p's value, so -p
            // goes last and the runner appends the prompt (see reads_stdin).
            // The `plan` builtin agent is read-only (no edits); --trust skips
            // the folder-trust prompt. Provider/model come from VIBE_HOME.
            Agent::Vibe => vec!["--agent", "plan", "--trust", "--output", "text", "-p"],
        }
    }

    /// Build the review command, via the native CLI or through gg. When
    /// `openrouter` is set, the CLI is pointed at OpenRouter on the resolved
    /// key; otherwise it runs on the user's own login. The caller pipes the
    /// prompt to stdin.
    pub fn command(&self, repo: &Path, openrouter: bool) -> Command {
        let mut cmd = match self.via() {
            Some(Via::Gg) => gg::locate().expect("via() said gg").tool(self.gg_tool()),
            // Native, or unresolved (let the spawn error surface).
            _ => Command::new(self.native_bin()),
        };
        cmd.args(self.args(openrouter));
        cmd.current_dir(repo);
        if openrouter && let Some(key) = openrouter::key() {
            for (name, value) in self.openrouter_env(key) {
                cmd.env(name, value);
            }
        }
        cmd
    }

    /// Ordered attempts for this agent: `false` runs on the native login,
    /// `true` runs on OpenRouter. The native login is tried first when the
    /// user has one; OpenRouter follows as a fallback (or as the only attempt
    /// when there is no usable login). `skip_native` drops the native attempt
    /// (the caller asked, via --skip-native, to go straight to OpenRouter). An
    /// empty plan means there is nothing to try - no login and no key.
    pub fn attempt_plan(&self, skip_native: bool) -> Vec<bool> {
        let mut plan = Vec::new();
        if self.authed() && !skip_native {
            plan.push(false);
        }
        if openrouter::key().is_some() && self.openrouter_capable() {
            plan.push(true);
        }
        plan
    }

    /// Can this agent reach OpenRouter in principle? All three can: Claude via
    /// the Anthropic Messages endpoint, Codex via the Responses endpoint, and
    /// Gemini via the gemshim bridge. Used to decide whether an un-authed
    /// agent is worth selecting when an OpenRouter key is present.
    pub fn supports_openrouter(&self) -> bool {
        true
    }

    /// Can this agent reach OpenRouter *right now*? Same as
    /// `supports_openrouter`, except Gemini additionally needs the gemshim
    /// bridge to be running (main.rs starts it before the fan-out).
    fn openrouter_capable(&self) -> bool {
        match self {
            Agent::Claude | Agent::Codex | Agent::Qwen => true,
            Agent::Gemini => gemshim::endpoint().is_some(),
            Agent::Vibe => vibe::home().is_some(),
        }
    }

    /// Provider env that points this agent's CLI at OpenRouter on `key`.
    /// Codex additionally needs the `-c` provider overrides from `args()`.
    fn openrouter_env(&self, key: &str) -> Vec<(&'static str, String)> {
        match self {
            // MAX_THINKING_TOKENS=0: headless `claude -p` returns empty text
            // through OpenRouter with thinking on - OpenRouter appends a
            // trailing redacted_thinking block that the -p text extractor
            // lands on. Disabling thinking is the V1 fix; the native-auth
            // path is unaffected and keeps thinking. A future stream-json
            // reader in the runner could restore thinking on this leg.
            Agent::Claude => vec![
                ("ANTHROPIC_BASE_URL", "https://openrouter.ai/api".into()),
                ("ANTHROPIC_AUTH_TOKEN", key.to_string()),
                ("ANTHROPIC_MODEL", self.openrouter_model().into()),
                ("MAX_THINKING_TOKENS", "0".into()),
            ],
            Agent::Codex => vec![("OPENROUTER_API_KEY", key.to_string())],
            // Gemini routes through the local gemshim bridge: HOME is the
            // throwaway dir holding the auth + read-only-git policy, and the
            // CLI is pointed at gemshim (which holds the real OpenRouter key,
            // so the key handed to the CLI is an ignored dummy).
            Agent::Gemini => match gemshim::endpoint() {
                Some((port, home)) => {
                    let home = home.to_string_lossy().into_owned();
                    vec![
                        ("HOME", home.clone()),
                        ("USERPROFILE", home),
                        // Redirecting HOME would also move gg's tool cache
                        // (default $HOME/.cache/gg) into the throwaway dir,
                        // forcing a full gemini-cli reinstall on every run.
                        // Pin it back to the real cache.
                        ("GG_CACHE_DIR", gg_cache_dir()),
                        ("GEMINI_API_KEY", "gemshim-substitutes-the-real-key".into()),
                        ("GOOGLE_GEMINI_BASE_URL", format!("http://127.0.0.1:{port}")),
                        ("GEMINI_CLI_TRUST_WORKSPACE", "true".into()),
                    ]
                }
                None => vec![],
            },
            // Qwen Code speaks the OpenAI-compatible API directly, so it needs
            // no bridge: point its OpenAI client at OpenRouter on the key.
            Agent::Qwen => vec![
                ("OPENAI_API_KEY", key.to_string()),
                ("OPENAI_BASE_URL", "https://openrouter.ai/api/v1".into()),
                ("OPENAI_MODEL", self.openrouter_model().into()),
            ],
            // Vibe reads its provider/model from the scratch VIBE_HOME and the
            // key from OPENROUTER_API_KEY (named in that config). VIBE_HOME is
            // not HOME, so gg's cache is untouched - no GG_CACHE_DIR needed.
            Agent::Vibe => match vibe::home() {
                Some(home) => vec![
                    ("VIBE_HOME", home.to_string_lossy().into_owned()),
                    ("OPENROUTER_API_KEY", key.to_string()),
                ],
                None => vec![],
            },
        }
    }

    /// One cached probe per agent per process: which spawnable name works,
    /// and what `--version` it reports. On Windows, npm installs `.cmd`
    /// shims which CreateProcess (and thus Command::new with the bare name)
    /// does not resolve; Rust does spawn them when the `.cmd` name is
    /// explicit. The probes are node startups (~1s each), so they must not
    /// run once per call site.
    fn native_probe(&self) -> Option<&'static (String, String)> {
        static CACHE: [OnceLock<Option<(String, String)>>; 5] = [
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
        ];
        CACHE[*self as usize]
            .get_or_init(|| {
                let mut names = vec![self.name().to_string()];
                if cfg!(windows) {
                    names.push(format!("{}.cmd", self.name()));
                }
                for name in names {
                    if let Ok(out) = Command::new(&name).arg("--version").output()
                        && out.status.success()
                    {
                        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        let v = if v.is_empty() { "unknown version".into() } else { v };
                        return Some((name, v));
                    }
                }
                None
            })
            .as_ref()
    }

    /// The spawnable native binary name (falls back to the plain name when
    /// nothing probed successfully - the spawn error then surfaces).
    fn native_bin(&self) -> String {
        self.native_probe()
            .map(|(bin, _)| bin.clone())
            .unwrap_or_else(|| self.name().to_string())
    }

    /// How to reach this agent: through gg if a capable gg is available
    /// (it owns version management and bootstrapping), else a native CLI on
    /// PATH as a fallback.
    pub fn via(&self) -> Option<Via> {
        if gg::locate().is_some() {
            Some(Via::Gg)
        } else if self.native_version().is_some() {
            Some(Via::Native)
        } else {
            None
        }
    }

    /// Is the CLI on PATH and able to report a version?
    pub fn native_version(&self) -> Option<String> {
        self.native_probe().map(|(_, version)| version.clone())
    }

    /// Best-effort: is some form of auth configured? File in $HOME (shared
    /// between a native install and a gg-bootstrapped one), API key in the
    /// environment, or (for Claude on macOS) the Keychain. We never read or
    /// touch the credentials themselves.
    pub fn authed(&self) -> bool {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        let exists = |p: &str| Path::new(&home).join(p).exists();
        let env_set =
            |k: &str| std::env::var_os(k).is_some_and(|v| !v.to_string_lossy().trim().is_empty());
        match self {
            Agent::Claude => {
                exists(".claude/.credentials.json")
                    || env_set("ANTHROPIC_API_KEY")
                    || claude_keychain_auth()
            }
            Agent::Codex => exists(".codex/auth.json") || env_set("OPENAI_API_KEY"),
            // Gemini's free Google-OAuth login (oauth_creds.json) is
            // interactive and cannot run headless, so it does not count: only
            // a real API key is headless-usable. Without one, an OpenRouter
            // key routes Gemini through the gemshim bridge instead.
            Agent::Gemini => env_set("GEMINI_API_KEY") || env_set("GOOGLE_API_KEY"),
            // Qwen and Vibe have no widely-held native login wired up; they
            // run through OpenRouter when a key is present (see attempt_plan).
            Agent::Qwen | Agent::Vibe => false,
        }
    }

    pub fn auth_hint(&self) -> String {
        if self.authed() {
            match self {
                Agent::Claude => "logged in (subscription or API)".into(),
                Agent::Codex => "logged in".into(),
                Agent::Gemini => "API key set".into(),
                Agent::Qwen | Agent::Vibe => "logged in".into(),
            }
        } else if matches!(self, Agent::Qwen | Agent::Vibe) {
            "runs via OpenRouter (needs a key; no native login wired up)".into()
        } else if matches!(self, Agent::Gemini) && gemini_oauth_present() {
            "Google OAuth login found, but it cannot run headless - set \
             GEMINI_API_KEY or pass an OpenRouter key"
                .into()
        } else {
            format!(
                "no credentials found - run `{}` once to log in",
                self.name()
            )
        }
    }
}

/// gg's tool cache, so it survives a redirected HOME on the gemini leg:
/// `$GG_CACHE_DIR` if set, else the default `$HOME/.cache/gg`. Read while the
/// real HOME is still in scope (the override applies only to the child).
fn gg_cache_dir() -> String {
    if let Some(v) = std::env::var_os("GG_CACHE_DIR") {
        return v.to_string_lossy().into_owned();
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    Path::new(&home)
        .join(".cache")
        .join("gg")
        .to_string_lossy()
        .into_owned()
}

/// Is a Gemini Google-OAuth login on disk? Used only to explain that it is
/// present but unusable headless; it is deliberately not counted by `authed`.
fn gemini_oauth_present() -> bool {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    Path::new(&home).join(".gemini/oauth_creds.json").exists()
}

/// Claude Code on macOS stores OAuth credentials in the Keychain, not in
/// ~/.claude. Querying item metadata (no -w) never prints the secret and
/// does not prompt.
#[cfg(target_os = "macos")]
fn claude_keychain_auth() -> bool {
    Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain_auth() -> bool {
    false
}
