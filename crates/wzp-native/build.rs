//! wzp-native build.rs — Oboe C++ bridge compile on Android.
//!
//! Near-verbatim copy of crates/wzp-android/build.rs (which is known to
//! work). The crucial distinction: this crate is a single-cdylib (no
//! staticlib, no rlib in crate-type) so rust-lang/rust#104707 doesn't
//! apply — bionic's internal pthread_create / __init_tcb symbols stay
//! UND and resolve against libc.so at runtime, as they should.
//!
//! On non-Android hosts we compile `cpp/oboe_stub.cpp` (empty stubs) so
//! `cargo check --target <host>` still works for IDEs and CI.

use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.contains("android") {
        // getauxval_fix: override compiler-rt's broken static getauxval
        // stub that SIGSEGVs in shared libraries.
        cc::Build::new()
            .file("cpp/getauxval_fix.c")
            .compile("wzp_native_getauxval_fix");

        let oboe_dir = fetch_oboe();
        match oboe_dir {
            Some(oboe_path) => {
                println!("cargo:warning=wzp-native: building with Oboe from {:?}", oboe_path);
                let mut build = cc::Build::new();
                build
                    .cpp(true)
                    .std("c++17")
                    // Shared libc++ — matches legacy wzp-android setup.
                    .cpp_link_stdlib(Some("c++_shared"))
                    .include("cpp")
                    .include(oboe_path.join("include"))
                    .include(oboe_path.join("src"))
                    .define("WZP_HAS_OBOE", None)
                    .file("cpp/oboe_bridge.cpp");
                add_cpp_files_recursive(&mut build, &oboe_path.join("src"));
                build.compile("wzp_native_oboe_bridge");
            }
            None => {
                println!("cargo:warning=wzp-native: Oboe not found, building stub");
                cc::Build::new()
                    .cpp(true)
                    .std("c++17")
                    .cpp_link_stdlib(Some("c++_shared"))
                    .file("cpp/oboe_stub.cpp")
                    .include("cpp")
                    .compile("wzp_native_oboe_bridge");
            }
        }

        // Oboe needs log + OpenSLES backends at runtime.
        println!("cargo:rustc-link-lib=log");
        println!("cargo:rustc-link-lib=OpenSLES");

        // Re-run if any cpp file changes
        println!("cargo:rerun-if-changed=cpp/oboe_bridge.cpp");
        println!("cargo:rerun-if-changed=cpp/oboe_bridge.h");
        println!("cargo:rerun-if-changed=cpp/oboe_stub.cpp");
        println!("cargo:rerun-if-changed=cpp/getauxval_fix.c");
    } else {
        // Non-Android hosts: compile the empty stub so lib.rs's extern
        // declarations resolve when someone runs `cargo check` on macOS
        // or Linux without an NDK.
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .file("cpp/oboe_stub.cpp")
            .include("cpp")
            .compile("wzp_native_oboe_bridge");
        println!("cargo:rerun-if-changed=cpp/oboe_stub.cpp");
    }
}

/// Recursively add all `.cpp` files from a directory to a cc::Build.
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

/// Fetch or find Oboe headers + sources (v1.8.1). Same logic as the
/// legacy wzp-android crate's build.rs.
fn fetch_oboe() -> Option<PathBuf> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let oboe_dir = out_dir.join("oboe");

    if oboe_dir.join("include").join("oboe").join("Oboe.h").exists() {
        return Some(oboe_dir);
    }

    let status = std::process::Command::new("git")
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
