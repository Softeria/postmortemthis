# postmortemthis

**Why.** Before you ship, one model's opinion isn't enough. You already have
several coding agents - get all their eyes on your diff at once.

**What.** `postmortem` runs every AI coding CLI you have - Claude Code, Codex,
Gemini, Qwen, Vibe - in parallel, read-only, on one prompt, and prints each
one's take. A calling agent (e.g. Claude Code) writes the prompt and merges the
takes into a single ship / don't-ship verdict.

**The catch (by design).** It only reads, holds no keys, and changes nothing.
It does not synthesize - the calling agent does. Think "panel of reviewers,"
not "autopilot."

## Setup

Paste this into Claude Code:

> From the latest release at github.com/Softeria/postmortemthis, download the
> `postmortem` binary for my OS and `postmortemthis.cmd` into a folder on my
> PATH, make `postmortem` executable, then run `postmortem skill` and follow the
> instructions it prints.

That builds a **postmortem** skill. From then on, before a commit, just say:
**"postmortem this."**

## Auth

Each agent uses your own login when it has one. For the rest, connect OpenRouter
once - `postmortem login` (OAuth, no key to copy) or set `OPENROUTER_API_KEY` -
and they run through it. OpenRouter usage bills to your account; this tool
resells nothing.

## CLI

```
echo "review the pending diff for bugs" | postmortem   # all available agents
postmortem doctor                                       # what's available
postmortem login                                        # connect OpenRouter
```

The prompt is read from stdin; agents run read-only in the current directory.
`--out <dir>` also writes each agent's full output to a file. `postmortemthis.cmd`
sits alongside the binary so it can bootstrap any agent CLI you don't have.

## Build

```
cargo build --release
```

MIT
