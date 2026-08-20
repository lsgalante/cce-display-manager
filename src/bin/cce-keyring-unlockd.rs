// cce-keyring-unlockd — root daemon that unlocks a user's KeePassXC database
// on request from that user's session.
//
// Listens on /run/cce-keyring-unlock.sock. A client sends "unlock\n"; the
// daemon resolves the caller's uid via SO_PEERCRED, decrypts the passwords
// registered for that uid (systemd-creds, see cce-keyring-unlock-setup),
// verifies the process owning the KeePassXC D-Bus name really is
// /usr/bin/keepassxc belonging to that uid, calls openDatabase, and then
// polls the Secret Service collection until it reports unlocked — retrying
// the openDatabase call if the unlock was swallowed (which happens when
// KeePassXC is still starting up). Replies "ok" or "error: <reason>".

#[path = "../keyring.rs"]
#[allow(dead_code)]
mod keyring;

use keyring::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

const OPEN_ATTEMPTS: u32 = 4;
const VERIFY_WINDOW: Duration = Duration::from_secs(12);
const NAME_WAIT: Duration = Duration::from_secs(30);
const READY_WAIT: Duration = Duration::from_secs(20);
/// Ceiling on one request, kept under the client's 120 s read timeout so the
/// client hears a verdict instead of timing out and retrying into a daemon
/// that is still working on the previous attempt.
const REQUEST_BUDGET: Duration = Duration::from_secs(100);
/// Per-call ceilings. No D-Bus call here is safe to wait on forever: see
/// [`with_timeout`].
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const PROP_TIMEOUT: Duration = Duration::from_secs(5);

/// Run one D-Bus call on a helper thread and give up on it after `timeout`.
///
/// `openDatabase` does not return while KeePassXC is showing its own modal
/// unlock prompt, and a suspend inside that window stretches the call across
/// the entire sleep — one such call once blocked this daemon for nine and a
/// half hours, and every later request with it. Threading whole requests is
/// not an option (`connect_user_bus` swaps euid, which is process-wide on
/// Linux), so each call is bounded instead. A timed-out helper is abandoned
/// rather than killed: it owns its own connection and exits if the call ever
/// returns.
fn with_timeout<T: Send + 'static>(
    label: &str,
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout)
        .map_err(|_| format!("{label} did not return within {timeout:?}"))
}

/// Remaining slice of a deadline, or None once it has passed.
fn remaining(deadline: std::time::Instant) -> Option<Duration> {
    deadline.checked_duration_since(std::time::Instant::now())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("cce-keyring-unlockd must run as root");
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener = match UnixListener::bind(SOCKET_PATH) {
        Ok(l) => l,
        Err(e) => {
            log::error!("cannot bind {SOCKET_PATH}: {e}");
            std::process::exit(1);
        }
    };
    // Any local user may connect; the daemon only ever acts on the caller's
    // own registered databases, and no secret material crosses the socket.
    if let Err(e) = std::fs::set_permissions(
        SOCKET_PATH,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    ) {
        log::error!("cannot chmod {SOCKET_PATH}: {e}");
        std::process::exit(1);
    }
    log::info!("listening on {SOCKET_PATH}");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = handle_client(s) {
                    log::warn!("request failed: {e}");
                }
            }
            Err(e) => log::warn!("accept failed: {e}"),
        }
    }
}

fn handle_client(stream: UnixStream) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (uid, gid) = peer_creds(&stream)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim() != "unlock" {
        reply(&stream, "error: unknown request");
        return Ok(());
    }
    log::info!("unlock request from uid {uid}");
    match unlock_for(uid, gid) {
        Ok(()) => {
            log::info!("uid {uid}: database(s) unlocked and verified");
            reply(&stream, "ok");
        }
        Err(e) => {
            log::warn!("uid {uid}: unlock failed: {e}");
            reply(&stream, &format!("error: {e}"));
        }
    }
    Ok(())
}

fn reply(mut stream: &UnixStream, msg: &str) {
    let _ = writeln!(stream, "{msg}");
}

fn unlock_for(uid: u32, gid: u32) -> Result<(), Box<dyn std::error::Error>> {
    let entries = load_entries(uid);
    if entries.is_empty() {
        return Err(format!("no databases registered for uid {uid} (run cce-keyring-unlock-setup)").into());
    }
    let budget = std::time::Instant::now() + REQUEST_BUDGET;

    let conn = connect_user_bus(uid, gid)?;

    let name_wait = remaining(budget).unwrap_or_default().min(NAME_WAIT);
    if !wait_for_name(&conn, KEEPASSXC_DBUS_NAME, name_wait) {
        return Err("KeePassXC never appeared on the session bus".into());
    }
    verify_keepassxc_owner(&conn, uid)?;

    // Don't fire openDatabase into a bootstrapping KeePassXC — wait until it
    // answers method calls.
    let ready_deadline = (std::time::Instant::now() + READY_WAIT).min(budget);
    while !keepassxc_answers(&conn) {
        if std::time::Instant::now() >= ready_deadline {
            return Err("KeePassXC owns its D-Bus name but never answered a call".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    for entry in &entries {
        let mut password = decrypt_password(entry)?;
        let result = open_and_verify(&conn, entry, &password, budget);
        zeroize(&mut password);
        result?;
    }
    Ok(())
}

/// The per-user bus refuses connections whose peer credentials aren't the
/// owning user, so swap effective uid/gid to the requester just for the
/// handshake. Once the socket is authenticated it keeps working after we
/// return to root (which systemd-creds decryption requires). The daemon is
/// single-threaded, so the swap can't leak into another request.
fn connect_user_bus(
    uid: u32,
    gid: u32,
) -> Result<zbus::blocking::Connection, Box<dyn std::error::Error>> {
    let addr = format!("unix:path=/run/user/{uid}/bus");
    if unsafe { libc::setegid(gid) } != 0 || unsafe { libc::seteuid(uid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = zbus::blocking::connection::Builder::address(addr.as_str())
        .and_then(|b| b.build());
    if unsafe { libc::seteuid(0) } != 0 || unsafe { libc::setegid(0) } != 0 {
        // Refuse to keep running with dropped privileges in an odd state.
        log::error!("cannot restore root euid/egid: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }
    Ok(result?)
}

/// The process owning the KeePassXC bus name must be the real keepassxc
/// binary, running as the requesting user — never hand the password to an
/// impostor that grabbed the name.
fn verify_keepassxc_owner(
    conn: &zbus::blocking::Connection,
    uid: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let pid: u32 = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(KEEPASSXC_DBUS_NAME,),
        )?
        .body()
        .deserialize()?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))?;
    // A package upgrade unlinks the running binary and the kernel reports
    // "/usr/bin/keepassxc (deleted)" — still the real KeePassXC, and refusing
    // it would break auto-unlock until the app restarts. Strip the marker.
    let exe_str = exe.to_string_lossy();
    let exe_path = exe_str.strip_suffix(" (deleted)").unwrap_or(&exe_str);
    if std::path::Path::new(exe_path) != std::path::Path::new(KEEPASSXC_EXE) {
        return Err(format!("bus name owned by {} (pid {pid}), not {KEEPASSXC_EXE}", exe.display()).into());
    }
    let meta = std::fs::metadata(format!("/proc/{pid}"))?;
    let owner = std::os::unix::fs::MetadataExt::uid(&meta);
    if owner != uid {
        return Err(format!("keepassxc pid {pid} belongs to uid {owner}, expected {uid}").into());
    }
    Ok(())
}

fn decrypt_password(entry: &DbEntry) -> Result<String, Box<dyn std::error::Error>> {
    let out = std::process::Command::new("systemd-creds")
        .arg("decrypt")
        .arg(format!("--name={}", entry.name))
        .arg(&entry.cred_path)
        .arg("-")
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "systemd-creds decrypt failed for {}: {}",
            entry.name,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn open_and_verify(
    conn: &zbus::blocking::Connection,
    entry: &DbEntry,
    password: &str,
    budget: std::time::Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_err: String = "unlock not confirmed".into();
    for attempt in 1..=OPEN_ATTEMPTS {
        if remaining(budget).is_none() {
            break;
        }
        let call = {
            let conn = conn.clone();
            let database = entry.database.clone();
            let keyfile = entry.keyfile.clone();
            let mut password = password.to_string();
            with_timeout("openDatabase", OPEN_TIMEOUT, move || {
                let r = conn
                    .call_method(
                        Some(KEEPASSXC_DBUS_NAME),
                        KEEPASSXC_DBUS_PATH,
                        Some(KEEPASSXC_DBUS_NAME),
                        "openDatabase",
                        &(database.as_str(), password.as_str(), keyfile.as_str()),
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                zeroize(&mut password);
                r
            })
        };
        match call {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                last_err = format!("openDatabase call failed: {e}");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            Err(e) => {
                // KeePassXC is most likely sitting on its own modal prompt.
                last_err = e;
                log::warn!("{}: {last_err}", entry.database);
                continue;
            }
        }
        // The call returning success is not enough — confirm via the Secret
        // Service that the collection really is unlocked.
        let deadline = (std::time::Instant::now() + VERIFY_WINDOW).min(budget);
        let mut secrets_seen = false;
        while std::time::Instant::now() < deadline {
            let locked = {
                let conn = conn.clone();
                with_timeout("Locked property read", PROP_TIMEOUT, move || {
                    default_collection_locked(&conn).map_err(|e| e.to_string())
                })
            };
            match locked {
                Ok(Ok(false)) => {
                    log::info!("{}: unlocked (attempt {attempt})", entry.database);
                    return Ok(());
                }
                Ok(Ok(true)) => secrets_seen = true,
                Ok(Err(_)) => {}  // secrets service not up yet
                Err(e) => log::warn!("{}: {e}", entry.database),
            }
            std::thread::sleep(Duration::from_millis(750));
        }
        if !secrets_seen {
            // FdoSecrets never answered; can't verify. Trust the call rather
            // than hammer retries against a database we cannot observe.
            log::warn!(
                "{}: openDatabase sent but Secret Service unavailable; assuming unlocked",
                entry.database
            );
            return Ok(());
        }
        last_err = format!("still locked {VERIFY_WINDOW:?} after openDatabase (attempt {attempt})");
        log::warn!("{}: {last_err}", entry.database);
    }
    Err(last_err.into())
}
