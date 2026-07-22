//! Where caucus's Claude `Stop` hook lives, and how it is recognized inside a
//! `~/.claude/settings.json` value. The single owner of "where does the hook
//! script go", "is this string a caucus hook", and "is the caucus Stop hook
//! present" — shared by [`crate::init`] (install/migrate) and [`crate::doctor`]
//! (health check) so neither drifts back to a loose "mentions caucus" substring
//! match, nor re-invents the script's location.
//!
//! A loose `s.contains("caucus")` over the whole `hooks.Stop` subtree
//! false-positives on any string that merely mentions caucus (a comment, an
//! unrelated path, a differently-named tool). These predicates instead match
//! the *shape* of a caucus hook command, so a stale or unrelated hook is never
//! mistaken for caucus's own.

use std::path::{Path, PathBuf};

/// Basename of the machine-wide turn-signal hook script.
pub(crate) const GLOBAL_HOOK_FILENAME: &str = "caucus-turn-signal";

/// Every Claude Code hook event caucus installs, with the argument the shared
/// script is invoked with under that event (`hook_event_command`). One script
/// serves all of them: `Stop` passes no argument — the script defaults to
/// posting a `stop` turn signal, keeping its settings command byte-identical
/// to prior installs — while the lifecycle events name themselves, and
/// `caucus signal post` lifts the payload's own `trigger` / `source` field
/// from stdin. `PostCompact` + `SessionStart` exist because a local slash
/// command (`/compact`, `/clear`) runs no agent turn: no Stop hook ever fires
/// for it, so without them a panel given one wedges in `working` forever.
///
/// This list is the whole truth about which caucus hooks belong in
/// `settings.json`: `init` prunes caucus-owned hooks under *every* event the
/// file carries, so an event dropped from here (e.g. the `PreCompact` entry an
/// unreleased build installed) is uninstalled on the next
/// `caucus init --install-hook` rather than left behind invoking the script
/// with an argument this binary no longer accepts.
pub(crate) const HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("Stop", None),
    ("PostCompact", Some("post-compact")),
    ("SessionStart", Some("session-start")),
];

/// The `settings.json` hook command for one installed event: the script path,
/// plus the event's argument when it has one.
pub(crate) fn hook_event_command(script: &Path, arg: Option<&str>) -> String {
    match arg {
        Some(arg) => format!("{} {arg}", script.display()),
        None => script.display().to_string(),
    }
}

/// Absolute path of the turn-signal hook script, under a `~/.claude` directory.
///
/// The Stop hook is installed **globally** (`~/.claude/settings.json`), so its
/// command must resolve in *every* Claude Code session on this machine — in any
/// project, at any time. The script therefore lives at one project-independent
/// path, and its body is project-independent too: it reads `CAUCUS_SOCK` from
/// the panel's environment and no-ops when unset.
///
/// Earlier versions wrote it to `<repo>/.caucus/bin/turn-signal` and pointed the
/// global hook at that absolute path. That made a global hook's validity depend
/// on one project's `.caucus/` still existing: deleting that project — or
/// merely running `caucus init` in a second one — left every Claude Code
/// session on the machine running a Stop hook that exited 127, so no panel
/// anywhere ever signalled and no round ever settled.
pub(crate) fn hook_script_path(claude_dir: &Path) -> PathBuf {
    claude_dir.join("hooks").join(GLOBAL_HOOK_FILENAME)
}

/// Whether a Stop-hook `command` belongs to caucus: it runs caucus's hook script
/// (the current global one, or a legacy per-project `.../.caucus/bin/...`) or a
/// caucus signal subcommand. This is caucus-specific — it does *not* match an
/// unrelated command that merely has "caucus" somewhere in its path — so
/// migration prunes only caucus's own hooks and leaves third-party Stop hooks
/// alone.
pub(crate) fn is_caucus_hook_command(cmd: &str) -> bool {
    cmd.contains(GLOBAL_HOOK_FILENAME)
        || cmd.contains("/.caucus/bin/")
        || cmd.contains("caucus signal post")
        || cmd.contains("caucus sentinel")
}

/// Whether the *current* caucus hook command for `event` (exact string) is
/// already wired into `settings.hooks.<event>`. Exact-match, not a loose
/// "mentions caucus" — so a stale caucus hook never masquerades as the current
/// one (the bug that blocked migration off `sentinel-stop`).
pub(crate) fn current_hook_present(
    settings: &serde_json::Value,
    event: &str,
    hook_command: &str,
) -> bool {
    event_strings(settings, event)
        .into_iter()
        .any(|s| s == hook_command)
}

/// Every caucus hook command string under `settings.hooks.<event>`, in
/// document order — any command that [`is_caucus_hook_command`], and not an
/// unrelated command that merely mentions "caucus". Empty means no caucus
/// hook is installed for that event. `caucus doctor` verifies each one
/// actually runs on *this* machine: a synced `~/.claude/settings.json` can
/// carry another machine's absolute `turn-signal` path, which passes a
/// presence check while every signal silently dies.
pub(crate) fn caucus_hook_commands(settings: &serde_json::Value, event: &str) -> Vec<String> {
    event_strings(settings, event)
        .into_iter()
        .filter(|s| is_caucus_hook_command(s))
        .map(str::to_string)
        .collect()
}

/// Every string under `settings.hooks.<event>`, gathered recursively so
/// command strings are found regardless of the exact nesting Claude uses
/// (matcher group → `hooks` → `command`).
fn event_strings<'a>(settings: &'a serde_json::Value, event: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    if let Some(node) = settings.get("hooks").and_then(|h| h.get(event)) {
        collect_strings(node, &mut out);
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

    /// The current, machine-wide hook script.
    const GLOBAL_HOOK_SCRIPT: &str = "/home/me/.claude/hooks/caucus-turn-signal";
    /// A legacy per-project hook script (pre-migration).
    const TURN_SIGNAL: &str = "/abs/repo/.caucus/bin/turn-signal";
    const SENTINEL_STOP: &str = "/abs/repo/.caucus/bin/sentinel-stop";
    const THIRD_PARTY: &str = "/Users/me/codes/claude-config/hooks/no-deferral-guard.py";

    #[test]
    fn current_hook_present_is_exact_match_not_substring() {
        // The current hook is wired → present.
        let with = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": TURN_SIGNAL }] }] }
        });
        assert!(current_hook_present(&with, "Stop", TURN_SIGNAL));
        let without = serde_json::json!({ "hooks": { "Stop": [] } });
        assert!(!current_hook_present(&without, "Stop", TURN_SIGNAL));
        // A *stale* caucus hook must NOT count as the current one — this exact
        // confusion (any "caucus" mention satisfies the check) is the bug that
        // blocked migration off sentinel-stop.
        let stale = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": SENTINEL_STOP }] }] }
        });
        assert!(!current_hook_present(&stale, "Stop", TURN_SIGNAL));
        // Presence is per-event: a command wired under Stop says nothing about
        // PostCompact — each installed event is checked under its own key.
        assert!(!current_hook_present(&with, "PostCompact", TURN_SIGNAL));
    }

    #[test]
    fn caucus_hook_commands_extracts_only_caucus_commands() {
        let settings = serde_json::json!({
            "hooks": { "Stop": [
                { "hooks": [{ "command": THIRD_PARTY }, { "command": TURN_SIGNAL }] },
            ] }
        });
        assert_eq!(caucus_hook_commands(&settings, "Stop"), vec![TURN_SIGNAL]);
        assert!(caucus_hook_commands(&serde_json::json!({}), "Stop").is_empty());
    }

    /// One script serves every installed event: the per-event command is the
    /// script path plus the event's argument, `Stop`'s stays the bare path
    /// (byte-identical to prior installs), and each is recognized as a caucus
    /// hook command.
    #[test]
    fn hook_event_command_appends_the_event_argument() {
        let script = Path::new(GLOBAL_HOOK_SCRIPT);
        let by_event: Vec<(&str, String)> = HOOK_EVENTS
            .iter()
            .map(|(event, arg)| (*event, hook_event_command(script, *arg)))
            .collect();
        assert_eq!(
            by_event,
            vec![
                ("Stop", GLOBAL_HOOK_SCRIPT.to_string()),
                ("PostCompact", format!("{GLOBAL_HOOK_SCRIPT} post-compact")),
                (
                    "SessionStart",
                    format!("{GLOBAL_HOOK_SCRIPT} session-start")
                ),
            ]
        );
        for (_, cmd) in by_event {
            assert!(
                is_caucus_hook_command(&cmd),
                "{cmd} must be recognized as caucus's own"
            );
        }
    }

    #[test]
    fn hook_script_path_is_project_independent() {
        let path = hook_script_path(Path::new("/home/me/.claude"));
        assert_eq!(
            path,
            PathBuf::from("/home/me/.claude/hooks/caucus-turn-signal")
        );
        // Nothing about it names a project — that was the whole defect.
        assert!(!path.to_string_lossy().contains("/.caucus/"));
        assert!(is_caucus_hook_command(&path.display().to_string()));
    }

    #[test]
    fn is_caucus_hook_command_matches_caucus_only() {
        assert!(is_caucus_hook_command(GLOBAL_HOOK_SCRIPT));
        // A legacy per-project hook is still recognized, so it gets pruned
        // rather than left stacked alongside the new global one.
        assert!(is_caucus_hook_command(TURN_SIGNAL));
        assert!(is_caucus_hook_command(SENTINEL_STOP));
        assert!(is_caucus_hook_command("caucus signal post --kind stop"));
        assert!(is_caucus_hook_command("/usr/bin/caucus sentinel write"));
        // A third-party Stop hook must not be mistaken for caucus's own.
        assert!(!is_caucus_hook_command(THIRD_PARTY));
        assert!(!is_caucus_hook_command("/usr/bin/true"));
    }

    #[test]
    fn caucus_hook_commands_matches_exact_family_not_substring() {
        // The current hook → found.
        let with = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": TURN_SIGNAL }] }] }
        });
        assert_eq!(caucus_hook_commands(&with, "Stop"), vec![TURN_SIGNAL]);
        // A stale caucus hook still counts (it is caucus's own).
        let stale = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": SENTINEL_STOP }] }] }
        });
        assert_eq!(caucus_hook_commands(&stale, "Stop"), vec![SENTINEL_STOP]);
        // No hooks at all → not installed.
        assert!(caucus_hook_commands(&serde_json::json!({}), "Stop").is_empty());
        assert!(
            caucus_hook_commands(&serde_json::json!({ "hooks": { "Stop": [] } }), "Stop")
                .is_empty()
        );
        // The false-OK this guards: a Stop hook whose only command merely
        // *mentions* "caucus" but is not a caucus hook command must NOT be
        // reported installed (the loose-substring bug).
        let unrelated = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{
                "command": "/opt/caucus-themed-other-tool/run.sh"
            }] }] }
        });
        assert!(
            caucus_hook_commands(&unrelated, "Stop").is_empty(),
            "a non-caucus command that mentions 'caucus' must not count as installed"
        );
    }
}
