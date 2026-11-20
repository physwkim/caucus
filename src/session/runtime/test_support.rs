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
    let (mux, _signal, _control) =
        Multiplexer::new(session, config, area(), 'a', LaunchMode::Fresh).unwrap();
    mux
}

/// Put `panel` on a sub-host link that never comes up, so none of its output
/// can reach main.
pub(crate) fn cut_feed(mux: &mut Multiplexer, panel: PanelId, tmp: &TempDir) {
    use crate::host::{HostLink, Transport};
    let (signals, _) = tokio::sync::mpsc::unbounded_channel();
    let socket = Transport::Socket(tmp.path().join("no-daemon.sock"));
    let link = HostLink::connect(
        "box",
        mux.session.id,
        socket,
        signals,
        Default::default(),
        None,
    );
    let cat = crate::pty::PtyCommand::new("/bin/cat");
    let remote = link.spawn(panel, &cat, 80, 24, false).unwrap();
    assert!(!link.connected());
    let p = mux.panels.iter_mut().find(|p| p.id == panel).unwrap();
    p.pty = crate::pty::Pty::Remote(remote);
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
