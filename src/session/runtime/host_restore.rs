//! Sub-host panels coming back on `caucus resume`.
//!
//! A sub-host panel can only come back once its host's link connects, which
//! is after the rest of the roster is rebuilt. A panel the host still runs
//! (main went away without killing it) is re-adopted: same panel, same
//! agent, its output replayed. Any other starts again on the host, resuming
//! its conversation, in a worktree checked out again on its lane's branch,
//! which the host kept. Until then the panel stays in the session record as
//! it was.

use std::path::PathBuf;

use tokio::sync::oneshot;
use tracing::{info, warn};

use super::hosts::{HostRepo, within};
use super::spawn::Orphan;
use super::*;
use crate::agent::manifest;
use crate::host::LinkStatus;
use crate::session::id::AgentId;
use crate::session::record::PanelRecord;

/// A recorded sub-host panel not back yet.
pub(super) struct HostRestore {
    host: String,
    /// The agent the panel ran.
    agent: AgentId,
    /// What the session record keeps for it meanwhile.
    pub(super) record: PanelRecord,
    stage: Stage,
}

enum Stage {
    /// Waiting for the host to connect.
    Linking,
    /// The host is checking the lane's branch out again.
    Checkout(oneshot::Receiver<Result<PathBuf, String>>),
    /// Checked out at this path; the launch waits for the host to connect.
    CheckedOut(PathBuf),
}

impl Multiplexer {
    /// Bring `record`'s panel back on `host` once that host connects
    /// ([`Multiplexer::poll_host_restores`]).
    pub fn restore_host_panel(&mut self, host: String, record: PanelRecord) {
        if self.host_repo(&host).is_err() {
            warn!(role = %record.role, %host, "hosts.toml no longer names the panel's host; not restored");
            return;
        }
        let Some(agent) = record.agent_id else {
            warn!(role = %record.role, %host, "sub-host panel recorded by an older caucus; not restored");
            return;
        };
        info!(role = %record.role, %host, "sub-host panel waits for its host to connect");
        self.hosts.restores.push(HostRestore {
            host,
            agent,
            record,
            stage: Stage::Linking,
        });
    }

    /// Advance every waiting sub-host panel whose host is connected. Called
    /// once per event-loop tick.
    pub fn poll_host_restores(&mut self) {
        if self.hosts.restores.is_empty() {
            return;
        }
        let restores = std::mem::take(&mut self.hosts.restores);
        let polled = restores.len();
        let waiting: Vec<_> = restores
            .into_iter()
            .filter_map(|restore| self.poll_host_restore(restore))
            .collect();
        // The record holds the panels still waiting, so it is written only
        // once they are back in the list.
        let changed = waiting.len() != polled;
        self.hosts.restores = waiting;
        if changed {
            self.persist_record();
        }
    }

    /// Advance `restore`; it is handed back while it still waits.
    fn poll_host_restore(&mut self, mut restore: HostRestore) -> Option<HostRestore> {
        if let Stage::Checkout(rx) = &mut restore.stage {
            match rx.try_recv() {
                Ok(Ok(path)) => restore.stage = Stage::CheckedOut(path),
                Err(oneshot::error::TryRecvError::Empty) => return Some(restore),
                Ok(Err(err)) => return self.give_up(restore, &err),
                Err(oneshot::error::TryRecvError::Closed) => {
                    return self.give_up(restore, "checkout task ended without an answer");
                }
            }
        }
        let Ok(repo) = self.host_repo(&restore.host) else {
            return self.give_up(restore, "host is no longer linked");
        };
        let LinkStatus::Connected(info) = repo.link.status() else {
            return Some(restore);
        };
        if let Stage::CheckedOut(path) = &restore.stage {
            let path = path.clone();
            return self.relaunch(restore, Some(path));
        }

        let old = match manifest::read(&self.session.root_dir, restore.agent) {
            Ok(manifest) => Some(manifest),
            Err(err) => {
                warn!(role = %restore.record.role, error = %err, "sub-host panel's manifest unreadable; starting it again");
                None
            }
        };
        if let Some(old) = old
            .as_ref()
            .filter(|m| info.panels.iter().any(|p| p.id == m.panel_id))
        {
            let branch = restore.record.worktree_branch.clone();
            return match self.adopt_panel(&repo.link, old.clone(), branch) {
                Ok(panel) => {
                    info!(%panel, host = %restore.host, "re-adopted a sub-host panel");
                    None
                }
                Err(err) => {
                    warn!(host = %restore.host, error = %format!("{err:#}"), "sub-host panel not adopted; retrying");
                    Some(restore)
                }
            };
        }
        // The host may still hold the panel's exited remains.
        if let Some(old) = &old {
            repo.link.kill(old.panel_id);
        }
        let Some(branch) = restore.record.worktree_branch.clone() else {
            return self.relaunch(restore, None);
        };
        let path = old.and_then(|m| m.worktree_path).unwrap_or_else(|| {
            restore
                .record
                .resume_worktree_path(&repo.dir, self.session.id)
        });
        restore.stage = Stage::Checkout(self.check_out_lane(&restore.host, repo, path, branch));
        Some(restore)
    }

    /// Have the host check `branch` out at `path` again. The answer comes on
    /// the returned receiver; a worktree it can no longer be delivered on is
    /// removed, its branch kept.
    fn check_out_lane(
        &mut self,
        host: &str,
        repo: HostRepo,
        path: PathBuf,
        branch: String,
    ) -> oneshot::Receiver<Result<PathBuf, String>> {
        let HostRepo { link, dir, .. } = repo;
        let host = host.to_string();
        let (tx, rx) = oneshot::channel();
        self.spawn_host_task(async move {
            let made = within(link.add_worktree(&dir, &path, &branch))
                .await
                .map_err(|err| format!("worktree on host {host}: {err}"));
            if let Err(Ok(path)) = tx.send(made)
                && let Err(err) = within(link.remove_worktree(&dir, &path, None)).await
            {
                warn!(%host, path = %path.display(), error = %err, "unclaimed sub-host worktree not removed");
            }
        });
        rx
    }

    /// Start the panel again on its host, in `path` if it had a worktree.
    fn relaunch(&mut self, restore: HostRestore, path: Option<PathBuf>) -> Option<HostRestore> {
        let HostRestore {
            host,
            agent,
            record,
            ..
        } = restore;
        let branch = path
            .is_some()
            .then(|| record.worktree_branch.clone())
            .flatten();
        let spawned = self.spawn_panel_resume(
            &record.role,
            Some(record.agent_cli),
            record.model.clone(),
            path.clone(),
            branch.clone(),
            record.claude_session_id.clone(),
            None,
            Some(host.clone()),
        );
        match spawned {
            Ok(panel) => info!(%panel, %host, role = %record.role, "restarted a sub-host panel"),
            Err(err) => {
                warn!(%host, role = %record.role, error = %format!("{err:#}"), "sub-host panel not restored");
                // The branch holds the earlier agent's work: only the
                // checkout goes.
                if let Some(path) = path {
                    self.dispose_worktrees(
                        vec![Orphan {
                            host: Some(host),
                            path,
                            branch,
                            owner: Some(agent),
                            unrecorded_turns: Vec::new(),
                        }],
                        false,
                    );
                }
            }
        }
        None
    }

    /// Drop `restore`, removing any worktree checked out for it (its branch
    /// is kept).
    fn give_up(&mut self, restore: HostRestore, why: &str) -> Option<HostRestore> {
        warn!(
            host = %restore.host,
            role = %restore.record.role,
            branch = restore.record.worktree_branch.as_deref().unwrap_or(""),
            error = %why,
            "sub-host panel not restored"
        );
        if let Stage::CheckedOut(path) = restore.stage {
            self.dispose_worktrees(
                vec![Orphan {
                    host: Some(restore.host),
                    path,
                    branch: restore.record.worktree_branch,
                    owner: Some(restore.agent),
                    unrecorded_turns: Vec::new(),
                }],
                false,
            );
        }
        None
    }

    /// Stop bringing sub-host panels back, on shutdown. They stay in the
    /// record already written; a worktree checked out for one is removed, its
    /// branch kept.
    pub(super) fn abort_host_restores(&mut self) {
        let mut orphans = Vec::new();
        for restore in std::mem::take(&mut self.hosts.restores) {
            let path = match restore.stage {
                Stage::Linking => None,
                Stage::Checkout(mut rx) => {
                    rx.close();
                    rx.try_recv().ok().and_then(Result::ok)
                }
                Stage::CheckedOut(path) => Some(path),
            };
            orphans.extend(path.map(|path| Orphan {
                host: Some(restore.host),
                path,
                branch: restore.record.worktree_branch,
                owner: Some(restore.agent),
                unrecorded_turns: Vec::new(),
            }));
        }
        self.dispose_worktrees(orphans, true);
    }
}
