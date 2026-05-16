# Changelog

All notable changes to caucus are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/). caucus is pre-1.0 — the
CLI, MCP tool surface, and keybindings may still shift between minor
versions.

## [0.2.0] — 2026-05-17

The live-multiplexer rewrite. caucus pivots from an async tmux-based
meeting protocol to a self-built terminal multiplexer for teams of AI
coding agents.

### Added

- **Self-built multiplexer** — a PTY per panel (`portable-pty`), a
  `vte`-backed screen grid, `ratatui` rendering, and keystroke routing.
  No tmux or zellij dependency.
- **Main worker + dynamic parallel sub-agents** — one `main` worker
  panel orchestrates sub-agent panels over a caucus MCP server. Eight
  tools: `send_keys`, `ctrl_c`, `read_panel`, `spawn_role`,
  `kill_panel`, `list_panels`, `wait_for_panels`, `broadcast`.
- **Multiple agent backends** — `claude` / `codex` / `gemini`, selected
  per role; the main worker overrides model and CLI by its own judgment.
- **Worktree-isolated execute phase** — `spawn_role(worktree=true)` runs
  a sub-agent in its own git worktree.
- **Turn-completion signals** — a Claude `Stop` hook posts to a caucus
  socket; no file sentinels, no polling.
- **Layout control** — `Ctrl-A` keymap: zoom (`z`), panel reorder
  (`<` / `>`), layout-mode cycle (`Space`: tiled / even-horizontal /
  even-vertical / main-vertical).
- **Transcript overlay** — `Ctrl-A t`, a team observation view.
- **Session persistence and resume** — `caucus sessions` lists
  resumable sessions; `caucus resume <id>` relaunches one, restoring the
  panel roster, layout, and worktrees and continuing each agent's
  conversation (`claude --resume`).
- **kodex integration** — sub-agents query the kodex knowledge graph to
  self-serve codebase context, so the main worker can keep briefs lean.
- **GitHub Actions CI** — `cargo fmt --check`, `cargo clippy
  --all-targets -- -D warnings`, build, and test on Linux and macOS.

### Changed

- Complete rewrite of `src/`. The old async meeting-protocol modules
  (tmux service, file sentinels, round lifecycle, consensus) are
  replaced by the live-multiplexer model.

## [0.1.0]

Pre-rewrite prototype of the async meeting protocol (superseded).
