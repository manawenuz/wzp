use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.contains("android") {
        let oboe_dir = fetch_oboe();
        match oboe_dir {
            Some(oboe_path) => {
                println!("cargo:warning=Building with Oboe from {:?}", oboe_path);

                let mut build = cc::Build::new();
                build
                    .cpp(true)
                    .std("c++17")
                    .cpp_link_stdlib(None)
                    .include("cpp")
                    .include(oboe_path.join("include"))
                    .include(oboe_path.join("src"))
                    .define("WZP_HAS_OBOE", None)
                    .file("cpp/oboe_bridge.cpp");

                // Compile all Oboe source files
                let src_dir = oboe_path.join("src");
                add_cpp_files_recursive(&mut build, &src_dir);

                build.compile("oboe_bridge");
            }
            None => {
                println!("cargo:warning=Oboe not found, building with stub");
                cc::Build::new()
                    .cpp(true)
                    .std("c++17")
                    .cpp_link_stdlib(None)
                    .file("cpp/oboe_stub.cpp")
                    .include("cpp")
                    .compile("oboe_bridge");
            }
        }

        // Android NDK splits the static C++ runtime into two archives:
        //   libc++_static.a  — STL (containers, strings, algorithms)
        //   libc++abi.a      — ABI (RTTI, exceptions, typeinfo vtables)
        // Both are required; cc crate's cpp_link_stdlib only handles the first.
        if let Ok(ndk) = std::env::var("ANDROID_NDK_HOME") {
            let arch = if target.contains("aarch64") {
                "aarch64-linux-android"
            } else if target.contains("armv7") {
                "arm-linux-androideabi"
            } else if target.contains("x86_64") {
                "x86_64-linux-android"
            } else {
                "aarch64-linux-android"
            };
            let lib_dir = format!("{ndk}/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/{arch}");
            println!("cargo:rustc-link-search=native={lib_dir}");
        }
        println!("cargo:rustc-link-lib=static=c++_static");
        println!("cargo:rustc-link-lib=static=c++abi");

        // Oboe needs liblog and libOpenSLES from Android
        println!("cargo:rustc-link-lib=log");
        println!("cargo:rustc-link-lib=OpenSLES");
    } else {
        // Non-Android: always use stub
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .file("cpp/oboe_stub.cpp")
            .include("cpp")
            .compile("oboe_bridge");
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

/// Try to find or fetch Oboe headers.
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
        Ok(s) if s.success() => {
            if oboe_dir.join("include").join("oboe").join("Oboe.h").exists() {
                Some(oboe_dir)
            } else {
                None
            }
        }
        _ => None,
    }
}
