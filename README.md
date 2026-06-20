<p align="center">
  <img src="assets/overview.svg" alt="postmortemthis: every coding agent you have, one verdict" width="900">
</p>

**Why?** We needed it.

**Why really?** The agent that wrote your code is the worst judge of it.

**What?** Every coding-agent CLI (Claude Code, Codex, Gemini, Qwen, Vibe) reads your diff so you don't have to (you weren't going to). Read-only, in parallel; you stay in your agent, it makes the call.

**How?** One tiny script that installs, updates, and runs all the agents for you, on Windows, macOS, and Linux. No setup, no server, no MCP.

## Setup

Paste this into Claude Code, Codex, or your coding agent of choice:

> Create a "/postmortemthis" skill to review whatever we are working on. Download
> https://github.com/Softeria/postmortemthis/releases/latest/download/postmortemthis.cmd
> once into the skill folder (re-fetch only if missing), then run the local copy — it runs
> one prompt across several coding agents at once. Run it as `echo "…" | sh postmortemthis.cmd`.

Then, before the real postmortem, run **/postmortemthis**.

That prompt is the whole install — your agent writes the skill from it. **Tweak the wording before you hit enter** (different agents, a sharper review focus, your own house rules); the [skill in this repo](skills/postmortemthis/SKILL.md) is just a starting point, and your agent will happily make one as good or better. The script is the only fixed part.

## Auth

Your own logins where you have them. For the rest, `sh postmortemthis.cmd login` (OpenRouter OAuth) or set `OPENROUTER_API_KEY`. Usage bills to your account; this resells nothing.

## CLI

```
echo "review the pending diff for bugs" | sh postmortemthis.cmd   # all agents
sh postmortemthis.cmd doctor                                      # what's available
```

MIT
