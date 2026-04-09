use std::path::PathBuf;
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

    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("android") {
        build_android_native();
    }

    tauri_build::build()
}

fn build_android_native() {
    // ─── Step A: cpp/hello.c sanity static lib ─────────────────────────────
    println!("cargo:rerun-if-changed=cpp/hello.c");
    cc::Build::new()
        .file("cpp/hello.c")
        .compile("wzp_hello");

    // ─── Step D: getauxval_fix shim ────────────────────────────────────────
    println!("cargo:rerun-if-changed=cpp/getauxval_fix.c");
    cc::Build::new()
        .file("cpp/getauxval_fix.c")
        .compile("getauxval_fix");

    // ─── Step E.1 with STATIC libc++ ───────────────────────────────────────
    // Every cpp_smoke variant (atomic-mutex-thread / atomic-only / empty
    // function) crashed identically with c++_shared linkage — byte-
    // identical crash offsets. The cpp code was dead-stripped from the
    // final .so in every case, so the only remaining delta was the
    // NEEDED entry for libc++_shared.so added by
    // cargo:rustc-link-lib=c++_shared. Theory: that NEEDED entry (and
    // Android's dynamic linker running libc++_shared.so's init_array at
    // dlopen time) is the trigger.
    //
    // Test: swap c++_shared → c++_static. Bundles libc++ code directly
    // into our .so, drops the NEEDED entry. If the app launches, we've
    // proven the NEEDED libc++_shared.so is the trigger and have a
    // working linkage for adding C++ to Tauri Android cdylibs.
    println!("cargo:rerun-if-changed=cpp/cpp_smoke.cpp");
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .cpp_link_stdlib(Some("c++_static"))
        .file("cpp/cpp_smoke.cpp")
        .compile("wzp_cpp_smoke");

    // Copy libc++_shared.so next to libwzp_desktop_lib.so in the Tauri
    // jniLibs directory so the dynamic linker can resolve it at runtime.
    if let Ok(ndk) = std::env::var("ANDROID_NDK_HOME")
        .or_else(|_| std::env::var("NDK_HOME"))
    {
        let (triple, abi) = match target_os_abi() {
            Some(v) => v,
            None => ("aarch64-linux-android", "arm64-v8a"),
        };
        let lib_dir = format!(
            "{ndk}/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/{triple}"
        );
        println!("cargo:rustc-link-search=native={lib_dir}");

        let shared_so = format!("{lib_dir}/libc++_shared.so");
        if std::path::Path::new(&shared_so).exists() {
            let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
            let jni_dir = format!("{manifest}/gen/android/app/src/main/jniLibs/{abi}");
            if std::fs::create_dir_all(&jni_dir).is_ok() {
                let _ = std::fs::copy(&shared_so, format!("{jni_dir}/libc++_shared.so"));
                println!("cargo:warning=Copied libc++_shared.so to {jni_dir}");
            }
        }
    }

    // Step E.4 drops the Oboe-specific -llog / -lOpenSLES link requirements
    // since cpp_smoke.cpp doesn't call into Android's logging or audio HAL.
    // Keep libc++_shared.so in jniLibs (copied above) because the smoke
    // file still dynamically links against libc++.
}

fn target_os_abi() -> Option<(&'static str, &'static str)> {
    let target = std::env::var("TARGET").ok()?;
    if target.contains("aarch64") {
        Some(("aarch64-linux-android", "arm64-v8a"))
    } else if target.contains("armv7") {
        Some(("arm-linux-androideabi", "armeabi-v7a"))
    } else if target.contains("x86_64") {
        Some(("x86_64-linux-android", "x86_64"))
    } else if target.contains("i686") {
        Some(("i686-linux-android", "x86"))
    } else {
        None
    }
}

/// Recursively add all .cpp files from a directory to a cc::Build.
#[allow(dead_code)] // re-enabled when Step E.x restores the full Oboe compile
fn add_cpp_files_recursive(build: &mut cc::Build, dir: &std::path::Path) {
    if !dir.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            add_cpp_files_recursive(build, &path);
        } else if path.extension().map_or(false, |e| e == "cpp") {
            build.file(&path);
        }
    }
}

/// Try to find or fetch Oboe headers + source (v1.8.1).
#[allow(dead_code)] // re-enabled when Step E.x restores the full Oboe compile
fn fetch_oboe() -> Option<PathBuf> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let oboe_dir = out_dir.join("oboe");

    if oboe_dir.join("include").join("oboe").join("Oboe.h").exists() {
        return Some(oboe_dir);
    }

    let status = Command::new("git")
        .args([
            "clone",
            "--depth=1",
            "--branch=1.8.1",
            "https://github.com/google/oboe.git",
            oboe_dir.to_str().unwrap(),
        ])
        .status();

    match status {
        Ok(s) if s.success() && oboe_dir.join("include").join("oboe").join("Oboe.h").exists() => {
            Some(oboe_dir)
        }
        _ => None,
    }
}
