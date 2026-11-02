//! Whether a panel's rendered screen shows the agent **working** or **idle at
//! its prompt** — screen evidence for the turn boundary, independent of the
//! `Stop` hook. Blocking prompts stay with [`super::prompt_scan`].
//!
//! The rules are ported from herdr's evidence-based detection manifests
//! (`src/detect/manifests/{claude,codex}.toml`) and checked against live
//! captures of Claude Code 2.1.316 and codex-cli 0.162.0. Each rule reads a
//! narrow region near the prompt, never whole-screen incidental text, and
//! anything that is neither positive working nor positive idle evidence is
//! [`ScreenActivity::Unknown`].

use std::sync::LazyLock;

use regex::Regex;

/// What the screen positively shows about the agent's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenActivity {
    /// A live-turn indicator is visible (spinner, interrupt hint, timer).
    Working,
    /// The input prompt is visible with no live-turn indicator.
    Idle,
    /// Neither: a dialog, a viewer, a blocker, or an unrecognised layout.
    Unknown,
}

fn re(pattern: &str) -> Regex {
    // The patterns are literals covered by the tests below.
    Regex::new(pattern).unwrap_or_else(|err| panic!("invalid screen regex {pattern}: {err}"))
}

/// Claude Code: interrupt hint after the mode indicator, or a spinner verb
/// ending in `…` (optionally followed by its elapsed timer). A finished turn's
/// `✻ Worked for 3s` line has no `…`.
static CLAUDE_LIVE_TURN: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    [
        re(r"^\s*[⏸⏵].*esc to interrupt(?:\s|·|$)"),
        re(r"^\s*[*·✢✳✶✻✽]\s+\S.*…(?:\s+\(\d+[smh](?:\s|·)|\s*$)"),
    ]
});

static CLAUDE_TITLE_WORKING: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[\x{2800}-\x{28FF}\x{25D0}-\x{25D3}] "));

static CLAUDE_BACKGROUND_AGENTS: LazyLock<Regex> =
    LazyLock::new(|| re(r"^\s*[*·✢✶✻✽]\s+Waiting for [1-9]\d* background agents? to finish\s*$"));

/// Activity summaries start at column zero; wrapped continuations are
/// indented, so typed prompt text cannot impersonate this line.
static CLAUDE_BACKGROUND_MCP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?m)^[*·✢✶✻✽][ \t]+\S[^\n]*?(?:\n[ \t]+[^\n]*?){0,3}·(?:[ \t]+|\n[ \t]*)[1-9]\d*(?:[ \t]+|\n[ \t]*)MCP(?:[ \t]+|\n[ \t]*)tasks?(?:[ \t]+|\n[ \t]*)still(?:[ \t]+|\n[ \t]*)running[ \t]*$",
    )
});

const CLAUDE_BACKGROUND_MCP_NOT: [&str; 6] = [
    "do you want to proceed?",
    "esc to cancel",
    "waiting for permission",
    "do you want to allow this connection?",
    "tab to amend",
    "ctrl+e to explain",
];

/// A chooser inside the prompt box is a menu, not an idle prompt.
const CLAUDE_PROMPT_BOX_MENU: [&str; 5] = [
    "enter to select",
    "esc to cancel",
    "tab/arrow keys",
    "arrow keys to navigate",
    "↑/↓ to navigate",
];

/// Classify a Claude Code screen. `lines` are the grid rows, trailing
/// whitespace trimmed.
pub fn claude_activity(lines: &[String], title: Option<&str>) -> ScreenActivity {
    if title.is_some_and(|t| CLAUDE_TITLE_WORKING.is_match(t)) {
        return ScreenActivity::Working;
    }
    let rows = trimmed_rows(lines);
    if claude_transcript_viewer(bottom_non_empty(rows, 3)) {
        return ScreenActivity::Unknown;
    }
    let bottom = bottom_non_empty(rows, 12);
    if bottom
        .iter()
        .any(|line| CLAUDE_LIVE_TURN.iter().any(|re| re.is_match(line)))
    {
        return ScreenActivity::Working;
    }
    let top = prompt_box_top(rows);
    let above = &rows[..top.unwrap_or(rows.len())];
    if above
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| CLAUDE_BACKGROUND_AGENTS.is_match(line))
    {
        return ScreenActivity::Working;
    }
    let bottom_text = bottom.join("\n");
    if CLAUDE_BACKGROUND_MCP.is_match(&bottom_text) {
        let lower = bottom_text.to_lowercase();
        if !CLAUDE_BACKGROUND_MCP_NOT.iter().any(|n| lower.contains(n)) {
            return ScreenActivity::Working;
        }
    }
    if let Some(top) = top {
        let body_end = rows[top + 1..]
            .iter()
            .position(|line| is_horizontal_rule(line))
            .map_or(rows.len(), |rel| top + 1 + rel);
        let body = &rows[top + 1..body_end];
        let body_lower = body.join("\n").to_lowercase();
        if body.iter().any(|line| line.trim_start().starts_with('❯'))
            && !CLAUDE_PROMPT_BOX_MENU
                .iter()
                .any(|n| body_lower.contains(n))
        {
            return ScreenActivity::Idle;
        }
    }
    ScreenActivity::Unknown
}

fn claude_transcript_viewer(bottom: &[String]) -> bool {
    let lower = bottom.join("\n").to_lowercase();
    lower.contains("showing detailed transcript")
        && [
            &["ctrl+o", "to toggle"][..],
            &["ctrl+e", "show all"],
            &["ctrl+e", "collapse"],
            &["↑↓ scroll"],
            &["? for shortcuts"],
        ]
        .iter()
        .any(|needles| needles.iter().all(|n| lower.contains(n)))
}

static CODEX_TITLE_SPINNER: LazyLock<Regex> = LazyLock::new(|| re(r"(?:^| )[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏](?: |$)"));

/// The live status line (`• Working (3s • esc to interrupt)`) with nothing
/// but non-response lines (queued inputs, a sparkle-prefixed prompt) after it.
static CODEX_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?m)^(?:[•◦][ \t]+)?[^\s›•◦■✗✓─][^\r\n]* \((?:[0-9]+[hm] )*[0-9]+s(?: • [^\r\n]+? to interrupt)?\)(?: · [^\r\n]*)?(?:\r?\n(?:[^•◦›■✗✓─\r\n][^\r\n]*|•[ \t]+(?:Queued\s+follow-up\s+inputs|Messages\s+to\s+be\s+submitted\s+after\s+next\s+tool\s+call(?:\s+\(press\s+[^\r\n]+?\s+to\s+interrupt\s+and\s+send\s+immediately\))?|Messages\s+to\s+be\s+submitted\s+at\s+end\s+of\s+turn)|›[⠁⠂⠄⠈⠐⠠⡀⢀][^\r\n]*)?)*\s*\z",
    )
});

/// A failed reconnect keeps its final elapsed timer but is not working.
static CODEX_RECONNECT_FAILED: LazyLock<Regex> = LazyLock::new(|| {
    re(r"^(?:[•◦][ \t]+)?Reconnect failed — check the endpoint, then relaunch \([0-9hms ]+\)$")
});

static CODEX_TRUST_HEADER: LazyLock<Regex> =
    LazyLock::new(|| re(r"\A> You are in [^\r\n]+(?:\r?\n|$)"));
static CODEX_TRUST_QUESTION: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?s)Do\s+you\s+trust\s+the\s+contents\s+of\s+this\s+directory\?"));
static CODEX_UPDATE_LEGACY: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    [
        re(r"Skip\s+until\s+next\s+version"),
        re(r"Press enter to continue\s*\z"),
    ]
});
static CODEX_UPDATE_PICKER: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    [
        re(r"(?m)^\s*Update\s+available\s+·"),
        re(r"enter\s+continue\s+·\s+esc\s+skip\s*\z"),
    ]
});
/// A sparkle may replace the space after `›`; a later response makes that
/// prompt stale, so the weak `[y/n]` blocker below it is not current.
static CODEX_SPARKLE_PROMPT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^›[⠁⠂⠄⠈⠐⠠⡀⢀][^\n]*(?:\n(?:[^•■✗✓\n][^\n]*)?)*\z"));

/// The `thread-id` title item `codex_args` enables. codex cuts each item to
/// 32 columns, so a UUID shows as its first 29 characters and `...`.
static CODEX_TITLE_THREAD: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{1,12}"));

/// The thread id (possibly a truncated prefix) a codex title shows. The last
/// match wins: `thread-id` is the last title item, and a thread or project
/// name before it may itself contain a UUID.
pub fn codex_title_thread(title: &str) -> Option<&str> {
    CODEX_TITLE_THREAD
        .find_iter(title)
        .last()
        .map(|m| m.as_str())
}

/// Classify a codex screen. `lines` are the grid rows, trailing whitespace
/// trimmed; `title` is the OSC window title codex keeps current.
pub fn codex_activity(lines: &[String], title: Option<&str>) -> ScreenActivity {
    let title = title.unwrap_or("");
    if title.contains("Action Required") {
        return ScreenActivity::Unknown;
    }
    if CODEX_TITLE_SPINNER.is_match(title) {
        return ScreenActivity::Working;
    }
    let rows = trimmed_rows(lines);
    let last_prompt = rows.iter().rposition(|line| codex_prompt_line(line));
    let current_prompt =
        last_prompt.filter(|&i| !rows[i + 1..].iter().any(|l| codex_block_line(l)));
    let after_last_prompt = last_prompt.map_or(rows, |i| &rows[i + 1..]);
    let after_lower = after_last_prompt.join("\n").to_lowercase();
    if codex_transcript_viewer(&after_lower)
        || codex_trust_dialog(rows)
        || codex_update_dialog(rows)
        || codex_strong_blocker(&after_lower)
        || (current_prompt.is_none() && codex_weak_blocker(rows))
    {
        return ScreenActivity::Unknown;
    }
    let before_prompt = &rows[..current_prompt.unwrap_or(rows.len())];
    let before_text = before_prompt.join("\n");
    if (before_text.contains(" to interrupt)") || before_text.contains("s)"))
        && CODEX_STATUS.is_match(&before_text)
        && !before_prompt
            .iter()
            .any(|line| CODEX_RECONNECT_FAILED.is_match(line))
    {
        return ScreenActivity::Working;
    }
    if current_prompt.is_some() && !title.trim().is_empty() {
        return ScreenActivity::Idle;
    }
    ScreenActivity::Unknown
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_line(line: &str) -> bool {
    line.starts_with(['•', '■', '✗', '✓'])
}

fn codex_transcript_viewer(lower: &str) -> bool {
    [
        "↑/↓ to scroll",
        "pgup/pgdn to",
        "home/end to jump",
        "q to quit",
    ]
    .iter()
    .all(|n| lower.contains(n))
        && (lower.contains("esc to edit prev") || lower.contains("esc/← to edit prev"))
}

fn codex_trust_dialog(rows: &[String]) -> bool {
    let end = rows
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .nth(19)
        .map_or(rows.len(), |(i, _)| i + 1);
    let top = rows[..end].join("\n");
    (CODEX_TRUST_HEADER.is_match(&top) || top.contains("Folder access"))
        && (CODEX_TRUST_QUESTION.is_match(&top)
            || (top.contains("Trust this folder?")
                && top.contains("Codex can read, edit, and run files here")
                && (top.contains("Trust and continue") || top.contains("enter continue"))))
}

fn codex_update_dialog(rows: &[String]) -> bool {
    let bottom = bottom_non_empty(rows, 20).join("\n");
    (bottom.contains("Update available!")
        && bottom.contains("Update now")
        && CODEX_UPDATE_LEGACY.iter().all(|re| re.is_match(&bottom)))
        || CODEX_UPDATE_PICKER.iter().all(|re| re.is_match(&bottom))
}

fn codex_strong_blocker(lower: &str) -> bool {
    [
        "press enter to confirm or esc to cancel",
        "enter to submit answer",
        "enter to submit all",
        "allow command?",
    ]
    .iter()
    .any(|n| lower.contains(n))
        || ["all results", "filesystem only", "plugins"]
            .iter()
            .all(|n| lower.contains(n))
}

fn codex_weak_blocker(rows: &[String]) -> bool {
    let text = rows.join("\n");
    let lower = text.to_lowercase();
    let yes_or_cursor = lower.contains("yes") || lower.contains('❯');
    // The regex costs more than the whole needle sweep, so it runs last.
    (lower.contains("[y/n]")
        || lower.contains("yes (y)")
        || (lower.contains("do you want to") && yes_or_cursor)
        || (lower.contains("would you like to") && yes_or_cursor))
        && !CODEX_SPARKLE_PROMPT.is_match(&text)
}

/// `lines` without trailing blank rows — the screen ends at its last text.
fn trimmed_rows(lines: &[String]) -> &[String] {
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |i| i + 1);
    &lines[..end]
}

/// From the `count`-th non-empty row counted from the bottom to the end.
fn bottom_non_empty(rows: &[String], count: usize) -> &[String] {
    let start = rows
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map_or(rows.len(), |(i, _)| i);
    &rows[start..]
}

/// Index of the prompt box's top border: the second horizontal rule from the
/// bottom.
fn prompt_box_top(rows: &[String]) -> Option<usize> {
    rows.iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| is_horizontal_rule(line))
        .nth(1)
        .map(|(i, _)| i)
}

/// A row of `─`, alone or (when at least three long) followed by a label.
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let rule_chars = trimmed.chars().take_while(|&ch| ch == '─').count();
    if rule_chars == 0 {
        return false;
    }
    let suffix = trimmed[rule_chars * '─'.len_utf8()..].trim_start();
    suffix.is_empty() || rule_chars >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<String> {
        text.lines().map(|l| l.trim_end().to_string()).collect()
    }

    const RULE: &str = "──────────────────────────────";

    fn claude_screen(above_box: &str, footer: &str) -> Vec<String> {
        rows(&format!("{above_box}\n{RULE}\n❯ \n{RULE}\n{footer}\n\n\n"))
    }

    #[test]
    fn claude_interrupt_hint_is_working() {
        let screen = claude_screen("● reading", "  ⏵⏵ mode on · esc to interrupt");
        assert_eq!(claude_activity(&screen, None), ScreenActivity::Working);
    }

    #[test]
    fn claude_spinner_verb_is_working_with_or_without_timer() {
        for line in ["✢ Thinking…", "· Thinking… (3s · ↓ 1k tokens)"] {
            let screen = claude_screen(line, "  ? for shortcuts");
            assert_eq!(
                claude_activity(&screen, None),
                ScreenActivity::Working,
                "{line}"
            );
        }
    }

    #[test]
    fn claude_finished_turn_line_is_idle() {
        let screen = claude_screen("● hi\n✻ Worked for 1s · done 9:16 AM", "  ⏵⏵ mode on");
        assert_eq!(claude_activity(&screen, None), ScreenActivity::Idle);
    }

    #[test]
    fn claude_spinner_far_above_the_bottom_is_not_working() {
        let mut above = String::from("✢ Thinking…");
        for i in 0..12 {
            above.push_str(&format!("\nline {i}"));
        }
        let screen = claude_screen(&above, "  ? for shortcuts");
        assert_eq!(claude_activity(&screen, None), ScreenActivity::Idle);
    }

    #[test]
    fn claude_background_waits_are_working() {
        let agents = claude_screen("✻ Waiting for 2 background agents to finish", "  hint");
        assert_eq!(claude_activity(&agents, None), ScreenActivity::Working);
        let mcp = claude_screen("✻ Crunched for 4s · 1 MCP task still running", "  hint");
        assert_eq!(claude_activity(&mcp, None), ScreenActivity::Working);
    }

    #[test]
    fn claude_spinner_title_is_working() {
        let screen = claude_screen("● hi", "  hint");
        assert_eq!(
            claude_activity(&screen, Some("⠂ task")),
            ScreenActivity::Working
        );
        assert_eq!(
            claude_activity(&screen, Some("✳ task")),
            ScreenActivity::Idle
        );
    }

    #[test]
    fn claude_menu_in_the_prompt_box_is_not_idle() {
        let screen = rows(&format!(
            "question\n{RULE}\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n{RULE}\n"
        ));
        assert_eq!(claude_activity(&screen, None), ScreenActivity::Unknown);
    }

    #[test]
    fn claude_without_a_prompt_box_is_unknown() {
        assert_eq!(
            claude_activity(&rows("just output\n"), None),
            ScreenActivity::Unknown
        );
        assert_eq!(claude_activity(&[], None), ScreenActivity::Unknown);
    }

    #[test]
    fn claude_transcript_viewer_is_unknown() {
        let screen = claude_screen("● hi", "Showing detailed transcript · ctrl+o to toggle");
        assert_eq!(claude_activity(&screen, None), ScreenActivity::Unknown);
    }

    fn codex_screen(body: &str) -> Vec<String> {
        rows(&format!(
            "  >_ OpenAI Codex\n{body}\n› Ask Codex to do anything\n\n  model · ~/work\n"
        ))
    }

    #[test]
    fn codex_status_line_is_working_even_with_an_idle_title() {
        let screen = codex_screen("› hi\n• Working (3s • esc to interrupt)");
        assert_eq!(
            codex_activity(&screen, Some("task | work")),
            ScreenActivity::Working
        );
        let reduced = codex_screen("› hi\nThinking (1m 3s)");
        assert_eq!(
            codex_activity(&reduced, Some("task")),
            ScreenActivity::Working
        );
    }

    #[test]
    fn codex_spinner_title_is_working() {
        let screen = codex_screen("› hi\n• hi\n  Worked for 3s • 9:16 AM");
        assert_eq!(
            codex_activity(&screen, Some("⠋ | work")),
            ScreenActivity::Working
        );
    }

    #[test]
    fn codex_finished_turn_with_idle_title_is_idle() {
        let screen = codex_screen("› hi\n• hi\n  Worked for 3s • 9:16 AM");
        assert_eq!(
            codex_activity(&screen, Some("hi | work")),
            ScreenActivity::Idle
        );
    }

    #[test]
    fn codex_status_followed_by_a_response_is_not_working() {
        let screen = codex_screen("• Working (3s • esc to interrupt)\n• hi");
        assert_eq!(
            codex_activity(&screen, Some("hi | work")),
            ScreenActivity::Idle
        );
    }

    #[test]
    fn codex_without_title_or_prompt_is_unknown() {
        let screen = codex_screen("› hi\n• hi");
        assert_eq!(codex_activity(&screen, None), ScreenActivity::Unknown);
        assert_eq!(
            codex_activity(&rows("• hi\n"), Some("t")),
            ScreenActivity::Unknown
        );
    }

    #[test]
    fn codex_blockers_are_unknown() {
        let screen = codex_screen("› hi");
        assert_eq!(
            codex_activity(&screen, Some("Action Required | work")),
            ScreenActivity::Unknown
        );
        let mut approval = codex_screen("› hi");
        approval.push("Allow command? Press enter to confirm or esc to cancel".into());
        assert_eq!(
            codex_activity(&approval, Some("t")),
            ScreenActivity::Unknown
        );
        let weak = rows("• ran\nProceed? [y/n]\n");
        assert_eq!(codex_activity(&weak, Some("t")), ScreenActivity::Unknown);
    }

    #[test]
    fn codex_reconnect_failed_timer_is_not_working() {
        let screen = codex_screen("Reconnect failed — check the endpoint, then relaunch (12s)");
        assert_eq!(codex_activity(&screen, Some("t")), ScreenActivity::Idle);
    }

    #[test]
    fn every_pattern_compiles() {
        LazyLock::force(&CLAUDE_LIVE_TURN);
        LazyLock::force(&CLAUDE_TITLE_WORKING);
        LazyLock::force(&CLAUDE_BACKGROUND_AGENTS);
        LazyLock::force(&CLAUDE_BACKGROUND_MCP);
        LazyLock::force(&CODEX_TITLE_SPINNER);
        LazyLock::force(&CODEX_STATUS);
        LazyLock::force(&CODEX_RECONNECT_FAILED);
        LazyLock::force(&CODEX_TRUST_HEADER);
        LazyLock::force(&CODEX_TRUST_QUESTION);
        LazyLock::force(&CODEX_UPDATE_LEGACY);
        LazyLock::force(&CODEX_UPDATE_PICKER);
        LazyLock::force(&CODEX_SPARKLE_PROMPT);
        LazyLock::force(&CODEX_TITLE_THREAD);
    }

    #[test]
    fn codex_title_thread_reads_the_last_visible_thread_id() {
        assert_eq!(
            codex_title_thread("⠙ Run sleep | repo | 01a19adc-cb89-7c71-bc0f-3af55..."),
            Some("01a19adc-cb89-7c71-bc0f-3af55")
        );
        assert_eq!(
            codex_title_thread(
                "[ ! ] Action Required | 0aaaaaaa-1111-2222-3333-444444444444 | 01a19adc-cb89-7c71-bc0f-3af55..."
            ),
            Some("01a19adc-cb89-7c71-bc0f-3af55")
        );
        assert_eq!(codex_title_thread("⠙ Run sleep | repo"), None);
    }

    #[test]
    fn horizontal_rule_shapes() {
        assert!(is_horizontal_rule("────"));
        assert!(is_horizontal_rule("─── label"));
        assert!(!is_horizontal_rule("── label"));
        assert!(!is_horizontal_rule("text ───"));
        assert!(!is_horizontal_rule(""));
    }
}
