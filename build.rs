use std::process::Command;

fn main() {
    // Capture the build date so the interface can show which build is running.
    // With no rerun-if-changed pin, Cargo re-runs this whenever the crate's
    // sources change, so CCE_BUILD_DATE tracks the last actual recompile.
    let date = Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CCE_BUILD_DATE={}", date);
}
