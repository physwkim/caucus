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
exec caucus signal post \\\n\
  --sock    \"$CAUCUS_SOCK\" \\\n\
  --session \"$CAUCUS_SESSION_ID\" \\\n\
  --panel   \"$CAUCUS_PANEL_ID\" \\\n\
  --kind    stop\n";

/// Result of the `--install-hook` step.
#[derive(Debug)]
pub enum HookInstall {
    /// The Stop hook was merged into `~/.claude/settings.json`. `backup` is
    /// the `.bak` of a prior settings file, when one was overwritten.
    Merged {
        settings: PathBuf,
        backup: Option<PathBuf>,
    },
    /// A caucus Stop hook was already present — settings left untouched.
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
    std::fs::create_dir_all(&bin_dir)
        .with_context(|| format!("create {}", bin_dir.display()))?;
    std::fs::create_dir_all(caucus_dir.join("sessions"))?;

    let hook_script = bin_dir.join("turn-signal");
    write_executable(&hook_script, TURN_SIGNAL_SCRIPT)
        .with_context(|| format!("write {}", hook_script.display()))?;

    let mut outcome = InitOutcome {
        caucus_dir,
        hook_script,
        ..InitOutcome::default()
    };

    if install_hook {
        outcome.hook_install = Some(install_claude_hook()?);
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
/// Idempotent: a caucus Stop hook already present leaves the file untouched
/// ([`HookInstall::AlreadyPresent`]); otherwise the hook is merged and any
/// prior file is backed up to `.bak` ([`HookInstall::Merged`]).
fn install_claude_hook() -> Result<HookInstall> {
    let home = std::env::var_os("HOME").context("$HOME not set — cannot locate ~/.claude")?;
    let claude_dir = PathBuf::from(home).join(".claude");
    std::fs::create_dir_all(&claude_dir)
        .with_context(|| format!("create {}", claude_dir.display()))?;
    let settings_path = claude_dir.join("settings.json");

    let mut settings: serde_json::Value = match std::fs::read_to_string(&settings_path) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(&text).with_context(|| {
                format!("{} is not valid JSON", settings_path.display())
            })?
        }
        // Missing or empty: start from a fresh object.
        _ => serde_json::json!({}),
    };

    if hook_already_present(&settings) {
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

    merge_stop_hook(&mut settings);
    let serialized = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&settings_path, serialized)
        .with_context(|| format!("write {}", settings_path.display()))?;

    Ok(HookInstall::Merged {
        settings: settings_path,
        backup,
    })
}

/// Whether a caucus Stop hook is already wired in `settings`.
fn hook_already_present(settings: &serde_json::Value) -> bool {
    settings
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .map(json_mentions_caucus)
        .unwrap_or(false)
}

/// Append the caucus Stop-hook entry into `settings.hooks.Stop`, creating the
/// intermediate objects/arrays as needed and preserving any existing hooks.
fn merge_stop_hook(settings: &mut serde_json::Value) {
    let entry = serde_json::json!({
        "hooks": [
            { "type": "command", "command": ".caucus/bin/turn-signal" }
        ]
    });

    let obj = settings
        .as_object_mut()
        .expect("settings root is a JSON object");
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .expect("hooks is a JSON object after init");
    let stop = hooks_obj
        .entry("Stop")
        .or_insert_with(|| serde_json::json!([]));
    if let Some(arr) = stop.as_array_mut() {
        arr.push(entry);
    } else {
        // A non-array `Stop` value is replaced with a fresh array.
        *stop = serde_json::json!([entry]);
    }
}

/// Recursively scan a JSON value for a string mentioning `caucus`.
fn json_mentions_caucus(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => s.contains("caucus"),
        serde_json::Value::Array(items) => items.iter().any(json_mentions_caucus),
        serde_json::Value::Object(map) => map.values().any(json_mentions_caucus),
        _ => false,
    }
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

    #[test]
    fn merge_stop_hook_into_empty_settings() {
        let mut settings = serde_json::json!({});
        merge_stop_hook(&mut settings);
        assert!(hook_already_present(&settings));
    }

    #[test]
    fn merge_stop_hook_preserves_existing_hooks() {
        let mut settings = serde_json::json!({
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "/other" }] }]
            }
        });
        merge_stop_hook(&mut settings);
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "existing hook kept, caucus hook appended");
        assert!(hook_already_present(&settings));
    }

    #[test]
    fn hook_already_present_is_idempotent_signal() {
        let with = serde_json::json!({
            "hooks": { "Stop": [{ "command": ".caucus/bin/turn-signal" }] }
        });
        assert!(hook_already_present(&with));
        let without = serde_json::json!({ "hooks": { "Stop": [] } });
        assert!(!hook_already_present(&without));
    }
}
