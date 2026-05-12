//! Merge a Stop-hook entry into `~/.claude/settings.json`. Idempotent:
//! the same `caucus-managed` command never lands twice. A `.bak.<ts>`
//! backup is written before the file is rewritten.
//!
//! The settings.json shape Claude Code expects is documented at
//! <https://docs.anthropic.com/en/docs/claude-code/hooks>. We treat
//! anything we don't recognise as opaque and preserve it verbatim.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde_json::{Map, Value, json};

/// Result of running [`install_stop_hook`].
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub settings_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub action: InstallAction,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum InstallAction {
    /// Inserted a new matching block.
    Inserted,
    /// A matching block was already present — file untouched.
    AlreadyPresent,
    /// Replaced a stale block (same matcher, different command).
    Replaced,
}

/// Install the caucus Stop hook into `~/.claude/settings.json`. The
/// `hook_path` is the absolute path to `bin/sentinel-stop` (the script
/// `caucus init` lays down).
pub fn install_stop_hook(hook_path: &Path) -> Result<InstallReport> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    let settings_path = PathBuf::from(home).join(".claude").join("settings.json");
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let (mut root, existed) = read_or_default(&settings_path)?;
    let backup_path = if existed {
        let bak = settings_path.with_extension(format!("json.bak.{}", Utc::now().timestamp()));
        std::fs::copy(&settings_path, &bak)
            .with_context(|| format!("backup {} → {}", settings_path.display(), bak.display()))?;
        Some(bak)
    } else {
        None
    };

    let hook_command = hook_path.display().to_string();
    let action = merge_stop_hook(&mut root, &hook_command);

    let bytes = serde_json::to_vec_pretty(&root)?;
    let tmp = settings_path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &settings_path)
        .with_context(|| format!("rename to {}", settings_path.display()))?;

    Ok(InstallReport {
        settings_path,
        backup_path,
        action,
    })
}

fn read_or_default(path: &Path) -> Result<(Value, bool)> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok((Value::Object(Map::new()), true)),
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            if !value.is_object() {
                return Err(anyhow!(
                    "{} must be a JSON object at the top level",
                    path.display()
                ));
            }
            Ok((value, true))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok((Value::Object(Map::new()), false))
        }
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Merge a `Stop` hook with `matcher = ""` and the given command into the
/// settings tree. Returns whether we inserted, replaced, or no-op'd.
pub(crate) fn merge_stop_hook(root: &mut Value, hook_command: &str) -> InstallAction {
    let hooks = root
        .as_object_mut()
        .expect("read_or_default guarantees object")
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = hooks.as_object_mut().expect("hooks must be object");

    let stops = hooks_obj
        .entry("Stop")
        .or_insert_with(|| Value::Array(Vec::new()));
    let stops_arr = match stops.as_array_mut() {
        Some(a) => a,
        None => {
            *stops = Value::Array(Vec::new());
            stops.as_array_mut().unwrap()
        }
    };

    // Look for an existing "" matcher entry.
    let mut existing_block: Option<usize> = None;
    for (idx, block) in stops_arr.iter().enumerate() {
        if block_has_empty_matcher(block) {
            existing_block = Some(idx);
            break;
        }
    }

    let desired_hook = json!({
        "type": "command",
        "command": hook_command,
    });

    match existing_block {
        Some(idx) => {
            let block = stops_arr.get_mut(idx).unwrap();
            let inner = block
                .as_object_mut()
                .unwrap()
                .entry("hooks")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .unwrap();
            // Already present?
            if inner.iter().any(|h| h == &desired_hook) {
                return InstallAction::AlreadyPresent;
            }
            // Replace any pre-existing caucus-managed command (heuristic:
            // path ends with "sentinel-stop"). If none, append.
            let mut replaced = false;
            for h in inner.iter_mut() {
                if hook_looks_like_caucus(h) {
                    *h = desired_hook.clone();
                    replaced = true;
                    break;
                }
            }
            if replaced {
                InstallAction::Replaced
            } else {
                inner.push(desired_hook);
                InstallAction::Inserted
            }
        }
        None => {
            stops_arr.push(json!({
                "matcher": "",
                "hooks": [desired_hook],
            }));
            InstallAction::Inserted
        }
    }
}

fn block_has_empty_matcher(block: &Value) -> bool {
    block
        .as_object()
        .and_then(|o| o.get("matcher"))
        .and_then(|m| m.as_str())
        .map(|s| s.is_empty())
        .unwrap_or(false)
}

fn hook_looks_like_caucus(hook: &Value) -> bool {
    hook.as_object()
        .and_then(|o| o.get("command"))
        .and_then(|c| c.as_str())
        .map(|cmd| cmd.ends_with("sentinel-stop"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_settings_get_a_fresh_block() {
        let mut root = json!({});
        let action = merge_stop_hook(&mut root, "/p/.caucus/bin/sentinel-stop");
        assert_eq!(action, InstallAction::Inserted);
        let stops = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0]["matcher"], "");
        let inner = stops[0]["hooks"].as_array().unwrap();
        assert_eq!(inner[0]["command"], "/p/.caucus/bin/sentinel-stop");
    }

    #[test]
    fn re_running_is_idempotent() {
        let mut root = json!({});
        merge_stop_hook(&mut root, "/p/.caucus/bin/sentinel-stop");
        let action = merge_stop_hook(&mut root, "/p/.caucus/bin/sentinel-stop");
        assert_eq!(action, InstallAction::AlreadyPresent);
        let stops = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0]["hooks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn stale_caucus_command_is_replaced_not_duplicated() {
        let mut root = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "",
                    "hooks": [{
                        "type": "command",
                        "command": "/old/.caucus/bin/sentinel-stop"
                    }]
                }]
            }
        });
        let action = merge_stop_hook(&mut root, "/new/.caucus/bin/sentinel-stop");
        assert_eq!(action, InstallAction::Replaced);
        let inner = root["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["command"], "/new/.caucus/bin/sentinel-stop");
    }

    #[test]
    fn unrelated_hooks_in_same_block_are_preserved() {
        let mut root = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "",
                    "hooks": [{
                        "type": "command",
                        "command": "/usr/local/bin/my-custom-hook"
                    }]
                }]
            }
        });
        let action = merge_stop_hook(&mut root, "/p/.caucus/bin/sentinel-stop");
        assert_eq!(action, InstallAction::Inserted);
        let inner = root["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 2);
        assert_eq!(inner[0]["command"], "/usr/local/bin/my-custom-hook");
        assert_eq!(inner[1]["command"], "/p/.caucus/bin/sentinel-stop");
    }

    #[test]
    fn matched_non_empty_matcher_gets_new_block() {
        // Existing block uses a non-empty matcher (matches one specific
        // tool, say); we need our own "" block.
        let mut root = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "/x/bash-stop"
                    }]
                }]
            }
        });
        let action = merge_stop_hook(&mut root, "/p/.caucus/bin/sentinel-stop");
        assert_eq!(action, InstallAction::Inserted);
        let stops = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 2);
        // The original block is untouched.
        assert_eq!(stops[0]["matcher"], "Bash");
        // Our block has empty matcher.
        assert_eq!(stops[1]["matcher"], "");
    }
}
