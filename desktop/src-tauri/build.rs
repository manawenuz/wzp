use std::process::Command;

fn main() {
    // ─── Embedded git hash ─────────────────────────────────────────────────
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

    // ─── Step A: single trivial cpp/hello.c compiled via cc::Build ─────────
    // We deliberately add this on Android only so we can verify that the
    // cc::Build → static archive → rustc-link pipeline itself does not
    // regress the working build #17. cpp/hello.c defines `wzp_hello_stub`
    // which is never called from Rust — if the crash comes back just from
    // adding a tiny C static lib, we know the build pipeline is the issue.
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("android") {
        println!("cargo:rerun-if-changed=cpp/hello.c");
        cc::Build::new()
            .file("cpp/hello.c")
            .compile("wzp_hello");
    }

    tauri_build::build()
}
