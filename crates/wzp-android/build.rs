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
                    // Use shared libc++ — avoids pulling in static libc stubs
                    // that crash in shared libraries (getauxval, pthread_create, etc.)
                    .cpp_link_stdlib(Some("c++_shared"))
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
                    .cpp_link_stdlib(Some("c++_shared"))
                    .file("cpp/oboe_stub.cpp")
                    .include("cpp")
                    .compile("oboe_bridge");
            }
        }

        // Dynamic C++ runtime — libc++_shared.so must be in jniLibs alongside
        // libwzp_android.so. We copy it there from the NDK sysroot.
        //
        // WHY NOT STATIC: libc++_static.a + libc++abi.a transitively pull in
        // object files from libc.a (static libc) which contain broken stubs for
        // getauxval, __init_tcb, pthread_create, etc. These stubs only work in
        // statically-linked executables. In shared libraries loaded by dlopen(),
        // they SIGSEGV because the static libc init hasn't run.
        // Google's official recommendation: use libc++_shared.so for native libs.
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
            let lib_dir = format!(
                "{ndk}/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/{arch}"
            );
            println!("cargo:rustc-link-search=native={lib_dir}");

            // Copy libc++_shared.so to the jniLibs directory
            let shared_so = format!("{lib_dir}/libc++_shared.so");
            if std::path::Path::new(&shared_so).exists() {
                let jni_abi = if target.contains("aarch64") {
                    "arm64-v8a"
                } else if target.contains("armv7") {
                    "armeabi-v7a"
                } else {
                    "arm64-v8a"
                };
                // Try to copy to the Gradle jniLibs directory
                let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
                let jni_dir = format!(
                    "{manifest}/../../android/app/src/main/jniLibs/{jni_abi}"
                );
                if let Ok(_) = std::fs::create_dir_all(&jni_dir) {
                    let _ = std::fs::copy(&shared_so, format!("{jni_dir}/libc++_shared.so"));
                    println!("cargo:warning=Copied libc++_shared.so to {jni_dir}");
                }
            }
        }

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

/// Try to find or fetch Oboe headers + source.
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
