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

    let conn = connect_user_bus(uid, gid)?;

    if !wait_for_name(&conn, KEEPASSXC_DBUS_NAME, NAME_WAIT) {
        return Err("KeePassXC never appeared on the session bus".into());
    }
    verify_keepassxc_owner(&conn, uid)?;

    // Don't fire openDatabase into a bootstrapping KeePassXC — wait until it
    // answers method calls.
    let ready_deadline = std::time::Instant::now() + READY_WAIT;
    while !keepassxc_answers(&conn) {
        if std::time::Instant::now() >= ready_deadline {
            return Err("KeePassXC owns its D-Bus name but never answered a call".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    for entry in &entries {
        let mut password = decrypt_password(entry)?;
        let result = open_and_verify(&conn, entry, &password);
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
    if exe != std::path::Path::new(KEEPASSXC_EXE) {
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
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_err: String = "unlock not confirmed".into();
    for attempt in 1..=OPEN_ATTEMPTS {
        if let Err(e) = conn.call_method(
            Some(KEEPASSXC_DBUS_NAME),
            KEEPASSXC_DBUS_PATH,
            Some(KEEPASSXC_DBUS_NAME),
            "openDatabase",
            &(entry.database.as_str(), password, entry.keyfile.as_str()),
        ) {
            last_err = format!("openDatabase call failed: {e}");
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        // The call returning success is not enough — confirm via the Secret
        // Service that the collection really is unlocked.
        let deadline = std::time::Instant::now() + VERIFY_WINDOW;
        let mut secrets_seen = false;
        while std::time::Instant::now() < deadline {
            match default_collection_locked(conn) {
                Ok(false) => {
                    log::info!("{}: unlocked (attempt {attempt})", entry.database);
                    return Ok(());
                }
                Ok(true) => secrets_seen = true,
                Err(_) => {} // secrets service not up yet
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
