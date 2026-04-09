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
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");

    // No cc::Build of ANY kind on Android — all C++ lives in the standalone
    // `wzp-native` crate which is built separately with cargo-ndk and loaded
    // via libloading at runtime. See docs/incident-tauri-android-init-tcb.md
    // for why this split exists.

    tauri_build::build()
}
