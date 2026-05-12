//! Regex-based pane-screen classification, used when the Claude `Stop` hook
//! isn't installed (yet) and caucus has to infer the agent's state from
//! the rendered terminal. Patterns are distilled from dmux's `PaneAnalyzer`
//! prompt + `paneAttentionHeuristics` regexes — see `docs/dmux-analysis.md`
//! §7. We intentionally keep this small: Claude-only, three classes, no LLM.

use once_cell::sync::Lazy;
use regex::Regex;

use crate::agent::derive_state::PaneScreenHint;

/// "esc to interrupt|cancel|stop|abort" — Claude's universal busy banner.
static ESC_INTERRUPT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)esc\s+to\s+(interrupt|cancel|stop|abort)").unwrap());

/// Permission-prompt patterns. Conservative — only the wordings Claude
/// Code actually emits.
static PERMISSION_PROMPT: Lazy<Regex> = Lazy::new(|| {
    // Three alternatives, each anchored to a substring Claude prints:
    //   "Allow this tool", "Approve the following", "[y/n]" or "(y/n)"
    Regex::new(r"(?i)(allow\s+this\s+tool|approve\s+the\s+following|[\[(]y/n[\])])").unwrap()
});

/// Bare prompt indicator (`>` or `❯` on its own line). Strong "idle"
/// hint when no busy banner is visible.
static BARE_PROMPT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*[>❯›]\s*$").unwrap());

/// Classify the last `look_back` lines (default 10) of pane capture text.
/// Returns `None` if no hint is strong enough.
pub fn classify(pane_text: &str, look_back: usize) -> Option<PaneScreenHint> {
    let tail = take_tail(pane_text, look_back);

    if PERMISSION_PROMPT.is_match(&tail) {
        return Some(PaneScreenHint::PermissionPromptVisible);
    }
    if ESC_INTERRUPT.is_match(&tail) {
        return Some(PaneScreenHint::EscToInterruptVisible);
    }
    if BARE_PROMPT.is_match(&tail) {
        return Some(PaneScreenHint::BareOpenPrompt);
    }
    None
}

fn take_tail(text: &str, look_back: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(look_back);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_to_interrupt_classified_as_busy() {
        let text = "Thinking… (esc to interrupt)";
        assert_eq!(
            classify(text, 10),
            Some(PaneScreenHint::EscToInterruptVisible)
        );
    }

    #[test]
    fn permission_prompt_dominates() {
        // Both patterns present — permission prompt is the stronger signal.
        let text = "Thinking… (esc to interrupt)\nAllow this tool? (y/n)";
        assert_eq!(
            classify(text, 10),
            Some(PaneScreenHint::PermissionPromptVisible)
        );
    }

    #[test]
    fn bare_prompt_is_idle() {
        let text = "some output\n\n>";
        assert_eq!(classify(text, 10), Some(PaneScreenHint::BareOpenPrompt));
    }

    #[test]
    fn random_text_yields_no_hint() {
        let text = "Built target/release/caucus in 12.3s";
        assert_eq!(classify(text, 10), None);
    }

    #[test]
    fn only_last_n_lines_inspected() {
        // The "esc to interrupt" sits *above* the look-back window.
        let mut lines = Vec::new();
        lines.push("(esc to interrupt)".to_string());
        for _ in 0..30 {
            lines.push("scrolled away".to_string());
        }
        let text = lines.join("\n");
        assert_eq!(classify(&text, 10), None);
    }

    #[test]
    fn yn_alternative_matches_permission_prompt() {
        let text = "Run rm -rf foo? [y/n]";
        assert_eq!(
            classify(text, 10),
            Some(PaneScreenHint::PermissionPromptVisible)
        );
    }
}
