//! Sub-host links (`hosts.toml`) and the signals their panels post.
//!
//! A remote panel's turn signal reaches main through its host's link, after
//! the output the panel printed before it. The event loop snapshots the
//! pending remote signals *before* the PTY pump and dispatches them *after*
//! it ([`Multiplexer::take_remote_signals`] /
//! [`Multiplexer::ingest_remote_signals`]), so a signal never settles a panel
//! whose grid still lacks the turn's last output.
//!
//! A worktree panel on a sub-host commits on a branch of the host's checkout.
//! The branch is fetched into this repo after every turn
//! ([`Multiplexer::fetch_lane`]) and once more when its worktree is removed,
//! so main reviews the lane's commits here and the turn's commit records are
//! checked against this repo, as a local lane's are against its worktree.

use std::ffi::OsString;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::warn;

use super::spawn::Orphan;
use super::*;
use crate::config::hosts::{HostConfig, LOCAL_HOST};
use crate::host::{HostInfo, HostLink, LinkStatus, RemoteSignal};
use crate::mcp::HostSummary;
use crate::signal::{SignalEvent, StopDirective};
use crate::worktree::cleanup::{WorktreeOwner, record_removal};
use crate::worktree::remote::fetch_branch;

/// How long shutdown waits for each host to take its panels' kills.
const HOST_CLOSE_WAIT: Duration = Duration::from_secs(3);

/// How long shutdown waits for sub-host worktree removals and lane fetches.
const HOST_TASKS_WAIT: Duration = Duration::from_secs(30);

/// Longest a push, fetch or worktree request against a sub-host may take.
const HOST_GIT_WAIT: Duration = Duration::from_secs(120);

/// The links to every connected sub-host, the channel their signals arrive
/// on, and the work in flight against their checkouts.
pub(super) struct Hosts {
    links: Vec<Linked>,
    signals_tx: mpsc::UnboundedSender<RemoteSignal>,
    signals_rx: mpsc::UnboundedReceiver<RemoteSignal>,
    /// Worktree making and disposal and lane fetches on sub-hosts.
    /// [`Multiplexer::close_hosts`] lets them finish before the links close.
    tasks: JoinSet<()>,
    /// The lane fetch of each sub-host worktree panel that has one running or
    /// owed ([`Multiplexer::poll_lane_fetches`]).
    pub(super) lane_fetches: HashMap<PanelId, LaneFetch>,
}

struct Linked {
    link: HostLink,
    host: HostConfig,
}

/// A sub-host's checkout of this repo: the daemon that works in it and how
/// git reaches it from here.
#[derive(Clone)]
pub(super) struct HostRepo {
    pub(super) link: HostLink,
    /// The checkout on the host (a leading `~` is the host user's home).
    pub(super) dir: PathBuf,
    /// [`HostConfig::git_url`] of `dir`.
    pub(super) url: OsString,
}

/// Where a new panel on a sub-host runs, as its daemon described itself.
pub(super) struct RemoteTarget {
    pub(super) repo: HostRepo,
    pub(super) info: HostInfo,
}

/// Ended turns of a sub-host lane whose commits are recorded once its branch
/// reaches this repo. Each turn is its final message, if it had one.
#[derive(Default)]
pub(super) struct LaneFetch {
    /// The fetch running now.
    running: Option<RunningFetch>,
    /// Turns that ended after `running` started, or whose fetch failed.
    waiting: Vec<Option<String>>,
}

struct RunningFetch {
    done: oneshot::Receiver<Result<(), String>>,
    /// The turns it covers.
    turns: Vec<Option<String>>,
}

/// `work` against a sub-host, failed if it takes over [`HOST_GIT_WAIT`].
pub(super) async fn within<T>(work: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(HOST_GIT_WAIT, work)
        .await
        .unwrap_or_else(|_| Err(format!("no answer in {}s", HOST_GIT_WAIT.as_secs())))
}

impl Hosts {
    pub(super) fn new() -> Self {
        let (signals_tx, signals_rx) = mpsc::unbounded_channel();
        Self {
            links: Vec::new(),
            signals_tx,
            signals_rx,
            tasks: JoinSet::new(),
            lane_fetches: HashMap::new(),
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
            host.name.clone(),
            self.session.id,
            host.transport(),
            self.hosts.signals_tx.clone(),
        );
        self.hosts.links.push(Linked { link, host });
    }

    fn host_link(&self, name: &str) -> Option<&HostLink> {
        self.host_links().find(|l| l.name() == name)
    }

    pub(super) fn host_links(&self) -> impl Iterator<Item = &HostLink> {
        self.hosts.links.iter().map(|l| &l.link)
    }

    /// The checkout of host `name`, connected or not: requests to its daemon
    /// wait for the link.
    pub(super) fn host_repo(&self, name: &str) -> Result<HostRepo> {
        let Some(linked) = self.hosts.links.iter().find(|l| l.link.name() == name) else {
            anyhow::bail!("no host named {name:?} (list_hosts shows the configured hosts)");
        };
        let dir = linked
            .host
            .dir
            .clone()
            .unwrap_or_else(|| self.session.repo_path.clone());
        Ok(HostRepo {
            link: linked.link.clone(),
            url: linked.host.git_url(&dir),
            dir,
        })
    }

    /// Resolve `name` for a new panel. Only a connected host takes one: the
    /// panel's signal socket, session dir and caucus binary are what its
    /// daemon reported on this connection.
    pub(super) fn remote_target(&self, name: &str) -> Result<RemoteTarget> {
        let repo = self.host_repo(name)?;
        let info = match repo.link.status() {
            LinkStatus::Connected(info) => info,
            LinkStatus::Connecting { last_error } => anyhow::bail!(
                "host {name} is not connected{}",
                last_error
                    .map(|e| format!(" (last error: {e})"))
                    .unwrap_or_default()
            ),
            LinkStatus::Closed => anyhow::bail!("host {name}: link closed"),
        };
        Ok(RemoteTarget { repo, info })
    }

    /// Run `task` against a sub-host until it ends or
    /// [`Multiplexer::close_hosts`] stops waiting for it.
    pub(super) fn spawn_host_task(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        while self.hosts.tasks.try_join_next().is_some() {}
        self.hosts.tasks.spawn(task);
    }

    /// Have `host`'s daemon remove `orphan`, a worktree of its checkout, from
    /// a host task. An owned worktree's removal is credited to its agent, and
    /// its branch, with whatever the removal salvaged onto it, is fetched
    /// here; an unowned one's branch is deleted on the host instead.
    pub(super) fn dispose_host_worktree(&mut self, host: &str, orphan: Orphan) {
        let HostRepo { link, dir, url } = match self.host_repo(host) {
            Ok(repo) => repo,
            Err(err) => {
                warn!(%host, path = %orphan.path.display(), error = %err, "sub-host worktree left in place");
                return;
            }
        };
        let here = self.session.repo_path.clone();
        let session_root = self.session.root_dir.clone();
        let host = host.to_string();
        self.spawn_host_task(async move {
            let Orphan {
                path,
                branch,
                owner,
                ..
            } = orphan;
            let delete = owner.is_none().then_some(branch.as_deref()).flatten();
            let removed = within(link.remove_worktree(&dir, &path, delete)).await;
            if let Err(err) = &removed {
                warn!(%host, path = %path.display(), error = %err, "sub-host worktree not removed");
            }
            let Some(agent_id) = owner else {
                return;
            };
            if removed.is_ok() {
                let owner = WorktreeOwner {
                    worktree: path,
                    session_root,
                    agent_id,
                };
                let _ = tokio::task::spawn_blocking(move || record_removal(&owner)).await;
            }
            if let Some(branch) = branch
                && let Err(err) = within(fetch_branch(&here, &url, &branch)).await
            {
                warn!(%host, %branch, error = %err, "sub-host lane branch not fetched");
            }
        });
    }

    /// Record a sub-host lane's ended turn, whose final message is `message`,
    /// once the lane's branch is fetched here: start the fetch, or queue the
    /// turn behind the one running. A panel with no worktree branch has
    /// nothing to fetch.
    pub(super) fn fetch_lane(&mut self, panel_id: PanelId, host: &str, message: Option<String>) {
        if !self.worktree_branches.contains_key(&panel_id) {
            return;
        }
        let lane = self.hosts.lane_fetches.entry(panel_id).or_default();
        lane.waiting.push(message);
        if lane.running.is_none() {
            self.start_lane_fetch(panel_id, host);
        }
    }

    /// Fetch the lane's branch for every waiting turn.
    fn start_lane_fetch(&mut self, panel_id: PanelId, host: &str) {
        let Some(branch) = self.worktree_branches.get(&panel_id).cloned() else {
            return;
        };
        let repo = match self.host_repo(host) {
            Ok(repo) => repo,
            Err(err) => {
                warn!(panel = %panel_id, %host, error = %err, "lane fetch not started");
                return;
            }
        };
        let here = self.session.repo_path.clone();
        let (tx, rx) = oneshot::channel();
        self.spawn_host_task(async move {
            let _ = tx.send(within(fetch_branch(&here, &repo.url, &branch)).await);
        });
        if let Some(lane) = self.hosts.lane_fetches.get_mut(&panel_id) {
            lane.running = Some(RunningFetch {
                done: rx,
                turns: std::mem::take(&mut lane.waiting),
            });
        }
    }

    /// Record the commits of every lane whose fetch landed, against this
    /// repo, and start the next fetch for turns that ended meanwhile. A failed
    /// fetch's turns wait for the lane's next turn to try again. Called once
    /// per event-loop tick.
    pub fn poll_lane_fetches(&mut self) {
        let landed: Vec<_> = self
            .hosts
            .lane_fetches
            .iter_mut()
            .filter_map(|(&panel_id, lane)| {
                let result = match lane.running.as_mut()?.done.try_recv() {
                    Ok(result) => result,
                    Err(oneshot::error::TryRecvError::Empty) => return None,
                    Err(oneshot::error::TryRecvError::Closed) => {
                        Err("fetch task ended without an answer".to_string())
                    }
                };
                let RunningFetch { turns, .. } = lane.running.take()?;
                Some((panel_id, result, turns))
            })
            .collect();
        let here = self.session.repo_path.clone();
        for (panel_id, result, turns) in landed {
            let again = match result {
                Ok(()) => {
                    for message in turns.iter().flatten() {
                        self.record_commit_provenance(panel_id, &here, message);
                    }
                    self.record_commit_supersessions(panel_id, &here);
                    self.hosts
                        .lane_fetches
                        .get(&panel_id)
                        .is_some_and(|lane| !lane.waiting.is_empty())
                }
                Err(err) => {
                    warn!(panel = %panel_id, error = %err, "lane fetch failed; retrying after the next turn");
                    let Some(lane) = self.hosts.lane_fetches.get_mut(&panel_id) else {
                        continue;
                    };
                    let again = !lane.waiting.is_empty();
                    lane.waiting.splice(0..0, turns);
                    again
                }
            };
            let host = self
                .panels
                .iter()
                .find(|p| p.id == panel_id)
                .and_then(|p| p.pty.host())
                .map(str::to_string);
            match host {
                Some(host) if again => self.start_lane_fetch(panel_id, &host),
                _ => {
                    if self
                        .hosts
                        .lane_fetches
                        .get(&panel_id)
                        .is_some_and(|lane| lane.running.is_none() && lane.waiting.is_empty())
                    {
                        self.hosts.lane_fetches.remove(&panel_id);
                    }
                }
            }
        }
    }

    /// Close every link after [`Multiplexer::shutdown`] queued its remote
    /// panels' kills and worktree removals: the host tasks get
    /// [`HOST_TASKS_WAIT`] to finish, then each host [`HOST_CLOSE_WAIT`] to
    /// take what is still queued.
    pub async fn close_hosts(&mut self) {
        let tasks = &mut self.hosts.tasks;
        if tokio::time::timeout(HOST_TASKS_WAIT, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            warn!(
                left = tasks.len(),
                "sub-host worktree work still running at exit; abandoned"
            );
            tasks.abort_all();
        }
        let mut closing = JoinSet::new();
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
    use std::path::Path;
    use std::time::Instant;

    use tempfile::TempDir;

    use super::*;
    use crate::agent::lane_event::LaneEventKind;
    use crate::agent::manifest::{self, AgentManifest};
    use crate::config::hosts::HostReach;
    use crate::host::e2e::{TestDaemon, test_paths};
    use crate::mcp::McpToolSurface;
    use crate::mcp::protocol::{ControlRequest, ControlResponse, Placement};
    use crate::panel::lifecycle::{Panel, PanelState};
    use crate::pty::{Pty, PtyCommand};
    use crate::role::spec::AgentCli;
    use crate::session::record::SessionRecord;
    use crate::session::runtime::test_support::*;
    use crate::signal::{TurnKind, TurnSignal, UnboundSignal};
    use crate::term::{Grid, OutputCapture};
    use crate::worktree::manager::run_git;

    const WAIT: Duration = Duration::from_secs(10);

    fn socket_host(name: &str, sock: PathBuf) -> HostConfig {
        HostConfig {
            name: name.into(),
            reach: HostReach::Socket(sock),
            dir: None,
        }
    }

    fn spawn_on(mux: &mut Multiplexer, host: &str) -> ControlResponse {
        mux.execute_control(ControlRequest::SpawnRole {
            role: "reviewer".into(),
            placement: Placement {
                host: Some(host.into()),
                worktree: false,
            },
            model: None,
            agent_cli: None,
            prompt: None,
        })
    }

    /// A working `/bin/cat` panel on `host`, with a manifest so a Stop can
    /// settle it.
    fn push_remote_panel(mux: &mut Multiplexer, host: &str) -> PanelId {
        push_remote_lane(mux, host, None)
    }

    /// [`push_remote_panel`], working in `worktree` (path, branch) on the
    /// host when given, with its manifest on disk.
    fn push_remote_lane(
        mux: &mut Multiplexer,
        host: &str,
        worktree: Option<(PathBuf, String)>,
    ) -> PanelId {
        let id = PanelId::new();
        let link = mux.remote_target(host).unwrap().repo.link;
        let pty = link
            .spawn(id, &PtyCommand::new("/bin/cat"), 80, 24, false)
            .map(Pty::Remote)
            .unwrap();
        let mut manifest = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        manifest.worktree_path = worktree.as_ref().map(|(path, _)| path.clone());
        manifest::write(&mut manifest, &mux.session.root_dir, None).unwrap();
        mux.panels.push(Panel {
            id,
            role: "reviewer".to_string(),
            agent_id: manifest.agent_id,
            state: PanelState::Idle,
            worktree_path: manifest.worktree_path.clone(),
            pty,
            grid: Grid::new(80, 24),
            capture: OutputCapture::new(),
        });
        mux.rebuild_layout_tree();
        mux.manifests.insert(id, manifest);
        if let Some((_, branch)) = worktree {
            mux.worktree_branches.insert(id, branch);
        }
        mux.note_prompt_delivered(id);
        id
    }

    fn stop(mux: &Multiplexer, panel: PanelId) -> SignalEvent {
        stop_saying(mux, panel, "done")
    }

    fn stop_saying(mux: &Multiplexer, panel: PanelId, message: &str) -> SignalEvent {
        SignalEvent::Turn(TurnSignal::now(
            mux.session.id,
            panel,
            TurnKind::Stop,
            Some(message.into()),
            serde_json::json!({ "session_id": "conv-1" }),
        ))
    }

    fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        run_git(dir, &args)
            .map(|out| out.trim().to_string())
            .map_err(|err| err.to_string())
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "t"],
            &["commit", "-q", "--allow-empty", "-m", "root"],
        ] {
            git(dir, args).unwrap();
        }
    }

    /// The lane branches and worktrees `repo` holds.
    fn lanes_in(repo: &Path) -> (String, bool) {
        let branches = git(
            repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads/caucus"],
        )
        .unwrap();
        let worktrees = std::fs::read_dir(repo.join(".caucus").join("worktrees"))
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        (branches, worktrees)
    }

    /// This repo at `here` and a host `box` whose checkout of it is
    /// `there`, served by a daemon in `host_tmp`.
    async fn with_host_checkout(
        here: &TempDir,
        host_tmp: &TempDir,
    ) -> (Multiplexer, TestDaemon, PathBuf) {
        let there = host_tmp.path().join("checkout");
        init_repo(here.path());
        init_repo(&there);
        let paths = test_paths(host_tmp);
        let daemon = TestDaemon::start(&paths).await;
        let mut mux = mux(here);
        mux.connect_host(HostConfig {
            dir: Some(there.clone()),
            ..socket_host("box", paths.control_sock.clone())
        });
        until("box to connect", || mux.remote_target("box").is_ok()).await;
        (mux, daemon, there)
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

    /// A worktree lane on a sub-host starts from this repo's `HEAD`; each
    /// turn's commit is fetched here and recorded against this repo; a kill
    /// salvages the lane's uncommitted work onto its branch, fetches that too,
    /// credits the removal and keeps the branch on the host.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sub_host_worktree_lane_reaches_this_repo() {
        let tmp = TempDir::new().unwrap();
        let host_tmp = TempDir::new().unwrap();
        let (mut mux, mut daemon, there) = with_host_checkout(&tmp, &host_tmp).await;
        let here = tmp.path().to_path_buf();

        let req = mux.role_worktree_request("reviewer");
        let repo = mux.host_repo("box").unwrap();
        let made = mux.make_host_worktree("box", repo, req);
        let handle = tokio::time::timeout(WAIT, made)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let (path, branch) = (handle.path.clone(), handle.branch.clone());
        assert!(
            path.starts_with(there.join(".caucus/worktrees")),
            "{path:?}"
        );
        assert_eq!(
            git(&path, &["rev-parse", "HEAD"]),
            git(&here, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            branch
        );

        let id = push_remote_lane(&mut mux, "box", Some((path.clone(), branch.clone())));
        let agent = mux.manifests[&id].agent_id;
        std::fs::write(path.join("a.txt"), "lane").unwrap();
        git(&path, &["add", "a.txt"]).unwrap();
        git(&path, &["commit", "-q", "-m", "lane work"]).unwrap();
        let sha = git(&path, &["rev-parse", "HEAD"]).unwrap();

        mux.ingest_signal(
            stop_saying(&mux, id, &format!("committed {}", &sha[..12])),
            None,
            Some("box"),
        );
        until("the turn's commit to be recorded", || {
            mux.poll_lane_fetches();
            mux.manifests[&id]
                .live_commits()
                .iter()
                .any(|c| c.commit == sha && c.worktree.as_deref() == Some(path.as_path()))
        })
        .await;
        assert_eq!(git(&here, &["rev-parse", &branch]).unwrap(), sha);
        assert!(
            mux.hosts.lane_fetches.is_empty(),
            "an idle lane keeps no entry"
        );

        std::fs::write(path.join("b.txt"), "unsaved").unwrap();
        mux.kill_panel(id).unwrap();
        until("the salvaged work to reach this repo", || {
            git(&here, &["rev-parse", &format!("{branch}~1")]).is_ok_and(|parent| parent == sha)
        })
        .await;
        assert!(!path.exists());
        assert!(
            git(&here, &["show", "--name-only", "--format=", &branch])
                .unwrap()
                .contains("b.txt")
        );
        assert_eq!(
            git(&there, &["rev-parse", &branch]),
            git(&here, &["rev-parse", &branch]),
            "the host keeps the branch"
        );
        let removed = manifest::read(&mux.session.root_dir, agent)
            .unwrap()
            .lane_events()
            .iter()
            .any(|e| matches!(&e.kind, LaneEventKind::WorktreeRemoved { path: p } if *p == path));
        assert!(removed, "the removal is credited to the lane's agent");

        mux.shutdown();
        mux.close_hosts().await;
        daemon.stop().await;
    }

    /// A sub-host worktree whose panel never launches, because the spawn
    /// failed or shutdown aborted it, leaves neither its directory nor its
    /// branch on the host.
    #[tokio::test(flavor = "multi_thread")]
    async fn sub_host_worktrees_no_lane_took_go_with_their_branch() {
        let tmp = TempDir::new().unwrap();
        let host_tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".caucus")).unwrap();
        std::fs::write(
            tmp.path().join(".caucus").join("roles.toml"),
            "[roles.broken]\nsystem_prompt_template = \"roles/absent-for-test.md\"\n",
        )
        .unwrap();
        let (mut mux, mut daemon, there) = with_host_checkout(&tmp, &host_tmp).await;
        let nothing = (String::new(), false);

        let (tx, rx) = oneshot::channel();
        mux.begin_spawn_role_worktree("broken".into(), Some("box".into()), None, None, None, tx);
        until("the spawn to finish", || {
            mux.poll_pending_spawns();
            mux.pending_spawns.is_empty()
        })
        .await;
        let err = error_of(rx.await.unwrap());
        assert!(err.contains("roles/absent-for-test.md"), "{err}");
        until("the failed spawn's worktree to go", || {
            lanes_in(&there) == nothing
        })
        .await;

        let (tx, rx) = oneshot::channel();
        mux.begin_spawn_role_worktree("reviewer".into(), Some("box".into()), None, None, None, tx);
        mux.shutdown();
        let err = error_of(rx.await.unwrap());
        assert!(err.contains("shutting down"), "{err}");
        mux.close_hosts().await;
        assert_eq!(lanes_in(&there), nothing);
        assert_eq!(lanes_in(tmp.path()), nothing);
        daemon.stop().await;
    }
}
