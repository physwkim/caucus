//! Detect an interactive **selection menu** in a panel's rendered grid — the
//! numbered `AskUserQuestion`-style chooser Claude Code shows when it asks the
//! user to pick an option (`docs/design.md` §8.3).
//!
//! caucus owns each panel's PTY, so when a sub-agent stops mid-turn on such a
//! prompt no `Stop` hook fires and the coarse panel state stays `Working`. The
//! main worker therefore cannot tell "still thinking" from "waiting for me to
//! choose". [`scan_menu`] reads the visible grid text and, *only when
//! confident*, returns the parsed menu so caucus can surface an
//! `awaiting_selection` signal and let the main worker answer it
//! ([`crate::mcp::McpToolSurface::select_option`]).
//!
//! This is a heuristic over Claude Code's TUI rendering — it is anchored on the
//! stable navigation footer ("… to navigate", "… to select") and the cursor
//! glyph, and returns `None` rather than guess. If Claude changes that
//! rendering this parser is the single place to update.

/// A selection menu detected on a panel's screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// The question text shown above the options (best-effort, may be empty).
    pub question: String,
    /// The options, in display order.
    pub options: Vec<MenuOption>,
    /// Index (0-based, into [`Menu::options`]) of the highlighted row — the
    /// `❯` cursor. `0` when no cursor glyph was found.
    pub cursor: usize,
}

/// One option in a [`Menu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuOption {
    /// The displayed number (`1.` → `1`) — how the main worker refers to it.
    pub number: usize,
    /// The option's title (first line only; wrapped descriptions are dropped).
    pub label: String,
}

/// Allocation-free case-insensitive substring test for an **ASCII** `needle`.
/// The footer anchors ("to navigate"/"to select"/"to cancel") are ASCII, so
/// byte-wise `eq_ignore_ascii_case` over the haystack windows is exact — and
/// avoids the `to_lowercase` String that a per-row, per-tick scan would
/// otherwise allocate just to usually find no menu. Non-ASCII bytes in the
/// haystack (the `↑/↓ · Esc` glyphs) simply never match the ASCII needle.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Cursor glyphs Claude Code may render before the highlighted option.
const CURSOR_GLYPHS: [char; 5] = ['❯', '›', '▶', '▸', '»'];

/// Maximum leading-space indent for a *non-cursor* option line. Option titles
/// sit at a shallow indent; their prose descriptions are indented deeper, so
/// this keeps a description that happens to start with "N." from being read as
/// an option.
const MAX_OPTION_INDENT: usize = 3;

/// Scan rendered grid rows (oldest/topmost first) for a selection menu.
///
/// Returns `None` unless the navigation footer is present *and* the options
/// form a contiguous `1..=N` (N ≥ 2) run — the two anchors that make a false
/// positive unlikely.
pub fn scan_menu(rows: &[String]) -> Option<Menu> {
    // 1. The navigation footer is the high-confidence anchor: an interactive
    //    chooser shows help like "↑/↓ to navigate · Enter to select". Search
    //    bottom-up (the footer sits at the screen's foot) and match without
    //    allocating a lowercased copy of every row — this runs per round panel
    //    every tick, and the common case is "no menu", which otherwise paid a
    //    `to_lowercase` String per row to conclude nothing.
    let footer = rows.iter().rposition(|r| {
        contains_ignore_ascii_case(r, "to navigate")
            && (contains_ignore_ascii_case(r, "to select")
                || contains_ignore_ascii_case(r, "to cancel"))
    })?;

    // 2. Options are the shallow-indented "N. title" lines above the footer; a
    //    leading cursor glyph marks the highlighted one.
    let mut options: Vec<MenuOption> = Vec::new();
    let mut cursor = 0usize;
    let mut first_option_row: Option<usize> = None;
    for (i, raw) in rows[..footer].iter().enumerate() {
        if let Some((number, label, has_cursor)) = parse_option_line(raw) {
            if has_cursor {
                cursor = options.len();
            }
            first_option_row.get_or_insert(i);
            options.push(MenuOption { number, label });
        }
    }

    // 3. Confidence gates: at least two options, numbered contiguously from 1.
    if options.len() < 2 {
        return None;
    }
    if options
        .iter()
        .enumerate()
        .any(|(idx, o)| o.number != idx + 1)
    {
        return None;
    }

    // 4. Question = the non-empty lines above the first option, joined.
    let question = first_option_row
        .map(|fo| {
            rows[..fo]
                .iter()
                .map(|r| r.trim())
                .filter(|r| !r.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    Some(Menu {
        question,
        options,
        cursor,
    })
}

/// Parse one candidate option line: an optional leading cursor glyph, a shallow
/// indent, then `N. title`. Returns `(number, label, has_cursor)`, or `None`
/// when the line is not an option (e.g. a deeply-indented description).
fn parse_option_line(raw: &str) -> Option<(usize, String, bool)> {
    let trimmed = raw.trim_start();
    let (has_cursor, rest) = match trimmed.chars().next() {
        Some(c) if CURSOR_GLYPHS.contains(&c) => (true, trimmed[c.len_utf8()..].trim_start()),
        _ => {
            // A non-cursor option must be shallowly indented; prose
            // descriptions are indented deeper and are rejected here.
            let indent = raw.len() - trimmed.len();
            if indent > MAX_OPTION_INDENT {
                return None;
            }
            (false, trimmed)
        }
    };
    let dot = rest.find('.')?;
    let number: usize = rest[..dot].trim().parse().ok()?;
    let label = rest[dot + 1..].trim();
    if label.is_empty() {
        return None;
    }
    Some((number, label.to_string(), has_cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendered grid from the `AskUserQuestion` prompt (per the captured
    /// screenshot): a header chip, a two-line question, five numbered options
    /// (each with an indented description), and the navigation footer. The
    /// cursor `❯` sits on option 1.
    fn fixture() -> Vec<String> {
        [
            "□ ackAny scope",
            "",
            "Item B (honor ackAny → pipeline ackAt). epics-pva-rs's watermark model",
            "differs structurally from pvxs. How far should I take it?",
            "",
            "❯ 1. Clamp existing watermarks",
            "    Parity-faithful, contained: parse ackAny in the pipeline branch",
            "    queueSize/2; clamp [1,queueSize]), then clamp the source-provided",
            "  2. Full pvxs parity",
            "    Larger: additionally DERIVE default (low,high) from queueSize",
            "  3. Parse + store only",
            "    Capture ackAny into the op/MonitorOptions state (so it's parsed)",
            "  4. Type something.",
            "  5. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn parses_the_askuserquestion_menu() {
        let menu = scan_menu(&fixture()).expect("a menu is detected");
        assert_eq!(menu.options.len(), 5);
        assert_eq!(menu.cursor, 0, "❯ is on option 1 → index 0");
        assert_eq!(menu.options[0].number, 1);
        assert_eq!(menu.options[0].label, "Clamp existing watermarks");
        assert_eq!(menu.options[3].label, "Type something.");
        assert_eq!(menu.options[4].label, "Chat about this");
        assert!(menu.question.contains("Item B (honor ackAny"));
    }

    #[test]
    fn descriptions_are_not_mistaken_for_options() {
        // The five options are picked up; the indented prose lines between
        // them (some of which contain digits/brackets) are not.
        let menu = scan_menu(&fixture()).unwrap();
        let labels: Vec<&str> = menu.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Clamp existing watermarks",
                "Full pvxs parity",
                "Parse + store only",
                "Type something.",
                "Chat about this",
            ]
        );
    }

    #[test]
    fn cursor_on_a_later_option_is_tracked() {
        let mut rows = fixture();
        // Move the cursor: strip it from option 1, put it on option 3.
        rows[5] = "  1. Clamp existing watermarks".to_string();
        rows[10] = "❯ 3. Parse + store only".to_string();
        let menu = scan_menu(&rows).unwrap();
        assert_eq!(menu.cursor, 2, "❯ now on option 3 → index 2");
    }

    #[test]
    fn no_footer_means_no_menu() {
        // Plain agent output with numbered lines but no navigation footer must
        // not be read as a menu.
        let rows: Vec<String> = [
            "Here is my plan:",
            "1. First do this",
            "2. Then do that",
            "3. Finally wrap up",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(scan_menu(&rows).is_none());
    }

    #[test]
    fn non_contiguous_numbers_are_rejected() {
        // Footer present, but the "options" do not number 1..N contiguously —
        // not a real chooser, so reject rather than guess.
        let rows: Vec<String> = [
            "1. alpha",
            "3. gamma",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(scan_menu(&rows).is_none());
    }

    #[test]
    fn a_single_option_is_not_a_menu() {
        let rows: Vec<String> = [
            "1. only choice",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(scan_menu(&rows).is_none());
    }

    /// The allocation-free footer matcher is case-insensitive over ASCII,
    /// tolerates non-ASCII bytes in the haystack (the `↑/↓ · Esc` glyphs), and
    /// handles the empty-needle / needle-longer-than-haystack edges.
    #[test]
    fn contains_ignore_ascii_case_edges() {
        assert!(contains_ignore_ascii_case("Enter to SELECT", "to select"));
        assert!(contains_ignore_ascii_case(
            "↑/↓ to navigate · Esc",
            "to navigate"
        ));
        assert!(!contains_ignore_ascii_case("plain output", "to select"));
        assert!(
            contains_ignore_ascii_case("anything", ""),
            "empty needle matches"
        );
        assert!(
            !contains_ignore_ascii_case("ab", "abc"),
            "needle longer than haystack"
        );
    }

    /// `scan_menu` searches the footer bottom-up: when an earlier line happens
    /// to read like a footer, the real chooser footer at the screen's foot is
    /// the one anchored, and the options between it and the top still parse.
    #[test]
    fn footer_is_anchored_bottom_up() {
        let rows: Vec<String> = [
            "(an aside mentioning to navigate and to select inline)",
            "❯ 1. first",
            "  2. second",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let menu = scan_menu(&rows).expect("the bottom footer anchors the menu");
        assert_eq!(menu.options.len(), 2);
        assert_eq!(menu.cursor, 0);
    }
}
