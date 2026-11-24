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
//!
//! [`install`] puts main's own build on an ssh host whose entry names no
//! `caucus`, for the link to run there.

pub mod bridge;
pub mod daemon;
#[cfg(test)]
pub(crate) mod e2e;
pub mod install;
pub mod link;
pub mod load;
pub mod proto;

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub use install::{RemoteCaucus, RemoteShell};
pub use link::{Checkout, HostLink, LinkError, LinkStatus, RemotePty, RemoteSignal, Transport};
pub use load::HostLoad;
pub use proto::{HostInfo, HostPanel};

/// Where a sub-host daemon keeps its state and listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPaths {
    /// Lock file, daemon log, per-session directories and, by default, both
    /// sockets.
    pub state_dir: PathBuf,
    /// The daemon's control socket. Whoever can connect to it can run any
    /// command as this user, so it lives in a directory only the user can
    /// enter.
    pub control_sock: PathBuf,
    /// The daemon's turn-signal socket — `CAUCUS_SOCK` for panels on this
    /// host. An unbound hook's broadcast (`caucus signal post --discover`)
    /// posts to the default one too.
    pub signal_sock: PathBuf,
}

/// Bytes a Unix socket address holds for its path, the closing NUL included.
const SUN_PATH: usize =
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path);

impl HostPaths {
    /// The defaults, each replaced by its override when given. Both sockets
    /// default into the state directory, which only this user can enter.
    pub fn resolve(
        state_dir: Option<PathBuf>,
        control_sock: Option<PathBuf>,
        signal_sock: Option<PathBuf>,
    ) -> io::Result<Self> {
        let state_dir = match state_dir {
            Some(dir) => dir,
            None => std::env::var_os("HOME")
                .map(|home| Self::default_state_dir(Path::new(&home)))
                .ok_or_else(|| io::Error::other("$HOME not set; pass --state-dir"))?,
        };
        let paths = Self {
            control_sock: control_sock.unwrap_or_else(|| state_dir.join("daemon.sock")),
            signal_sock: signal_sock.unwrap_or_else(|| state_dir.join("signal.sock")),
            state_dir,
        };
        for sock in [&paths.control_sock, &paths.signal_sock] {
            let len = sock.as_os_str().len();
            if len >= SUN_PATH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} is {len} bytes, too long for a Unix socket (under {SUN_PATH}); \
                         pass a shorter --state-dir, --control-sock or --signal-sock",
                        sock.display()
                    ),
                ));
            }
        }
        Ok(paths)
    }

    /// `~/.caucus/host/<hostname>`: one per machine, since several can share
    /// a home.
    pub fn default_state_dir(home: &Path) -> PathBuf {
        home.join(".caucus").join("host").join(daemon::hostname())
    }

    /// Lock file a live daemon holds; it also carries the daemon's pid.
    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("daemon.lock")
    }

    /// Where an auto-started daemon's stderr goes.
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }

    /// Make the control socket's directory this user's alone
    /// ([`private_dir`]), so a socket in it can only be this user's daemon.
    /// The daemon runs it before binding, the bridge before connecting.
    pub(crate) fn own_control_dir(&self) -> io::Result<()> {
        match self.control_sock.parent() {
            Some(dir) => private_dir(dir),
            None => Ok(()),
        }
    }
}

/// Create `dir` mode 0700, or accept an existing one only if this user owns it
/// and no one else can enter it. In a shared `/tmp`, a directory another user
/// made first would let them swap the daemon's socket for their own; so would
/// a symlink of theirs, which they can repoint after the check.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a symlink, not a directory", dir.display()),
        ));
    }
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

/// This machine's systemd or D-Bus machine id, or on macOS its hardware UUID;
/// `None` on a machine with none of them.
pub(crate) fn machine_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut uuid = [0u8; 16];
        let wait = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        // SAFETY: `uuid` has room for the 16 bytes `gethostuuid` writes.
        if unsafe { libc::gethostuuid(uuid.as_mut_ptr(), &wait) } == 0 {
            return Some(uuid.iter().map(|b| format!("{b:02x}")).collect());
        }
    }
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|path| {
            let id = std::fs::read_to_string(path).ok()?;
            let id = id.trim();
            (!id.is_empty()).then(|| id.to_string())
        })
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

/// Quote `path` as one POSIX shell word whose leading `~/`, as in
/// `hosts.toml`, is the home of the user the shell runs as.
pub(crate) fn sh_quote_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
        None => sh_quote(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moving the state directory moves the sockets with it.
    #[test]
    fn the_sockets_default_into_the_state_dir() {
        let paths = HostPaths::resolve(Some("/srv/caucus".into()), None, None).unwrap();
        assert_eq!(paths.control_sock, Path::new("/srv/caucus/daemon.sock"));
        assert_eq!(paths.signal_sock, Path::new("/srv/caucus/signal.sock"));
    }

    /// A socket path no address can hold is refused when the paths resolve,
    /// not when a daemon started from them fails to bind.
    #[test]
    fn a_socket_path_too_long_for_an_address_is_refused() {
        let deep = PathBuf::from("/").join("d".repeat(SUN_PATH));
        let err = HostPaths::resolve(Some(deep), None, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("--state-dir"), "{err}");
        let longest = PathBuf::from("/").join("s".repeat(SUN_PATH - 2));
        HostPaths::resolve(None, Some(longest.clone()), Some(longest)).unwrap();
    }

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
    fn private_dir_refuses_a_symlink_even_to_this_users_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("sock");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = private_dir(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
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
