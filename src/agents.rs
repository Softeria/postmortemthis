use crate::gemshim;
use crate::gg;
use crate::openrouter;
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
}

pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Gemini];

/// Codex `-c` overrides that define OpenRouter as a custom model provider,
/// plus the model slug - all compile-time constant. `wire_api` is omitted:
/// codex defaults to the Responses API, which OpenRouter implements. `name`
/// is mandatory (codex errors on an empty provider name) though cosmetic.
const CODEX_OPENROUTER_ARGS: [&str; 10] = [
    "-c",
    "model_provider=\"openrouter\"",
    "-c",
    "model_providers.openrouter.name=\"OpenRouter\"",
    "-c",
    "model_providers.openrouter.base_url=\"https://openrouter.ai/api/v1\"",
    "-c",
    "model_providers.openrouter.env_key=\"OPENROUTER_API_KEY\"",
    "-m",
    "openai/gpt-5",
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
        }
    }

    /// The tool name in gg's registry (gemini's binary is `gemini`, but the
    /// gg tool is `gemini-cli`).
    pub fn gg_tool(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini-cli",
        }
    }

    pub fn from_name(s: &str) -> Option<Agent> {
        match s.trim().to_lowercase().as_str() {
            "claude" | "claude-code" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "gemini" | "gemini-cli" => Some(Agent::Gemini),
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
                let mut a = vec!["exec"];
                if openrouter {
                    a.extend_from_slice(&CODEX_OPENROUTER_ARGS);
                }
                a.extend_from_slice(&["--sandbox", "read-only", "-"]);
                a
            }
            // Read-only: default approval denies tools that need it (shell,
            // edits) in a headless session, while file reads still work; also
            // skip the folder-trust prompt.
            Agent::Gemini => vec!["--skip-trust", "--approval-mode", "default"],
        }
    }

    /// Build the review command, via the native CLI or through gg.
    /// The caller pipes the prompt to stdin.
    pub fn command(&self, repo: &Path) -> Command {
        let mut cmd = match self.via() {
            Some(Via::Gg) => gg::locate().expect("via() said gg").tool(self.gg_tool()),
            // Native, or unresolved (let the spawn error surface).
            _ => Command::new(self.native_bin()),
        };
        // Agents the user has native auth for run untouched (BYO mode).
        // Otherwise, if an OpenRouter key is present and this agent can use
        // it, point the CLI at OpenRouter on that key.
        let or_key = (!self.authed() && self.openrouter_capable())
            .then(openrouter::key)
            .flatten();
        cmd.args(self.args(or_key.is_some()));
        cmd.current_dir(repo);
        if let Some(key) = or_key {
            for (name, value) in self.openrouter_env(key) {
                cmd.env(name, value);
            }
        }
        cmd
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
            Agent::Claude | Agent::Codex => true,
            Agent::Gemini => gemshim::endpoint().is_some(),
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
                ("ANTHROPIC_MODEL", "anthropic/claude-sonnet-4.6".into()),
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
                        ("GEMINI_API_KEY", "gemshim-substitutes-the-real-key".into()),
                        ("GOOGLE_GEMINI_BASE_URL", format!("http://127.0.0.1:{port}")),
                        ("GEMINI_CLI_TRUST_WORKSPACE", "true".into()),
                    ]
                }
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
        static CACHE: [OnceLock<Option<(String, String)>>; 3] =
            [OnceLock::new(), OnceLock::new(), OnceLock::new()];
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
            Agent::Gemini => {
                exists(".gemini/oauth_creds.json")
                    || env_set("GEMINI_API_KEY")
                    || env_set("GOOGLE_API_KEY")
            }
        }
    }

    pub fn auth_hint(&self) -> String {
        if self.authed() {
            match self {
                Agent::Claude => "logged in (subscription or API)".into(),
                Agent::Codex => "logged in".into(),
                Agent::Gemini => "logged in (Google OAuth)".into(),
            }
        } else {
            format!(
                "no credentials found - run `{}` once to log in",
                self.name()
            )
        }
    }
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
