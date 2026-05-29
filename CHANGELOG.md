# Changelog

All notable changes to caucus are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/). caucus is pre-1.0 — the
CLI, MCP tool surface, and keybindings may still shift between minor
versions.

## [0.5.0] — 2026-05-29

Survives multi-monitor display switches, and lets you remap the prefix key.

### Added

- **Configurable prefix key.** The reserved prefix (still `Ctrl-A` by default)
  can be remapped with `--prefix <letter>` or `CAUCUS_PREFIX=<letter>`, so it can
  dodge a collision with an outer multiplexer — e.g. a tmux remapped to `Ctrl-A`.
  `--prefix b` (or `CAUCUS_PREFIX=b`) reserves `Ctrl-B`; a bare letter or a
  `ctrl-b` / `^b` form both parse, case-insensitively. The status-bar hint and
  the literal-prefix passthrough (`prefix prefix` → one literal `Ctrl-<key>`
  byte) follow the configured key. Applies to fresh launches and `resume`.

### Fixed

- **The TUI survives a monitor/DPI switch.** A WezTerm window moving between
  displays of different DPI — e.g. the MacBook built-in panel handing off to an
  external monitor as it powers on — fires a `SIGWINCH` storm. crossterm's
  `terminal::size()` does the `TIOCGWINSZ` ioctl with no `EINTR` retry, so each
  storm-interrupted call failed and surfaced as an error out of `event::poll`;
  with the signal staying pending no successful idle poll ever cleared the
  streak, and the consecutive-error give-up budget tripped after ~2.5 s and tore
  down a live session (clean exit to the shell). caucus now treats every
  terminal I/O error as transient and ends the session only on a genuine
  `SIGHUP` (window closed, parent gone, SSH dropped), which a monitor switch
  never sends — and still does so through the orderly shutdown path.

## [0.4.1] — 2026-05-27

Patch release: turn-completion and rendering fixes on top of 0.4.0.

### Fixed

- **Codex panels now report turn completion, so the main worker wakes on time.**
  caucus detected the end of a turn only through claude's `Stop` hook; codex has
  no such hook, so a codex panel sat in `working` forever after its first prompt
  and a registered round settled only at its (long) fallback deadline — then
  misreported the finished panel as *"still working."* caucus now registers
  `caucus signal codex-notify` as codex's `notify` program (`-c notify=[...]`,
  injected for every codex panel); codex invokes it on `agent-turn-complete` with
  the event JSON, and it posts the same `Stop` turn-signal claude's hook posts,
  so both backends settle a panel through one owner. The grid-hint path that was
  intended for this (`GridHint::PromptReady`) was never wired and is unchanged.
- **Capture turns survive a second submit/selection in the same turn.** A submit
  or selection response arriving while a panel was still `Working` could drop the
  active output capture; the open capture turn is now preserved.
- **Round backlog stops launching tasks after the fallback deadline.** Once a
  round's fallback deadline passes no new backlog task is sent, and any queued
  backlog must drain before the round settles.
- **Scroll pager opens at the true bottom.** The pager page height was corrected
  so opening at the bottom includes the newest retained line.
- **Stale wide-glyph halves are cleared on overwrite**, preventing shifted
  rendering when a wide cell is partially overwritten.

## [0.4.0] — 2026-05-27

Free-form roles: the main worker is no longer limited to a fixed roster.

### Added

- **Selectable main-worker backend.** `caucus --agent-cli claude|codex` picks
  the CLI that runs the main panel you talk to (default `claude`). A codex main
  still orchestrates sub-agents: caucus registers its MCP control plane with
  codex via `-c mcp_servers.caucus.command/args` (codex has no `--mcp-config`
  flag and reads MCP servers only from `[mcp_servers.<name>]` config), so the
  same ten control-plane tools are available regardless of backend. When the
  flag switches the backend away from a role's native model, that role's model
  is dropped rather than passed to the other CLI.
- **Free-form sub-agent roles.** `spawn_role` now takes a free-form role
  *label* plus an optional inline `prompt`. A known preset name reuses that
  preset's tool allowlist and permission mode; any other label is an ad-hoc
  role built on the generic `worker` defaults under that name. When `prompt`
  is set it *is* the agent's system prompt (replacing the preset's template),
  so the main worker invents a role on the fly — naming it, writing its
  instructions, and picking its model and backend CLI by its own judgment.
  An unknown role label no longer errors; it spawns a generic worker.

### Changed

- **Codex-backed roles now receive their system prompt**, injected via codex's
  `-c instructions=<text>` base-instructions override (codex has no
  `--append-system-prompt` flag). Previously the role prompt was dropped for
  the codex backend; now both preset (`serious-reviewer`) and free-form
  codex roles run with their instructions.
- **Codex panels no longer stall on the directory-trust gate.** codex prompts
  *"Do you trust the contents of this directory?"* before its first turn the
  first time it runs in a directory; caucus drives codex non-interactively, so
  nothing answered it and the panel hung. caucus now pre-grants trust for each
  codex panel's cwd (`[projects."<realpath>"] trust_level = "trusted"`) in
  codex's on-disk config — the same entry codex persists on "Yes". A runtime
  `-c` override is not honored for the trust decision, so the on-disk entry is
  required; the edit is format-preserving (`toml_edit`) and best-effort (a
  write failure logs a warning and the panel still launches). Honours
  `CODEX_HOME` when set.

### Removed

- **Dropped the `gemini` backend.** The Gemini CLI is no longer a supported
  `agent_cli`: the `AgentCli::Gemini` variant, its argv builder, and the
  `caucus doctor` gemini probe are gone, and the `spawn_role` `agent_cli` enum
  is now `claude` | `codex`. A persisted `session.json` that pinned any panel
  to `gemini` no longer deserializes, so `caucus sessions` skips that whole
  session (logged as an unreadable record) and it cannot be resumed.

## [0.3.1] — 2026-05-26

### Fixed

- Extended-colour (256-colour / truecolor) indices are folded into the ANSI
  encoding, so dark text rendered through an agent CLI's palette stays legible
  in caucus panels.

## [0.3.0] — 2026-05-26

Push-based rounds, an in-session scrollback pager, and sub-agent menu control.

### Added

- **Push-based round fan-in.** `register_round` replaces the old blocking
  `wait_for_panels`: caucus watches the named panels and, once they all settle
  (or `fallback_secs` elapses), assembles their results and pushes them to the
  main worker as a fresh turn — so the main worker ends its turn instead of
  sleep-polling. An optional per-panel `backlog` queue feeds an early finisher
  its next task so it never idles at the barrier.
- **Sub-agent selection menus.** `read_menu` / `select_option` let the main
  worker read and answer an AskUserQuestion-style chooser shown in a sub-agent
  panel; caucus detects a stuck `awaiting_selection` panel and notifies the
  main worker so the round can settle.
- **In-session scrollback pager** (`Ctrl-A [`): a tmux copy-mode-style view of
  the focused panel's scrollback (the grid ring, up to 10,000 rows).
- **Role system-prompt injection.** A role's prompt template is resolved at
  spawn and injected into the `claude` backend via `--append-system-prompt`.
- **Cross-panel handoffs.** `CAUCUS_SESSION_DIR` is injected into every panel —
  a shared path reachable even from an isolated worktree cwd — for artifacts
  passed between panels (e.g. a review doc).
- **tmux-style panel reorder** — `Ctrl-A <` / `Ctrl-A >` move the focused panel
  earlier / later in the arrangement.
- **Resilient event loop** with a consecutive-error budget, and a screen-grid
  clamp that bounds panel dimensions to a safe maximum.

### Changed

- The MCP tool surface is now ten tools: `wait_for_panels` is gone, replaced by
  `register_round`, alongside the new `broadcast`, `read_menu`, and
  `select_option`.
- `src/runtime` is decomposed from one god-file into focused submodules
  (control / input / layout / mcp / rounds / scroll / spawn / persist) with no
  behaviour change.

### Fixed

- Prompts are delivered as bracketed paste with the submitting Enter deferred
  as a discrete keypress, so multi-line prompts submit reliably.
- Non-worktree panels run in the session repo root, not `$HOME`.
- Control + turn-signal sockets are removed on shutdown; `caucus mcp-serve`
  self-terminates when the main caucus process is gone.
- `caucus init` errors instead of panicking on a malformed `settings.json`, and
  migrates stale caucus Stop hooks rather than bailing.

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
