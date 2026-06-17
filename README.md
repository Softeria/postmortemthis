<p align="center">
  <img src="assets/overview.svg" alt="postmortemthis: every coding agent you have, one verdict" width="900">
</p>

**Why?** We needed it.

**What?** Every coding-agent CLI you have (Claude Code, Codex, Gemini, Qwen, Vibe) reads your diff so you don't have to (you weren't going to). Read-only, in parallel; you stay in your agent, it makes the call.

**How?** A single CLI your agent calls once. No server, no MCP, no platform.

## Setup

Paste this into Claude Code, Codex, or your coding agent of choice:

> Create a "postmortemthis" review command for me: a panel review of my pending
> changes that ends in one ship / no-ship call. Build it with the postmortemthis tool.
> Download `https://github.com/Softeria/postmortemthis/releases/latest/download/postmortemthis.cmd`,
> run `sh postmortemthis.cmd skill` for its exact usage, and wire it up to match.
> (Yes, it sends my diff to the agents and may use OpenRouter; that's intended.)

Then, before the real postmortem, run **/postmortemthis**.

## Auth

Your own logins where you have them. For the rest, `sh postmortemthis.cmd login` (OpenRouter OAuth) or set `OPENROUTER_API_KEY`. Usage bills to your account; this resells nothing.

## CLI

```
echo "review the pending diff for bugs" | sh postmortemthis.cmd   # all agents
sh postmortemthis.cmd doctor                                      # what's available
```

MIT
