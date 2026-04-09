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
    // ─── Step D: also compile getauxval_fix.c (legacy wzp-android shim) ────
    // getauxval_fix.c overrides the broken static getauxval stub that
    // compiler-rt pulls in for Android targets. It's been shipping in the
    // legacy wzp-android .so for months without issue, so including it here
    // is low-risk — but it's an incremental variable we want to isolate.
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("android") {
        println!("cargo:rerun-if-changed=cpp/hello.c");
        cc::Build::new()
            .file("cpp/hello.c")
            .compile("wzp_hello");

        println!("cargo:rerun-if-changed=cpp/getauxval_fix.c");
        cc::Build::new()
            .file("cpp/getauxval_fix.c")
            .compile("getauxval_fix");
    }

    tauri_build::build()
}
