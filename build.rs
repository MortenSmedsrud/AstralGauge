//! Keeps the embedded third-party license file (`licenses.txt`, shown in
//! the desktop About dialog) fresh: when a crate is added, removed or
//! updated (Cargo.lock changes) this re-runs and regenerates the file via
//! `licenses.sh` before compiling. The staleness rule lives in
//! `licenses.sh --check` (also usable directly in CI or a pre-commit hook).
//!
//! A failed regeneration only warns: the checked-in file stays in place,
//! so a build never fails just because the license text is out of date
//! (e.g. cargo-about is not installed).

fn main() {
    for input in ["Cargo.lock", "about.toml", "about-text.hbs"] {
        println!("cargo:rerun-if-changed={input}");
    }

    // Still up to date: nothing to do.
    if let Ok(out) = std::process::Command::new("sh")
        .args(["licenses.sh", "--check"])
        .output()
    {
        if out.status.success() {
            return;
        }
    }

    // Stale: regenerate with the same script (needs cargo-about, which
    // itself only runs the read-only `cargo metadata`).
    match std::process::Command::new("sh").arg("licenses.sh").output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            println!("cargo:warning=licenses.txt is stale and could not be regenerated:\n{stderr}");
        }
        Err(e) => {
            println!("cargo:warning=licenses.txt is stale and could not be regenerated: {e}");
        }
    }
}
