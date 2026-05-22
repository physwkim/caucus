//! `caucus init [--install-hook]` (`docs/design.md` §7.2, §7.3, §10).
//!
//! Creates the project's `.caucus/` directory and the `bin/turn-signal` hook
//! script, and — with `--install-hook` — merges a Claude `Stop` hook into
//! `~/.claude/settings.json` (keeping a `.bak` of the prior file).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The Stop-hook script body (`docs/design.md` §7.3). `CAUCUS_*` env vars are
/// injected by caucus when it spawns the panel; the Claude hook payload
/// arrives on stdin and is forwarded by `caucus signal post`.
const TURN_SIGNAL_SCRIPT: &str = "#!/bin/sh\n\
# caucus turn-signal hook. CAUCUS_* env is injected by caucus at panel spawn;\n\
# the Claude hook payload is read from stdin by `caucus signal post`.\n\
#\n\
# The Stop hook is installed globally (~/.claude/settings.json), so it also\n\
# fires in ordinary Claude Code sessions that are NOT caucus panels. There the\n\
# CAUCUS_* env is unset — exit quietly so the hook is a harmless no-op.\n\
[ -n \"$CAUCUS_SOCK\" ] || exit 0\n\
exec caucus signal post \\\n\
  --sock    \"$CAUCUS_SOCK\" \\\n\
  --session \"$CAUCUS_SESSION_ID\" \\\n\
  --panel   \"$CAUCUS_PANEL_ID\" \\\n\
  --kind    stop\n";

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
#[derive(Debug, Default)]
pub struct InitOutcome {
    /// The `.caucus/` directory created or confirmed.
    pub caucus_dir: PathBuf,
    /// The turn-signal hook script written.
    pub hook_script: PathBuf,
    /// The Stop-hook install result, set when `--install-hook` ran.
    pub hook_install: Option<HookInstall>,
}

/// Run `caucus init` for the project rooted at `repo`.
///
/// Always creates `<repo>/.caucus/` (plus `bin/`, `sessions/`) and writes
/// `bin/turn-signal`. When `install_hook` is set, also merges the Stop hook
/// into `~/.claude/settings.json`.
pub fn run(repo: &Path, install_hook: bool) -> Result<InitOutcome> {
    let caucus_dir = repo.join(".caucus");
    let bin_dir = caucus_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("create {}", bin_dir.display()))?;
    std::fs::create_dir_all(caucus_dir.join("sessions"))?;

    let hook_script = bin_dir.join("turn-signal");
    write_executable(&hook_script, TURN_SIGNAL_SCRIPT)
        .with_context(|| format!("write {}", hook_script.display()))?;
    // The Stop hook is installed globally and fires in every Claude session
    // regardless of cwd — the hook command must be an absolute path, never
    // relative to whatever directory a panel's agent happens to run in.
    let hook_script = hook_script.canonicalize().unwrap_or(hook_script);

    let mut outcome = InitOutcome {
        caucus_dir,
        hook_script,
        ..InitOutcome::default()
    };

    if install_hook {
        outcome.hook_install = Some(install_claude_hook(&outcome.hook_script)?);
    }

    Ok(outcome)
}

/// Write `content` to `path` and mark it executable (`0o755` on unix).
fn write_executable(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Merge the caucus `Stop` hook into `~/.claude/settings.json`.
///
/// Idempotent on the *current* hook: when the exact `turn-signal` command is
/// already wired, the file is left untouched ([`HookInstall::AlreadyPresent`]).
/// Otherwise the hook is merged ([`HookInstall::Merged`]) and any prior file is
/// backed up to `.bak`. A *stale* caucus hook (a different caucus hook command,
/// e.g. a prior `sentinel-stop`) is removed in the process so the new install
/// replaces it rather than stacking a second, dead hook — the migration the
/// loose "mentions caucus" check used to block.
fn install_claude_hook(hook_script: &Path) -> Result<HookInstall> {
    let home = std::env::var_os("HOME").context("$HOME not set — cannot locate ~/.claude")?;
    let claude_dir = PathBuf::from(home).join(".claude");
    std::fs::create_dir_all(&claude_dir)
        .with_context(|| format!("create {}", claude_dir.display()))?;
    let settings_path = claude_dir.join("settings.json");

    let mut settings: serde_json::Value = match std::fs::read_to_string(&settings_path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", settings_path.display()))?,
        // Missing or empty: start from a fresh object.
        _ => serde_json::json!({}),
    };

    let hook_command = hook_script.display().to_string();

    if current_hook_present(&settings, &hook_command) {
        return Ok(HookInstall::AlreadyPresent {
            settings: settings_path,
        });
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
    std::fs::write(&settings_path, serialized)
        .with_context(|| format!("write {}", settings_path.display()))?;

    Ok(HookInstall::Merged {
        settings: settings_path,
        backup,
        migrated,
    })
}

/// Whether the *current* caucus hook (`hook_command`, the exact `turn-signal`
/// path) is already wired into `settings.hooks.Stop`. Exact-match, not a loose
/// "mentions caucus" — so a stale caucus hook never masquerades as the current
/// one (the bug that blocked migration).
fn current_hook_present(settings: &serde_json::Value, hook_command: &str) -> bool {
    stop_strings(settings)
        .into_iter()
        .any(|s| s == hook_command)
}

/// Whether a Stop-hook `command` belongs to caucus: it runs one of caucus's
/// hook scripts (`.../.caucus/bin/...`) or a caucus signal subcommand. This is
/// caucus-specific — it does *not* match an unrelated command that merely has
/// "caucus" somewhere in its path — so migration prunes only caucus's own
/// hooks and leaves third-party Stop hooks alone.
fn is_caucus_hook_command(cmd: &str) -> bool {
    cmd.contains("/.caucus/bin/")
        || cmd.contains("caucus signal post")
        || cmd.contains("caucus sentinel")
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

/// Every string under `settings.hooks.Stop`, gathered recursively so command
/// strings are found regardless of the exact nesting Claude uses (matcher
/// group → `hooks` → `command`).
fn stop_strings(settings: &serde_json::Value) -> Vec<&str> {
    let mut out = Vec::new();
    if let Some(stop) = settings.get("hooks").and_then(|h| h.get("Stop")) {
        collect_strings(stop, &mut out);
    }
    out
}

/// Recursively push every string value in `v` onto `out`.
fn collect_strings<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match v {
        serde_json::Value::String(s) => out.push(s),
        serde_json::Value::Array(items) => items.iter().for_each(|i| collect_strings(i, out)),
        serde_json::Value::Object(map) => map.values().for_each(|i| collect_strings(i, out)),
        _ => {}
    }
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
    use tempfile::TempDir;

    #[test]
    fn init_creates_caucus_dir_and_hook_script() {
        let tmp = TempDir::new().unwrap();
        let outcome = run(tmp.path(), false).unwrap();
        assert!(outcome.caucus_dir.is_dir());
        assert!(outcome.hook_script.is_file());
        let body = std::fs::read_to_string(&outcome.hook_script).unwrap();
        assert!(body.contains("caucus signal post"));
        assert!(body.starts_with("#!/bin/sh"));
        assert!(outcome.hook_install.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn hook_script_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let outcome = run(tmp.path(), false).unwrap();
        let mode = std::fs::metadata(&outcome.hook_script)
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "turn-signal script must be executable");
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
    fn current_hook_present_is_exact_match_not_substring() {
        // The current hook is wired → present.
        let with = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": TURN_SIGNAL }] }] }
        });
        assert!(current_hook_present(&with, TURN_SIGNAL));
        let without = serde_json::json!({ "hooks": { "Stop": [] } });
        assert!(!current_hook_present(&without, TURN_SIGNAL));
        // A *stale* caucus hook must NOT count as the current one — this exact
        // confusion (any "caucus" mention satisfies the check) is the bug that
        // blocked migration off sentinel-stop.
        let stale = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": SENTINEL_STOP }] }] }
        });
        assert!(!current_hook_present(&stale, TURN_SIGNAL));
    }

    #[test]
    fn is_caucus_hook_command_matches_caucus_only() {
        assert!(is_caucus_hook_command(TURN_SIGNAL));
        assert!(is_caucus_hook_command(SENTINEL_STOP));
        assert!(is_caucus_hook_command("caucus signal post --kind stop"));
        assert!(is_caucus_hook_command("/usr/bin/caucus sentinel write"));
        // A third-party Stop hook must not be mistaken for caucus's own.
        assert!(!is_caucus_hook_command(
            "/Users/me/codes/claude-config/hooks/no-deferral-guard.py"
        ));
        assert!(!is_caucus_hook_command("/usr/bin/true"));
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
