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

    // ─── Step E: full Oboe C++ bridge ──────────────────────────────────────
    // Clones google/oboe@1.8.1 into OUT_DIR and compiles the bridge + all
    // Oboe source files as a single static library. NOT yet called from
    // Rust — this step only verifies that the C++ compile + link path
    // doesn't regress the known-good build. Same approach as the legacy
    // crates/wzp-android/build.rs, copied verbatim below.
    println!("cargo:rerun-if-changed=cpp/oboe_bridge.cpp");
    println!("cargo:rerun-if-changed=cpp/oboe_bridge.h");
    println!("cargo:rerun-if-changed=cpp/oboe_stub.cpp");

    match fetch_oboe() {
        Some(oboe_path) => {
            println!("cargo:warning=Building with Oboe from {:?}", oboe_path);

            let mut build = cc::Build::new();
            build
                .cpp(true)
                .std("c++17")
                // Shared libc++ — static pulls broken libc stubs that crash
                // in .so libraries (getauxval, __init_tcb, pthread_create).
                // Google's official NDK guidance.
                .cpp_link_stdlib(Some("c++_shared"))
                .include("cpp")
                .include(oboe_path.join("include"))
                .include(oboe_path.join("src"))
                .define("WZP_HAS_OBOE", None)
                .file("cpp/oboe_bridge.cpp");

            add_cpp_files_recursive(&mut build, &oboe_path.join("src"));
            build.compile("oboe_bridge");
        }
        None => {
            println!("cargo:warning=Oboe not found, building with stub");
            cc::Build::new()
                .cpp(true)
                .std("c++17")
                .cpp_link_stdlib(Some("c++_shared"))
                .file("cpp/oboe_stub.cpp")
                .include("cpp")
                .compile("oboe_bridge");
        }
    }

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

    // Oboe requires Android log + OpenSLES backends
    println!("cargo:rustc-link-lib=log");
    println!("cargo:rustc-link-lib=OpenSLES");
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
