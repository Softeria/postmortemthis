# postmortemthis

Runs the AI agent CLIs you have (Claude Code, Codex, Gemini) in parallel on one
prompt, read-only, and prints each one's output. That is all it does: the
caller writes the prompt and synthesizes the results.

## Setup (Claude Code)

Paste this into Claude Code:

> Create a postmortem skill: download
> https://github.com/Softeria/postmortemthis/releases/latest/download/postmortemthis.cmd
> and run `sh postmortemthis.cmd skill`, then follow what it prints.

It creates a skill that composes a review prompt, runs the panel, and
synthesizes the verdict. Then ask Claude Code to "postmortem this".

## Auth

Each agent uses your own login if it finds one. For agents you are not logged
into, set `OPENROUTER_API_KEY` and they run through OpenRouter instead.

## CLI

```
echo "review the pending changes for bugs" | postmortem   # all available agents
postmortem --agents claude,codex                          # subset
postmortem doctor                                         # show available agents
```

The prompt is read from stdin. Agents run read-only in the current directory.

## Build

```
cargo build --release
```

MIT
