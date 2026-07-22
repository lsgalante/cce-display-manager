// Shared pieces for the cce keyring-unlock binaries (daemon, client, setup).
//
// The scheme: a root daemon holds the only privileged capability — decrypting
// KeePassXC database passwords stored with systemd-creds (TPM-backed where
// available) under /etc/cce/keyring-unlock/<uid>/. A per-session user client
// asks it to unlock over a unix socket once KeePassXC is up; the daemon
// identifies the caller via SO_PEERCRED, verifies the process owning the
// KeePassXC D-Bus name, calls openDatabase, and confirms the Secret Service
// collection actually reports unlocked, retrying until it does. The password
// never crosses the socket.

pub const SOCKET_PATH: &str = "/run/cce-keyring-unlock.sock";
pub const STORE_DIR: &str = "/etc/cce/keyring-unlock";
pub const KEEPASSXC_DBUS_NAME: &str = "org.keepassxc.KeePassXC.MainWindow";
pub const KEEPASSXC_DBUS_PATH: &str = "/keepassxc";
pub const KEEPASSXC_EXE: &str = "/usr/bin/keepassxc";
pub const SECRETS_DBUS_NAME: &str = "org.freedesktop.secrets";
pub const DEFAULT_COLLECTION_PATH: &str = "/org/freedesktop/secrets/aliases/default";

/// One registered database: the .conf file next to its .cred blob.
#[derive(Debug, Clone)]
pub struct DbEntry {
    pub name: String,
    pub database: String,
    pub keyfile: String,
    pub cred_path: std::path::PathBuf,
}

pub fn store_dir_for(uid: u32) -> std::path::PathBuf {
    std::path::Path::new(STORE_DIR).join(uid.to_string())
}

/// Load every <name>.conf under the user's store directory.
pub fn load_entries(uid: u32) -> Vec<DbEntry> {
    let dir = store_dir_for(uid);
    let mut entries = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return entries;
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().map_or(true, |x| x != "conf") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut database = String::new();
        let mut keyfile = String::new();
        let mut cred = String::new();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("database=") {
                database = v.to_string();
            } else if let Some(v) = line.strip_prefix("keyfile=") {
                keyfile = v.to_string();
            } else if let Some(v) = line.strip_prefix("cred=") {
                cred = v.to_string();
            }
        }
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !database.is_empty() && !cred.is_empty() {
            entries.push(DbEntry {
                name,
                database,
                keyfile,
                cred_path: dir.join(cred),
            });
        }
    }
    entries
}

/// Uid and gid of the process at the other end of a unix socket.
pub fn peer_creds(stream: &std::os::unix::net::UnixStream) -> std::io::Result<(u32, u32)> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((cred.uid, cred.gid))
}

/// Best-effort scrub of secret material.
pub fn zeroize(s: &mut String) {
    unsafe {
        for b in s.as_mut_vec().iter_mut() {
            std::ptr::write_volatile(b, 0);
        }
    }
    s.clear();
}

/// True once `name` has an owner on `conn`.
pub fn name_has_owner(conn: &zbus::blocking::Connection, name: &str) -> bool {
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(name,),
    )
    .and_then(|m| Ok(m.body().deserialize::<bool>()?))
    .unwrap_or(false)
}

/// Poll until `name` is owned, up to `timeout`.
pub fn wait_for_name(
    conn: &zbus::blocking::Connection,
    name: &str,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if name_has_owner(conn, name) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// KeePassXC answers a method call => its event loop is dispatching, not
/// still bootstrapping. (The bare name appearing is not enough: openDatabase
/// sent during startup is accepted and lost.)
pub fn keepassxc_answers(conn: &zbus::blocking::Connection) -> bool {
    conn.call_method(
        Some(KEEPASSXC_DBUS_NAME),
        KEEPASSXC_DBUS_PATH,
        Some("org.freedesktop.DBus.Introspectable"),
        "Introspect",
        &(),
    )
    .is_ok()
}

/// Lock state of the default Secret Service collection.
/// Ok(true/false) when readable, Err when the service isn't answering.
pub fn default_collection_locked(
    conn: &zbus::blocking::Connection,
) -> Result<bool, Box<dyn std::error::Error>> {
    let msg = conn.call_method(
        Some(SECRETS_DBUS_NAME),
        DEFAULT_COLLECTION_PATH,
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &("org.freedesktop.Secret.Collection", "Locked"),
    )?;
    let value = msg.body().deserialize::<zbus::zvariant::OwnedValue>()?;
    Ok(bool::try_from(value)?)
}
