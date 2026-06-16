use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// A located gg.cmd that supports the agent CLI tools (claude, codex,
/// gemini-cli). gg bootstraps each tool on first use into its own cache;
/// the user's auth (~/.claude, ~/.codex, ~/.gemini) is untouched and shared
/// with any native install.
pub struct Gg {
    path: PathBuf,
}

impl Gg {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Base command that invokes this file as plain gg, whatever it is named.
    /// The literal `gg` first argument is the applet jump-out: on a renamed
    /// gg.cmd (e.g. postmortem.cmd) it bypasses applet dispatch, and on a
    /// plain gg.cmd it is stripped uniformly. Only gg >= 177 understands it,
    /// but older ggs also lack the agent tools and never pass `probe`.
    fn base(&self) -> Command {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(&self.path);
            c
        } else {
            // The polyglot script is always sh-compatible and may lack an
            // executable bit (e.g. freshly downloaded).
            let mut c = Command::new("sh");
            c.arg(&self.path);
            c
        };
        cmd.arg("gg");
        cmd
    }

    /// Command that runs `tool` (a gg tool name or `a:b:c` chain) with args.
    /// For a chain, gg prepares every tool in parallel and executes the first.
    pub fn tool(&self, chain: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg(chain);
        cmd
    }

    /// `gg update -u`: update every tool gg manages, in parallel.
    pub fn update_all(&self) -> Command {
        let mut cmd = self.base();
        cmd.arg("update").arg("-u");
        cmd
    }

    /// Does this gg know the agent CLI tools? Offline check against its
    /// baked-in registry; also weeds out pre-applet gg versions, which choke
    /// on the `gg` jump-out word.
    fn probe(&self) -> bool {
        self.base()
            .arg("tools")
            .output()
            .map(|out| {
                out.status.success() && String::from_utf8_lossy(&out.stdout).contains("claude")
            })
            .unwrap_or(false)
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();

    // 1. We were launched through a gg.cmd (applet or otherwise); its wrapper
    //    exports GG_CMD_PATH with the exact path it was invoked as. Calling
    //    back into the same file is what the jump-out exists for, and it is the
    //    normal flow: when the caller runs `sh /path/postmortemthis.cmd ...`,
    //    that path arrives here.
    if let Some(p) = std::env::var_os("GG_CMD_PATH") {
        found.push(PathBuf::from(p));
    }

    // 2. A postmortemthis.cmd / gg.cmd sitting next to the binary itself (the
    //    co-located install, e.g. both downloaded into one folder). Anchored to
    //    the executable's own directory - deliberately NOT the working
    //    directory. This tool runs inside the repo under review, and probing a
    //    CWD/git-root gg.cmd would execute a script shipped by the code being
    //    audited. The binary's own location is trusted; the review cwd is not.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        found.push(dir.join("postmortemthis.cmd"));
        found.push(dir.join("gg.cmd"));
    }

    // 3. gg or gg.cmd on PATH.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            found.push(dir.join("gg.cmd"));
            found.push(dir.join("gg"));
        }
    }

    found
}

/// Find a usable gg, once per process. Returns None if no gg.cmd with agent
/// tool support is reachable.
pub fn locate() -> Option<&'static Gg> {
    static GG: OnceLock<Option<Gg>> = OnceLock::new();
    GG.get_or_init(|| {
        for path in candidates() {
            if path.is_file() {
                let gg = Gg { path };
                if gg.probe() {
                    return Some(gg);
                }
            }
        }
        None
    })
    .as_ref()
}
