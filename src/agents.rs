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

    /// Does this agent take the prompt on stdin? It depends on the CLI that runs
    /// the leg (grok borrows codex on OpenRouter). Vibe, antigravity and grok's
    /// own CLI take the prompt as `-p`'s value; every other harness reads stdin.
    pub fn reads_stdin(&self, openrouter: bool) -> bool {
        let runner = if openrouter { self.openrouter_runner() } else { *self };
        !matches!(runner, Agent::Vibe | Agent::Grok | Agent::Antigravity)
    }

    /// The tool name in gg's registry. grok is the `grok` tool added in gg 187,
    /// which bootstraps the @xai-official/grok npm package; antigravity landed
    /// in gg 199.
    ///
    /// `antigravity-cli` and not `antigravity` on purpose: gg caches it under
    /// the repo name whatever alias you run, and `gg update` looks it up by
    /// cache name, so `update antigravity` silently finds nothing.
    pub fn gg_tool(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Antigravity => "antigravity-cli",
            Agent::Qwen => "qwen",
            Agent::Vibe => "vibe",
            Agent::Grok => "grok",
        }
    }

    /// The OpenRouter model slug this agent runs on when it has no usable native
    /// login (or is forced there), or None for a native-only agent that has no
    /// OpenRouter route at all. Single source for the env/args and the provenance
    /// shown to the caller; the None forces callers to rule out native-only
    /// agents rather than emitting a fake slug.
    pub fn openrouter_model(&self) -> Option<&'static str> {
        match self {
            Agent::Claude => Some("anthropic/claude-sonnet-4.6"),
            Agent::Codex => Some("openai/gpt-5"),
            Agent::Qwen => Some("qwen/qwen3-coder"),
            Agent::Vibe => Some("mistralai/mistral-medium-3.1"),
            // Antigravity is native-only: no OpenRouter model. None (not a fake
            // slug) so the type forces callers to have already ruled this out.
            Agent::Antigravity => None,
            // Grok's OpenRouter leg drives its own model through codex's harness.
            Agent::Grok => Some("x-ai/grok-build-0.1"),
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
            Agent::Grok => "run `grok login`, or set XAI_API_KEY (console.x.ai)",
            Agent::Qwen | Agent::Vibe => "no native login; runs on OpenRouter",
        }
    }

    pub fn from_name(s: &str) -> Option<Agent> {
        match s.trim().to_lowercase().as_str() {
            "claude" | "claude-code" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "antigravity" | "antigravity-cli" | "agy" => Some(Agent::Antigravity),
            "qwen" | "qwen-code" => Some(Agent::Qwen),
            "vibe" | "mistral-vibe" => Some(Agent::Vibe),
            "grok" | "grok-build" => Some(Agent::Grok),
            _ => None,
        }
    }

    /// Headless, read-only flags for an agent run through its own CLI (i.e. not
    /// codex, whose command is built in `codex_exec_command`). The prompt is
    /// delivered on stdin where possible - it is large and multiline, and
    /// Windows .cmd shims reject newline-containing arguments outright.
    fn args(&self) -> Vec<&'static str> {
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
            // Codex (and grok's OpenRouter leg, which borrows it) build their
            // command in `codex_exec_command`; `command()` never routes codex
            // through here. Panic loudly rather than silently launch codex's
            // interactive TUI with no args if that ever changes.
            Agent::Codex => unreachable!("codex builds its command in codex_exec_command"),
            // -p: single-prompt headless mode. Antigravity does not read stdin -
            // `-p -` takes the literal `-` as the prompt and answers "Hello! How
            // can I help you today?", losing the whole review. So `-p` goes last
            // and the runner appends the prompt (see reads_stdin). Ok as an argv
            // operand: gg ships antigravity as a native binary, not a .cmd shim,
            // so multiline survives.
            // Read-only rests on the print-mode default: WITHOUT
            // --dangerously-skip-permissions, any write tool is diverted into
            // Antigravity's own scratch dir and never touches the workspace
            // (verified empirically), while file reads and read-only git run
            // against the real cwd. The explicit `--sandbox` flag is
            // deliberately NOT used: it hangs headless `-p` runs until the
            // print-timeout fires. --print-timeout is parked far above any
            // realistic outer --timeout so postmortemthis's own timeout governs,
            // not Antigravity's 5m print-mode default.
            Agent::Antigravity => vec!["--print-timeout", "24h", "-p"],
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

    /// The base command that reaches this agent's CLI - through gg when a gg is
    /// available, else the native binary on PATH. Callers layer their own args
    /// on top (`command` for a review, `login_command` for a login).
    fn base_command(&self) -> Command {
        match self.via() {
            Some(Via::Gg) => gg::locate().expect("via() said gg").tool(self.gg_tool()),
            // Native, or unresolved (let the spawn error surface).
            _ => Command::new(self.native_bin()),
        }
    }

    /// Build the review command, via the native CLI or through gg. When
    /// `openrouter` is set, the CLI is pointed at OpenRouter on the resolved
    /// key; otherwise it runs on the user's own login. The caller pipes the
    /// prompt to stdin.
    pub fn command(&self, repo: &Path, openrouter: bool) -> Command {
        // The CLI that runs this leg: normally self, but on the OpenRouter leg an
        // agent may borrow another's harness (grok -> codex).
        let runner = if openrouter { self.openrouter_runner() } else { *self };
        let mut cmd = if runner == Agent::Codex {
            // Codex's exec harness; the model is passed only on the OpenRouter
            // leg (grok supplies its own model, codex supplies gpt-5).
            codex_exec_command(repo, if openrouter { self.openrouter_model() } else { None })
        } else {
            let mut cmd = runner.base_command();
            cmd.args(self.args());
            cmd.current_dir(repo);
            cmd
        };
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

    /// A native-only agent has no OpenRouter route at all - only Antigravity
    /// (Google). Derived from `openrouter_model()` so "has an OpenRouter route"
    /// has ONE source: an agent with no model slug is native-only, and every
    /// capability and guard below follows from it - they can't disagree. (Grok
    /// is NOT native-only: its own CLI can't reach OpenRouter, but its model
    /// x-ai/grok-build-0.1 is there and its OpenRouter leg borrows codex's
    /// harness - see `openrouter_runner`/`command`.)
    pub fn is_native_only(&self) -> bool {
        self.openrouter_model().is_none()
    }

    /// Can this agent reach OpenRouter in principle? Most can: Claude via the
    /// Anthropic Messages endpoint, Codex via the Responses endpoint, Qwen and
    /// Vibe via OpenAI-compatible endpoints; native-only agents cannot. Used to
    /// decide whether an un-authed agent is worth selecting when a key is present.
    pub fn supports_openrouter(&self) -> bool {
        !self.is_native_only()
    }

    /// Does this agent have a native login worth offering in `setup`? Derived
    /// from `login_invocation` so the set lives in exactly one place: qwen and
    /// vibe have no login (they only ever run on OpenRouter); the rest do.
    pub fn has_native_login(&self) -> bool {
        self.login_invocation().is_some()
    }

    /// A short vendor label, for `setup` display.
    pub fn vendor(&self) -> &'static str {
        match self {
            Agent::Claude => "Anthropic",
            Agent::Codex => "OpenAI",
            Agent::Antigravity => "Google",
            Agent::Qwen => "Alibaba",
            Agent::Vibe => "Mistral",
            Agent::Grok => "xAI",
        }
    }

    /// The agent whose CLI actually executes this agent's OpenRouter leg.
    /// Normally itself; grok is the exception - its own CLI can't reach
    /// OpenRouter, so grok's OpenRouter leg runs through codex's harness pointed
    /// at grok's model (x-ai/grok-build-0.1). The single place that fact lives.
    pub fn openrouter_runner(&self) -> Agent {
        match self {
            Agent::Grok => Agent::Codex,
            other => *other,
        }
    }

    /// Could this agent use OpenRouter in this environment? supports_openrouter()
    /// plus, for an agent that borrows another's harness (grok -> codex), that
    /// runner being installed/bootstrappable. Does NOT check per-run scratch
    /// state (Vibe's VIBE_HOME), which is only ready after selection - so this is
    /// the check for selection and planning. See openrouter_capable for run time.
    pub fn openrouter_reachable(&self) -> bool {
        if !self.supports_openrouter() {
            return false;
        }
        let runner = self.openrouter_runner();
        runner == *self || runner.via().is_some()
    }

    /// Can this agent reach OpenRouter *right now* (at attempt time)? As
    /// openrouter_reachable, but Vibe also needs its scratch VIBE_HOME written
    /// (main.rs prepares it before the fan-out).
    fn openrouter_capable(&self) -> bool {
        self.openrouter_reachable()
            && match self {
                Agent::Vibe => vibe::home().is_some(),
                _ => true,
            }
    }

    /// Args that run this agent's interactive login, or None when it has none.
    /// Some CLIs have a dedicated subcommand (`codex login`, `grok login`);
    /// claude and antigravity prompt on a bare interactive launch instead.
    fn login_invocation(&self) -> Option<&'static [&'static str]> {
        match self {
            Agent::Claude | Agent::Antigravity => Some(&[]),
            Agent::Codex | Agent::Grok => Some(&["login"]),
            Agent::Qwen | Agent::Vibe => None,
        }
    }

    /// A command that runs this agent's login interactively, via gg or the
    /// native binary. None when the agent has no native login. The caller
    /// inherits the terminal and waits for it to exit.
    pub fn login_command(&self) -> Option<Command> {
        let args = self.login_invocation()?;
        let mut cmd = self.base_command();
        cmd.args(args);
        Some(cmd)
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
                (
                    "ANTHROPIC_MODEL",
                    self.openrouter_model().expect("claude has an OpenRouter model").into(),
                ),
                ("MAX_THINKING_TOKENS", "0".into()),
            ],
            // Grok's OpenRouter leg runs through codex, which reads the same key.
            Agent::Codex | Agent::Grok => vec![("OPENROUTER_API_KEY", key.to_string())],
            // Antigravity has no OpenRouter route, so it is never run with
            // `openrouter` set - no env to inject.
            Agent::Antigravity => vec![],
            // Qwen Code speaks the OpenAI-compatible API directly, so it needs
            // no bridge: point its OpenAI client at OpenRouter on the key.
            Agent::Qwen => vec![
                ("OPENAI_API_KEY", key.to_string()),
                ("OPENAI_BASE_URL", "https://openrouter.ai/api/v1".into()),
                (
                    "OPENAI_MODEL",
                    self.openrouter_model().expect("qwen has an OpenRouter model").into(),
                ),
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
            // Grok Build authenticates headless two ways: `grok login` (device
            // OAuth) writes ~/.grok/auth.json and runs against the subscription
            // proxy, or XAI_API_KEY hits the plain API. auth.json is written only
            // on login (NOT on install, unlike the ~/.grok dir), so it is a safe
            // signal. Either counts as usable headless auth.
            Agent::Grok => exists(".grok/auth.json") || env_set("XAI_API_KEY"),
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
                Agent::Grok => "signed in (grok login or XAI_API_KEY)".into(),
                Agent::Qwen | Agent::Vibe => "logged in".into(),
            }
        } else if matches!(self, Agent::Qwen | Agent::Vibe) {
            "runs via OpenRouter (needs a key; no native login wired up)".into()
        } else if matches!(self, Agent::Grok) {
            "not signed in - run `grok login`, or set XAI_API_KEY (console.x.ai)".into()
        } else {
            format!(
                "no credentials found - run `{}` once to log in",
                self.name()
            )
        }
    }
}

/// Codex's read-only `exec` harness. `or_model = Some(model)` points it at
/// OpenRouter on that model (the `-c` provider overrides go right after `exec`,
/// which codex's built-in openai provider needs for the Responses wire API);
/// None runs codex on its own login. Grok's OpenRouter leg reuses this - the
/// grok CLI can't reach OpenRouter, but codex can drive grok's model there.
/// --ignore-user-config runs a clean one-shot: the user's config.toml (MCP
/// servers, custom tools) is irrelevant to a read-only review and can inject
/// malformed tool schemas that upstream providers reject. --skip-git-repo-check
/// lets it run outside a git repo; the sandbox already enforces read-only.
fn codex_exec_command(repo: &Path, or_model: Option<&str>) -> Command {
    let mut cmd = Agent::Codex.base_command();
    cmd.arg("exec").arg("--ignore-user-config");
    if let Some(model) = or_model {
        cmd.args(CODEX_OPENROUTER_ARGS);
        cmd.arg("-m").arg(model);
    }
    cmd.args(["--sandbox", "read-only", "--skip-git-repo-check", "-"]);
    cmd.current_dir(repo);
    cmd
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
