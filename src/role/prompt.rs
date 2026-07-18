//! Resolve a role's `system_prompt_template` into the system-prompt text the
//! agent CLI is launched with (`docs/design.md` §6.1).
//!
//! The roles caucus ships with are embedded in the binary (`include_str!`), so
//! an installed `caucus` needs no `roles/` directory on disk. Any template that
//! is not one of the shipped defaults is treated as user-supplied and read from
//! the filesystem, resolved relative to the session repo root.

use std::path::{Path, PathBuf};

/// The question contract appended to every **sub-agent** system prompt
/// (`crate::agent::spawn::build_command`, `docs/design.md` §6.1): ask in plain
/// text and end the turn, never through an interactive chooser.
///
/// A sub-agent's panel is read by the caucus orchestrator, not a human, so an
/// `AskUserQuestion`-style menu stalls the panel `Working` with no turn signal
/// (§8.3). The claude backend additionally disallows the tool outright
/// (`--disallowedTools AskUserQuestion`); this text is the backend-neutral
/// half that tells the model what to do instead. Appended at the single spawn
/// path so it covers preset roles, free-form inline prompts, and roles with no
/// prompt — the role `.md` files restate only a short form of it.
pub const SUBAGENT_QUESTION_CONTRACT: &str = "\
# caucus: asking questions

Your panel is read by the caucus orchestrator (the main worker), not by a \
human — nothing answers an interactive chooser, so a selection menu stalls \
your panel indefinitely. Never ask through interactive question tools \
(AskUserQuestion is disabled in this panel). When you need a decision or \
clarification, or you are blocked: write the question as plain text — \
numbered options if there are concrete choices — and end your turn. The \
main worker reads your panel output when your turn ends and answers with a \
follow-up message.";

/// The turn contract appended to every **sub-agent** system prompt
/// (`crate::agent::spawn::build_command`, `docs/design.md` §4, §8.5).
///
/// caucus's only signal that a panel is done is its backend's turn-completion
/// hook — the end of its turn. A round latches each panel's result at the
/// instant it settles (Invariant I-9), so the end of the turn *is* the moment
/// the panel's contribution is captured and frozen.
///
/// That signal is only as good as the agent's turn discipline. A sub-agent that
/// starts a long command in a background shell and ends its turn — or that sets
/// up a wait-loop polling that shell across turns — reports "done" while its
/// work is still running: caucus captures a half-finished result. When the shell
/// later completes, the CLI wakes itself into a turn caucus never prompted,
/// whose output belongs to no round. This text is the backend-neutral statement
/// of the contract the completion signal assumes, appended at the single spawn
/// path so it covers preset roles, free-form inline prompts, and roles with no
/// prompt.
pub const SUBAGENT_TURN_CONTRACT: &str = "\
# caucus: when your turn ends, your work is done

caucus reads exactly one thing from this panel: the end of your turn. The \
moment your turn ends, caucus captures your result and treats your task as \
complete — so ending your turn is a claim that the work is finished.

- Never end your turn with work still in flight. Do not start a long command \
in a background shell and end the turn, and do not set up a wait-loop that \
polls a background shell across turns.
- Run long commands in the foreground and wait for them (raise the command \
timeout if you need to). If you do background something, wait for it and \
report its outcome *in the same turn*.
- Nothing you produce after your turn ends counts. Your result is already \
captured; a turn that starts on its own later — because a background shell \
finished, say — is read by nobody.
- If the work genuinely cannot finish in one turn, do not leave it running \
silently: say so in plain text, with what you did and what remains, and end \
the turn. The main worker decides what happens next.";

/// The note contract appended to every **sub-agent** system prompt
/// (`crate::agent::spawn::build_command`, `docs/design.md` §7): how to talk to
/// caucus *mid-turn*, without ending the turn.
///
/// The turn contract makes the end of the turn the completion signal — which
/// leaves a long turn silent until it ends. `caucus signal note` is the
/// backchannel for exactly that window: a progress heartbeat, an artifact
/// reference, or a question the main worker can answer while the panel keeps
/// working. The `CAUCUS_*` env vars injected at spawn (§7.1) default the
/// CLI's socket/session/panel arguments, so the command works as-is from the
/// panel's shell.
pub const SUBAGENT_NOTE_CONTRACT: &str = "\
# caucus: mid-turn notes

The end of your turn is your completion signal — but during a long turn you \
can talk to caucus without ending it, by running:

    caucus signal note --kind progress \"one line on where the work stands\"
    caucus signal note --kind artifact \"path/to/thing-you-produced\"
    caucus signal note --kind question \"a question the main worker can answer\"

The socket, session, and panel are read from environment variables already \
set in this panel — pass only the text. A note is one line on your timeline, \
not a payload channel: bodies are capped at 2 KiB, so name an artifact by \
path instead of pasting its content.

- `progress`: post at natural checkpoints of a long task, so the \
orchestrator sees the panel advancing rather than wedged.
- `artifact`: name a file the moment it is useful to others.
- `question`: forwarded to the main worker as a notice. Use it only for a \
question you can keep working past — the answer arrives as a message typed \
into this panel. When you are blocked on the answer, use the question \
contract instead: write the question as plain text and end your turn.";

/// The worktree contract appended to a **sub-agent that owns a git worktree**
/// (`crate::agent::spawn::build_command`, `docs/design.md` §5).
///
/// The panel's process cwd is already its worktree, so relative paths are
/// correct by construction. What is not correct by construction is an
/// *absolute* path: the same repository is checked out twice — once at the
/// session repo root, once at this worktree — and nothing in a sub-agent's
/// context names which checkout it owns. An agent that infers a plausible
/// absolute path (`/repo/crates/...`, sitting right next to the absolute
/// reference paths its brief handed it) silently reads, edits, and commits in
/// the *shared* checkout, racing every sibling panel. Naming the worktree
/// removes the ambiguity rather than asking the model to resist it.
///
/// Backend-neutral, and injected at the single spawn path so it covers preset
/// roles, free-form inline prompts, and roles with no prompt.
pub fn subagent_worktree_contract(worktree: &Path) -> String {
    format!(
        "\
# caucus: your worktree

This panel owns a dedicated git worktree, and it is already your working \
directory:

    {}

That path is the ONLY checkout you may touch. The same repository is also \
checked out at the session repo root, shared with every other panel — \
reading it is pointless and writing it corrupts your siblings' \
in-progress work.

- Prefer relative paths; your cwd is already the worktree.
- Never `cd` out of the worktree, and never name another checkout of this \
repository by absolute path. If you catch yourself typing an absolute path \
into this repository, it must start with the path above.
- Absolute paths to *other* projects (reference sources you were told to \
read) are fine — the rule is about this repository.
- Commit on your worktree's branch. Never push, merge, or rebase.",
        worktree.display()
    )
}

/// Embedded text of a default role template, keyed by its
/// `system_prompt_template` value (`roles/<name>.md`). `None` for any template
/// caucus does not ship — those are read from disk by [`resolve`].
fn embedded(template: &str) -> Option<&'static str> {
    Some(match template {
        "roles/main.md" => include_str!("../../roles/main.md"),
        "roles/worker.md" => include_str!("../../roles/worker.md"),
        "roles/architect.md" => include_str!("../../roles/architect.md"),
        "roles/backend.md" => include_str!("../../roles/backend.md"),
        "roles/reviewer.md" => include_str!("../../roles/reviewer.md"),
        "roles/qa.md" => include_str!("../../roles/qa.md"),
        "roles/scribe.md" => include_str!("../../roles/scribe.md"),
        "roles/serious-reviewer.md" => include_str!("../../roles/serious-reviewer.md"),
        _ => return None,
    })
}

/// Resolve `template` to the system-prompt text to inject:
///
/// - an empty `template` → `Ok(None)` (the role configures no prompt);
/// - a shipped default → its embedded text, with no disk access;
/// - any other path → read from `base.join(template)` (or the path itself when
///   absolute), so a user `roles.toml` template resolves against the repo root.
///
/// Returns `Err` only when a non-embedded template cannot be read — a
/// misconfigured `system_prompt_template`, surfaced at spawn rather than
/// silently dropped (the role *is* its prompt).
pub fn resolve(template: &str, base: &Path) -> std::io::Result<Option<String>> {
    if template.is_empty() {
        return Ok(None);
    }
    if let Some(text) = embedded(template) {
        return Ok(Some(text.to_string()));
    }
    let path = if Path::new(template).is_absolute() {
        PathBuf::from(template)
    } else {
        base.join(template)
    };
    std::fs::read_to_string(path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::embedded_defaults;

    #[test]
    fn empty_template_resolves_to_none() {
        assert_eq!(resolve("", Path::new("/nonexistent")).unwrap(), None);
    }

    #[test]
    fn embedded_default_resolves_without_disk() {
        // A bogus base proves the embedded text needs no filesystem.
        let text = resolve("roles/reviewer.md", Path::new("/nonexistent"))
            .unwrap()
            .expect("reviewer template is embedded");
        assert!(text.contains("reviewer"), "got: {text:?}");
    }

    /// Every embedded default role resolves to non-empty text — so a renamed
    /// role file or a missing `include_str!` arm is caught here, not at spawn.
    #[test]
    fn every_default_role_template_is_embedded() {
        for spec in embedded_defaults() {
            let text = resolve(&spec.system_prompt_template, Path::new("/nonexistent"))
                .unwrap_or_else(|e| panic!("role {}: {e}", spec.name))
                .unwrap_or_else(|| panic!("role {} has no embedded prompt", spec.name));
            assert!(
                !text.trim().is_empty(),
                "role {} prompt is empty",
                spec.name
            );
        }
    }

    #[test]
    fn user_template_is_read_relative_to_base() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("custom")).unwrap();
        std::fs::write(tmp.path().join("custom/role.md"), "CUSTOM PROMPT").unwrap();
        let text = resolve("custom/role.md", tmp.path()).unwrap().unwrap();
        assert_eq!(text, "CUSTOM PROMPT");
    }

    #[test]
    fn missing_user_template_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve("custom/missing.md", tmp.path()).is_err());
    }
}
