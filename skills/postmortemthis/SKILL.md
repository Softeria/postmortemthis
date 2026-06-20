---
name: postmortemthis
description: Fan one prompt across every coding agent on this machine (Claude Code, Codex, Gemini, Qwen, Vibe) at once and synthesize their replies. A multi-agent second opinion on anything — a diff, an idea, a design, a question. Use when the user wants several agents to weigh in, asks for a "postmortem" or a second opinion, or runs /postmortemthis.
---

# postmortemthis

`postmortemthis.cmd` pipes whatever you send it on stdin to every coding agent that's
installed and logged in on this machine, in parallel, and prints each one's reply. This
skill builds the prompt, runs the fan-out, and synthesizes the answers.

It is **read-only by default**: it reports what the agents say, it does not edit, stage,
commit, or push. Applying fixes is a separate, explicit step.

## 1. Decide what to ask

The prompt is whatever the user wants several agents to weigh in on. There is **no git
requirement** — a postmortem can be about anything:

- **Reviewing current work** — the sensible default when the user just runs
  `/postmortemthis` inside a repo with changes. Scope it to the uncommitted + branch
  changes and ask for bugs, risks, and edge cases. Don't paste the diff into the prompt —
  the agents read the working tree themselves.
- **An idea, plan, design, or question** — even in an empty folder. Just pass it through;
  no repo, no diff needed.
- **Whatever the user named** as an argument (e.g. `/postmortemthis is this API shape
  sane?`) — use that as the prompt directly.

If it's genuinely ambiguous what they want a second opinion on, ask one short question
rather than assuming a diff.

Keep the prompt to a focused single line about **what to look for**.

## 2. Get the wrapper (once)

Use the local copy bundled next to this SKILL.md. Only fetch it if missing — don't
re-download and execute a remote script on every run:

```bash
cmd="$(dirname "$0")/postmortemthis.cmd"   # or the folder this SKILL.md lives in
if [ ! -f "$cmd" ]; then
  curl -fsSL https://github.com/Softeria/postmortemthis/releases/latest/download/postmortemthis.cmd -o "$cmd"
fi
sh "$cmd" doctor   # optional: show which agents are wired up
```

If `doctor` reports zero agents, point the user at `sh "$cmd" login` (OpenRouter OAuth) or
setting `OPENROUTER_API_KEY`. Don't run `login` yourself — it's interactive.

## 3. Run

Pipe the prompt on stdin. This drives several agents, so it can take a few minutes — give
it a generous timeout, and persist the output so a killed or scrolled-past run isn't lost:

```bash
cache="${XDG_CACHE_HOME:-$HOME/.cache}/postmortemthis/runs"
mkdir -p "$cache"
log="$cache/$(date +%Y%m%d-%H%M%S).log"
echo "Full agent output streaming to: $log"
echo "YOUR PROMPT HERE" | sh "$cmd" 2>&1 | tee "$log"
```

Announce the log path before the wait. Run the fan-out **once** per request; don't loop it
(the agents bill to the user's own accounts).

## 4. Synthesize

Read from the log file, not the inline scrollback (which can truncate). Each agent answers
independently, so you'll get several overlapping opinions — don't just dump them all:

- **Consensus** — what more than one agent flagged (highest signal).
- **Notable single-agent points** — worth a look even if only one raised them.
- **Disagreements / likely false positives** — where agents conflict or are clearly wrong.
- Cite `file:line` where the agents did.

Lead your reply with the log path, keep the synthesis tight, and end with a clear takeaway
— for a review, a ship / no-ship call; for an idea, where the agents agreed and where they
pushed back.
