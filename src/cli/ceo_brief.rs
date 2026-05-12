//! Install / remove the `/caucus-ceo` and `/caucus-ceo-off` Claude Code
//! slash commands in `<repo>/.claude/commands/`. These are *live* toggles:
//! typing the slash command inside an already-running Claude Code session
//! injects the file body as a user message, so CEO mode activates mid-
//! conversation without restarting the session.
//!
//! The CLAUDE.md route (auto-load on session start) was rejected because it
//! only takes effect on the *next* `claude` invocation — useless for an
//! operator who is mid-session and just realised they wanted CEO discipline.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Slash-command body delivered when the user types `/caucus-ceo`.
pub const CEO_ON_BODY: &str = r#"From this turn on, treat me as having toggled CAUCUS CEO mode ON for our session.

You are operating in CAUCUS CEO mode. Your job is orchestration, not implementation.

Rules (active until I type `/caucus-ceo-off`):
1. Do NOT read this project's source files yourself. Reading code is the architect / reviewer role's job — they read inside their own tmux panes.
2. Write a SHORT topic sentence and a SHORT agenda based on what I told you. If you don't have enough context, ask me one specific question. Do not try to "understand the codebase" by globbing files.
3. Spawn the meeting first, then write each round's agenda to a temp file and call `caucus round start`. The role panes will fill in their response files.
4. Read `.caucus/sessions/<id>/round-NN/response-<role>.md` to inspect their work. Synthesise, decide, and call `caucus session converge` with the decision file.
5. Notion / kodex / git push are YOUR job (via MCP / shell). caucus itself never touches those.
6. For long-running polls, make `caucus session is-terminal "$SID"` the FIRST line of any wakeup prompt so the loop self-terminates on Merged / Abandoned / missing sessions. Sub-agents already get `--dangerously-skip-permissions` (claude) or `--dangerously-bypass-approvals-and-sandbox` (codex) by default — don't add them manually.
7. **Treat the `next_action` field in any caucus JSON result as your immediate marching orders.** caucus emits it after every state-changing command (session new / converge / deadlock, round start / status, execute start / pipeline, is-terminal) — that's the cheapest, most reliable instruction channel between caucus and you. Don't improvise around it; if you disagree, say so out loud first and then deviate.

When I say "let's caucus on X", your first action is `caucus session new --topic "X" --roles ... --format json`. Not file reads.

Confirm in one sentence that CEO mode is now active, then wait for my topic.
"#;

/// Slash-command body delivered when the user types `/caucus-ceo-off`.
pub const CEO_OFF_BODY: &str = r#"From this turn on, treat me as having toggled CAUCUS CEO mode OFF.

The CEO rules I sent earlier are suspended. Resume your default Claude Code behaviour: reading source files, planning, implementing, and answering questions directly.

If a later turn invokes `/caucus-ceo`, re-adopt the CEO rules from that point forward.

Confirm in one sentence that CEO mode is now off.
"#;

/// Slash-command file names (matching the `/caucus-ceo`, `/caucus-ceo-off`
/// names the user types).
pub const ON_FILE: &str = "caucus-ceo.md";
pub const OFF_FILE: &str = "caucus-ceo-off.md";

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ToggleAction {
    /// Both slash-command files were written (created or refreshed).
    Installed,
    /// Both files were already present with identical content — no write.
    AlreadyPresent,
    /// Both files were removed.
    Removed,
    /// Files were already absent.
    NotPresent,
}

#[derive(Debug, Clone)]
pub struct ToggleReport {
    pub commands_dir: PathBuf,
    pub on_path: PathBuf,
    pub off_path: PathBuf,
    pub action: ToggleAction,
    pub enabled: bool,
}

/// Write `caucus-ceo.md` and `caucus-ceo-off.md` into
/// `<repo>/.claude/commands/`. Idempotent: re-running when files already
/// match returns `AlreadyPresent` without touching disk.
pub fn enable(repo: &Path) -> Result<ToggleReport> {
    let dir = repo.join(".claude").join("commands");
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    let on_path = dir.join(ON_FILE);
    let off_path = dir.join(OFF_FILE);

    let on_unchanged = file_matches(&on_path, CEO_ON_BODY)?;
    let off_unchanged = file_matches(&off_path, CEO_OFF_BODY)?;
    if on_unchanged && off_unchanged {
        return Ok(ToggleReport {
            commands_dir: dir,
            on_path,
            off_path,
            action: ToggleAction::AlreadyPresent,
            enabled: true,
        });
    }

    write_atomic(&on_path, CEO_ON_BODY)?;
    write_atomic(&off_path, CEO_OFF_BODY)?;
    Ok(ToggleReport {
        commands_dir: dir,
        on_path,
        off_path,
        action: ToggleAction::Installed,
        enabled: true,
    })
}

/// Remove the slash-command files. No-op if both are already absent.
pub fn disable(repo: &Path) -> Result<ToggleReport> {
    let dir = repo.join(".claude").join("commands");
    let on_path = dir.join(ON_FILE);
    let off_path = dir.join(OFF_FILE);

    let mut removed_any = false;
    for path in [&on_path, &off_path] {
        match std::fs::remove_file(path) {
            Ok(()) => removed_any = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("rm {}", path.display()));
            }
        }
    }

    Ok(ToggleReport {
        commands_dir: dir,
        on_path,
        off_path,
        action: if removed_any {
            ToggleAction::Removed
        } else {
            ToggleAction::NotPresent
        },
        enabled: false,
    })
}

/// `caucus ceo status`: are both slash-command files present?
pub fn status(repo: &Path) -> Result<bool> {
    let dir = repo.join(".claude").join("commands");
    Ok(dir.join(ON_FILE).is_file() && dir.join(OFF_FILE).is_file())
}

fn file_matches(path: &Path, body: &str) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s == body),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn enable_creates_both_slash_command_files() {
        let tmp = TempDir::new().unwrap();
        let r = enable(tmp.path()).unwrap();
        assert_eq!(r.action, ToggleAction::Installed);
        assert!(r.on_path.exists());
        assert!(r.off_path.exists());
        let on = std::fs::read_to_string(&r.on_path).unwrap();
        assert!(on.contains("CAUCUS CEO mode"));
        assert!(on.contains("caucus session new"));
        let off = std::fs::read_to_string(&r.off_path).unwrap();
        assert!(off.contains("CEO mode OFF"));
    }

    #[test]
    fn enable_twice_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        enable(tmp.path()).unwrap();
        let r2 = enable(tmp.path()).unwrap();
        assert_eq!(r2.action, ToggleAction::AlreadyPresent);
    }

    #[test]
    fn enable_then_disable_removes_files() {
        let tmp = TempDir::new().unwrap();
        let r1 = enable(tmp.path()).unwrap();
        assert!(status(tmp.path()).unwrap());
        let r2 = disable(tmp.path()).unwrap();
        assert_eq!(r2.action, ToggleAction::Removed);
        assert!(!status(tmp.path()).unwrap());
        assert!(!r1.on_path.exists());
        assert!(!r1.off_path.exists());
    }

    #[test]
    fn disable_when_absent_is_no_op() {
        let tmp = TempDir::new().unwrap();
        let r = disable(tmp.path()).unwrap();
        assert_eq!(r.action, ToggleAction::NotPresent);
    }

    #[test]
    fn refresh_after_edit_rewrites_file() {
        let tmp = TempDir::new().unwrap();
        enable(tmp.path()).unwrap();
        // Operator tampers with the slash command body.
        let on_path = tmp.path().join(".claude").join("commands").join(ON_FILE);
        std::fs::write(&on_path, "stale").unwrap();
        let r = enable(tmp.path()).unwrap();
        assert_eq!(r.action, ToggleAction::Installed);
        let body = std::fs::read_to_string(&on_path).unwrap();
        assert!(body.contains("CAUCUS CEO mode"));
    }

    #[test]
    fn status_requires_both_files() {
        let tmp = TempDir::new().unwrap();
        enable(tmp.path()).unwrap();
        let off_path = tmp.path().join(".claude").join("commands").join(OFF_FILE);
        std::fs::remove_file(&off_path).unwrap();
        // Only the ON file remains.
        assert!(!status(tmp.path()).unwrap());
    }
}
