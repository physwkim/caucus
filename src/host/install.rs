//! Main's own build on an ssh host whose `hosts.toml` entry names no `caucus`.
//!
//! The copy lives at [`MANAGED_CAUCUS`]. Which build it is, the copy says
//! itself (`caucus host build-id`), so there is no record beside it that could
//! disagree with it. The transport exits [`NOT_INSTALLED`] when the copy is
//! missing or another build; the link then uploads this build with
//! [`install`] and connects again.

use std::ffi::OsString;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use tokio::io::AsyncWriteExt;

use super::{sh_quote, sh_quote_path};

/// Where a managed host keeps main's build; `~` is the host user's home.
pub const MANAGED_CAUCUS: &str = "~/.caucus/bin/caucus";
/// The exit status of [`managed_script`] when the host lacks main's build.
pub const NOT_INSTALLED: i32 = 213;
/// The exit status of the install script when the copy cannot run there.
const CANNOT_RUN: i32 = 214;

/// Which caucus an ssh host runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCaucus {
    /// Main's own build, which main copies to [`MANAGED_CAUCUS`] there.
    Managed,
    /// One the host's user keeps installed: a path (a leading `~/` is their
    /// home) or a name their login shell's PATH resolves.
    At(String),
}

/// How to run a shell command line on a host as its user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteShell {
    pub program: OsString,
    /// Arguments ahead of the command line, which comes last.
    pub args: Vec<OsString>,
}

impl RemoteShell {
    /// `ssh <destination>`, failing rather than prompting.
    pub fn ssh(destination: &str) -> Self {
        let args = [
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=3",
            "--",
            destination,
        ];
        Self {
            program: "ssh".into(),
            args: args.into_iter().map(OsString::from).collect(),
        }
    }

    /// The arguments running POSIX sh `script` there. The host user's own
    /// shell parses the command line, so the script reaches `sh` as one
    /// quoted word.
    pub fn args(&self, script: &str) -> Vec<OsString> {
        let mut args = self.args.clone();
        args.push(format!("sh -c {}", sh_quote(script)).into());
        args
    }
}

/// The sh script running `<caucus> <subcommand>` in the host user's login
/// shell, so it and the daemon it starts see the PATH their profile sets.
pub fn login_script(caucus: &str, subcommand: &str) -> String {
    let run = format!("exec {} {subcommand}", sh_quote_path(caucus));
    format!("exec \"${{SHELL:-/bin/sh}}\" -lc {}", sh_quote(&run))
}

/// [`login_script`] for [`MANAGED_CAUCUS`], exiting [`NOT_INSTALLED`] unless
/// the copy there is build `id`.
pub fn managed_script(id: &str, subcommand: &str) -> String {
    format!(
        "[ \"$({} host build-id 2>/dev/null)\" = {} ] || exit {NOT_INSTALLED}; {}",
        sh_quote_path(MANAGED_CAUCUS),
        sh_quote(id),
        login_script(MANAGED_CAUCUS, subcommand)
    )
}

/// This process's executable image. On Linux `/proc/self/exe` stays the
/// running image after an upgrade replaces the file it was started from.
pub fn running_image() -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().unwrap_or_default()
    }
}

/// What tells one build from another: a hash of its executable image. The
/// hasher is the image's own std's, so a copy of the image hashing itself
/// gets the same id.
pub fn build_id(image: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(image)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(&bytes);
    Ok(format!("{:016x}", hasher.finish()))
}

/// [`build_id`] of this process, read once.
pub fn running_build_id() -> Result<&'static str, String> {
    static ID: OnceLock<Result<String, String>> = OnceLock::new();
    ID.get_or_init(|| {
        let image = running_image();
        build_id(&image).map_err(|e| format!("read {}: {e}", image.display()))
    })
    .as_deref()
    .map_err(Clone::clone)
}

/// Why [`install`] did not install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// The copy does not run there (another OS or CPU, or older system
    /// libraries); copying it again will not help.
    CannotRun(String),
    /// The copy did not get there; another attempt may.
    Failed(String),
}

/// Reads the build from stdin into a temporary file beside
/// [`MANAGED_CAUCUS`], runs it, and moves it into place only once it ran.
const INSTALL_SCRIPT: &str = "d=\"$HOME/.caucus/bin\"; t=\"$d/.caucus.$$\"; \
    mkdir -p \"$d\" && cat >\"$t\" && chmod 755 \"$t\" || { rm -f \"$t\"; exit 1; }; \
    out=$(\"$t\" host build-id 2>&1) || { \
    printf '%s (%s)\\n' \"$out\" \"$(uname -sm)\" >&2; rm -f \"$t\"; exit 214; }; \
    mv -f \"$t\" \"$d/caucus\"";

/// Copy the build at `image` to [`MANAGED_CAUCUS`] on the host `shell`
/// reaches. A copy that does not run there replaces nothing.
pub async fn install(shell: &RemoteShell, image: &Path) -> Result<(), InstallError> {
    let failed = InstallError::Failed;
    let build = tokio::fs::read(image)
        .await
        .map_err(|e| failed(format!("read {}: {e}", image.display())))?;
    let mut child = tokio::process::Command::new(&shell.program)
        .args(shell.args(INSTALL_SCRIPT))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| failed(format!("run {:?}: {e}", shell.program)))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    // A script that stopped reading has already failed; its status says why.
    let send = async move {
        let _ = stdin.write_all(&build).await;
    };
    let (_, out) = tokio::join!(send, child.wait_with_output());
    let out = out.map_err(|e| failed(format!("wait for {:?}: {e}", shell.program)))?;
    let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
    match out.status.code() {
        Some(0) => Ok(()),
        Some(CANNOT_RUN) => Err(InstallError::CannotRun(format!(
            "this build of caucus does not run there: {said}"
        ))),
        _ if said.is_empty() => Err(failed(format!("copy caucus there: {}", out.status))),
        _ => Err(failed(format!("copy caucus there: {}: {said}", out.status))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RemoteShell` for a "host" that is a local `sh` whose home is `home`.
    fn local(home: &Path) -> RemoteShell {
        RemoteShell {
            program: "env".into(),
            args: vec![
                format!("HOME={}", home.display()).into(),
                "sh".into(),
                "-c".into(),
            ],
        }
    }

    /// An executable script at `dir/name` running `body`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run(shell: &RemoteShell, script: &str) -> std::process::Output {
        std::process::Command::new(&shell.program)
            .args(shell.args(script))
            .output()
            .unwrap()
    }

    #[test]
    fn ssh_runs_the_script_as_one_word_in_sh() {
        let args = RemoteShell::ssh("me@h").args("echo 'a b'");
        assert_eq!(args.last().unwrap(), r#"sh -c 'echo '\''a b'\'''"#);
        assert_eq!(args[args.len() - 2], "me@h");
    }

    #[test]
    fn the_build_id_is_the_images_and_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let a = script(tmp.path(), "a", "exit 0");
        let b = script(tmp.path(), "b", "exit 1");
        assert_eq!(build_id(&a).unwrap(), build_id(&a).unwrap());
        assert_ne!(build_id(&a).unwrap(), build_id(&b).unwrap());
    }

    /// The managed check runs the copy only when it reports build `id`.
    #[test]
    fn the_managed_script_runs_only_the_named_build() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = local(tmp.path());
        let bin = tmp.path().join(".caucus").join("bin");
        let missing = run(&shell, &managed_script("abc", "x"));
        assert_eq!(missing.status.code(), Some(NOT_INSTALLED));

        std::fs::create_dir_all(&bin).unwrap();
        script(
            &bin,
            "caucus",
            r#"if [ "$1 $2" = "host build-id" ]; then echo abc; else echo "ran $*"; fi"#,
        );
        let other = run(&shell, &managed_script("abd", "x"));
        assert_eq!(other.status.code(), Some(NOT_INSTALLED));
        let same = run(&shell, &managed_script("abc", "host bridge"));
        assert!(same.status.success(), "{same:?}");
        assert_eq!(String::from_utf8_lossy(&same.stdout), "ran host bridge\n");
    }

    #[tokio::test]
    async fn install_puts_a_build_that_runs_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let image = script(tmp.path(), "image", "echo id");
        install(&local(tmp.path()), &image).await.unwrap();
        let copy = tmp.path().join(".caucus/bin/caucus");
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            std::fs::read(&image).unwrap()
        );
        let leftovers = std::fs::read_dir(copy.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "only the copy itself");
    }

    /// A build that does not run there leaves the host's copy as it was.
    #[tokio::test]
    async fn a_build_that_does_not_run_replaces_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".caucus/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let before = script(&bin, "caucus", "echo old");
        let image = script(
            tmp.path(),
            "image",
            "echo 'Exec format error' >&2; exit 126",
        );
        let err = install(&local(tmp.path()), &image).await.unwrap_err();
        let InstallError::CannotRun(msg) = err else {
            panic!("expected CannotRun, got {err:?}");
        };
        assert!(msg.contains("Exec format error"), "{msg}");
        assert!(
            std::fs::read_to_string(&before)
                .unwrap()
                .contains("echo old")
        );
        assert_eq!(
            std::fs::read_dir(&bin).unwrap().count(),
            1,
            "no temp file left"
        );
    }

    #[tokio::test]
    async fn a_copy_that_does_not_arrive_is_a_failure_to_retry() {
        let tmp = tempfile::tempdir().unwrap();
        // `~/.caucus` is a file, so the bin directory cannot be made.
        std::fs::write(tmp.path().join(".caucus"), "").unwrap();
        let image = script(tmp.path(), "image", "echo id");
        let err = install(&local(tmp.path()), &image).await.unwrap_err();
        assert!(matches!(err, InstallError::Failed(_)), "{err:?}");
    }
}
