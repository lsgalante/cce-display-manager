// cce-keyring-unlock-setup — register a KeePassXC database for automatic
// unlock at login by cce-keyring-unlockd.
//
//   sudo cce-keyring-unlock-setup <user> <database.kdbx> [keyfile]
//
// Prompts for the database password (twice) and stores it encrypted with
// systemd-creds (TPM-backed where available) under
// /etc/cce/keyring-unlock/<uid>/, readable by root only.

#[path = "../keyring.rs"]
#[allow(dead_code)]
mod keyring;

use keyring::*;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("must run as root (sudo)".into());
    }
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        return Err(format!("usage: {} <user> <database.kdbx> [keyfile]", args[0]).into());
    }
    let user = users::get_user_by_name(&args[1])
        .ok_or_else(|| format!("unknown user '{}'", args[1]))?;
    let uid = user.uid();
    let database = std::fs::canonicalize(&args[2])
        .map_err(|e| format!("database '{}': {e}", args[2]))?;
    if !database.is_file() {
        return Err(format!("'{}' is not a file", database.display()).into());
    }
    let keyfile = if args.len() == 4 {
        std::fs::canonicalize(&args[3])
            .map_err(|e| format!("keyfile '{}': {e}", args[3]))?
            .display()
            .to_string()
    } else {
        String::new()
    };

    let name: String = database
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "database".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();

    let mut password = ask_password(&format!("Password for {}: ", database.display()))?;
    let mut confirm = ask_password("Type the password again: ")?;
    if password != confirm {
        zeroize(&mut password);
        zeroize(&mut confirm);
        return Err("passwords do not match".into());
    }
    zeroize(&mut confirm);

    let dir = store_dir_for(uid);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(STORE_DIR, std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;

    let cred_file = dir.join(format!("{name}.cred"));
    encrypt_password(&name, &password, &cred_file)?;
    zeroize(&mut password);

    let conf_file = dir.join(format!("{name}.conf"));
    std::fs::write(
        &conf_file,
        format!(
            "database={}\nkeyfile={}\ncred={name}.cred\n",
            database.display(),
            keyfile
        ),
    )?;
    std::fs::set_permissions(&conf_file, std::fs::Permissions::from_mode(0o600))?;
    std::fs::set_permissions(&cred_file, std::fs::Permissions::from_mode(0o600))?;

    println!(
        "registered {} for uid {uid}; cce-keyring-unlockd will unlock it at login",
        database.display()
    );
    Ok(())
}

/// Prompt on the controlling terminal with echo off.
fn ask_password(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::io::AsRawFd;
    let mut tty_out = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    let tty_in = std::fs::OpenOptions::new().read(true).open("/dev/tty")?;
    write!(tty_out, "{prompt}")?;
    tty_out.flush()?;

    let fd = tty_in.as_raw_fd();
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let saved = termios;
    termios.c_lflag &= !libc::ECHO;
    termios.c_lflag |= libc::ICANON;
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &termios) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut line = String::new();
    let read_result = BufReader::new(&tty_in).read_line(&mut line);
    unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &saved) };
    let _ = writeln!(tty_out);
    read_result?;

    let s = line.trim_end_matches(['\n', '\r']).to_string();
    if s.is_empty() {
        return Err("empty password".into());
    }
    Ok(s)
}

fn encrypt_password(
    name: &str,
    password: &str,
    cred_file: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut child = Command::new("systemd-creds")
        .arg("encrypt")
        .arg(format!("--name={name}"))
        .arg("-")
        .arg(cred_file)
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("no stdin for systemd-creds")?
        .write_all(password.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        return Err("systemd-creds encrypt failed".into());
    }
    Ok(())
}
