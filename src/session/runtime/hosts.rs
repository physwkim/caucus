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

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::warn;

use super::host_restore::HostRestore;
use super::input::{commit_created, commits_superseded};
use super::spawn::Orphan;
use super::*;
use crate::agent::lane_event::LaneEvent;
use crate::agent::manifest::{self, AgentManifest};
use crate::config::hosts::{HostConfig, LOCAL_HOST};
use crate::host::{Checkout, HostInfo, HostLink, LinkStatus, RemoteSignal};
use crate::mcp::HostSummary;
use crate::session::id::AgentId;
use crate::session::record::PanelRecord;
use crate::signal::{SignalEvent, StopDirective};
use crate::worktree::cleanup::{WorktreeOwner, record_removal};
use crate::worktree::remote::LocalBranch;

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
    /// Recorded panels waiting for their host to come back on resume.
    pub(super) restores: Vec<HostRestore>,
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
            restores: Vec::new(),
        }
    }

    /// Take the ended turns of `panel_id`'s lane not yet recorded, oldest
    /// first. A panel going away hands them to whatever fetches its branch
    /// last ([`fetch_left_lane`]).
    pub(super) fn take_unrecorded_turns(&mut self, panel_id: PanelId) -> Vec<Option<String>> {
        let Some(lane) = self.lane_fetches.remove(&panel_id) else {
            return Vec::new();
        };
        let mut turns = lane.running.map(|r| r.turns).unwrap_or_default();
        turns.extend(lane.waiting);
        turns
    }
}

/// Fetch `branch` of a lane on `host` here, then record the `turns` agent
/// `agent_id` ended on it that its panel went away without recording.
async fn fetch_left_lane(
    host: &str,
    here: PathBuf,
    url: &OsStr,
    branch: String,
    session_root: PathBuf,
    agent_id: AgentId,
    turns: Vec<Option<String>>,
) {
    // Held until the turns are recorded against what this fetch landed.
    let local = LocalBranch::hold(&here, &branch).await;
    if let Err(err) = within(local.fetch(url)).await {
        warn!(%host, %branch, unrecorded_turns = turns.len(), error = %err, "sub-host lane branch not fetched");
        return;
    }
    if turns.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        record_left_turns(&session_root, agent_id, &here, &branch, &turns)
    })
    .await;
}

/// Record `turns` of agent `agent_id`'s lane, as the live path does once a
/// fetch lands (`Multiplexer::poll_lane_fetches`), against its manifest on
/// disk: no live panel holds that manifest any more, so whoever disposes of
/// the lane owns it.
fn record_left_turns(
    session_root: &Path,
    agent_id: AgentId,
    here: &Path,
    branch: &str,
    turns: &[Option<String>],
) {
    let mut manifest = match manifest::read(session_root, agent_id) {
        Ok(m) => m,
        Err(err) => {
            warn!(agent = %agent_id, error = %err, "left lane turns not recorded");
            return;
        }
    };
    let record = |manifest: &mut AgentManifest, kind| {
        if let Err(err) = manifest::write(manifest, session_root, Some(LaneEvent::now(kind))) {
            warn!(agent = %agent_id, error = %err, "left lane turn write failed");
        }
    };
    for message in turns.iter().flatten() {
        if let Some(event) = commit_created(&manifest, here, branch, message) {
            record(&mut manifest, event);
        }
    }
    for event in commits_superseded(&manifest, here) {
        record(&mut manifest, event);
    }
}

impl Multiplexer {
    /// Start a link to every host in `hosts.toml`, bringing back the
    /// sub-host panels `restoring` names (a resumed session's). Must run
    /// inside the tokio runtime; links connect (and reconnect) in the
    /// background.
    pub fn connect_hosts(&mut self, restoring: Vec<PanelRecord>) {
        self.link_hosts(self.config.hosts.clone(), restoring);
    }

    /// [`Multiplexer::connect_hosts`] for `hosts`. Every restore is known
    /// before any link starts: a link kills the panels its host has for this
    /// session that it is not told to adopt.
    fn link_hosts(&mut self, hosts: Vec<HostConfig>, restoring: Vec<PanelRecord>) {
        for record in restoring {
            if let Some(host) = record.host.clone() {
                self.restore_host_panel(host, record);
            }
        }
        for host in hosts {
            self.connect_host(host);
        }
    }

    /// Start a link to one host. A second link under a name already linked is
    /// refused.
    fn connect_host(&mut self, host: HostConfig) {
        if self.host_link(&host.name).is_some() {
            warn!(host = %host.name, "host already linked; ignoring the second entry");
            return;
        }
        let link = HostLink::connect(
            host.name.clone(),
            self.session.id,
            host.transport(),
            self.hosts.signals_tx.clone(),
            self.host_adoptions(&host.name),
            Some(Checkout {
                dir: host.dir.clone(),
                repo: self.session.repo_path.clone(),
            }),
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
        Ok(HostRepo {
            link: linked.link.clone(),
            url: linked.host.git_url(),
            dir: linked.host.dir.clone(),
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
                unrecorded_turns,
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
                    session_root: session_root.clone(),
                    agent_id,
                };
                let _ = tokio::task::spawn_blocking(move || record_removal(&owner)).await;
            }
            if let Some(branch) = branch {
                fetch_left_lane(
                    &host,
                    here,
                    &url,
                    branch,
                    session_root,
                    agent_id,
                    unrecorded_turns,
                )
                .await;
            }
        });
    }

    /// Record `turns`, which agent `agent_id` ended on its lane's `branch` on
    /// `host` and left unrecorded when its panel went, once a fetch from a
    /// host task lands the branch here. For a lane whose worktree stays, as
    /// [`Multiplexer::dispose_host_worktree`] does it for one that goes.
    pub(super) fn record_left_lane(
        &mut self,
        host: &str,
        agent_id: AgentId,
        branch: String,
        turns: Vec<Option<String>>,
    ) {
        if turns.is_empty() {
            return;
        }
        let url = match self.host_repo(host) {
            Ok(repo) => repo.url,
            Err(err) => {
                warn!(%host, %branch, unrecorded_turns = turns.len(), error = %err, "left lane turns not recorded");
                return;
            }
        };
        let here = self.session.repo_path.clone();
        let session_root = self.session.root_dir.clone();
        let host = host.to_string();
        self.spawn_host_task(async move {
            fetch_left_lane(&host, here, &url, branch, session_root, agent_id, turns).await;
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
            let local = LocalBranch::hold(&here, &branch).await;
            let _ = tx.send(within(local.fetch(&repo.url)).await);
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
    use crate::session::id::AgentId;
    use crate::session::record::SessionRecord;
    use crate::session::runtime::test_support::*;
    use crate::signal::{TurnKind, TurnSignal, UnboundSignal};
    use crate::term::{Grid, OutputCapture};
    use crate::worktree::manager::run_git;

    const WAIT: Duration = Duration::from_secs(10);

    /// A host on this machine at `sock`, whose panels start in the socket's
    /// directory; [`with_host_checkout`] gives one a real checkout.
    fn socket_host(name: &str, sock: PathBuf) -> HostConfig {
        HostConfig {
            name: name.into(),
            dir: sock
                .parent()
                .expect("a socket in a directory")
                .to_path_buf(),
            reach: HostReach::Socket(sock),
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
            dir: there.clone(),
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

    fn shows(mux: &Multiplexer, panel: PanelId, text: &str) -> bool {
        let grid = mux.panels.iter().find(|p| p.id == panel).unwrap().grid();
        (0..grid.size().1).any(|row| grid.row_text(row).contains(text))
    }

    /// `gone`'s session, resumed by a new caucus.
    fn resume(gone: Multiplexer, tmp: &TempDir) -> (Multiplexer, SessionRecord) {
        let record = SessionRecord::read(&gone.session.root_dir).unwrap();
        drop(gone);
        let (mux, _signal, _control) = Multiplexer::new(
            Session::from_record(&record),
            Config::load(tmp.path()).unwrap(),
            area(),
            'a',
            LaunchMode::Resume,
        )
        .unwrap();
        (mux, record)
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

    /// An ssh host can turn out to be this machine. One whose checkout then
    /// is this repo never connects: its lanes would be pushed to and fetched
    /// back from this repo itself.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_on_this_machine_whose_checkout_is_this_repo_never_connects() {
        let tmp = TempDir::new().unwrap();
        let host_tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        let paths = test_paths(&host_tmp);
        let mut daemon = TestDaemon::start(&paths).await;
        let mut mux = mux(&tmp);
        mux.connect_host(HostConfig {
            dir: tmp.path().to_path_buf(),
            ..socket_host("box", paths.control_sock.clone())
        });

        let error = |mux: &Multiplexer| mux.host_summaries()[1].error.clone();
        until("box to refuse its checkout", || error(&mux).is_some()).await;
        let error = error(&mux).unwrap();
        assert!(error.contains("is this repo"), "{error}");
        let spawn = error_of(spawn_on(&mut mux, "box"));
        assert!(spawn.contains("is this repo"), "{spawn}");

        mux.shutdown();
        mux.close_hosts().await;
        daemon.stop().await;
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

    /// A sub-host lane whose turn ended with a commit, the turn's fetch not
    /// landed yet, and whose panel then went — killed, restarted or shut
    /// down — still has the commit recorded against its agent.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sub_host_lane_gone_mid_fetch_still_records_its_turn() {
        let tmp = TempDir::new().unwrap();
        let host_tmp = TempDir::new().unwrap();
        let (mut mux, mut daemon, _there) = with_host_checkout(&tmp, &host_tmp).await;

        let mut lanes = Vec::new();
        for how in ["killed", "restarted", "shut down"] {
            // A spawn counts the role; these lanes bypass it.
            *mux.role_counts.entry("reviewer".into()).or_default() += 1;
            let req = mux.role_worktree_request("reviewer");
            let repo = mux.host_repo("box").unwrap();
            let made = mux.make_host_worktree("box", repo, req);
            let handle = tokio::time::timeout(WAIT, made)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let path = handle.path.clone();
            let id = push_remote_lane(&mut mux, "box", Some((path.clone(), handle.branch)));
            std::fs::write(path.join("a.txt"), how).unwrap();
            git(&path, &["add", "a.txt"]).unwrap();
            git(&path, &["commit", "-q", "-m", how]).unwrap();
            let sha = git(&path, &["rev-parse", "HEAD"]).unwrap();
            mux.ingest_signal(
                stop_saying(&mux, id, &format!("committed {}", &sha[..12])),
                None,
                Some("box"),
            );
            lanes.push((how, id, mux.manifests[&id].agent_id, sha));
        }
        // Never polled: as far as main knows, every turn is still fetching.
        assert_eq!(mux.hosts.lane_fetches.len(), 3);

        mux.kill_panel(lanes[0].1).unwrap();
        mux.restart_panel(lanes[1].1).unwrap();
        mux.shutdown();
        mux.close_hosts().await;

        let unrecorded: Vec<_> = lanes
            .iter()
            .filter(|(_, _, agent, sha)| {
                !manifest::read(&mux.session.root_dir, *agent)
                    .unwrap()
                    .live_commits()
                    .iter()
                    .any(|c| c.commit == *sha)
            })
            .map(|(how, ..)| *how)
            .collect();
        assert!(unrecorded.is_empty(), "turns lost when {unrecorded:?}");
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

    /// A sub-host panel still running when its caucus went away comes back
    /// on resume as the same panel and agent, its output replayed.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_re_adopts_a_sub_host_panel_still_running() {
        let tmp = TempDir::new().unwrap();
        let host_tmp = TempDir::new().unwrap();
        let paths = test_paths(&host_tmp);
        let mut daemon = TestDaemon::start(&paths).await;
        let host = socket_host("box", paths.control_sock.clone());
        let mut gone = mux(&tmp);
        gone.connect_host(host.clone());
        until("box to connect", || gone.remote_target("box").is_ok()).await;
        let id = push_remote_panel(&mut gone, "box");
        let agent = gone.manifests[&id].agent_id;
        gone.panels[0].write_input(b"still here\n").unwrap();
        until("the echo", || {
            gone.pump_all();
            shows(&gone, id, "still here")
        })
        .await;
        gone.persist_record();
        // Gone without a kill: the links close before the panels drop.
        gone.close_hosts().await;

        let (mut back, record) = resume(gone, &tmp);
        back.link_hosts(vec![host], vec![record.panels[0].clone()]);
        until("the panel to be re-adopted", || {
            back.poll_host_restores();
            !back.panels.is_empty()
        })
        .await;
        assert_eq!((back.panels[0].id, back.panels[0].agent_id), (id, agent));
        until("its output to replay", || {
            back.pump_all();
            shows(&back, id, "still here")
        })
        .await;
        back.panels[0].write_input(b"and back\n").unwrap();
        until("it to answer", || {
            back.pump_all();
            shows(&back, id, "and back")
        })
        .await;
        let recorded = SessionRecord::read(&back.session.root_dir).unwrap();
        assert_eq!(recorded.panels[0].agent_id, Some(agent));

        back.shutdown();
        back.close_hosts().await;
        daemon.stop().await;
    }

    /// On resume, the panels the host still has for the session that the
    /// record does not hold, running or exited, are killed there; the
    /// recorded one is adopted.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_kills_the_sub_host_panels_the_record_does_not_hold() {
        let tmp = TempDir::new().unwrap();
        let host_tmp = TempDir::new().unwrap();
        let paths = test_paths(&host_tmp);
        let mut daemon = TestDaemon::start(&paths).await;
        let host = socket_host("box", paths.control_sock.clone());
        let mut gone = mux(&tmp);
        gone.link_hosts(vec![host.clone()], Vec::new());
        until("box to connect", || gone.remote_target("box").is_ok()).await;
        let kept = push_remote_panel(&mut gone, "box");
        gone.persist_record();
        // Started after the record was last written: one still running, one
        // exited.
        let link = gone.remote_target("box").unwrap().repo.link;
        let cat = PtyCommand::new("/bin/cat");
        let running = link.spawn(PanelId::new(), &cat, 80, 24, false).unwrap();
        let true_ = PtyCommand::new("/bin/true");
        let mut exited = link.spawn(PanelId::new(), &true_, 80, 24, false).unwrap();
        until("the second to exit", || {
            let _ = exited.read();
            !exited.is_alive()
        })
        .await;
        // Gone without a kill: the links close before the panels drop.
        gone.close_hosts().await;
        drop((running, exited));

        let (mut back, record) = resume(gone, &tmp);
        back.link_hosts(vec![host.clone()], record.panels.clone());
        until("the recorded panel to be re-adopted", || {
            back.poll_host_restores();
            !back.panels.is_empty()
        })
        .await;
        assert_eq!(back.panels[0].id, kept);
        back.close_hosts().await;

        let (tx, _signals) = mpsc::unbounded_channel();
        let adopting = HashSet::from([kept]);
        let probe = HostLink::connect("box", back.session.id, host.transport(), tx, adopting, None);
        let info = probe.wait_connected(WAIT).await.unwrap();
        let left: Vec<_> = info.panels.iter().map(|p| p.id).collect();
        assert_eq!(left, vec![kept]);
        probe.close(WAIT).await;
        daemon.stop().await;
    }

    /// A sub-host lane its caucus shut down starts again on resume in its
    /// old worktree, checked out again on the branch the host kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_restarts_a_sub_host_lane_on_its_kept_branch() {
        let tmp = TempDir::new().unwrap();
        let host_tmp = TempDir::new().unwrap();
        let (mut gone, mut daemon, there) = with_host_checkout(&tmp, &host_tmp).await;
        let host = gone.hosts.links[0].host.clone();
        let req = gone.role_worktree_request("reviewer");
        let repo = gone.host_repo("box").unwrap();
        let made = gone.make_host_worktree("box", repo, req);
        let handle = tokio::time::timeout(WAIT, made)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let (path, branch) = (handle.path.clone(), handle.branch.clone());
        let old = push_remote_lane(&mut gone, "box", Some((path.clone(), branch.clone())));
        std::fs::write(path.join("a.txt"), "lane").unwrap();
        git(&path, &["add", "a.txt"]).unwrap();
        git(&path, &["commit", "-q", "-m", "lane work"]).unwrap();
        let sha = git(&path, &["rev-parse", "HEAD"]).unwrap();
        gone.shutdown();
        gone.close_hosts().await;
        assert!(!path.exists());

        let (mut back, record) = resume(gone, &tmp);
        back.link_hosts(vec![host], vec![record.panels[0].clone()]);
        until("the lane to start again", || {
            back.poll_host_restores();
            back.hosts.restores.is_empty()
        })
        .await;
        let panel = &back.panels[0];
        assert_ne!(panel.id, old);
        assert_eq!(panel.pty.host(), Some("box"));
        assert_eq!(panel.worktree_path.as_deref(), Some(path.as_path()));
        assert_eq!(back.worktree_branches[&panel.id], branch);
        assert_eq!(
            git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            branch
        );
        assert_eq!(git(&path, &["rev-parse", "HEAD"]).unwrap(), sha);
        let recorded = SessionRecord::read(&back.session.root_dir).unwrap();
        assert_eq!(
            recorded.panels[0].worktree_branch.as_deref(),
            Some(branch.as_str())
        );

        back.shutdown();
        back.close_hosts().await;
        assert!(!path.exists());
        assert!(
            git(&there, &["rev-parse", &branch]).is_ok(),
            "the host keeps the branch"
        );
        daemon.stop().await;
    }

    /// A sub-host panel whose host is not back by shutdown stays in the
    /// session record for the next resume; one whose host left `hosts.toml`
    /// does not.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sub_host_panel_waiting_for_its_host_stays_recorded() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let waiting = PanelRecord {
            role: "reviewer".into(),
            agent_cli: AgentCli::Claude,
            model: None,
            order_index: 3,
            is_main: false,
            worktree_branch: Some("caucus/x/reviewer-1".into()),
            claude_session_id: Some("conv-1".into()),
            host: Some("box".into()),
            agent_id: Some(AgentId::new()),
        };
        mux.link_hosts(
            vec![socket_host("box", tmp.path().join("absent.sock"))],
            vec![
                waiting.clone(),
                PanelRecord {
                    host: Some("gone".into()),
                    ..waiting.clone()
                },
            ],
        );
        mux.poll_host_restores();
        mux.shutdown();
        let record = SessionRecord::read(&mux.session.root_dir).unwrap();
        assert_eq!(
            record.panels,
            vec![PanelRecord {
                order_index: 0,
                ..waiting
            }]
        );
        mux.close_hosts().await;
    }
}
