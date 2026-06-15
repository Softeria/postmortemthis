# postmortemthis

A pre-ship review panel. `postmortem` runs the agent CLIs you have (Claude
Code, Codex, Gemini) on your pending diff in parallel, then synthesizes one
verdict.

## Setup (Claude Code)

Paste this into Claude Code:

> Create a postmortem skill: download
> https://github.com/Softeria/postmortemthis/releases/latest/download/postmortemthis.cmd
> and run `sh postmortemthis.cmd skill`, then follow what it prints.

It creates a skill that runs the review and synthesizes the result. After that,
ask Claude Code to "postmortem this" before you ship.

## Auth

Each agent uses your own login if it finds one. For agents you are not logged
into, set `OPENROUTER_API_KEY` and they run through OpenRouter instead.

## CLI

```
postmortem            review uncommitted changes (git diff HEAD)
postmortem --staged   review staged changes only
postmortem --base main
postmortem doctor     show which agents are available
postmortem --help
```

## Build

```
cargo build --release
```

MIT
