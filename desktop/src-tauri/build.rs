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

        // Step D+1: identical-content clone of hello.c as a third cc::Build
        // static library. Kept around as a sanity check: if this C compile
        // suddenly started crashing, we'd know the environment regressed.
        println!("cargo:rerun-if-changed=cpp/hello2.c");
        cc::Build::new()
            .file("cpp/hello2.c")
            .compile("wzp_hello2");

        // ─── minSdkVersion theory test: the original E.1 crashing cpp ──────
        // Re-add the smallest crashing variant (cpp_smoke.cpp with cpp(true)
        // + cpp_link_stdlib("c++_shared")) on top of the working Step D+1
        // baseline. The only additional variable compared to the previous
        // crashing runs is tauri.conf.json bundle.android.minSdkVersion=26,
        // which may make tauri-cli stop hardcoding API 24 in its rustc
        // invocation. If THIS build launches, the minSdkVersion fix is
        // validated and we can proceed with Oboe integration.
        println!("cargo:rerun-if-changed=cpp/cpp_smoke.cpp");
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .cpp_link_stdlib(Some("c++_shared"))
            .file("cpp/cpp_smoke.cpp")
            .compile("wzp_cpp_smoke");

        // Per rust-lang/rust#104707 + the android-ndk advice: force the
        // linker to keep bionic symbols (pthread_create, __init_tcb) as
        // UND dynamic references resolved against libc.so at runtime,
        // not bound locally from libc.a that cc-rs + cpp(true) drag in.
        // llvm-nm confirmed these symbols were landing in our .so as
        // LOCAL (lowercase t), which is exactly the bug.
        println!("cargo:rustc-link-arg=-Wl,--exclude-libs,ALL");
        println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");

        // Copy libc++_shared.so from the NDK sysroot to gen/android jniLibs
        // so the runtime linker can find it at dlopen time (it's now in the
        // .so's NEEDED list thanks to cpp_link_stdlib("c++_shared") above).
        if let Ok(ndk) = std::env::var("ANDROID_NDK_HOME").or_else(|_| std::env::var("NDK_HOME")) {
            let lib_dir = format!(
                "{ndk}/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android"
            );
            println!("cargo:rustc-link-search=native={lib_dir}");
            let shared_so = format!("{lib_dir}/libc++_shared.so");
            if std::path::Path::new(&shared_so).exists() {
                let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
                let jni_dir = format!("{manifest}/gen/android/app/src/main/jniLibs/arm64-v8a");
                if std::fs::create_dir_all(&jni_dir).is_ok() {
                    let _ = std::fs::copy(&shared_so, format!("{jni_dir}/libc++_shared.so"));
                }
            }
        }
    }

    tauri_build::build()
}
