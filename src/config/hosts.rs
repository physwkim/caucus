//! `hosts.toml` — the sub-hosts main may place panels on.
//!
//! ```toml
//! [[host]]
//! name = "build-1"            # what spawn_role's `host` and list_hosts use
//! ssh = "me@build-1.lan"      # ssh destination; defaults to `name`
//! caucus = "~/bin/caucus"     # caucus on that host; defaults to `caucus`
//! dir = "~/src/project"       # the repo checkout there; defaults to this repo's path
//!
//! [[host]]
//! name = "box"
//! socket = "/run/user/1000/caucus-host/daemon.sock"   # a daemon on this machine
//! ```
//!
//! Read from `~/.caucus/hosts.toml` and `<repo>/.caucus/hosts.toml`; a project
//! entry replaces the global entry of the same name.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::host::{Transport, home_relative};

/// The name `list_hosts` gives the machine main runs on.
pub const LOCAL_HOST: &str = "local";

/// One sub-host and how to reach its daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub name: String,
    pub reach: HostReach,
    /// Where panels on the host start: its checkout of this repo. `None` means
    /// the path the repo has on this machine. A leading `~` is the remote
    /// user's home.
    pub dir: Option<PathBuf>,
}

/// How main reaches a host: its daemon, and git to its checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostReach {
    /// `dest` is an ssh destination (`[user@]host` or `ssh://[user@]host[:port]`).
    Ssh { dest: String, caucus: String },
    /// A daemon on this machine, at its control socket.
    Socket(PathBuf),
}

impl HostConfig {
    pub fn transport(&self) -> Transport {
        match &self.reach {
            HostReach::Ssh { dest, caucus } => Transport::ssh(dest, caucus),
            HostReach::Socket(sock) => Transport::Socket(sock.clone()),
        }
    }

    /// The git remote for the host's checkout at `dir`. A leading `~` is the
    /// host user's home: git's ssh transport passes it through to the host's
    /// `git-receive-pack`/`git-upload-pack`, which expand it there.
    pub fn git_url(&self, dir: &Path) -> OsString {
        let mut url = OsString::new();
        match &self.reach {
            HostReach::Socket(_) => url.push(home_relative(dir.to_path_buf())),
            HostReach::Ssh { dest, .. } if dest.contains("://") => {
                url.push(dest.trim_end_matches('/'));
                if dir.starts_with("~") {
                    url.push("/");
                } else if dir.is_relative() {
                    url.push("/~/");
                }
                url.push(dir);
            }
            HostReach::Ssh { dest, .. } => {
                url.push(dest);
                url.push(":");
                url.push(dir);
            }
        }
        url
    }
}

#[derive(Debug, Error)]
pub enum HostsError {
    #[error("hosts io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("hosts toml ({path}): {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("hosts ({path}): {msg}")]
    Invalid { path: PathBuf, msg: String },
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HostsFile {
    host: Vec<HostEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostEntry {
    name: String,
    ssh: Option<String>,
    socket: Option<PathBuf>,
    caucus: Option<String>,
    dir: Option<PathBuf>,
}

impl HostEntry {
    fn into_config(self) -> Result<HostConfig, String> {
        let name_ok = !self.name.is_empty()
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !name_ok {
            return Err(format!(
                "host name {:?} must be non-empty ASCII letters, digits, '-', '_' or '.'",
                self.name
            ));
        }
        if self.name == LOCAL_HOST {
            return Err(format!(
                "host name {LOCAL_HOST:?} is reserved for this machine"
            ));
        }
        let reach = match (self.socket, self.ssh) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "host {}: set `ssh` or `socket`, not both",
                    self.name
                ));
            }
            (Some(_), None) if self.caucus.is_some() => {
                return Err(format!(
                    "host {}: `caucus` applies only to an ssh host",
                    self.name
                ));
            }
            (Some(socket), None) => HostReach::Socket(socket),
            (None, ssh) => HostReach::Ssh {
                dest: ssh.unwrap_or_else(|| self.name.clone()),
                caucus: self.caucus.unwrap_or_else(|| "caucus".to_string()),
            },
        };
        Ok(HostConfig {
            name: self.name,
            reach,
            dir: self.dir,
        })
    }
}

/// Read one `hosts.toml`; a missing file has no hosts.
fn load_file(path: &Path) -> Result<Vec<HostConfig>, HostsError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(HostsError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let file: HostsFile = toml::from_str(&text).map_err(|source| HostsError::Toml {
        path: path.to_owned(),
        source,
    })?;
    let invalid = |msg| HostsError::Invalid {
        path: path.to_owned(),
        msg,
    };
    let mut hosts: Vec<HostConfig> = Vec::new();
    for entry in file.host {
        let host = entry.into_config().map_err(invalid)?;
        if hosts.iter().any(|h| h.name == host.name) {
            return Err(invalid(format!("host {} is listed twice", host.name)));
        }
        hosts.push(host);
    }
    Ok(hosts)
}

/// Global hosts, then project hosts; a project host replaces the global host
/// of the same name in place.
pub fn load(global_dir: Option<&Path>, project_dir: &Path) -> Result<Vec<HostConfig>, HostsError> {
    let mut hosts = match global_dir {
        Some(dir) => load_file(&dir.join("hosts.toml"))?,
        None => Vec::new(),
    };
    for host in load_file(&project_dir.join("hosts.toml"))? {
        match hosts.iter_mut().find(|h| h.name == host.name) {
            Some(slot) => *slot = host,
            None => hosts.push(host),
        }
    }
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("hosts.toml"), text).unwrap();
    }

    #[test]
    fn missing_files_mean_no_hosts() {
        let tmp = TempDir::new().unwrap();
        assert!(
            load(Some(&tmp.path().join("g")), tmp.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ssh_defaults_to_the_name_and_socket_hosts_connect_locally() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            r#"
            [[host]]
            name = "build-1"
            dir = "~/src/p"

            [[host]]
            name = "box"
            socket = "/tmp/d.sock"
            "#,
        );
        let hosts = load(None, tmp.path()).unwrap();
        assert_eq!(hosts[0].name, "build-1");
        assert_eq!(hosts[0].transport(), Transport::ssh("build-1", "caucus"));
        assert_eq!(hosts[0].dir, Some(PathBuf::from("~/src/p")));
        assert_eq!(
            hosts[1].transport(),
            Transport::Socket("/tmp/d.sock".into())
        );
        assert_eq!(hosts[1].dir, None);
    }

    #[test]
    fn a_project_host_replaces_the_global_one_of_the_same_name() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("g");
        write(
            &global,
            "[[host]]\nname = \"a\"\n[[host]]\nname = \"b\"\nssh = \"old\"\n",
        );
        write(tmp.path(), "[[host]]\nname = \"b\"\nssh = \"new\"\n");
        let hosts = load(Some(&global), tmp.path()).unwrap();
        let names: Vec<_> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(hosts[1].transport(), Transport::ssh("new", "caucus"));
    }

    #[test]
    fn git_url_names_the_checkout_in_scp_and_url_form() {
        let ssh = |dest: &str| HostConfig {
            name: "h".into(),
            reach: HostReach::Ssh {
                dest: dest.into(),
                caucus: "caucus".into(),
            },
            dir: None,
        };
        for (dest, dir, url) in [
            ("me@h", "~/src/p", "me@h:~/src/p"),
            ("me@h", "/srv/p", "me@h:/srv/p"),
            ("h", "src/p", "h:src/p"),
            ("ssh://me@h:2222", "~/src/p", "ssh://me@h:2222/~/src/p"),
            ("ssh://me@h:2222/", "/srv/p", "ssh://me@h:2222/srv/p"),
            ("ssh://h", "src/p", "ssh://h/~/src/p"),
        ] {
            assert_eq!(ssh(dest).git_url(Path::new(dir)), *url, "{dest} {dir}");
        }
        let socket = HostConfig {
            name: "box".into(),
            reach: HostReach::Socket("/tmp/d.sock".into()),
            dir: None,
        };
        assert_eq!(socket.git_url(Path::new("/srv/p")), "/srv/p");
    }

    #[test]
    fn bad_entries_are_load_errors() {
        for text in [
            "[[host]]\nname = \"local\"\n",
            "[[host]]\nname = \"a b\"\n",
            "[[host]]\nname = \"a\"\nssh = \"x\"\nsocket = \"/s\"\n",
            "[[host]]\nname = \"a\"\nsocket = \"/s\"\ncaucus = \"c\"\n",
            "[[host]]\nname = \"a\"\n[[host]]\nname = \"a\"\n",
            "[[host]]\nname = \"a\"\nport = 22\n",
        ] {
            let tmp = TempDir::new().unwrap();
            write(tmp.path(), text);
            assert!(load(None, tmp.path()).is_err(), "accepted {text:?}");
        }
    }
}
