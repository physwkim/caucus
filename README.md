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
caucus session    new | list | show | converge | deadlock | kill
caucus round      start | status | next
caucus execute    start | status | finish | abandon
caucus agent      show | send | kill
caucus role       list | show
caucus sentinel   write              # the Claude Stop hook calls this
caucus watch      <session-id>       # foreground stdout event stream for the CEO
```

Every command accepts `--format json | text` (text is default) and `--repo <path>` (defaults to CWD). Exit codes follow `docs/design.md` §10.1 — `0` success, `2` user error, `3` environment error, `4` state corruption.

## Roles

The five embedded roles are read-only defaults; override per-project in `<repo>/.caucus/roles.toml` or globally in `~/.caucus/roles.toml`:

| Role        | Tools                                              | Permission mode |
|-------------|----------------------------------------------------|------------------|
| `architect` | Read, Glob, Grep, WebFetch, WebSearch, TodoWrite   | `plan`           |
| `backend`   | + Edit, Write, Bash                                | `acceptEdits`    |
| `reviewer`  | Read, Glob, Grep, Bash                             | `default`        |
| `qa`        | Read, Glob, Grep, Bash                             | `default`        |
| `scribe`    | Read, Glob, Grep, Edit, Write                      | `acceptEdits`    |

System-prompt templates live under `roles/`. Each role inherits the claw-code "4-constraint scaffolding" (delegated task / only tools / no questions / concise result).

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
