//! `caucus init [--install-hook]` (`docs/design.md` §7.2, §7.3, §10).
//!
//! Creates the project's `.caucus/` directory, and — with `--install-hook` —
//! writes the machine-wide turn-signal script to `~/.claude/hooks/` and merges a
//! Claude `Stop` hook pointing at it into `~/.claude/settings.json` (keeping a
//! `.bak` of the prior file).
//!
//! The hook script is deliberately *not* project-local: a global `Stop` hook
//! that names one project's path dies the moment that project does. See
//! [`crate::hook::hook_script_path`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::hook::{current_hook_present, hook_script_path, is_caucus_hook_command};

/// The Stop-hook script body (`docs/design.md` §7.3, §7.8). `CAUCUS_*` env
/// vars are injected by caucus when it spawns the panel; the Claude hook
/// payload arrives on stdin and is forwarded by `caucus signal post`. The body
/// carries no project-specific state, which is what lets one copy serve every
/// project.
///
/// When the env is absent the conversation may still BE a caucus panel whose
/// process no longer inherits the panel's env (Claude Code's daemon re-hosts
/// live sessions across restarts as env-less forks). If any caucus session
/// socket exists, fall back to `--discover`: post the payload unbound to every
/// live socket and let each server decide ownership from the payload itself.
/// With no socket present the hook is a harmless no-op, as before.
const TURN_SIGNAL_SCRIPT: &str = "#!/bin/sh\n\
# caucus turn-signal hook. CAUCUS_* env is injected by caucus at panel spawn;\n\
# the Claude hook payload is read from stdin by `caucus signal post`.\n\
#\n\
# The Stop hook is installed globally (~/.claude/settings.json), so it also\n\
# fires in ordinary Claude Code sessions that are NOT caucus panels — and in\n\
# caucus panels whose env was lost to a daemon re-host. With the env present,\n\
# post directly; otherwise, if any caucus socket is live, post unbound to all\n\
# of them (each server resolves ownership from the payload); with neither,\n\
# exit quietly so the hook is a harmless no-op.\n\
if [ -n \"$CAUCUS_SOCK\" ]; then\n\
  exec caucus signal post \\\n\
    --sock    \"$CAUCUS_SOCK\" \\\n\
    --session \"$CAUCUS_SESSION_ID\" \\\n\
    --panel   \"$CAUCUS_PANEL_ID\" \\\n\
    --kind    stop\n\
fi\n\
set -- \"${TMPDIR:-/tmp}\"/caucus-*.sock\n\
[ -e \"$1\" ] || exit 0\n\
exec caucus signal post --discover --kind stop\n";

/// Result of the `--install-hook` step.
#[derive(Debug)]
pub enum HookInstall {
    /// The current caucus Stop hook was merged into `~/.claude/settings.json`.
    /// `backup` is the `.bak` of a prior settings file, when one was
    /// overwritten. `migrated` is set when a *stale* caucus hook (e.g. a prior
    /// `sentinel-stop`) was removed in the process.
    Merged {
        settings: PathBuf,
        backup: Option<PathBuf>,
        migrated: bool,
    },
    /// The current caucus Stop hook was already present — settings left
    /// untouched.
    AlreadyPresent { settings: PathBuf },
}

/// What `caucus init` did, for the human-readable report.
#[derive(Debug)]
pub struct InitOutcome {
    /// The `.caucus/` directory created or confirmed.
    pub caucus_dir: PathBuf,
    /// The machine-wide turn-signal hook script, written only when
    /// `--install-hook` ran. `None` otherwise: without the hook install there is
    /// no script to write, because it does not live in the project.
    pub hook_script: Option<PathBuf>,
    /// What happened to the project `.gitignore`.
    pub gitignore: GitignoreOutcome,
    /// The Stop-hook install result, set when `--install-hook` ran.
    pub hook_install: Option<HookInstall>,
}

/// Result of ensuring `.caucus/sessions/` is ignored by the project
/// `.gitignore`.
#[derive(Debug)]
pub enum GitignoreOutcome {
    /// `.caucus/sessions/` was appended to (or, when `created`, the file written
    /// with) `<repo>/.gitignore`.
    Updated { path: PathBuf, created: bool },
    /// The session state was already ignored — the file was left untouched.
    AlreadyIgnored { path: PathBuf },
}

/// Run `caucus init` for the project rooted at `repo`.
///
/// Always creates `<repo>/.caucus/sessions/` and ensures `<repo>/.gitignore`
/// ignores it (per-session worktrees, panel logs, and round reports — local
/// state that must never be committed). Project config under `.caucus/`
/// (`roles.toml`, `settings.toml`) is deliberately *not* ignored, so it stays
/// committable.
///
/// When `install_hook` is set, writes the turn-signal script to
/// `~/.claude/hooks/` and merges a Stop hook pointing at it into
/// `~/.claude/settings.json`. Nothing about the hook is project-scoped: one
/// script per machine serves every project, and installing from a second
/// project is a no-op rather than a hijack of the first.
pub fn run(repo: &Path, install_hook: bool) -> Result<InitOutcome> {
    let caucus_dir = repo.join(".caucus");
    let sessions_dir = caucus_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)
        .with_context(|| format!("create {}", sessions_dir.display()))?;

    let gitignore = ensure_gitignore(repo)?;

    let mut outcome = InitOutcome {
        caucus_dir,
        hook_script: None,
        gitignore,
        hook_install: None,
    };

    if install_hook {
        let home = std::env::var_os("HOME").context("$HOME not set — cannot locate ~/.claude")?;
        let (script, install) = install_claude_hook(&PathBuf::from(home).join(".claude"))?;
        outcome.hook_script = Some(script);
        outcome.hook_install = Some(install);
    }

    Ok(outcome)
}

/// The single `.gitignore` line `caucus init` ensures is present. Scoped to the
/// session-state subdirectory, *not* all of `.caucus/`, so project config
/// (`roles.toml`, `settings.toml`) directly under `.caucus/` stays committable.
const GITIGNORE_ENTRY: &str = ".caucus/sessions/";

/// Ensure `<repo>/.gitignore` ignores `.caucus/sessions/`, idempotently.
///
/// Appends the entry (under a one-line comment) when missing, creating the file
/// if absent; leaves the file untouched when the session state is already
/// covered ([`gitignore_covers_session_state`]) — including a pre-existing
/// broader `.caucus/` ignore. Existing entries and the trailing-newline style
/// of the file are preserved — a missing final newline is added before the
/// appended block so the new entry lands on its own line.
fn ensure_gitignore(repo: &Path) -> Result<GitignoreOutcome> {
    let path = repo.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };

    if existing
        .as_deref()
        .is_some_and(gitignore_covers_session_state)
    {
        return Ok(GitignoreOutcome::AlreadyIgnored { path });
    }

    let created = existing.is_none();
    let mut content = existing.unwrap_or_default();
    // Separate the appended block from prior content: close an unterminated
    // final line, then leave one blank line before our comment.
    if !content.is_empty() {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push('\n');
    }
    content.push_str("# caucus local session state (worktrees, panel logs, round reports)\n");
    content.push_str(GITIGNORE_ENTRY);
    content.push('\n');
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(GitignoreOutcome::Updated { path, created })
}

/// Whether `text` already ignores the caucus session state. Matches the narrow
/// `.caucus/sessions` entry *and* a broader `.caucus` ignore (which already
/// covers `sessions/`) — each with or without a leading `/` or trailing slash —
/// ignoring blank lines, comments, and surrounding whitespace. A negation
/// (`!...`) does not count as covering it.
fn gitignore_covers_session_state(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        matches!(
            line.trim_end_matches('/'),
            ".caucus" | "/.caucus" | ".caucus/sessions" | "/.caucus/sessions"
        )
    })
}

/// Write `content` to `path` atomically, optionally with a unix `mode`: fully
/// write (and chmod) a sibling temp file, then `rename` it over `path`.
///
/// A plain `write` truncates `path` in place, and a following `chmod` restores
/// the mode only afterwards — two windows in which the file on disk is empty,
/// partial, or not executable. Both files caucus writes here are read
/// concurrently by processes that take no lock:
///
/// - the hook script fires on every agent turn of every live caucus session, so
///   an `init --install-hook` (after a `cargo install` upgrade, say) could kill
///   the turn signal of a panel in an unrelated session mid-rewrite;
/// - `~/.claude/settings.json` is read by Claude Code itself, and by
///   `caucus doctor`.
///
/// A lock cannot help those readers — they never take one. `rename(2)` can: it
/// swaps the directory entry to a new inode in one step. A reader holding the
/// old inode reads it to completion; every open after the rename sees the new
/// file whole. No instant exists at which `path` names a broken file.
fn write_atomic(path: &Path, content: &str, mode: Option<u32>) -> Result<()> {
    let dir = path.parent().context("target path has no parent")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("target path has no file name")?;
    // Same directory, so the rename never crosses a filesystem boundary.
    //
    // The temp name must be unique per *call*, not per process: two writers in
    // one process (two threads, or two installs) would otherwise share a temp
    // path, and each would rename or delete the other's half-written file. The
    // pid separates processes; the counter separates callers within one.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.tmp.{}.{}", name, std::process::id(), seq));

    let write_tmp = || -> Result<()> {
        std::fs::write(&tmp, content)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        std::fs::rename(&tmp, path)?;
        Ok(())
    };

    write_tmp().inspect_err(|_| {
        // Never leave the temp file behind on a partial write.
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Basename of the lock file guarding `~/.claude/settings.json` against
/// concurrent caucus writers.
const SETTINGS_LOCK_FILENAME: &str = ".caucus-settings.lock";

/// Take the exclusive caucus lock on `<claude_dir>/settings.json`, blocking
/// until it is free. The returned [`File`] is the lock: dropping it (closing the
/// descriptor) releases it, including on panic or early `?`.
///
/// Installing the hook is a read-modify-write of a file caucus does not own —
/// the user's Claude settings carry unrelated hooks, permissions, and plugin
/// config. Two concurrent `caucus init --install-hook` runs would otherwise both
/// read the old file and both write their own edit, and the loser's changes —
/// possibly a third-party hook added between the two reads — would vanish. The
/// `.bak` would be overwritten by the second run too, so the original could not
/// be recovered.
///
/// The lock lives in its own file rather than on `settings.json`, because
/// [`write_atomic`] replaces that file's inode: a lock held on the old inode
/// would guard nothing once the rename lands. A dedicated lock file has a stable
/// inode for the whole transaction. It is never truncated, so it is safe to
/// share with any other caucus process holding it open.
fn lock_settings(claude_dir: &Path) -> Result<std::fs::File> {
    let path = claude_dir.join(SETTINGS_LOCK_FILENAME);
    let file = std::fs::File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}

/// Write the turn-signal script under `claude_dir` and merge a caucus `Stop`
/// hook pointing at it into `<claude_dir>/settings.json`. Returns the script's
/// absolute path alongside the install result.
///
/// The script is rewritten on every install, so an upgraded caucus refreshes a
/// stale script body in place; it carries no project state, so rewriting it can
/// never clobber another project's configuration.
///
/// Idempotent on the *current* hook: when the exact command is already wired,
/// the settings file is left untouched ([`HookInstall::AlreadyPresent`]).
/// Otherwise the hook is merged ([`HookInstall::Merged`]) and any prior file is
/// backed up to `.bak`. A *stale* caucus hook (a different caucus hook command —
/// a prior `sentinel-stop`, or a legacy per-project `.caucus/bin/turn-signal`)
/// is removed in the process so the new install replaces it rather than stacking
/// a second, dead hook.
///
/// The whole settings read-modify-write runs under [`lock_settings`], so two
/// concurrent installs serialize instead of racing: the second reads what the
/// first wrote, finds its hook already present, and returns
/// [`HookInstall::AlreadyPresent`] without touching the file.
fn install_claude_hook(claude_dir: &Path) -> Result<(PathBuf, HookInstall)> {
    let hook_script = hook_script_path(claude_dir);
    let hooks_dir = hook_script.parent().expect("hook script has a parent dir");
    std::fs::create_dir_all(hooks_dir)
        .with_context(|| format!("create {}", hooks_dir.display()))?;

    // Held for the rest of this function: the read of settings.json, the `.bak`
    // copy, and the write must be one transaction, or a concurrent installer's
    // edit — or a third-party hook added between our read and our write — is
    // silently lost. Dropped on every exit path, including `?` and panic.
    let _settings_lock = lock_settings(claude_dir)?;

    write_atomic(&hook_script, TURN_SIGNAL_SCRIPT, Some(0o755))
        .with_context(|| format!("write {}", hook_script.display()))?;
    // The Stop hook fires in every Claude session regardless of cwd — the hook
    // command must be an absolute path, never relative to whatever directory a
    // panel's agent happens to run in.
    let hook_script = hook_script.canonicalize().unwrap_or(hook_script);

    let settings_path = claude_dir.join("settings.json");

    let mut settings: serde_json::Value = match std::fs::read_to_string(&settings_path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", settings_path.display()))?,
        // Missing or empty: start from a fresh object.
        _ => serde_json::json!({}),
    };

    let hook_command = hook_script.display().to_string();

    if current_hook_present(&settings, &hook_command) {
        return Ok((
            hook_script,
            HookInstall::AlreadyPresent {
                settings: settings_path,
            },
        ));
    }

    // Back up the prior file before rewriting it.
    let backup = if settings_path.exists() {
        let bak = settings_path.with_extension("json.bak");
        std::fs::copy(&settings_path, &bak)
            .with_context(|| format!("back up {}", settings_path.display()))?;
        Some(bak)
    } else {
        None
    };

    // Drop any stale caucus hook (a caucus hook command that is not the current
    // one), then wire the current one — so a prior `sentinel-stop` is replaced,
    // not left alongside a dead duplicate.
    let migrated = prune_stale_caucus_hooks(&mut settings, &hook_command) > 0;
    merge_stop_hook(&mut settings, &hook_command)?;
    let serialized = serde_json::to_string_pretty(&settings)?;
    // Atomic, because Claude Code and `caucus doctor` read this file without
    // taking the lock — a truncate-in-place write is visible to them.
    write_atomic(&settings_path, &serialized, None)
        .with_context(|| format!("write {}", settings_path.display()))?;

    Ok((
        hook_script,
        HookInstall::Merged {
            settings: settings_path,
            backup,
            migrated,
        },
    ))
}

/// Remove every *stale* caucus Stop hook — a caucus hook command that is not
/// the current `hook_command` — from `settings.hooks.Stop`, preserving all
/// other hooks. Matcher groups left with no hooks are dropped. Returns how
/// many stale caucus hook commands were removed.
fn prune_stale_caucus_hooks(settings: &mut serde_json::Value, hook_command: &str) -> usize {
    let Some(stop) = settings
        .get_mut("hooks")
        .and_then(|h| h.get_mut("Stop"))
        .and_then(|s| s.as_array_mut())
    else {
        return 0;
    };
    let mut removed = 0;
    for group in stop.iter_mut() {
        if let Some(hooks) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
            let before = hooks.len();
            hooks.retain(|hook| match hook.get("command").and_then(|c| c.as_str()) {
                Some(cmd) => !(is_caucus_hook_command(cmd) && cmd != hook_command),
                None => true,
            });
            removed += before - hooks.len();
        }
    }
    // Drop matcher groups whose hooks array is now empty; leave groups without
    // a `hooks` array untouched.
    stop.retain(|group| {
        group
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_none_or(|a| !a.is_empty())
    });
    removed
}

/// Append the caucus Stop-hook entry into `settings.hooks.Stop`, creating the
/// intermediate objects/arrays as needed and preserving any existing hooks.
///
/// `~/.claude/settings.json` is user-editable, so its shape cannot be assumed.
/// A root or `hooks` value that is present but not a JSON object is a
/// malformed settings file: coercing it would silently discard the user's
/// entire configuration (root) or every hook (`hooks`), so we error out and
/// leave the file untouched rather than destroy it. A non-array `Stop` is the
/// one shape we replace, since it can hold at most one stale event entry.
fn merge_stop_hook(settings: &mut serde_json::Value, hook_command: &str) -> Result<()> {
    let entry = serde_json::json!({
        "hooks": [
            { "type": "command", "command": hook_command }
        ]
    });

    let obj = settings.as_object_mut().context(
        "~/.claude/settings.json root is not a JSON object; \
         fix or remove the file, then re-run `caucus init --install-hook`",
    )?;
    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks.as_object_mut().context(
        "~/.claude/settings.json `hooks` is not a JSON object; \
         fix or remove it, then re-run `caucus init --install-hook`",
    )?;
    let stop = hooks_obj
        .entry("Stop")
        .or_insert_with(|| serde_json::json!([]));
    if let Some(arr) = stop.as_array_mut() {
        arr.push(entry);
    } else {
        // A non-array `Stop` value is replaced with a fresh array.
        *stop = serde_json::json!([entry]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::GLOBAL_HOOK_FILENAME;
    use tempfile::TempDir;

    #[test]
    fn init_creates_caucus_dir_and_writes_no_project_local_hook() {
        let tmp = TempDir::new().unwrap();
        let outcome = run(tmp.path(), false).unwrap();
        assert!(outcome.caucus_dir.is_dir());
        assert!(outcome.caucus_dir.join("sessions").is_dir());
        assert!(outcome.hook_install.is_none());
        // The hook does not live in the project. Without --install-hook there is
        // no script at all, and `.caucus/bin/` is never created: a global Stop
        // hook must not be able to name a project-local path.
        assert!(
            outcome.hook_script.is_none(),
            "no script without the install"
        );
        assert!(
            !outcome.caucus_dir.join("bin").exists(),
            "no project-local hook dir"
        );
    }

    #[test]
    fn init_creates_gitignore_ignoring_session_state() {
        let tmp = TempDir::new().unwrap();
        let outcome = run(tmp.path(), false).unwrap();
        match outcome.gitignore {
            GitignoreOutcome::Updated { created, .. } => assert!(created, "file was absent"),
            other => panic!("expected a created .gitignore, got {other:?}"),
        }
        let body = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(
            gitignore_covers_session_state(&body),
            "session state is now ignored"
        );
        // Scoped to sessions/, so project config under .caucus/ stays trackable.
        assert!(body.contains(".caucus/sessions/"), "narrow entry: {body:?}");
        assert!(
            !body.lines().any(|l| l.trim() == ".caucus/"),
            "must not ignore all of .caucus/: {body:?}"
        );
    }

    #[test]
    fn ensure_gitignore_appends_without_clobbering_existing_entries() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".gitignore");
        // No trailing newline — the appended block must not glue onto `target`.
        std::fs::write(&path, "/target").unwrap();

        let outcome = ensure_gitignore(tmp.path()).unwrap();
        assert!(matches!(
            outcome,
            GitignoreOutcome::Updated { created: false, .. }
        ));

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.lines().any(|l| l.trim() == "/target"), "kept /target");
        assert!(
            gitignore_covers_session_state(&body),
            "added .caucus/sessions/"
        );
        // `/target` and the appended entry are on separate lines.
        assert!(
            !body.contains("/target.caucus"),
            "entries not glued: {body:?}"
        );
    }

    #[test]
    fn ensure_gitignore_treats_a_broad_caucus_ignore_as_covered() {
        // A repo that already ignores all of `.caucus/` (e.g. from an earlier
        // caucus, or by hand) is left untouched — the broad ignore already
        // covers the session state, so no narrower entry is appended.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "/target\n.caucus/\n").unwrap();
        let outcome = ensure_gitignore(tmp.path()).unwrap();
        assert!(matches!(outcome, GitignoreOutcome::AlreadyIgnored { .. }));
        let body = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(
            !body.contains(".caucus/sessions/"),
            "no redundant narrow entry: {body:?}"
        );
    }

    #[test]
    fn ensure_gitignore_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        // First run creates it; second must report AlreadyIgnored and not touch
        // the file (no stacked duplicate entry).
        ensure_gitignore(tmp.path()).unwrap();
        let after_first = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        let outcome = ensure_gitignore(tmp.path()).unwrap();
        assert!(matches!(outcome, GitignoreOutcome::AlreadyIgnored { .. }));
        let after_second = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(
            after_first, after_second,
            "second run left the file untouched"
        );
    }

    #[test]
    fn gitignore_covers_session_state_recognizes_spellings() {
        // Each of these counts as already-ignored: the narrow sessions entry and
        // a broader `.caucus` ignore that subsumes it, in their common spellings.
        for text in [
            ".caucus/sessions/",
            ".caucus/sessions",
            "/.caucus/sessions/",
            ".caucus/",
            ".caucus",
            "/.caucus",
            "foo\n.caucus/sessions/\nbar",
        ] {
            assert!(
                gitignore_covers_session_state(text),
                "should cover: {text:?}"
            );
        }
        // These do not — a comment, a negation, and unrelated entries.
        for text in [
            "# .caucus/sessions/",
            "!.caucus/sessions/",
            ".caucusx",
            "caucus/",
            ".caucus/roles.toml",
            "",
        ] {
            assert!(
                !gitignore_covers_session_state(text),
                "should not cover: {text:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn installed_hook_script_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let claude = TempDir::new().unwrap();
        let (script, _) = install_claude_hook(claude.path()).unwrap();
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "turn-signal script must be executable");
    }

    #[test]
    fn install_writes_the_script_outside_any_project() {
        let claude = TempDir::new().unwrap();
        let (script, install) = install_claude_hook(claude.path()).unwrap();

        assert!(script.is_file());
        let body = std::fs::read_to_string(&script).unwrap();
        assert!(body.starts_with("#!/bin/sh"));
        assert!(body.contains("caucus signal post"));
        // The script's location names no project, and the wired command is the
        // absolute path to that project-independent script.
        assert!(!script.to_string_lossy().contains("/.caucus/"));
        assert!(script.is_absolute(), "hook command must be absolute");
        assert!(matches!(install, HookInstall::Merged { .. }));

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert!(current_hook_present(
            &settings,
            &script.display().to_string()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn reinstalling_never_leaves_a_broken_script_behind() {
        use std::os::unix::fs::PermissionsExt;
        let claude = TempDir::new().unwrap();
        let (script, _) = install_claude_hook(claude.path()).unwrap();

        // Reinstall over the live script, as a `cargo install` upgrade would.
        // The rewrite is a rename over a fully-written, already-chmod'd inode,
        // so the path never names an empty or non-executable script.
        install_claude_hook(claude.path()).unwrap();

        let body = std::fs::read_to_string(&script).unwrap();
        assert_eq!(body, TURN_SIGNAL_SCRIPT, "script is complete after rewrite");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "script is executable after rewrite");

        // No temp file is left in the hooks dir.
        let leftovers: Vec<_> = std::fs::read_dir(script.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|n| n.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left: {leftovers:?}");
    }

    #[test]
    fn install_is_idempotent_across_projects() {
        // The defect this closes: `caucus init --install-hook` in project B used
        // to repoint the global hook at B's `.caucus/bin/turn-signal`, killing
        // every panel in project A. Installing twice — as two projects would —
        // must leave one hook, wired to the same machine-wide script.
        let claude = TempDir::new().unwrap();
        let (first, _) = install_claude_hook(claude.path()).unwrap();
        let (second, install) = install_claude_hook(claude.path()).unwrap();

        assert_eq!(first, second, "both projects wire the same script");
        assert!(
            matches!(install, HookInstall::AlreadyPresent { .. }),
            "the second project's install is a no-op, not a hijack"
        );
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "no stacked duplicate hook: {stop:?}");
    }

    /// Every caucus Stop-hook command in a settings file, flattened.
    fn stop_commands(settings: &serde_json::Value) -> Vec<String> {
        settings["hooks"]["Stop"]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
                    .filter_map(|h| h["command"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn read_settings(claude_dir: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        serde_json::from_str(&text).expect("settings.json is always valid JSON")
    }

    #[test]
    fn concurrent_installs_serialize_and_preserve_the_original_backup() {
        // The race the lock closes. N installers each read the settings, see no
        // caucus hook, and merge one in. Unlocked, several of them reach the
        // "back up the prior file" step *after* another has already written its
        // edit — so `.bak` captures a file that caucus itself modified, and the
        // user's original is unrecoverable. That is the observable damage: the
        // final settings.json looks fine either way, because every installer
        // computes the same result from the same input.
        //
        // Under the lock, exactly one install performs the Merge (and backs up
        // the true original); the rest read what it wrote and report
        // AlreadyPresent without touching the file or the backup.
        let claude = TempDir::new().unwrap();
        let third_party = "/opt/other/hook.sh";
        std::fs::write(
            claude.path().join("settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "permissions": { "defaultMode": "dontAsk" },
                "hooks": { "Stop": [{ "hooks": [
                    { "type": "command", "command": third_party }
                ] }] }
            }))
            .unwrap(),
        )
        .unwrap();

        // Release every thread into the read-modify-write at once. Without the
        // barrier, thread-spawn latency staggers them enough that they serialize
        // by luck and the race never fires — the test would pass with no lock at
        // all, which is the same as not having a test.
        const WRITERS: usize = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));
        let dir = claude.path().to_path_buf();
        let handles: Vec<_> = (0..WRITERS)
            .map(|_| {
                let dir = dir.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    install_claude_hook(&dir)
                })
            })
            .collect();
        let results: Vec<(PathBuf, HookInstall)> = handles
            .into_iter()
            .map(|h| h.join().expect("no panic").expect("install succeeds"))
            .collect();

        // Every installer agrees on the script path.
        assert!(results.windows(2).all(|w| w[0].0 == w[1].0));

        // Exactly one installer wrote; the others read what it wrote and found
        // the hook already present. Unlocked, several race past that check.
        let merges = results
            .iter()
            .filter(|(_, i)| matches!(i, HookInstall::Merged { .. }))
            .count();
        assert_eq!(merges, 1, "exactly one install merges, {merges} did");

        // The backup holds the user's ORIGINAL file, not one caucus already
        // edited. This is the assertion a lost update actually breaks: the
        // final settings.json looks correct either way, because every installer
        // computes the same result from the same input.
        let bak = std::fs::read_to_string(claude.path().join("settings.json.bak")).unwrap();
        let bak: serde_json::Value = serde_json::from_str(&bak).unwrap();
        let bak_commands = stop_commands(&bak);
        assert_eq!(
            bak_commands,
            vec![third_party],
            "backup is the pre-caucus original: {bak_commands:?}"
        );

        let settings = read_settings(claude.path());
        let commands = stop_commands(&settings);
        assert_eq!(
            commands
                .iter()
                .filter(|c| c.as_str() == third_party)
                .count(),
            1,
            "the third-party hook survives exactly once: {commands:?}"
        );
        let caucus_hooks: Vec<_> = commands
            .iter()
            .filter(|c| c.contains(GLOBAL_HOOK_FILENAME))
            .collect();
        assert_eq!(
            caucus_hooks.len(),
            1,
            "exactly one caucus hook, not eight: {commands:?}"
        );
        // Unrelated config is not collateral damage of a lost update.
        assert_eq!(settings["permissions"]["defaultMode"], "dontAsk");
    }

    #[test]
    fn install_leaves_no_temp_files_in_the_claude_dir() {
        // settings.json is written by rename too, so its temp file must not
        // survive either — `~/.claude` is the user's directory, not a scratch pad.
        let claude = TempDir::new().unwrap();
        install_claude_hook(claude.path()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(claude.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|n| n.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left: {leftovers:?}");
    }

    #[test]
    fn install_migrates_a_legacy_project_local_hook() {
        // The exact failure seen in the field: the global Stop hook named a
        // deleted project's script, so every Claude session's hook exited 127.
        let claude = TempDir::new().unwrap();
        let third_party = "/opt/other/hook.sh";
        std::fs::write(
            claude.path().join("settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": { "Stop": [{ "hooks": [
                    { "type": "command", "command": "/gone/project/.caucus/bin/turn-signal" },
                    { "type": "command", "command": third_party }
                ] }] }
            }))
            .unwrap(),
        )
        .unwrap();

        let (script, install) = install_claude_hook(claude.path()).unwrap();
        assert!(
            matches!(install, HookInstall::Merged { migrated: true, .. }),
            "the dead project-local hook is reported as migrated"
        );

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        let commands: Vec<String> = settings["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|h| h["command"].as_str().map(str::to_string))
            .collect();
        assert!(
            !commands.iter().any(|c| c.contains("/.caucus/bin/")),
            "the dead project-local hook is gone: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c == third_party),
            "the third-party hook survives: {commands:?}"
        );
        assert!(current_hook_present(
            &settings,
            &script.display().to_string()
        ));
    }

    const TURN_SIGNAL: &str = "/abs/repo/.caucus/bin/turn-signal";
    const SENTINEL_STOP: &str = "/abs/repo/.caucus/bin/sentinel-stop";

    #[test]
    fn merge_stop_hook_into_empty_settings() {
        let mut settings = serde_json::json!({});
        merge_stop_hook(&mut settings, TURN_SIGNAL).unwrap();
        assert!(current_hook_present(&settings, TURN_SIGNAL));
        // The hook command must be absolute, not a relative path.
        let cmd = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.starts_with('/'), "hook command must be absolute: {cmd}");
    }

    #[test]
    fn merge_stop_hook_preserves_existing_hooks() {
        let mut settings = serde_json::json!({
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "/other" }] }]
            }
        });
        merge_stop_hook(&mut settings, TURN_SIGNAL).unwrap();
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "existing hook kept, caucus hook appended");
        assert!(current_hook_present(&settings, TURN_SIGNAL));
    }

    #[test]
    fn merge_stop_hook_errors_on_non_object_root() {
        // A hand-edited settings.json whose root is an array or scalar must
        // surface a clear error, not panic — and must not be overwritten.
        for mut settings in [serde_json::json!([1, 2, 3]), serde_json::json!("oops")] {
            let before = settings.clone();
            let err = merge_stop_hook(&mut settings, TURN_SIGNAL).unwrap_err();
            assert!(
                err.to_string().contains("root is not a JSON object"),
                "unexpected error: {err}"
            );
            assert_eq!(settings, before, "malformed root left untouched");
        }
    }

    #[test]
    fn merge_stop_hook_errors_on_non_object_hooks() {
        // `"hooks": []` is the plausible hand-edit that previously panicked:
        // `or_insert_with` returns the existing array, `.as_object_mut()` is
        // None. It must now error and preserve the user's existing hooks value.
        for hooks in [serde_json::json!([]), serde_json::json!("x")] {
            let mut settings = serde_json::json!({ "hooks": hooks });
            let before = settings.clone();
            let err = merge_stop_hook(&mut settings, TURN_SIGNAL).unwrap_err();
            assert!(
                err.to_string().contains("`hooks` is not a JSON object"),
                "unexpected error: {err}"
            );
            assert_eq!(settings, before, "malformed hooks left untouched");
        }
    }

    #[test]
    fn merge_stop_hook_replaces_a_non_array_stop() {
        // A non-array `Stop` is the one shape we coerce (it can hold at most
        // one stale entry): replaced with a fresh array carrying the hook.
        let mut settings = serde_json::json!({ "hooks": { "Stop": "garbage" } });
        merge_stop_hook(&mut settings, TURN_SIGNAL).unwrap();
        assert!(settings["hooks"]["Stop"].is_array());
        assert!(current_hook_present(&settings, TURN_SIGNAL));
    }

    #[test]
    fn prune_stale_caucus_hooks_replaces_sentinel_and_keeps_others() {
        // A real-world settings.json: the stale sentinel-stop hook sits
        // alongside an unrelated third-party hook in one Stop group.
        let third_party = "/Users/me/codes/claude-config/hooks/no-deferral-guard.py";
        let mut settings = serde_json::json!({
            "hooks": {
                "Stop": [{
                    "matcher": "",
                    "hooks": [
                        { "type": "command", "command": SENTINEL_STOP },
                        { "type": "command", "command": third_party }
                    ]
                }]
            }
        });

        let removed = prune_stale_caucus_hooks(&mut settings, TURN_SIGNAL);
        assert_eq!(removed, 1, "the stale sentinel-stop hook is pruned");

        let hooks = settings["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        let commands: Vec<&str> = hooks.iter().filter_map(|h| h["command"].as_str()).collect();
        assert_eq!(
            commands,
            vec![third_party],
            "third-party hook preserved, stale caucus hook gone"
        );

        // After the prune, merging the current hook yields a clean single
        // caucus hook — no stacked duplicate.
        merge_stop_hook(&mut settings, TURN_SIGNAL).unwrap();
        assert!(current_hook_present(&settings, TURN_SIGNAL));
    }

    #[test]
    fn prune_stale_caucus_hooks_keeps_the_current_hook() {
        // The current hook is already wired — prune must leave it alone.
        let mut settings = serde_json::json!({
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": TURN_SIGNAL }] }]
            }
        });
        let removed = prune_stale_caucus_hooks(&mut settings, TURN_SIGNAL);
        assert_eq!(removed, 0);
        assert!(current_hook_present(&settings, TURN_SIGNAL));
    }

    #[test]
    fn prune_stale_caucus_hooks_drops_emptied_groups() {
        // A Stop group containing only the stale caucus hook is removed
        // entirely, not left as an empty husk.
        let mut settings = serde_json::json!({
            "hooks": {
                "Stop": [{ "matcher": "", "hooks": [{ "command": SENTINEL_STOP }] }]
            }
        });
        let removed = prune_stale_caucus_hooks(&mut settings, TURN_SIGNAL);
        assert_eq!(removed, 1);
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert!(stop.is_empty(), "the emptied group is dropped: {stop:?}");
    }
}
