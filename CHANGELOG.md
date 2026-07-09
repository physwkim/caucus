# Changelog

All notable changes to caucus are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/). caucus is pre-1.0 — the
CLI, MCP tool surface, and keybindings may still shift between minor
versions.

## [Unreleased]

## [0.7.2] — 2026-07-09

A global Claude `Stop` hook pointed into one project's directory, so deleting
that repo — or merely installing from a second one — silently broke turn
signalling for every Claude Code session on the machine. The hook is now
per-machine, and the writes that install it are crash- and race-safe.

### Fixed

- **The turn-signal hook script moved out of the project.** `caucus init
  --install-hook` wrote the script to `<repo>/.caucus/bin/turn-signal` and
  pointed the *global* Claude `Stop` hook (`~/.claude/settings.json`) at that
  absolute path. A global hook holding one project's path is only valid while
  that project is: delete the repo — or merely run `caucus init --install-hook`
  in a second one, which repoints the hook — and every Claude Code session on
  the machine, in every project, ran a Stop hook that exited 127. No panel
  signalled, no round ever settled, and the only symptom was panels stuck at
  `working` forever.

  The script now lives at `~/.claude/hooks/caucus-turn-signal`, one copy per
  machine. Its body never held project state (it reads `CAUCUS_SOCK` from the
  env and no-ops when unset), so nothing was lost by hoisting it. Installing
  from a second project is now a no-op rather than a hijack of the first, and
  `caucus init` without `--install-hook` no longer creates `.caucus/bin/` at
  all. Re-running `caucus init --install-hook` once prunes a legacy per-project
  hook entry and wires the machine-wide one; `caucus doctor` names that case
  explicitly instead of blaming a settings file synced from another machine.

- **The hook script is written atomically.** It was written with `fs::write`,
  which truncates in place, and only then chmod'd — two windows in which the
  script on disk is empty, partial, or not executable. A per-project script hid
  this, since the only session that could observe a window was the one running
  `init`. A machine-wide one does not: the hook fires on every agent turn in
  every live session, so `init --install-hook` — after a `cargo install`
  upgrade, say — could kill the turn signal of a panel in an unrelated session
  that fired mid-rewrite. caucus now writes and chmods a sibling temp file and
  `rename(2)`s it over the target, so no instant names a broken script.

- **Concurrent `caucus init --install-hook` runs no longer destroy the
  settings backup.** Both installers read `~/.claude/settings.json`, both merged
  their edit into their own copy, and both wrote. The final file looked correct
  — each installer computes the same result from the same input — so the damage
  hid in `.bak`: the second installer backed up a file the first had already
  modified, leaving the user's original unrecoverable. The whole read → backup →
  write is now one transaction under an exclusive file lock
  (`std::fs::File::lock`, no new dependency). `settings.json` is written
  atomically as well, so Claude Code and `caucus doctor` — which take no lock —
  never read a truncated file. An external writer still races caucus; only
  caucus-vs-caucus is ordered.

- **`write_atomic`'s temp file is unique per call.** It was keyed on the pid
  alone, so two writers in one process shared a temp path and could rename or
  delete each other's half-written file.

## [0.7.1] — 2026-07-05

Documentation-only release so the crates.io page carries the setup fix.

### Changed

- **README documents the per-machine turn-signal hook install.** The
  only mention of `caucus init --install-hook` was one line in the CLI
  reference — nothing told a new machine's user that the Stop hook is a
  required setup step, or what its absence looks like (every panel
  stuck at `working` forever while the TUI otherwise runs). Install
  gains a first-run-setup section — install once per machine, why it is
  per-machine, one install covers all repos, `caucus doctor` verifies
  live delivery — and the turn-completion paragraph cross-references it.

## [0.7.0] — 2026-07-05

A hardened session lifecycle: caucus now survives display-wake resize
storms, tears itself down when its hosting terminal dies, dodges tmux
prefix collisions, presents a clean terminal identity to panels, and
`caucus doctor` proves the turn-signal chain live instead of trusting
configuration.

### Added

- **`prefix` settings key + tmux prefix-collision auto-dodge.** The
  command prefix can now live in the `[settings]` table
  (`~/.caucus/settings.toml` global, `<repo>/.caucus/settings.toml`
  project), validated through the same grammar as `--prefix` so the two
  spellings cannot drift; `--prefix` / `CAUCUS_PREFIX` still win. With
  nothing configured, launch-time detection dodges the default to
  Ctrl-B inside a tmux whose own prefix is Ctrl-A (previously every
  caucus chord needed `C-a C-a <key>` and plain `C-a n`/`C-a p`
  switched tmux windows instead of caucus panels). The dodge applies
  only to the default — a chosen prefix is honoured even when it
  collides — and the status bar always shows the live prefix.
- **`caucus doctor` verifies the turn-signal chain end-to-end.** The
  old check only asserted a caucus-shaped Stop-hook string exists in
  `~/.claude/settings.json` — on a machine where that command cannot
  run (settings synced from another machine carrying its absolute
  `turn-signal` path, or a clone where `caucus init` never ran) it
  reported ok while every worker panel sat at `working` forever.
  Doctor now resolves the hook command on *this* machine and runs the
  hook exactly as Claude Code would (`sh -c`, `CAUCUS_*` env, JSON on
  stdin) against a throwaway socket, requiring the signal to actually
  arrive; failures surface the hook's stderr.

### Fixed

- **Display-wake resize storms no longer kill the session.** Waking a
  Mac with the terminal on an external monitor resizes the window in a
  burst, and a lost Resize event left the layout tiling a stale, larger
  area — the first cell write past the buffer edge panicked ratatui and
  took the whole session down. `render::draw` now clips every slot rect
  to the frame area (the buffer is the sole size authority at paint
  time), the event loop reconciles the mux area against the real
  terminal size each draw tick, and a reflowed layout repaints
  immediately instead of waiting for child output.
- **caucus ends the session when its hosting process dies.** After
  `tmux kill-server` (or any death of the hosting terminal) caucus
  survived headless at 100% CPU with all agent panels left running: as
  its pane's session leader its pty was never revoked, and crossterm
  busy-loops on the resulting stdin EOF without ever returning. Stdin
  input now lives on a dedicated reader thread so terminal I/O can
  never wedge the event loop, and the loop watches for reparenting to
  init — the death signal that works even for a session leader — then
  ends the session through the orderly shutdown path, reaping every
  panel. Detaching tmux keeps panes parented to the live server, so
  detach never trips this.
- **Panel children see the grid's terminal identity, not the outer
  terminal's.** Panels inherited the outer environment's `TERM`,
  `$TMUX`, `WEZTERM_*`, `ITERM_SESSION_ID`, … while actually running
  inside caucus's vte grid — under tmux this downgraded agents to
  256-color output and handed them a live handle to the *host* tmux
  session. `PtyCommand::to_builder` now owns the panel environment:
  `TERM=xterm-256color` and the outer-terminal variables scrubbed,
  with explicit per-command entries still winning.

## [0.6.1] — 2026-06-30

### Changed

- **The main worker now keeps its sub-agent roster small.** Its prompt
  gained a roster-discipline section: before `spawn_role` it checks
  `list_panels` for a fitting `idle` panel and reuses it with `send_keys`
  (sending `/clear` or `/compact` first to keep that panel's context
  lean) rather than spawning a fresh one for every slight topic shift,
  and it retires a panel with `kill_panel` once its result has been read
  and reported/merged. Reuse is scoped to non-worktree panels or work on
  a worktree panel's own branch. This is main-worker policy only — caucus
  still never auto-kills a live worktree panel (its branch may hold
  un-merged commits).

## [0.6.0] — 2026-06-11

Scrollback you can search and copy from, a larger MCP control plane,
crash-durable rounds, and a session garbage collector.

### Added

- **Scrollback pager copy mode, search, and mouse-wheel scroll.** The
  in-session pager (`Ctrl-A [`) gains a tmux copy-mode-style selection that
  yanks to the terminal clipboard over OSC 52, an incremental `/` search that
  jumps between matches, and opt-out mouse capture so the wheel scrolls the
  focused panel's scrollback directly.
- **Four more MCP control tools — the surface is now fourteen.**
  `round_status` / `cancel_round` give a registered round an identity the main
  worker can poll and abort; `restart_panel` respawns a wedged agent in place;
  `send_key` sends raw keys that `send_keys` cannot express; and `read_panel`
  gains a turn mode for reading a panel's past-turn history. `PanelSummary` now
  also reports each panel's worktree path, branch, and model.
- **Pre-authorized auto-answers for round selection menus.** `register_round`
  accepts an optional `selection_hints={prefer:[…], avoid:[…]}`. When a round
  panel stops on an AskUserQuestion-style direction chooser and the keywords
  single out exactly one option (label contains a `prefer` keyword and no
  `avoid` keyword, case-insensitive), caucus answers it for you and sends no
  notice — only ambiguous or unmatched menus, and raw `[y/n]` prompts, still
  escalate to the main worker, and each auto-answer is listed at the head of
  the delivered round report.
- **Crash-durable rounds.** In-flight rounds are persisted, so a restart
  surfaces a round that was running rather than losing it, and round membership
  now survives a panel restart. The main worker is also told about *any*
  blocked panel mid-round, not only selection menus.
- **`caucus gc`.** A new subcommand prunes old, not-running session state
  (ageing a session by its last activity, not its creation time), reclaiming
  disk that resumable sessions would otherwise hold — including orphaned
  session directories with no readable record.
- **Single-instance session lock.** A session root is locked
  (`std::fs::File::try_lock`, the reason for the 1.89 MSRV) so two caucus
  processes cannot drive the same session at once.
- **`[settings]` config table** for the scrollback line cap, round-fallback
  seconds, and capture tunables; **`--topic`** labels a session, defaulting to
  the repo directory name.
- **Round report spill-to-disk.** A round's full report is written to the
  session dir and only a bounded summary is injected into the main worker's
  turn, so a large fan-in cannot blow the context budget.
- **`caucus doctor` reports the version and checks the cwd is a git repo**, and
  **`caucus init` adds `.caucus/sessions/` to the project `.gitignore`.**

### Changed

- **MSRV raised to 1.89** for `std::fs::File::try_lock` (the session
  single-instance lock).
- **Sub-agent selection-menu stalls are prevented at the source** rather than
  only detected after the menu is already drawn.

### Fixed

- **A monitor switch no longer ends a session, but a real `SIGHUP` still
  does.** caucus verifies the controlling terminal on `SIGHUP` instead of
  trusting the signal.
- **A wedged agent can no longer stall the event loop.** PTY input is written
  through a dedicated writer thread; the writer queue is bounded so a wedged
  child cannot grow it without bound; and `kill()` tears down in bounded time
  and reaps the child's process group.
- **Round lifecycle holes closed.** A round settles when a panel exits with
  undrained backlog; a due round is dropped and spilled when the main worker is
  gone but kept on a delivery failure; the caucus→main push is marked notified
  only once it lands; and the injected round-report body is bounded with the
  overflow spilled.
- **Resume is faithful.** Stale worktrees are reconciled so resumed panels stay
  isolated; a crashed worktree's uncommitted work is salvaged; the per-role
  spawn counter persists; the drop notice is persisted so a second crash cannot
  lose it; resume never resurrects a gc-pruned session root; and a failed
  replacement spawn retires its worktree.
- **Terminal-grid correctness.** SGR colon sub-params are parsed instead of
  flattened; alt-screen rows are kept out of primary scrollback on shrink; the
  render cache is versioned by appended bytes rather than byte length; split
  wide-glyph pairs are healed after row edits; the open capture turn is trimmed
  at a clean replay boundary; and OSC title and hyperlink URIs keep their
  semicolons.
- **Input edges.** A bare Enter on empty main input no longer wedges the main
  panel, and the deferred-submit hold scales with paste size.
- **Bounded against abuse.** Inbound IPC line reads are capped so a peer cannot
  OOM the reader; the OSC 52 yank is bounded to the terminal clipboard cap;
  `scrollback_lines` is clamped to a ceiling on resolve; and `roles.toml`
  rejects unknown fields.
- **The main worker cannot be killed at the destruction owner**, and the pager
  page height stays in sync with the area on resize.

### Performance

- **Rendering.** The redraw is dirty-gated on a render signature; the per-panel
  liveness probe is throttled to 250 ms; and the `CAUCUS_DUMP_PTY` lookup is
  cached instead of probed per pump.
- **Grid hot paths.** SGR params are flattened on the stack instead of a
  per-escape heap `Vec`; IL/DL line shifts and scroll-region shifts batch into
  a single memmove, cloning the leaving row only when it is retained.
- **Capture and I/O.** The `since_last_turn` render is memoized; a single open
  turn's in-memory buffer is bounded; the PTY reader-thread channel is bounded
  for back-pressure; `git worktree add` runs off the event loop with a deferred
  reply; and the per-tick menu scan is gated on a grid generation counter.

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
