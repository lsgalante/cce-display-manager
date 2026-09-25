//! The session worker is a separate process now (`--session-worker`), run by
//! the root daemon with the password on its stdin. These run the real binary
//! as the (non-root) test user and check the two ways it must refuse before
//! it goes anywhere near PAM.

use std::io::Write;
use std::process::{Command, Stdio};

fn worker(args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cce-display-manager"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the binary");
    // A worker that refuses early may not read this; ignore EPIPE.
    let _ = child.stdin.take().unwrap().write_all(b"not-a-password");
    child.wait_with_output().expect("wait")
}

/// Anyone can run the installed binary. As a user it must not open a PAM
/// session or report one — and it checks before reading the password.
#[test]
fn a_non_root_session_worker_refuses() {
    if unsafe { libc_getuid() } == 0 {
        eprintln!("running as root; the refusal is not what this run can test");
        return;
    }
    let out = worker(&["--session-worker", "tty1", "nobody", "true", "bash"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "no SESSION_ID line: {:?}", String::from_utf8_lossy(&out.stdout));
}

/// A malformed worker argv is an error, not a fall-through into the daemon
/// (which, as root, would start a second greeter on the tty).
#[test]
fn a_malformed_session_worker_argv_is_an_error() {
    let out = worker(&["--session-worker", "tty1"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}
