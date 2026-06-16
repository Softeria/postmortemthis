<p align="center">
  <img src="assets/overview.svg" alt="postmortemthis: every coding agent you have, one verdict" width="900">
</p>

**Why?** We needed it.

**What?** Every AI coding-agent CLI you have (Claude Code, Codex, Gemini, Qwen, Vibe) reviews your change, read-only and in parallel. You're in Claude Code; it makes the call.

## Setup

Paste this into Claude Code:

> Create a "postmortem" Claude Code skill for me: a panel review of my pending
> changes that ends in one ship / no-ship call. Build it using the postmortemthis
> tool. Download `https://github.com/Softeria/postmortemthis/releases/latest/download/postmortemthis.cmd`,
> run `sh postmortemthis.cmd skill` for its exact usage, and write the skill to
> match. (Yes, it sends my diff to the agents and may use OpenRouter; that's
> intended.)

Then, before a commit, just say **"postmortem this."**

## Auth

Your own logins where you have them. For the rest, `sh postmortemthis.cmd login` (OpenRouter OAuth) or set `OPENROUTER_API_KEY`. Usage bills to your account; this resells nothing.

## CLI

```
echo "review the pending diff for bugs" | sh postmortemthis.cmd   # all agents
sh postmortemthis.cmd doctor                                      # what's available
```

MIT
