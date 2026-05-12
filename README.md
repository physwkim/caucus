# caucus

Collaboration swarm orchestrator for Claude Code agents over tmux + git worktree.

> Status: **v0.** API and CLI surface may shift until the first tagged release.
> See [`docs/design.md`](./docs/design.md) for the full specification.

## What it is

`caucus` is the infrastructure layer for running a *meeting* of Claude Code sub-agents on the same problem. A **CEO** session — your main `claude` process — drives the meeting through `caucus` shell commands: opening role-typed agent panes, distributing an agenda, collecting written responses, and (after consensus) spawning execution agents in isolated git worktrees.

`caucus` itself is *deliberately small*. It manages tmux panes, worktrees, sentinel files, and an event-sourced agent manifest. **It does not call Notion, kodex, or any external API.** External synchronisation is the CEO's job, using its own MCP toolbox.

## What it is not

- Not a replacement for [`dmux`](https://github.com/formkit/dmux) — `dmux` is for *humans* running many agents in parallel.
- Not a replacement for [`claude-code`](https://github.com/anthropics/claude-code) (or its Rust port [`claw-code`](https://github.com/ultraworkers/claw-code)) — those drive a single agent.
- Not a "swarm intelligence platform" — there is no learning loop, no auto-tuning, no proprietary memory.

`caucus` is one well-defined thing: a meeting protocol over tmux.

## Install

```bash
git clone https://github.com/physwkim/caucus.git
cd caucus
cargo install --path .       # places `caucus` on $PATH
```

Requirements:

- tmux 3.0+
- git 2.20+
- Claude Code CLI (`claude` 2.x) on `PATH`
- Codex CLI (`codex`) on `PATH` — optional, only needed if you use a Codex-backed role like `serious-reviewer`
- Rust 1.85+ (edition 2024)

## Quick start

From any git repository where you want to run a meeting:

```bash
# 1. Bootstrap .caucus/ and the sentinel hook script, and merge the
#    Claude Stop hook into ~/.claude/settings.json (with .bak backup).
caucus init --install-hook

# 3. Health check.
caucus doctor
#   ✓ tmux: tmux 3.6a
#   ✓ git: git version 2.53.0
#   ✓ claude: 2.1.x (Claude Code)
#   ✓ .caucus dir
#   ✓ sentinel hook
#   ✓ roles: 5 role(s): architect, backend, qa, reviewer, scribe

# 4. Start a meeting. Each role becomes its own tmux pane.
caucus session new \
  --topic "write_loop refactor" \
  --roles architect,backend,reviewer

# 5. Run round 1 — give every role the same agenda file.
caucus round start <session-id> --agenda-file /tmp/agenda.md

# 6. Poll until every role has written its response.md.
caucus round status <session-id> --format json

# 7. (Optional) iterate.
caucus round next <session-id> --agenda-file /tmp/agenda-r2.md

# 8. Lock in a decision. Transitions Meeting* → MeetingConverged.
caucus session converge <session-id> --decision-file /tmp/decision.md

# 9. Spawn execute agents in their own worktrees.
caucus execute start <session-id> --role backend --task-file /tmp/decision.md
caucus execute status <session-id> --format json
caucus execute finish <session-id> --role backend   # captures commit_provenance
```

## CLI surface

```
caucus init
caucus doctor
caucus session    new | list | show | converge | deadlock | kill | transcript | is-terminal | relayout
caucus round      start | status | next
caucus ceo        enable | disable | status | show
caucus execute    start | status | finish | abandon
caucus agent      show | send | kill
caucus role       list | show
caucus sentinel   write              # the Claude Stop hook calls this
caucus watch      <session-id>       # foreground stdout event stream for the CEO
```

Every command accepts `--format json | text` (text is default) and `--repo <path>` (defaults to CWD). Exit codes follow `docs/design.md` §10.1 — `0` success, `2` user error, `3` environment error, `4` state corruption.

### Pane placement: split vs window

By default caucus splits the current tmux window into one pane per role.
That gives an at-a-glance overview but cramps quickly past three or four
roles. The `--placement window` alternative gives each role its own tmux
window (tab) — full-width, visually isolated, switchable with the standard
tmux prefix-n / prefix-p:

```bash
caucus session new --topic ... --roles architect,backend,reviewer \
  --placement window
# → one new tab per role: caucus-architect, caucus-backend, caucus-reviewer.
#   the CEO's original window stays clean.
```

`--placement window` is honoured by `caucus session new`,
`caucus execute start`, and `caucus execute pipeline`. The `--layout` flag
is ignored under `--placement window` (each tab has a single pane, so
there's nothing to balance).

### Pane layout (`--placement split` only)

`caucus session new` and `caucus execute start` accept `--layout` with one of:

```
auto              # default: even-horizontal for 2 panes, tiled for 3+
tiled             # 2D grid (uses both horizontal and vertical splits)
even-horizontal   # side-by-side
even-vertical     # stacked
main-horizontal   # one large pane on top, others tiled below
main-vertical     # one large pane on left, others stacked right
```

After a terminal resize or manual rearrangement, re-balance without
respawning anything:

```bash
caucus session relayout <session-id> --layout tiled
```

### CEO mode (live toggle)

A plain `claude` session has no idea it's supposed to act as the caucus CEO —
it'll happily read the repo's source files and try to "understand the
codebase" instead of spawning role panes. To toggle the CEO discipline
*inside* an already-running Claude Code session, install the slash commands:

```bash
caucus ceo enable
# writes .claude/commands/{caucus-ceo.md, caucus-ceo-off.md}
```

Then in your Claude session, type `/caucus-ceo` to activate CEO rules
("don't read files, spawn the meeting, …") and `/caucus-ceo-off` to suspend
them. No restart needed — the slash command body becomes a user turn that
sets the rules from that point forward.

```bash
caucus ceo status     # is it installed?
caucus ceo disable    # remove the slash command files
caucus ceo show       # print the activation prompt verbatim
```

### Plan → implement → review pipeline

`caucus execute start` runs one role. The full architect → backend → reviewer
loop the meeting design implies is exposed as a single subcommand:

```bash
caucus execute pipeline <session-id> \
  --task-file /tmp/decision.md \
  --plan architect \           # optional: refines task, output feeds impl
  --implement backend \        # required: writes code in the shared worktree
  --review reviewer \          # optional: APPROVE | BLOCK verdict on impl
  --retry-on-block 1           # default 0 — no retry
```

All three steps share one worktree (`<repo>/.caucus/worktrees/<session>-pipeline-NN/`)
and run sequentially: caucus waits for each role's sentinel before starting
the next. The reviewer's response is scanned for `^Recommendation:\s*BLOCK`
(case-insensitive, matches the format `roles/reviewer.md` already
standardises); when present and `--retry-on-block` has budget left, caucus
folds the review findings into a fresh task and re-runs plan → impl.

Per-attempt artefacts:

```
<session_root>/pipeline-01/
├── attempt-01/
│   ├── plan/{task,system,response}.md
│   ├── implement/{task,system,response}.md
│   ├── review-brief.md
│   └── review/{task,system,response}.md
├── retry-01.md
└── attempt-02/...
```

The pipeline emits a final JSON status — `approved`, `no_reviewer`,
`blocked {attempts: N}`, or `step_failed {step}` — that the CEO can branch
on. Codex-backed roles work the same way (with their own command shape);
the `--continue-meeting` flag does *not* combine with pipeline (the chain
spawns a fresh process per step by design).

### Carrying meeting context into the execute phase

By default `caucus execute start` spawns a fresh `claude` process in a new
worktree with only `decision.md` as input. The meeting-phase backend's
contextual memory of the spec discussion is lost — the operator pays a
re-load cost for every implementation pass.

`--continue-meeting` resumes the same Claude session inside the new
worktree:

```bash
caucus execute start <session-id> --role backend \
  --task-file /tmp/decision.md --continue-meeting
```

caucus reads the meeting agent's `claude_session_id` (captured from the
Stop hook payload), kills the meeting pane (Claude refuses concurrent
resumes of the same session id), and spawns the execute pane via
`claude --resume <session-id>` in the worktree. The conversation history
carries forward — no re-loading the spec discussion.

Trade-offs to know about:

- **Context window growth**: long meetings + code reading after resume
  can hit the auto-compaction threshold. If the meeting was tight, this
  is a win; if it sprawled, fresh-context might be cheaper.
- **No going back**: the meeting pane is killed. The transcript is still
  on disk (`.caucus/sessions/<id>/round-NN/response-*.md`) — that's the
  durable copy.
- **Codex roles**: ignored. The Codex CLI doesn't expose `--resume` the
  same way, so `--continue-meeting` on a Codex-backed role falls back to
  a fresh process.

Requires at least one Stop hook to have fired for the meeting agent
(otherwise caucus has no session id to resume).

### Self-terminating polling loops

`caucus session is-terminal <id>` is a cheap exit gate for CEO wakeup loops:
exit `0` if the session is in `Merged`, `Abandoned`, or missing-on-disk; exit
`1` if still active. Use it as the first line of any scheduled wakeup prompt
so an abandoned/merged session stops polling itself:

```bash
if caucus session is-terminal "$SID"; then exit 0; fi
# … otherwise poll round status, decide next step …
```

With `--format json` it also prints `{session_id, state, terminal, kind}` so
the CEO can see the underlying state in the same call.

## Roles

The five embedded roles are read-only defaults; override per-project in `<repo>/.caucus/roles.toml` or globally in `~/.caucus/roles.toml`:

| Role               | Agent CLI | Tools                                            | Permission mode |
|--------------------|-----------|--------------------------------------------------|------------------|
| `architect`        | claude    | Read, Glob, Grep, WebFetch, WebSearch, TodoWrite | `plan`           |
| `backend`          | claude    | + Edit, Write, Bash                              | `acceptEdits`    |
| `reviewer`         | claude    | Read, Glob, Grep, Bash                           | `default`        |
| `qa`               | claude    | Read, Glob, Grep, Bash                           | `default`        |
| `scribe`           | claude    | Read, Glob, Grep, Edit, Write                    | `acceptEdits`    |
| `serious-reviewer` | **codex** | Read, Glob, Grep, Bash                           | `default`        |

System-prompt templates live under `roles/`. Each role inherits the claw-code "4-constraint scaffolding" (delegated task / only tools / no questions / concise result).

### Mixing agent CLIs

caucus supports two backends per role: `claude` (default) and `codex`. The
embedded `serious-reviewer` role runs on Codex as an *adversarial second
opinion* — use it when a Claude reviewer stalls, rubber-stamps, or you want a
different model to argue. Add a Codex-backed role to your `~/.caucus/roles.toml`
or `<repo>/.caucus/roles.toml`:

```toml
[roles.serious-architect]
description = "Adversarial second-opinion architect on codex."
allowed_tools = ["Read", "Glob", "Grep", "WebSearch"]
permission_mode = "default"
system_prompt_template = "roles/serious-reviewer.md"
agent_cli = "codex"
model = "gpt-5.1-codex"
```

Sub-agents always get `--dangerously-skip-permissions` (claude) or
`--dangerously-bypass-approvals-and-sandbox` (codex) by default — the role's
`allowed_tools` is the real safety boundary, and inline permission prompts
inside a tmux pane just freeze the agent. Opt out per-session with
`caucus session new --require-permissions` or `caucus execute start
--require-permissions`.

## Architecture

```
sub-agents (claude in tmux panes) ─┐
                                   ├─► .caucus/sessions/<id>/agents/<id>.json   (manifests)
                                   ├─► .caucus/sessions/<id>/round-NN/...       (per-round)
                                   └─► .caucus/sessions/<id>/agents/<id>.sentinel.json
                                              ▲
                                              │  Claude Stop hook
                                              │
CEO (your main claude session) ◄─── caucus watch ◄─── notify (inotify/FSEvents)
```

The CEO drives the state machine; `caucus` provides typed primitives. Manifest writes go through `agent::manifest::write_json`; session transitions through `session::state::transition`; worktree removals through `worktree::cleanup::CleanupQueue`. See `docs/design.md` §9.1 for the single-owner table and §12 for the invariants.

## Testing

```bash
cargo test --workspace                     # 89 unit + 2 fast integration tests
cargo test --workspace -- --ignored        # adds 4 ignored tests that touch tmux + git
```

The ignored tests exercise a real detached tmux session, a real git worktree, and the notify-backed sentinel watcher.

## Related projects

- [`dmux`](https://github.com/formkit/dmux) — TUI for humans running many agents in parallel. Settled detection, worktree isolation, and tmux service patterns borrowed in `docs/dmux-analysis.md`.
- [`claw-code`](https://github.com/ultraworkers/claw-code) — Rust port of claude-code. Subagent typing, derived state machine, lane events, and commit-provenance heuristic borrowed in `docs/claw-code-analysis.md`.

## License

MIT.
