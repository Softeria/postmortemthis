use crate::git::Diff;
use crate::runner::Report;

pub fn review(diff: &Diff) -> String {
    format!(
        r#"You are performing a final pre-merge code review of pending changes in this repository. This is the last review before the author ships.

To see the changes, run: `{cmd}`

Changed files:
{stat}

Instructions:
- Review the diff. Read surrounding code where you need context to judge correctness.
- Hunt for REAL problems: bugs, broken edge cases, security issues, data loss, race conditions, silently changed behavior. Not style, not nitpicks.
- Do NOT modify any files. This is a read-only review.

Output exactly this structure (markdown):

## Findings

For each finding:
### [SEVERITY] short title
- **Where:** file:line
- **What:** what is wrong and why it matters
- **Confidence:** high / medium / low

Severities: CRITICAL (will break or leak), MAJOR (likely bug or risk), MINOR (worth knowing).
If you find nothing significant, output exactly: `No significant findings.`

## Summary

One short paragraph: overall risk assessment of shipping this diff."#,
        cmd = diff.command,
        stat = diff.stat,
    )
}

pub fn synthesis(reports: &[&Report]) -> String {
    let mut body = String::new();
    for r in reports {
        body.push_str(&format!(
            "\n\n---\n\n# Report from agent `{}`\n\n{}",
            r.agent.name(),
            r.output
        ));
    }
    format!(
        r#"You are the chair of a code review panel. {n} independent AI agents, each with a different underlying model, reviewed the SAME diff. Their full reports follow at the end.

Your job - produce the final verdict:
1. Deduplicate: merge findings that refer to the same underlying issue, even if described differently.
2. Score consensus: for each merged finding, note which agents flagged it. A finding flagged independently by multiple agents deserves elevated confidence.
3. Judge: drop findings that are clearly mistaken or pure style nitpicks. Keep disagreements visible - if one agent flags something the others missed, that can be the most valuable finding; do not bury it.
4. Rank by (severity, consensus, confidence).

Do not run any commands or read any files. Work only from the reports.

Output (markdown):

# Postmortem verdict

## Confirmed findings
Ranked list. For each: severity, title, file:line, which agents flagged it (e.g. `claude+codex`), one-paragraph description, what to do about it.

## Single-agent findings worth a look
Findings only one agent raised that are plausible and non-trivial.

## Dismissed
One line each: what was dropped and why.

## Ship verdict
One of: SHIP / SHIP WITH FIXES / DO NOT SHIP - with a one-paragraph justification.
{body}"#,
        n = reports.len(),
        body = body,
    )
}
