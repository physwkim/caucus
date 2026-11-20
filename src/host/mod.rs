//! Sub-hosts: panels a main caucus runs on another machine.
//!
//! Three pieces, one per process role:
//!
//! - [`daemon`] — `caucus host serve`, one per user on a sub-host. It owns the
//!   sub-host's PTYs, keeps each panel's recent output, and outlives the link
//!   to main, so an SSH drop costs nothing but the bytes main re-reads when it
//!   reconnects.
//! - [`bridge`] — `caucus host bridge`, what main runs over `ssh`. It joins its
//!   stdin/stdout to the daemon's control socket, starting the daemon first
//!   when none is running.
//! - [`link`] — main's side: a [`HostLink`] per sub-host keeps one connection
//!   alive across drops, and each remote panel is a [`RemotePty`] behind the
//!   same [`Pty`](crate::pty::Pty) façade a local panel uses.

pub mod bridge;
pub mod daemon;
#[cfg(test)]
pub(crate) mod e2e;
pub mod link;
pub mod load;
pub mod proto;

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub use link::{HostLink, LinkError, LinkStatus, RemotePty, RemoteSignal, Transport};
pub use load::HostLoad;
pub use proto::HostInfo;

/// Where a sub-host daemon keeps its state and listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPaths {
    /// Lock file, daemon log, and per-session directories.
    pub state_dir: PathBuf,
    /// The daemon's control socket. Whoever can connect to it can run any
    /// command as this user, so it lives in a directory only the user can
    /// enter.
    pub control_sock: PathBuf,
    /// The daemon's turn-signal socket — `CAUCUS_SOCK` for panels on this
    /// host. Named like a session's signal socket so an unbound hook's
    /// broadcast (`caucus signal post --discover`) reaches it too.
    pub signal_sock: PathBuf,
}

impl HostPaths {
    /// The per-user defaults, each replaced by its override when given.
    pub fn resolve(
        state_dir: Option<PathBuf>,
        control_sock: Option<PathBuf>,
        signal_sock: Option<PathBuf>,
    ) -> io::Result<Self> {
        // SAFETY: `getuid` has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let tmp = std::env::temp_dir();
        let state_dir = match state_dir {
            Some(dir) => dir,
            None => std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".caucus").join("host"))
                .ok_or_else(|| io::Error::other("$HOME not set; pass --state-dir"))?,
        };
        Ok(Self {
            state_dir,
            control_sock: control_sock
                .unwrap_or_else(|| tmp.join(format!("caucus-host-{uid}")).join("daemon.sock")),
            signal_sock: signal_sock.unwrap_or_else(|| tmp.join(format!("caucus-host-{uid}.sock"))),
        })
    }

    /// Lock file a live daemon holds; it also carries the daemon's pid.
    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("daemon.lock")
    }

    /// Where an auto-started daemon's stderr goes.
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }
}

/// Create `dir` mode 0700, or accept an existing one only if this user owns it
/// and no one else can enter it. In a shared `/tmp`, a directory another user
/// made first would let them swap the daemon's socket for their own.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = std::fs::metadata(dir)?;
    // SAFETY: `getuid` has no preconditions and cannot fail.
    if meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Resolve a leading `~` against this process's `$HOME`: main names a host's
/// checkout without knowing the remote user's home.
pub(crate) fn home_relative(path: PathBuf) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path,
    }
}

/// Quote `s` as one POSIX shell word.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_tightens_an_open_directory_this_user_owns() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sock");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn sh_quote_survives_a_single_quote() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", sh_quote("it's /a b")))
            .output()
            .unwrap();
        assert_eq!(out.stdout, b"it's /a b");
    }
}
