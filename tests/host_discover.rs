//! An unbound hook (`caucus signal post --discover`) reaches this machine's
//! host daemon, whose signal socket is not in the temp dir it scans.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use caucus::host::HostPaths;

#[test]
fn an_unbound_hook_posts_to_the_host_daemon_signal_socket() {
    let home = tempfile::tempdir().unwrap();
    // An empty temp dir, so no live session on this machine gets the post.
    let tmp = tempfile::tempdir().unwrap();
    let paths =
        HostPaths::resolve(Some(HostPaths::default_state_dir(home.path())), None, None).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let listener = UnixListener::bind(&paths.signal_sock).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut hook = Command::new(env!("CARGO_BIN_EXE_caucus"))
        .args(["signal", "post", "--discover"])
        .env("HOME", home.path())
        .env("TMPDIR", tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"conv-x"}"#)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if let Some(status) = hook.try_wait().unwrap() {
                    panic!("the hook exited ({status}) without posting to the host daemon");
                }
                assert!(Instant::now() < deadline, "the hook never posted");
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("accept: {err}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    assert!(line.contains("conv-x"), "{line}");
    // No reply line means allow, which lets the hook exit.
    drop(stream);
    assert!(hook.wait().unwrap().success());
}
