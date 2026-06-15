use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

pub struct Diff {
    pub repo_root: PathBuf,
    /// The exact command agents should run to see the diff, e.g. "git diff HEAD".
    pub command: String,
    /// `git diff --stat` of the same range, embedded in the prompt for orientation.
    pub stat: String,
    /// Untracked files, which no `git diff` variant will show.
    pub untracked: Vec<String>,
}

fn git(repo: Option<&PathBuf>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(r) = repo {
        cmd.current_dir(r);
    }
    let out = cmd.args(args).output().context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve what to review: --staged, --base <ref>, or uncommitted changes (default).
pub fn resolve(staged: bool, base: Option<&str>) -> Result<Diff> {
    let root = PathBuf::from(git(None, &["rev-parse", "--show-toplevel"])?);

    // On an unborn branch (fresh init, nothing committed) HEAD does not
    // exist; the staged diff is the only sensible default.
    let unborn = git(Some(&root), &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err();
    let staged = if !staged && base.is_none() && unborn {
        eprintln!("postmortem: no commits yet; reviewing staged changes instead");
        true
    } else {
        staged
    };

    let range_args: Vec<&str> = if staged {
        vec!["--cached"]
    } else if let Some(b) = base {
        vec![b]
    } else {
        vec!["HEAD"]
    };

    let mut stat_args = vec!["diff", "--stat"];
    stat_args.extend(&range_args);
    let stat = git(Some(&root), &stat_args)?;

    if stat.is_empty() {
        bail!(
            "no changes to review ({}). Try --staged or --base <ref>.",
            if staged {
                "nothing staged".to_string()
            } else if let Some(b) = base {
                format!("no diff against {b}")
            } else {
                "working tree is clean".to_string()
            }
        );
    }

    let untracked = git(Some(&root), &["ls-files", "--others", "--exclude-standard"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();

    Ok(Diff {
        repo_root: root,
        command: format!("git diff {}", range_args.join(" ")),
        stat,
        untracked,
    })
}
