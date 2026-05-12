# caucus

Collaboration swarm orchestrator for Claude Code agents over tmux + git worktree.

> Status: **v0 in progress.** API and CLI surface may shift until the first tagged release. See [`docs/design.md`](./docs/design.md) for the full specification.

## What it is

`caucus` is the infrastructure layer for running a *meeting* of Claude Code sub-agents on the same problem. A **CEO** session (your main `claude` process) drives the meeting via `caucus` shell commands — opening role-typed agent panes, distributing an agenda, collecting written responses, and (after consensus) spawning execution agents with isolated git worktrees.

`caucus` itself is *deliberately small*. It manages tmux panes, worktrees, sentinel files, and an event-sourced agent manifest. **It does not call Notion, kodex, or any external API.** External synchronisation is the CEO's job, using its own MCP toolbox.

## What it is not

- Not a replacement for [`dmux`](https://github.com/formkit/dmux) — `dmux` is for *humans* running many agents in parallel.
- Not a replacement for [`claude-code`](https://github.com/anthropics/claude-code) (or its Rust port [`claw-code`](https://github.com/ultraworkers/claw-code)) — those drive a single agent.
- Not a "swarm intelligence platform" — there is no learning loop, no auto-tuning, no proprietary memory.

`caucus` is one well-defined thing: a meeting protocol over tmux.

## Design

See [`docs/design.md`](./docs/design.md). Key decisions:

- **Mode**: `teammateMode = "tmux"` (v0). Each agent is a `claude` CLI process in its own tmux pane.
- **State**: every agent gets a JSON manifest with a `LaneEvent` timeline and a derived 8-state machine (`working`, `finished_cleanable`, `blocked_merge_conflict`, …).
- **Coordination**: CEO Claude reads response files, decides convergence, drives the state machine via `caucus session converge` etc.
- **Settle detection**: Claude's `Stop` hook writes a sentinel JSON; `caucus` watches for it. Screen-scraping is a fallback only.

Related notes:

- [`docs/dmux-analysis.md`](./docs/dmux-analysis.md) — patterns borrowed from `dmux` (tmux service, status detection, worktree cleanup).
- [`docs/claw-code-analysis.md`](./docs/claw-code-analysis.md) — patterns borrowed from `claw-code` (subagent typing, lane events, derived state).

## Status

Pre-alpha. Public API may break without notice. License: MIT.
