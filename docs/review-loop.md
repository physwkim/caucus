# Review → fix → regression loop (orchestration playbook)

A pattern the **main worker** runs to drive a continuous loop over a given
part of the codebase: one reviewer finds issues and records them in a doc, a
fixer addresses them, a second reviewer regression-checks the fix, repeat
until clean. caucus supplies the mechanism (`spawn_role`, `send_keys`,
`register_round`, `read_panel`); this playbook is the *policy* main follows —
it is not a built-in caucus feature.

The loop's state lives in a **durable review doc**, not in any agent's
context window. That single choice is what makes the loop robust: termination
is decided by parsing the doc, agents can be re-spawned fresh each pass
without losing continuity, and an isolated-worktree fixer still sees the doc
because it sits at a shared path.

## The review doc (the loop's state and contract)

One doc per reviewed part, at a path every panel can reach:
`$CAUCUS_SESSION_DIR/reviews/<part>.md` (`CAUCUS_SESSION_DIR` is injected into
every panel and points outside any worktree — see `design.md` §7.1).

```markdown
# review: src/foo.rs
status: FINDINGS          # CLEAN | FINDINGS — the ONLY line main parses to decide termination
iteration: 3
reviewer: reviewer-2
findings:
- id: F1
  severity: high          # high | med | low
  site: src/foo.rs:42
  summary: unchecked unwrap on a user-supplied path
  fixed: no               # no | yes(it<n>)
  regression: -           # - | pass | fail   (filled by the regression reviewer)
- id: F2
  severity: med
  site: src/foo.rs:88
  summary: off-by-one in the window clamp
  fixed: yes(it2)
  regression: pass
```

Rules for the doc:
- `status` is machine-checkable. `CLEAN` ⟺ every finding is `fixed: yes(...)`
  with `regression: pass`. Anything else is `FINDINGS`.
- Findings have **stable ids**. A fixer never deletes a finding; it sets
  `fixed: yes(it<n>)`. The regression reviewer sets `regression: pass|fail`.
  Stable ids are what let main detect a regression (a `pass` that flips to
  `fail`) or no-progress (the same id never reaching `pass`).

## Actors (existing roles)

- **reviewer** (`roles/reviewer.md`) — finds issues, writes/updates the doc.
- **backend** or **worker** (`roles/backend.md`) — fixes the findings.
- **serious-reviewer** (`roles/serious-reviewer.md`) — the *regression*
  reviewer; re-checks the fix against the recorded findings and the tests.

## The loop (one pass)

main runs this as its turn-by-turn orchestration. Each step ends main's turn
with `register_round`; caucus wakes main when the panel settles.

1. **Review.** `send_keys(reviewer, "Review src/foo.rs. Write findings to
   $CAUCUS_SESSION_DIR/reviews/foo.md using the review-doc schema. Set status:
   CLEAN if you find nothing.")`, then `register_round([reviewer])`, end turn.
2. On wake: `read_panel(reviewer, last_message)`, then read the doc.
   If `status: CLEAN` → **done**, report to the user.
3. **Fix.** `send_keys(backend, "Fix the open findings (fixed: no) in
   $CAUCUS_SESSION_DIR/reviews/foo.md. Mark each fixed: yes(it<n>). Do not
   delete findings.")`, `register_round([backend])`, end turn.
4. **Regression.** On wake: `send_keys(serious-reviewer, "Regression-check
   the fix against $CAUCUS_SESSION_DIR/reviews/foo.md and the test suite. Set
   regression: pass|fail per finding; add any new finding with a fresh id;
   set status accordingly.")`, `register_round([serious-reviewer])`, end turn.
5. On wake: read the doc. Apply the **termination rule** below. If not done,
   go to step 3 with the still-open findings (or step 1 to re-scan).

## Termination rule (main decides — caucus has no loop control)

Stop and report when **any** holds:
- `status: CLEAN`.
- `iteration >= MAX` (default 5) — stop and hand the doc to the user.
- **No-progress guard**: a finding marked `fixed: yes(...)` whose
  `regression` is `fail`, *twice* for the same id → stop and escalate; the
  fixer is not converging on that finding.

Without an explicit rule the loop runs forever and burns tokens. The rule is
deterministic precisely because `status` and the per-finding fields are
structured, not prose main has to judge.

## Worktree / visibility

The flow is serial (fix needs the review; regression needs the fix), so
**isolation is usually unnecessary — prefer `worktree=false`** for all three
agents. They then share the launch-dir working tree, so the doc and the edits
are trivially visible to each, and the regression reviewer runs the tests in
the very tree the fixer changed.

Only isolate when this loop runs alongside other concurrent writers. Then:
spawn the fixer with `worktree=true`, keep the review doc at
`$CAUCUS_SESSION_DIR/reviews/<part>.md` (outside any worktree, so still
shared), and spawn the regression reviewer with `worktree=true` pointed at the
fixer's branch so it tests the fixed tree.

## Context hygiene — re-brief from the doc, spawn fresh

Because the doc holds the state, **spawn a fresh reviewer each pass and hand
it the current doc** rather than reusing one long-lived panel. This keeps each
panel's context bounded and stops a reviewer from anchoring on (defending) its
earlier findings. Continuity comes from the file, not the context window, and
regression stays objective (the reviewer checks code against recorded ids, not
a fuzzy memory). If you do keep a panel alive across passes, `send_keys(panel,
"/compact", enter=true)` periodically and brief it to "re-evaluate from the
current code, do not defend prior findings".

## Throughput — pipeline across parts, do not fake-parallelize one part

A single part's review→fix→regression chain is genuinely serial; do not try to
parallelize it. To use more agents at once, run the loop over **independent
parts** and stagger them: while the fixer works part X, a reviewer scans part
Y. Independent review docs map cleanly onto `register_round`'s parallel
fan-in. You can also fan in several reviewers on one part for breadth, then
fold their findings into the one doc.
