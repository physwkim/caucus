//! Shell-quote helpers for `tmux send-keys`. The shell vs raw-key split is
//! the single most important footgun in driving tmux programmatically — see
//! `docs/dmux-analysis.md` §4.3 — so the two paths live in separately named
//! functions with no convenience alias merging them.

/// Wrap a shell command in POSIX single quotes, escaping any embedded
/// single quotes via the `'\''` idiom. The result is one token that
/// `tmux send-keys -t <pane> <this>` will deliver verbatim to the pane's
/// shell, spaces and metacharacters intact.
///
/// Use this for: shell command lines (`git commit -m 'fix'`).
/// Do **not** use this for tmux key sequences like `Enter` or `C-l` —
/// those are not shell tokens; they're tmux key names. Send those raw.
pub fn single_quote_shell(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_quotes_inside() {
        assert_eq!(single_quote_shell("echo hi"), "'echo hi'");
    }

    #[test]
    fn empty_string() {
        assert_eq!(single_quote_shell(""), "''");
    }

    #[test]
    fn embedded_single_quote() {
        // git commit -m 'fix bug'
        // → 'git commit -m '\''fix bug'\'''
        let got = single_quote_shell("git commit -m 'fix bug'");
        assert_eq!(got, r"'git commit -m '\''fix bug'\'''");
    }

    #[test]
    fn metacharacters_preserved() {
        // Pipes, semicolons, dollar signs — all neutralised by single quotes.
        let got = single_quote_shell("ls | grep $HOME; echo done");
        assert_eq!(got, "'ls | grep $HOME; echo done'");
    }

    #[test]
    fn unicode_passes_through() {
        assert_eq!(single_quote_shell("echo 안녕"), "'echo 안녕'");
    }
}
