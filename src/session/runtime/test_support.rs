use super::*;
use tempfile::TempDir;

pub(crate) fn area() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 40,
    }
}

/// Build a multiplexer rooted in a temp dir. The tokio runtime is needed
/// because `SignalServer::bind` and `CleanupQueue::spawn` call
/// `tokio::spawn`.
pub(crate) fn mux(tmp: &TempDir) -> Multiplexer {
    let session = Session::new("test", tmp.path().to_path_buf());
    let config = Config::load(tmp.path()).unwrap();
    let (mux, _signal, _control) = Multiplexer::new(session, config, area(), 'a').unwrap();
    mux
}

/// Build a two-option menu with the given question, labels, and cursor.
pub(crate) fn menu_of(question: &str, labels: [&str; 2], cursor: usize) -> crate::term::Menu {
    crate::term::Menu {
        question: question.to_string(),
        options: labels
            .iter()
            .enumerate()
            .map(|(i, l)| crate::term::MenuOption {
                number: i + 1,
                label: l.to_string(),
            })
            .collect(),
        cursor,
    }
}
