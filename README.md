# caucus

A terminal multiplexer for **teams of AI coding agents**.

> Status: **v1 feature-complete, pre-1.0.** The pivot from an async
> file-based meeting protocol to a live multiplexer is done: the live
> multiplexer, the MCP control plane, layout control, the transcript
> overlay, and session persistence/resume have all landed.
> [`docs/design.md`](./docs/design.md) now matches the shipped code. The
> CLI and MCP surfaces are not yet stable — expect changes before 1.0.

## What it is

`caucus` is a full-screen terminal multiplexer — think `tmux` or `zellij` —
purpose-built for running a *team* of Claude Code / Codex agents on one
problem. Each agent gets its own panel. You watch every agent's session
live, side by side.

One panel is the **main worker**: a Claude Code (or, with `--agent-cli codex`,
a Codex) agent you talk to directly. It is not a passive boss — it does the work. You give it a task; it does the
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
The main worker gets fourteen caucus tools (see [MCP tools](#mcp-tools) below)
so a single instruction from you turns into the main worker decomposing the
task, spawning sub-agent panels, and feeding each one its sub-task as real
keystrokes.

**Sub-agents are dynamic parallel workers.** The team is not a fixed roster
of specialists — not even a fixed set of *roles*. The main worker splits a
task into sub-tasks and spawns as many sub-agents as the work needs, and it is
**not limited to a preset role list**: `spawn_role` takes a free-form role
*label* plus an optional inline `prompt`, so the main worker invents a role on
the fly — naming it, writing its instructions, and choosing its model and
backend CLI (`claude` / `codex`) entirely by its own judgment. The
named presets (`worker`, `architect`, `backend`, `reviewer`, …) are convenient
starting points; any other label is an ad-hoc role built on the generic
`worker` defaults. Each code-writing sub-agent gets its own git worktree, so
parallel work stays isolated and the main worker merges the results. It spawns
and kills panels as the work demands — caucus reflows the layout — and watches
per-panel token usage to send `/compact` or `/clear` when a context grows
inefficient. caucus provides the mechanism; the main worker owns the policy.

**Panels are fully interactive.** A caucus panel is a real bidirectional
terminal, not a read-only view. You — or the main worker — can type into any
panel, including driving interactive flows such as a `claude` login or an
OAuth device-code prompt.

**Turn completion is live.** Each agent's Claude `Stop` hook posts to a
caucus socket the moment a turn ends — no polling, no sentinel files. The
main worker sees "that sub-agent finished its turn" immediately and reacts.
This hook is installed once per machine with `caucus init --install-hook`
(see [Install](#install)); without it every panel stays `working` forever.

## Keymap

caucus reserves a single prefix key, `Ctrl-A` by default, for its own commands.
Every other keystroke — including `Ctrl-C` — is encoded to terminal bytes and
forwarded verbatim to the focused panel's PTY, so an agent CLI sees a real
terminal.

The prefix is configurable, so it can dodge a collision with an outer
multiplexer — for example a tmux remapped to `Ctrl-A`. Pass `--prefix b`, set
`CAUCUS_PREFIX=b`, or persist it once with `prefix = "b"` in the `[settings]`
table of `~/.caucus/settings.toml` (or per-repo in
`<repo>/.caucus/settings.toml`) to use `Ctrl-B` instead (any letter; a bare
`b` or a `ctrl-b` / `^b` form both work; flag/env beats settings). With no
configuration at all, launching inside a tmux whose own prefix is `Ctrl-A`
auto-dodges the default to `Ctrl-B` — otherwise tmux would swallow every
caucus command — and the status bar shows the live prefix either way. The
table below shows the default; substitute your prefix for `Ctrl-A`.

| Key                           | Action                                   |
|-------------------------------|------------------------------------------|
| `Ctrl-A` then `n` / `p`       | focus the next / previous panel (cycle)  |
| `Ctrl-A` then `↑` `↓` `←` `→` | focus the panel in that direction        |
| `Ctrl-A` then `Ctrl-↑↓←→`     | resize the focused panel (tmux-style)    |
| `Ctrl-A` then `q`             | quit caucus                              |
| `Ctrl-A` then `z`             | toggle zoom on the focused panel         |
| `Ctrl-A` then `<` / `>`       | move the focused panel one step earlier / later |
| `Ctrl-A` then `x`             | close the focused panel (y/n confirm)    |
| `Ctrl-A` then `Space`         | cycle the layout arrangement mode        |
| `Ctrl-A` then `t`             | toggle the transcript overlay            |
| `Esc` (overlay open)          | hide the transcript overlay              |
| `Ctrl-A` then `[`             | open the scrollback pager (focused panel) |
| scroll wheel                  | `PageUp` / `PageDown` — sent to the focused panel, or paged in the pager (mouse capture on) |
| `Ctrl-A` then `Ctrl-A`        | send a literal `Ctrl-A` to the panel     |

The layout modes cycled by `Ctrl-A Space` are **Tiled**, **EvenHorizontal**,
**EvenVertical**, and **MainVertical** — caucus reflows the panels into the
chosen arrangement.

The **transcript overlay** (`Ctrl-A t`) is a read-only team observation
view: one row per panel showing role, derived state, completed-turn count,
worktree branch, and the first line of the agent's last message. `Esc`
hides it; it does not capture input, so every other key still reaches the
focused panel while it is open.

The **scrollback pager** (`Ctrl-A [`) is a tmux copy-mode-style view of the
focused panel's terminal scrollback (the grid ring, up to 10,000 rows). It
opens at the newest line; `↑/↓ k/j` scroll a line, `PgUp/PgDn` a page,
`g/Home` jumps to the oldest line, `G/End` to the newest, and `Esc/q` exits.
Inside the pager, `/` searches (case-insensitive; `Enter` runs it, `n`/`N`
step matches), and `v` starts a line-selection **copy mode** — move to extend
the selection, then `y`/`Enter` copies it to your clipboard (via OSC 52),
`Esc` cancels. Unlike the transcript overlay, the pager **captures** input —
keys drive scrolling and none reach the panel's PTY — and shows a frozen
snapshot (the panel keeps running; new output appears after you exit).

The **scroll wheel** (with mouse capture on — the default; the `mouse` setting)
has no binding of its own: a notch up/down is a `PageUp`/`PageDown` keypress. In
the live view that goes to the focused panel, so the wheel scrolls the agent's
own pager and you see what you scrolled to; while the caucus pager is open it
pages the pager, exactly as the key does.

## MCP tools

caucus exposes fourteen tools to the main worker over MCP. The main worker
calls them to spawn, drive, observe, and reap sub-agent panels:

| Tool              | What it does                                                              |
|-------------------|---------------------------------------------------------------------------|
| `send_keys`       | Type text into a panel's terminal; `enter=true` appends a newline.        |
| `send_key`        | Send one named key (`enter`, `esc`, `ctrl-c`, `up`, …) to a panel — a single keypress, not text. |
| `broadcast`       | Send the same text to several panels at once — a round's fan-out.         |
| `ctrl_c`          | Send `Ctrl-C` (interrupt) to a panel.                                     |
| `read_panel`      | Read a panel's captured output (modes below).                             |
| `spawn_role`      | Spawn a sub-agent panel. `role` is a free-form label; an inline `prompt` becomes the role's instructions; `worktree` / `model` / `agent_cli` overrides. |
| `kill_panel`      | Kill a panel; its worktree (if any) is enqueued for cleanup.              |
| `restart_panel`   | Restart a panel's agent CLI in place, reusing its role and worktree.      |
| `list_panels`     | List every live panel with its role and derived state.                   |
| `register_round`  | Register a round; caucus pushes the panels' results back when they settle (or `fallback_secs`). A per-panel `backlog` queue keeps early finishers working until their tasks drain. Optional `selection_hints` let caucus auto-answer a panel's direction menus by option-label keyword, so a pre-authorized fork never interrupts you. |
| `round_status`    | Report a registered round's progress — which panels have settled, which are still working. |
| `cancel_round`    | Cancel a registered round so caucus stops awaiting it.                    |
| `read_menu`       | Read a panel's interactive selection menu (question + numbered options).  |
| `select_option`   | Answer a panel's selection menu by picking an option number.              |

`read_panel` takes a `mode`: `screen` (the visible grid), `scrollback` (the
full scrollback buffer), `since_last_turn` (everything since the last
prompt — the whole turn, no racing the screen), or `last_message` (the
agent's final message from its turn signal).

When a sub-agent stops mid-turn on an interactive chooser (an
AskUserQuestion-style menu), no turn signal fires, so the panel reads
`awaiting_selection` and its round cannot settle on its own. caucus detects
the menu and pushes the main worker a notice; the main worker answers it
with `read_menu` + `select_option` (or, for a free-text reply, picks the
menu's "type something" option, then `send_keys`), unblocking the round.

## CLI

`caucus` with no subcommand launches the multiplexer TUI. The subcommands
are for bootstrap, inspection, and resume — live control is exposed to the
main worker over MCP, not the CLI.

```
caucus                              # launch the multiplexer TUI in the current git repo
caucus --roles architect,backend,reviewer
                                    # launch with an initial panel roster
caucus --agent-cli codex            # run the main worker on codex instead of
                                    # claude (default); sub-agent backends are
                                    # still chosen per spawn_role
caucus init [--install-hook]        # create .caucus/; --install-hook writes the
                                    # machine-wide turn-signal script and merges
                                    # the Claude Stop hook
caucus doctor                       # check caucus version, git + repo, agent CLIs, hook, role allowlists
caucus role list                    # list known roles
caucus role show <name>             # show one role's full spec
caucus sessions [--format json]     # list resumable sessions, newest first
caucus resume <session_id>          # relaunch the TUI restoring a persisted session
```

(`caucus signal post` and `caucus mcp-serve` also exist but are internal —
the Stop hook and the main worker's Claude Code instance invoke them, not a
human.)

## Sessions

A caucus session is otherwise ephemeral — when caucus exits, the agent
processes die. To make a session resumable, caucus writes a session record
to `.caucus/sessions/<session_id>/session.json` whenever the panel roster
changes: the roles, their order, each panel's CLI/model, any worktree
branch, and the Claude conversation id.

`caucus sessions` lists those records (id, age, panel count, topic).
`caucus resume <session_id>` relaunches the TUI and recreates the panels
from the record — re-attaching worktrees on their branches and continuing
each Claude conversation via `claude --resume`.

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
- Rust 1.85+ (edition 2024)

No tmux dependency — caucus is its own multiplexer.

### First-run setup: install the turn-signal hook

Once per machine, from any git repo:

```bash
caucus init --install-hook   # writes ~/.claude/hooks/caucus-turn-signal and
                             # merges the Claude Stop hook into ~/.claude/settings.json
caucus doctor                # verifies the chain with a live end-to-end signal test
```

The Stop hook is how caucus learns an agent's turn has finished. **Without
it, every panel stays `working` forever and rounds never settle** — the TUI
runs, agents work, but no result ever comes back. Plain `caucus init`
(without `--install-hook`) creates `.caucus/` and leaves your Claude
settings, and the hook script, untouched.

One install covers every repo on the machine. Both halves of that are
deliberate: the script lives at a fixed path under `~/.claude/`, and its body
reads the session from the `CAUCUS_*` env caucus injects at panel spawn, so it
is tied to neither a repo nor a session. Running `caucus init --install-hook`
again from a second project is a no-op, not a hijack of the first.

The script execs `caucus` by bare name, and the hook entry is an absolute
path on *this* machine, so:

- run `caucus init --install-hook` on **every machine** you use caucus on —
  a `~/.claude/settings.json` synced from another machine carries a `$HOME`
  that does not exist locally, and turn signals silently die;
- keep `caucus` on `PATH` (a `cargo install` already does).

If you used caucus before this hook moved out of `.caucus/bin/`, re-run
`caucus init --install-hook` once: it prunes the old per-project hook entry and
wires the machine-wide one. Until then, deleting the project that happened to
own the script left every Claude Code session — in every project — running a
Stop hook that exits 127.

When in doubt, `caucus doctor` tests actual delivery, not just configuration.

## Roles

Roles are **presets, not a closed set.** The main worker is free to spawn an
ad-hoc role — `spawn_role(role="<any label>", prompt="<its instructions>", …)`
— and caucus builds it on the generic `worker` defaults (tool allowlist,
permission mode) under that label, with the inline `prompt` as its system
prompt. The presets below are the named starting points; reach for one when a
sub-task clearly calls for it, and invent a role when none fits.

The embedded presets are read-only defaults; override per-project in
`<repo>/.caucus/roles.toml` or globally in `~/.caucus/roles.toml`:

| Role               | Agent CLI | Default model | Tools                                                  | Permission mode † |
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
sub-agent the main worker spawns for parallel work. The remaining presets are
optional specialist hints — use them when a sub-task clearly calls for one, or
go off-list with a free-form role as above.

### † Permissions & sandbox posture — read before you trust a role

**Every** caucus panel — `main` and every sub-agent — is spawned with the
backend's *skip-all-approval-prompts* flag: `claude
--dangerously-skip-permissions`, `codex
--dangerously-bypass-approvals-and-sandbox`. This is deliberate and structural,
not an oversight: a caucus panel is a backgrounded, non-interactive PTY, so an
interactive "allow this edit / command?" prompt has **no one to answer it** —
it would stall the agent's turn indefinitely with no in-band way for you or the
main worker to respond. caucus's whole model depends on agents that never block
on a prompt.

The consequences you must account for:

- **The tool allowlist is the only real boundary.** What an agent *can* do is
  bounded by its role's `allowed_tools` (claude `--allowedTools` /
  `--disallowedTools`, e.g. a `reviewer` with no `Edit`/`Write`/`Bash` cannot
  modify files), **not** by interactive approval gating. Treat `allowed_tools`
  as the security perimeter; pick a role whose tools match the trust you extend.
- **`permission_mode` is not an approval gate here.** For `claude` it is still
  passed (`--permission-mode`), so its *behavioural* effect remains — notably
  `plan` keeps a role planning-first rather than editing — but it adds **no**
  confirmation step on top of the skip flag. A `default`-mode role does **not**
  pause for your approval before an allowed action.
- **codex panels run with no sandbox.** The bypass flag disables codex's
  filesystem/network sandbox entirely. The role-`permission_mode`→`--sandbox`
  mapping in the code is a non-production fallback (it applies only when the
  skip flag is off, which the live spawn path never does).

In short: run caucus on a repository you trust, and rely on `allowed_tools` —
plus the per-panel worktree isolation for execute-phase roles — as the boundary.
There is no per-action human-in-the-loop confirmation.

For a worked orchestration pattern — a continuous review → fix → regression
loop driven by `main` over a durable review doc — see
[`docs/review-loop.md`](./docs/review-loop.md).

Cost tiers reflect each role's cognitive load: Opus where shaping decisions
and finding subtle issues matters, Sonnet where executing a defined plan
suffices, Haiku where the work is mechanical. The default-model column uses
claude CLI tier *aliases* (`opus` / `sonnet` / `haiku`) so caucus follows the
latest generation automatically; pin a version in `roles.toml` for
reproducibility.

System-prompt templates live under `roles/`. Each preset role inherits the
claw-code "4-constraint scaffolding" (delegated task / only tools / no
questions / concise result); a free-form role's inline `prompt` is its system
prompt verbatim, so write that scaffolding into it when you want it.

The system prompt reaches each backend differently: `claude` gets it via
`--append-system-prompt`, and `codex` via its `-c instructions=…` base-
instructions override — so a Codex-backed role (preset or free-form) also runs
with its instructions.

Every role's `allowed_tools` includes `mcp__kodex`: a sub-agent queries the
kodex knowledge graph (`recall_for_task`) to self-serve codebase context — so
the main worker hands each one a lean brief and lets it fetch the depth, and
records discoveries with `learn`. kodex is the user's global MCP server; swap
or drop it per-project in `roles.toml`.

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
