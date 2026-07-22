// cce-keyring-unlock — per-session client that asks cce-keyring-unlockd to
// unlock this user's KeePassXC database(s).
//
// Runs as a oneshot user service in graphical-session.target. Waits for
// KeePassXC to appear on the session bus (the compositor's session restore
// launches it), then pings the root daemon over its socket. All secret
// handling stays in the daemon.

#[path = "../keyring.rs"]
#[allow(dead_code)]
mod keyring;

use keyring::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

// Session restore can be slow on a busy login; be patient before concluding
// KeePassXC just isn't part of this session.
const APPEAR_WAIT: Duration = Duration::from_secs(120);
const REQUEST_ATTEMPTS: u32 = 3;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let conn = match session_bus() {
        Ok(c) => c,
        Err(e) => {
            log::error!("cannot connect to session bus: {e}");
            std::process::exit(1);
        }
    };

    if !wait_for_name(&conn, KEEPASSXC_DBUS_NAME, APPEAR_WAIT) {
        log::info!("KeePassXC did not appear within {APPEAR_WAIT:?}; nothing to unlock");
        return;
    }
    match default_collection_locked(&conn) {
        Ok(false) => {
            log::info!("database already unlocked");
            return;
        }
        _ => {}
    }

    for attempt in 1..=REQUEST_ATTEMPTS {
        match request_unlock() {
            Ok(reply) if reply == "ok" => {
                log::info!("daemon confirmed unlock");
                dismiss_orphaned_unlock_dialog();
                return;
            }
            Ok(reply) => log::warn!("attempt {attempt}: daemon said: {reply}"),
            Err(e) => log::warn!("attempt {attempt}: {e}"),
        }
        std::thread::sleep(Duration::from_secs(4));
    }
    log::error!("giving up after {REQUEST_ATTEMPTS} attempts");
    std::process::exit(1);
}

/// Apps that hit the Secret Service during the first seconds of login make
/// KeePassXC pop its standalone "Unlock Database" prompt; when the database
/// is then unlocked over D-Bus that dialog is orphaned and never dismisses
/// itself (keepassxc#9297). Ask the compositor to close it. Best effort —
/// outside a cce session there is nothing to do.
fn dismiss_orphaned_unlock_dialog() {
    let Ok(display) = std::env::var("WAYLAND_DISPLAY") else {
        return;
    };
    let sock = format!("/tmp/cce-{display}.sock");
    // The dialog may still be mid-spawn right after the unlock; check twice.
    for wait in [2, 5] {
        std::thread::sleep(Duration::from_secs(wait));
        let Ok(stream) = UnixStream::connect(&sock) else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
        if writeln!(&stream, "close-window org.keepassxc.KeePassXC unlock database").is_err() {
            return;
        }
        let mut reply = String::new();
        let _ = BufReader::new(stream).read_line(&mut reply);
        if reply.starts_with("ok") {
            log::info!("closed orphaned KeePassXC unlock dialog");
            return;
        }
    }
}

fn session_bus() -> Result<zbus::blocking::Connection, Box<dyn std::error::Error>> {
    if let Ok(c) = zbus::blocking::Connection::session() {
        return Ok(c);
    }
    // User units usually have DBUS_SESSION_BUS_ADDRESS set, but fall back to
    // the standard per-user bus path if not.
    let uid = unsafe { libc::getuid() };
    let addr = format!("unix:path=/run/user/{uid}/bus");
    Ok(zbus::blocking::connection::Builder::address(addr.as_str())?.build()?)
}

fn request_unlock() -> Result<String, Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(SOCKET_PATH)
        .map_err(|e| format!("cannot reach {SOCKET_PATH} (is cce-keyring-unlockd running?): {e}"))?;
    // The daemon itself waits for KeePassXC readiness and verifies the
    // unlock, so give it a generous window before assuming it's wedged.
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    writeln!(&stream, "unlock")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply.trim().to_string())
}
