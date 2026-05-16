# caucus

A terminal multiplexer for **teams of AI coding agents**.

> Status: **v0 — redesign in progress.** caucus is pivoting from an async
> file-based meeting protocol to a live multiplexer. The README describes the
> target; parts of [`docs/design.md`](./docs/design.md) still reflect the old
> model and are being rewritten section by section.

## What it is

`caucus` is a full-screen terminal multiplexer — think `tmux` or `zellij` —
purpose-built for running a *team* of Claude Code / Codex agents on one
problem. Each agent gets its own panel. You watch every agent's session
live, side by side.

One panel is the **main worker**: a Claude Code agent you talk to directly.
It is not a passive boss — it does the work. You give it a task; it does the
small, sequential parts itself, and for work that parallelizes it spawns
**sub-agent panels** and hands each a focused piece. The main worker is the
intelligence; caucus is the frame that lets it reach into the sub-agents'
terminals.

```
        you
         │  keystrokes
         ▼
 ┌───────────────────────────────────────────────┐
 │  caucus  (full-screen multiplexer process)      │
 │  ┌──────────┬──────────┬──────────┬──────────┐  │
 │  │ main     │ worker   │ worker   │ worker   │  │  ← panel = agent, one PTY each
 │  │ (claude) │ (claude) │ (codex)  │ (claude) │  │
 │  └──────────┴──────────┴──────────┴──────────┘  │
 │   pty (portable-pty) · term (vte grid) ·         │
 │   render (ratatui)   · input routing             │
 │   MCP server ◄──────── main worker drives subs   │
 └───────────────────────────────────────────────┘
```

## What it is not

- **Not a general-purpose multiplexer.** caucus knows about agent roles,
  turn boundaries, and a main worker orchestrating sub-agents. For plain
  human terminal multiplexing, use `tmux` or `zellij`.
- **Not a replacement for `claude-code`.** That drives a single agent.
  caucus arranges many of them into a team.
- **Not a "swarm intelligence platform."** No learning loop, no
  auto-tuning, no proprietary memory. caucus is one thing: a live frame
  for a main worker that fans work out to parallel sub-agents.

## How it works

**caucus owns the terminal.** It manages a PTY per panel (`portable-pty`),
parses each agent's output into a screen grid (`vte` + a hand-written grid),
and renders the panels (`ratatui`). tmux and zellij are *not* dependencies —
they are studied as references for the grid and layout design.

**The main worker drives sub-agents over MCP.** caucus runs an MCP server.
The main worker gets caucus tools — `send_keys`, `ctrl_c`, `read_panel`,
`spawn_role`, `kill_panel`, and so on — so a single instruction from you
turns into the main worker decomposing the task, spawning sub-agent panels,
and feeding each one its sub-task as real keystrokes.

**Sub-agents are dynamic parallel workers.** The team is not a fixed roster
of specialists. The main worker splits a task into sub-tasks and spawns as
many homogeneous sub-agents as the work needs — the default sub-agent role
is a generic `worker`; the specialist roles (`architect`, `backend`,
`reviewer`, …) are optional hints. Each code-writing sub-agent gets its own
git worktree, so parallel work stays isolated and the main worker merges the
results. By its own judgment the main worker picks the model and backend CLI
(`claude` / `codex` / `gemini`) per sub-agent, spawns and kills panels as the
work demands — caucus reflows the layout — and watches per-panel token usage
to send `/compact` or `/clear` when a context grows inefficient. caucus
provides the mechanism; the main worker owns the policy.

**Panels are fully interactive.** A caucus panel is a real bidirectional
terminal, not a read-only view. You — or the main worker — can type into any
panel, including driving interactive flows such as a `claude` / `gemini`
login or an OAuth device-code prompt.

**Turn completion is live.** Each agent's Claude `Stop` hook posts to a
caucus socket the moment a turn ends — no polling, no sentinel files. The
main worker sees "that sub-agent finished its turn" immediately and reacts.

## Install

```bash
git clone https://github.com/physwkim/caucus.git
cd caucus
cargo install --path .       # places `caucus` on $PATH
```

Requirements:

- git 2.20+
- Claude Code CLI (`claude` 2.x) on `PATH`
- Codex CLI (`codex`) on `PATH` — optional, for Codex-backed roles
- Gemini CLI (`gemini`) on `PATH` — optional, for Gemini-backed roles
- Rust 1.85+ (edition 2024)

No tmux dependency — caucus is its own multiplexer.

## Roles

The embedded roles are read-only defaults; override per-project in
`<repo>/.caucus/roles.toml` or globally in `~/.caucus/roles.toml`:

| Role               | Agent CLI | Default model | Tools                                                  | Permission mode |
|--------------------|-----------|---------------|--------------------------------------------------------|-----------------|
| `main`             | claude    | `opus`        | Read, Glob, Grep, Edit, Write, Bash, TodoWrite, Web*   | `default`       |
| `worker`           | claude    | `sonnet`      | Read, Glob, Grep, Edit, Write, Bash, TodoWrite         | `acceptEdits`   |
| `architect`        | claude    | `opus`        | Read, Glob, Grep, WebFetch, WebSearch, TodoWrite       | `plan`          |
| `backend`          | claude    | `sonnet`      | + Edit, Write, Bash                                    | `acceptEdits`   |
| `reviewer`         | claude    | `opus`        | Read, Glob, Grep, Bash                                 | `default`       |
| `qa`               | claude    | `haiku`       | Read, Glob, Grep, Bash                                 | `default`       |
| `scribe`           | claude    | `haiku`       | Read, Glob, Grep, Edit, Write                          | `acceptEdits`   |
| `serious-reviewer` | codex     | (codex picks) | Read, Glob, Grep, Bash                                 | `default`       |

`main` is the orchestrator panel you talk to; `worker` is the default
sub-agent the main worker spawns for parallel work. The remaining roles are
optional specialist hints — use them when a sub-task clearly calls for one.

Cost tiers reflect each role's cognitive load: Opus where shaping decisions
and finding subtle issues matters, Sonnet where executing a defined plan
suffices, Haiku where the work is mechanical. The default-model column uses
claude CLI tier *aliases* (`opus` / `sonnet` / `haiku`) so caucus follows the
latest generation automatically; pin a version in `roles.toml` for
reproducibility.

System-prompt templates live under `roles/`. Each sub-agent role inherits the
claw-code "4-constraint scaffolding" (delegated task / only tools / no
questions / concise result).

## Related projects

- [`tmux`](https://github.com/tmux/tmux) — reference for the session /
  pane / keystroke-routing model.
- [`zellij`](https://github.com/zellij-org/zellij) — Rust multiplexer;
  reference for the `vte`-based grid and layout engine. caucus reuses the
  same published crates (`vte`, `portable-pty`) zellij stands on rather
  than vendoring zellij itself.
- [`dmux`](https://github.com/formkit/dmux) — TUI for humans running many
  agents in parallel.
- [`claw-code`](https://github.com/ultraworkers/claw-code) — Rust port of
  claude-code; source of the role scaffolding and lane-event model.

## License

MIT.
