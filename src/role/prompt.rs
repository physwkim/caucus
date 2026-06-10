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
