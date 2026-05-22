//! Detection of caucus's Claude `Stop` hook inside a `~/.claude/settings.json`
//! value. The single owner of "is this string a caucus hook" and "is the
//! caucus Stop hook present" — shared by [`crate::init`] (install/migrate) and
//! [`crate::doctor`] (health check) so neither drifts back to a loose
//! "mentions caucus" substring match.
//!
//! A loose `s.contains("caucus")` over the whole `hooks.Stop` subtree
//! false-positives on any string that merely mentions caucus (a comment, an
//! unrelated path, a differently-named tool). These predicates instead match
//! the *shape* of a caucus hook command, so a stale or unrelated hook is never
//! mistaken for caucus's own.

/// Whether a Stop-hook `command` belongs to caucus: it runs one of caucus's
/// hook scripts (`.../.caucus/bin/...`) or a caucus signal subcommand. This is
/// caucus-specific — it does *not* match an unrelated command that merely has
/// "caucus" somewhere in its path — so migration prunes only caucus's own
/// hooks and leaves third-party Stop hooks alone.
pub(crate) fn is_caucus_hook_command(cmd: &str) -> bool {
    cmd.contains("/.caucus/bin/")
        || cmd.contains("caucus signal post")
        || cmd.contains("caucus sentinel")
}

/// Whether the *current* caucus hook (`hook_command`, the exact `turn-signal`
/// path) is already wired into `settings.hooks.Stop`. Exact-match, not a loose
/// "mentions caucus" — so a stale caucus hook never masquerades as the current
/// one (the bug that blocked migration off `sentinel-stop`).
pub(crate) fn current_hook_present(settings: &serde_json::Value, hook_command: &str) -> bool {
    stop_strings(settings)
        .into_iter()
        .any(|s| s == hook_command)
}

/// Whether *a* caucus turn-signal Stop hook is installed in `settings` — any
/// command string under `hooks.Stop` that [`is_caucus_hook_command`]. Used by
/// `caucus doctor`: it reports the hook present for the current path or any
/// caucus hook command, but not for an unrelated command that merely mentions
/// "caucus", so the health check cannot give a false OK.
pub(crate) fn caucus_stop_hook_installed(settings: &serde_json::Value) -> bool {
    stop_strings(settings)
        .into_iter()
        .any(is_caucus_hook_command)
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

#[cfg(test)]
mod tests {
    use super::*;

    const TURN_SIGNAL: &str = "/abs/repo/.caucus/bin/turn-signal";
    const SENTINEL_STOP: &str = "/abs/repo/.caucus/bin/sentinel-stop";
    const THIRD_PARTY: &str = "/Users/me/codes/claude-config/hooks/no-deferral-guard.py";

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
        assert!(!is_caucus_hook_command(THIRD_PARTY));
        assert!(!is_caucus_hook_command("/usr/bin/true"));
    }

    #[test]
    fn caucus_stop_hook_installed_is_exact_family_not_substring() {
        // The current hook → installed.
        let with = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": TURN_SIGNAL }] }] }
        });
        assert!(caucus_stop_hook_installed(&with));
        // A stale caucus hook still counts as installed (it is caucus's own).
        let stale = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": SENTINEL_STOP }] }] }
        });
        assert!(caucus_stop_hook_installed(&stale));
        // No hooks at all → not installed.
        assert!(!caucus_stop_hook_installed(&serde_json::json!({})));
        assert!(!caucus_stop_hook_installed(
            &serde_json::json!({ "hooks": { "Stop": [] } })
        ));
        // The false-OK this guards: a Stop hook whose only command merely
        // *mentions* "caucus" but is not a caucus hook command must NOT be
        // reported installed (the loose-substring bug).
        let unrelated = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{
                "command": "/opt/caucus-themed-other-tool/run.sh"
            }] }] }
        });
        assert!(
            !caucus_stop_hook_installed(&unrelated),
            "a non-caucus command that mentions 'caucus' must not count as installed"
        );
    }
}
