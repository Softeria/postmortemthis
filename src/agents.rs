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
    Antigravity,
    Qwen,
    Vibe,
    Grok,
}

pub const ALL: [Agent; 6] = [
    Agent::Claude,
    Agent::Codex,
    Agent::Antigravity,
    Agent::Qwen,
    Agent::Vibe,
    Agent::Grok,
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
            Agent::Antigravity => "antigravity",
            Agent::Qwen => "qwen",
            Agent::Vibe => "vibe",
            Agent::Grok => "grok",
        }
    }

    /// Does this agent take the prompt on stdin? Most do; vibe and grok want it
    /// as a command-line argument (the value of `-p`, appended by the runner).
    pub fn reads_stdin(&self) -> bool {
        !matches!(self, Agent::Vibe | Agent::Grok)
    }

    /// The tool name in gg's registry. Antigravity is pulled straight from its
    /// GitHub release repo (no short alias); grok is the `grok` tool added in
    /// gg 187, which bootstraps the @xai-official/grok npm package.
    pub fn gg_tool(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Antigravity => "gh/google-antigravity/antigravity-cli",
            Agent::Qwen => "qwen",
            Agent::Vibe => "vibe",
            Agent::Grok => "grok",
        }
    }

    /// The OpenRouter model slug this agent runs on when it has no usable
    /// native login (or is forced there). Single source for the env/args and
    /// for the provenance shown to the caller.
    pub fn openrouter_model(&self) -> &'static str {
        match self {
            Agent::Claude => "anthropic/claude-sonnet-4.6",
            Agent::Codex => "openai/gpt-5",
            Agent::Qwen => "qwen/qwen3-coder",
            Agent::Vibe => "mistralai/mistral-medium-3.1",
            // Antigravity and Grok are native-only (no OpenRouter route), so
            // this is never used for provenance; kept honest in case it leaks.
            Agent::Antigravity | Agent::Grok => "native-only (no OpenRouter)",
        }
    }

    /// One-line hint, shown in the run notes, for restoring an agent's native
    /// login after it failed. Only the agents that have a native path are ever
    /// shown this (qwen/vibe never fall back - they have no native login).
    pub fn native_fix_hint(&self) -> &'static str {
        match self {
            Agent::Claude => "run `claude` once to refresh its login",
            Agent::Codex => "run `codex login` to refresh its login",
            Agent::Antigravity => "run `antigravity` once to refresh its Google login",
            Agent::Grok => "set XAI_API_KEY (headless auth; get one at console.x.ai)",
            Agent::Qwen | Agent::Vibe => "no native login; runs on OpenRouter",
        }
    }

    pub fn from_name(s: &str) -> Option<Agent> {
        match s.trim().to_lowercase().as_str() {
            "claude" | "claude-code" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "antigravity" | "antigravity-cli" => Some(Agent::Antigravity),
            "qwen" | "qwen-code" => Some(Agent::Qwen),
            "vibe" | "mistral-vibe" => Some(Agent::Vibe),
            "grok" | "grok-build" => Some(Agent::Grok),
            _ => None,
        }
    }

    /// Headless, read-only flags. The prompt itself is delivered on stdin:
    /// it is large and multiline, and Windows .cmd shims reject
    /// newline-containing arguments outright.
    fn args(&self, openrouter: bool) -> Vec<&'static str> {
        match self {
            // -p: headless print mode. `dontAsk` keeps it read-only WITHOUT
            // diverting the review into a plan. Plan mode delivers the model's
            // analysis through the ExitPlanMode tool call, which `-p` text
            // output never prints - only the trailing sign-off survives, so the
            // entire review was being lost. `dontAsk` instead auto-denies any
            // tool that needs permission (writes, edits, arbitrary shell)
            // without prompting, while the allow-listed read tools - plus
            // read-only git, so it can actually see the diff - run freely and
            // the model answers as normal text on stdout. (`--bare` would also
            // skip keychain reads, breaking native login on macOS, so it is
            // deliberately not used.)
            Agent::Claude => vec![
                "-p",
                "--permission-mode",
                "dontAsk",
                "--allowedTools",
                "Read,Grep,Glob,Bash(git diff:*),Bash(git log:*),Bash(git show:*),Bash(git status:*),Bash(git rev-parse:*)",
            ],
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
            // -p -: single-prompt headless mode reading the prompt from stdin
            // (the `-` operand), like the other stdin agents - so the large,
            // multiline review prompt is never passed as a CLI argument (which
            // Windows .cmd shims reject and argv limits truncate).
            // Read-only rests on the print-mode default: WITHOUT
            // --dangerously-skip-permissions, any write tool is diverted into
            // Antigravity's own scratch dir and never touches the workspace
            // (verified empirically), while file reads and read-only git run
            // against the real cwd. The explicit `--sandbox` flag is
            // deliberately NOT used: it hangs headless `-p` runs until the
            // print-timeout fires. --print-timeout is parked far above any
            // realistic outer --timeout so postmortemthis's own timeout governs,
            // not Antigravity's 5m print-mode default.
            Agent::Antigravity => vec!["--print-timeout", "24h", "-p", "-"],
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
            // Grok Build's permission model mirrors Claude's: `--permission-mode
            // dontAsk` auto-denies any tool that needs approval (writes, edits,
            // arbitrary shell) while read-only tools (read_file, list_dir, grep,
            // web_search, and a curated set of safe shell) stay auto-approved, so
            // the model can read the diff and answer but cannot mutate the tree.
            // `-p` (alias --single) takes the prompt as its value, not on stdin,
            // so it goes last and the runner appends the prompt (see reads_stdin).
            Agent::Grok => vec!["--permission-mode", "dontAsk", "-p"],
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

    /// Can this agent reach OpenRouter in principle? Most can: Claude via the
    /// Anthropic Messages endpoint, Codex via the Responses endpoint, Qwen and
    /// Vibe via OpenAI-compatible endpoints. Antigravity and Grok cannot - they
    /// are closed-source CLIs bound to their vendor's own backend, with no
    /// endpoint to repoint - so they are native-login only. Used to decide
    /// whether an un-authed agent is worth selecting when a key is present.
    pub fn supports_openrouter(&self) -> bool {
        !matches!(self, Agent::Antigravity | Agent::Grok)
    }

    /// Can this agent reach OpenRouter *right now*? Same as
    /// `supports_openrouter`, except Vibe additionally needs its scratch
    /// VIBE_HOME to be written (main.rs prepares it before the fan-out).
    fn openrouter_capable(&self) -> bool {
        match self {
            Agent::Claude | Agent::Codex | Agent::Qwen => true,
            Agent::Antigravity | Agent::Grok => false,
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
            // Antigravity and Grok have no OpenRouter route (native-login only),
            // so they are never run with `openrouter` set - no env to inject.
            Agent::Antigravity | Agent::Grok => vec![],
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
        static CACHE: [OnceLock<Option<(String, String)>>; ALL.len()] = [
            OnceLock::new(),
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
            // Antigravity logs in with a Google account saved under ~/.gemini
            // (the dir it shares with the legacy gemini CLI) and, unlike the old
            // gemini-cli, runs that login headless - so the saved account is
            // usable auth on its own, no API key required.
            Agent::Antigravity => exists(".gemini/google_accounts.json"),
            // Grok Build authenticates headless with an xAI API key
            // (console.x.ai). Its browser OAuth caches credentials we don't
            // probe, and the ~/.grok dir is created on install before any login
            // (verified), so dir-existence is NOT an auth signal - only the key.
            Agent::Grok => env_set("XAI_API_KEY"),
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
                Agent::Antigravity => "logged in (Google account)".into(),
                Agent::Grok => "XAI_API_KEY set".into(),
                Agent::Qwen | Agent::Vibe => "logged in".into(),
            }
        } else if matches!(self, Agent::Qwen | Agent::Vibe) {
            "runs via OpenRouter (needs a key; no native login wired up)".into()
        } else if matches!(self, Agent::Grok) {
            // Grok headless auth is XAI_API_KEY; `grok login` (browser OAuth) is
            // not probed and does not satisfy authed(), so don't suggest it here.
            "no XAI_API_KEY set - headless auth needs a key from console.x.ai".into()
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
