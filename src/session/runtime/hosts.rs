//! Sub-host links (`hosts.toml`) and the signals their panels post.
//!
//! A remote panel's turn signal reaches main through its host's link, after
//! the output the panel printed before it. The event loop snapshots the
//! pending remote signals *before* the PTY pump and dispatches them *after*
//! it ([`Multiplexer::take_remote_signals`] /
//! [`Multiplexer::ingest_remote_signals`]), so a signal never settles a panel
//! whose grid still lacks the turn's last output.

use std::time::Duration;

use tokio::sync::mpsc;
use tracing::warn;

use super::*;
use crate::config::hosts::{HostConfig, LOCAL_HOST};
use crate::host::{HostInfo, HostLink, LinkStatus, RemoteSignal};
use crate::mcp::HostSummary;
use crate::signal::{SignalEvent, StopDirective};

/// How long shutdown waits for each host to take its panels' kills.
const HOST_CLOSE_WAIT: Duration = Duration::from_secs(3);

/// The links to every connected sub-host, and the channel their signals
/// arrive on.
pub(super) struct Hosts {
    links: Vec<Linked>,
    signals_tx: mpsc::UnboundedSender<RemoteSignal>,
    signals_rx: mpsc::UnboundedReceiver<RemoteSignal>,
}

struct Linked {
    link: HostLink,
    /// [`HostConfig::dir`].
    dir: Option<PathBuf>,
}

/// Where a new panel on a sub-host runs, as its daemon described itself.
pub(super) struct RemoteTarget {
    pub(super) link: HostLink,
    pub(super) info: HostInfo,
    /// The panel's cwd on the host.
    pub(super) dir: PathBuf,
}

impl Hosts {
    pub(super) fn new() -> Self {
        let (signals_tx, signals_rx) = mpsc::unbounded_channel();
        Self {
            links: Vec::new(),
            signals_tx,
            signals_rx,
        }
    }
}

impl Multiplexer {
    /// Start a link to every host in `hosts.toml`. Must run inside the tokio
    /// runtime; links connect (and reconnect) in the background.
    pub fn connect_hosts(&mut self) {
        for host in self.config.hosts.clone() {
            self.connect_host(host);
        }
    }

    /// Start a link to one host. A second link under a name already linked is
    /// refused.
    pub fn connect_host(&mut self, host: HostConfig) {
        if self.host_link(&host.name).is_some() {
            warn!(host = %host.name, "host already linked; ignoring the second entry");
            return;
        }
        let link = HostLink::connect(
            host.name,
            self.session.id,
            host.transport,
            self.hosts.signals_tx.clone(),
        );
        self.hosts.links.push(Linked {
            link,
            dir: host.dir,
        });
    }

    fn host_link(&self, name: &str) -> Option<&HostLink> {
        self.host_links().find(|l| l.name() == name)
    }

    pub(super) fn host_links(&self) -> impl Iterator<Item = &HostLink> {
        self.hosts.links.iter().map(|l| &l.link)
    }

    /// Resolve `name` for a new panel. Only a connected host takes one: the
    /// panel's signal socket, session dir and caucus binary are what its
    /// daemon reported on this connection.
    pub(super) fn remote_target(&self, name: &str) -> Result<RemoteTarget> {
        let Some(linked) = self.hosts.links.iter().find(|l| l.link.name() == name) else {
            anyhow::bail!("no host named {name:?} (list_hosts shows the configured hosts)");
        };
        let info = match linked.link.status() {
            LinkStatus::Connected(info) => info,
            LinkStatus::Connecting { last_error } => anyhow::bail!(
                "host {name} is not connected{}",
                last_error
                    .map(|e| format!(" (last error: {e})"))
                    .unwrap_or_default()
            ),
            LinkStatus::Closed => anyhow::bail!("host {name}: link closed"),
        };
        Ok(RemoteTarget {
            link: linked.link.clone(),
            info,
            dir: linked
                .dir
                .clone()
                .unwrap_or_else(|| self.session.repo_path.clone()),
        })
    }

    /// Close every link after [`Multiplexer::shutdown`] queued its remote
    /// panels' kills, giving each host [`HOST_CLOSE_WAIT`] to take them.
    pub async fn close_hosts(&mut self) {
        let mut closing = tokio::task::JoinSet::new();
        for Linked { link, .. } in std::mem::take(&mut self.hosts.links) {
            closing.spawn(async move { link.close(HOST_CLOSE_WAIT).await });
        }
        while closing.join_next().await.is_some() {}
    }

    /// Every remote signal that has arrived so far. Taken before the PTY pump;
    /// see the module docs.
    pub fn take_remote_signals(&mut self) -> Vec<RemoteSignal> {
        let mut batch = Vec::new();
        while let Ok(signal) = self.hosts.signals_rx.try_recv() {
            batch.push(signal);
        }
        batch
    }

    /// Apply a batch from [`Multiplexer::take_remote_signals`], after the PTY
    /// pump. The host released each hook already, so none carries a reply.
    pub fn ingest_remote_signals(&mut self, batch: Vec<RemoteSignal>) {
        for RemoteSignal { host, line } in batch {
            match crate::signal::server::ingest(&line) {
                Ok(event) => self.ingest_signal(event, None, Some(&host)),
                Err(err) => warn!(%host, error = %err, "unreadable sub-host signal"),
            }
        }
    }

    /// Route one parsed signal to its handler. `source` is the host it came
    /// through (`None`: this machine's signal socket). A signal is accepted
    /// only from the host its panel runs on: a panel's signals are ordered
    /// after its output only on its own host's path.
    pub fn ingest_signal(
        &mut self,
        event: SignalEvent,
        reply: Option<tokio::sync::oneshot::Sender<StopDirective>>,
        source: Option<&str>,
    ) {
        let panel = match &event {
            SignalEvent::Turn(s) => Some(s.panel_id),
            SignalEvent::Note(n) => Some(n.panel_id),
            SignalEvent::Lifecycle(l) => Some(l.panel_id),
            SignalEvent::Unbound(_) => None,
        };
        if let Some(panel) = panel
            && !self.panel_runs_on(panel, source)
        {
            if self.panels.iter().any(|p| p.id == panel) {
                warn!(%panel, source = source.unwrap_or("local"), "signal from a host the panel does not run on; dropped");
            }
            return;
        }
        match event {
            SignalEvent::Turn(signal) => self.handle_signal_with_reply(signal, reply),
            SignalEvent::Unbound(sig) => self.handle_unbound_signal(sig, reply, source),
            SignalEvent::Note(note) => self.handle_note(note),
            SignalEvent::Lifecycle(sig) => self.handle_lifecycle(sig),
        }
    }

    /// The `list_hosts` rows: this machine first, then each sub-host in
    /// `hosts.toml` order.
    pub(super) fn host_summaries(&self) -> Vec<HostSummary> {
        let panels_on =
            |host: Option<&str>| self.panels.iter().filter(|p| p.pty.host() == host).count();
        let local = HostSummary {
            name: LOCAL_HOST.to_string(),
            status: "connected".to_string(),
            error: None,
            hostname: Some(crate::host::daemon::hostname()),
            panels: panels_on(None),
            load: Some(crate::host::load::sample()),
        };
        let remote = self.host_links().map(|link| {
            let (status, error, hostname) = match link.status() {
                LinkStatus::Connected(info) => ("connected", None, Some(info.hostname)),
                LinkStatus::Connecting { last_error } => ("connecting", last_error, None),
                LinkStatus::Closed => ("closed", None, None),
            };
            HostSummary {
                name: link.name().to_string(),
                status: status.to_string(),
                error,
                hostname,
                panels: panels_on(Some(link.name())),
                load: link.load(),
            }
        });
        std::iter::once(local).chain(remote).collect()
    }

    /// Whether `panel` is live and runs on `host` (`None`: this machine).
    pub(super) fn panel_runs_on(&self, panel: PanelId, host: Option<&str>) -> bool {
        self.panels
            .iter()
            .any(|p| p.id == panel && p.pty.host() == host)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tempfile::TempDir;

    use super::*;
    use crate::agent::manifest::AgentManifest;
    use crate::host::Transport;
    use crate::host::e2e::{TestDaemon, test_paths};
    use crate::mcp::McpToolSurface;
    use crate::mcp::protocol::{ControlRequest, ControlResponse, Placement};
    use crate::panel::lifecycle::{Panel, PanelState};
    use crate::pty::{Pty, PtyCommand};
    use crate::role::spec::AgentCli;
    use crate::session::id::AgentId;
    use crate::session::record::SessionRecord;
    use crate::session::runtime::test_support::*;
    use crate::signal::{TurnKind, TurnSignal, UnboundSignal};
    use crate::term::{Grid, OutputCapture};

    const WAIT: Duration = Duration::from_secs(10);

    fn socket_host(name: &str, sock: PathBuf) -> HostConfig {
        HostConfig {
            name: name.into(),
            transport: Transport::Socket(sock),
            dir: None,
        }
    }

    fn spawn_on(mux: &mut Multiplexer, host: &str) -> ControlResponse {
        mux.execute_control(ControlRequest::SpawnRole {
            role: "reviewer".into(),
            placement: Placement::Host(host.into()),
            model: None,
            agent_cli: None,
            prompt: None,
        })
    }

    /// A working `/bin/cat` panel on `host`, with a manifest so a Stop can
    /// settle it.
    fn push_remote_panel(mux: &mut Multiplexer, host: &str) -> PanelId {
        let id = PanelId::new();
        let link = mux.remote_target(host).unwrap().link;
        let pty = link
            .spawn(id, &PtyCommand::new("/bin/cat"), 80, 24, false)
            .map(Pty::Remote)
            .unwrap();
        mux.panels.push(Panel {
            id,
            role: "reviewer".to_string(),
            agent_id: AgentId::new(),
            state: PanelState::Idle,
            worktree_path: None,
            pty,
            grid: Grid::new(80, 24),
            capture: OutputCapture::new(),
        });
        mux.rebuild_layout_tree();
        let manifest = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        mux.manifests.insert(id, manifest);
        mux.note_prompt_delivered(id);
        id
    }

    fn stop(mux: &Multiplexer, panel: PanelId) -> SignalEvent {
        SignalEvent::Turn(TurnSignal::now(
            mux.session.id,
            panel,
            TurnKind::Stop,
            Some("done".into()),
            serde_json::json!({ "session_id": "conv-1" }),
        ))
    }

    fn state(mux: &Multiplexer, panel: PanelId) -> PanelState {
        mux.panels.iter().find(|p| p.id == panel).unwrap().state()
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn error_of(resp: ControlResponse) -> String {
        match resp {
            ControlResponse::Error { message } => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_role_on_an_unknown_or_unconnected_host_is_refused() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.connect_host(socket_host("down", tmp.path().join("absent.sock")));

        let unknown = error_of(spawn_on(&mut mux, "nope"));
        assert!(unknown.contains("no host named \"nope\""), "{unknown}");
        let down = error_of(spawn_on(&mut mux, "down"));
        assert!(down.contains("host down is not connected"), "{down}");
        assert!(mux.panels.is_empty());
        assert!(
            mux.role_counts.is_empty(),
            "a refused spawn numbers no agent"
        );

        let rows = mux.host_summaries();
        assert_eq!(rows[1].name, "down");
        assert_eq!(rows[1].status, "connecting");
        assert_eq!(rows[1].load, None);
        mux.shutdown();
        mux.close_hosts().await;
    }

    /// A panel on a sub-host is counted, listed and recorded on that host;
    /// its turn signal counts only when it arrives through that host; and a
    /// restart is refused while the host is down, leaving the panel in place.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sub_host_panel_belongs_to_its_host() {
        let tmp = TempDir::new().unwrap();
        let host_tmp = TempDir::new().unwrap();
        let paths = test_paths(&host_tmp);
        let mut daemon = TestDaemon::start(&paths).await;
        let mut mux = mux(&tmp);
        mux.connect_host(socket_host("box", paths.control_sock.clone()));
        until("box to connect and report its load", || {
            mux.host_summaries()[1].load.is_some()
        })
        .await;

        let id = push_remote_panel(&mut mux, "box");
        let rows = mux.host_summaries();
        assert_eq!(
            (
                rows[0].name.as_str(),
                rows[0].status.as_str(),
                rows[0].panels
            ),
            (LOCAL_HOST, "connected", 0)
        );
        assert!(rows[0].load.is_some());
        assert_eq!(
            (
                rows[1].name.as_str(),
                rows[1].status.as_str(),
                rows[1].panels
            ),
            ("box", "connected", 1)
        );
        assert!(rows[1].hostname.is_some());
        let listed = mux.list_panels();
        assert_eq!(
            listed.iter().find(|p| p.panel_id == id).unwrap().host,
            "box"
        );

        assert_eq!(state(&mux, id), PanelState::Working);
        mux.ingest_signal(stop(&mux, id), None, None);
        assert_eq!(
            state(&mux, id),
            PanelState::Working,
            "a Stop through this machine's socket is not the panel's"
        );
        mux.ingest_signal(stop(&mux, id), None, Some("box"));
        assert_eq!(state(&mux, id), PanelState::Idle);

        // A Stop without the panel's env is claimed by conversation id, and
        // only among the panels of the host it came through.
        mux.manifests.get_mut(&id).unwrap().claude_session_id = Some("conv-remote".into());
        mux.note_prompt_delivered(id);
        let unbound = || {
            SignalEvent::Unbound(UnboundSignal::now(
                TurnKind::Stop,
                Some("done".into()),
                serde_json::json!({ "session_id": "conv-remote", "cwd": "/" }),
            ))
        };
        mux.ingest_signal(unbound(), None, None);
        assert_eq!(state(&mux, id), PanelState::Working);
        mux.ingest_signal(unbound(), None, Some("box"));
        assert_eq!(state(&mux, id), PanelState::Idle);

        mux.persist_record();
        let record = SessionRecord::read(&mux.session.root_dir).unwrap();
        assert_eq!(record.panels[0].host.as_deref(), Some("box"));

        daemon.stop().await;
        until("box to drop", || mux.remote_target("box").is_err()).await;
        let err = mux.restart_panel(id).unwrap_err().to_string();
        assert!(err.contains("host box is not connected"), "{err}");
        assert!(mux.panels.iter().any(|p| p.id == id));
        assert!(mux.manifests.contains_key(&id));

        mux.shutdown();
        mux.close_hosts().await;
    }
}
