use std::process::Command;

fn main() {
    // Capture short git hash so the running app can prove which build it is.
    // Falls back to "unknown" if git isn't available (e.g. when building from
    // a tarball without a .git dir).
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=WZP_GIT_HASH={git_hash}");
    // Re-run if the HEAD pointer or its target moves so the embedded hash
    // tracks reality between builds.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");

    tauri_build::build()
}
