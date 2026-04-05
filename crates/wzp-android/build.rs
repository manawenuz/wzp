use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.contains("android") {
        // Compile a getauxval override FIRST so it takes precedence over the
        // broken static stub from compiler-rt/CRT that crashes in shared libs.
        cc::Build::new()
            .file("cpp/getauxval_fix.c")
            .compile("getauxval_fix");

        let oboe_dir = fetch_oboe();
        match oboe_dir {
            Some(oboe_path) => {
                println!("cargo:warning=Building with Oboe from {:?}", oboe_path);

                let mut build = cc::Build::new();
                build
                    .cpp(true)
                    .std("c++17")
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

        // Use libc++_shared.so (dynamic) instead of static linking.
        //
        // Static libc++ pulls in getauxval.o from libc.a, which has a
        // stub that reads from __libc_auxv (only initialized for executables,
        // not shared libs). This causes SIGSEGV when ring/cpufeatures calls
        // getauxval at load time. Using the shared library avoids this.
        //
        // libc++_shared.so must be bundled in the APK alongside libwzp_android.so.
        // build.rs copies it to jniLibs/ automatically.
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

            // Copy libc++_shared.so next to libwzp_android.so
            let shared_so = format!("{lib_dir}/libc++_shared.so");
            let shared_path = std::path::Path::new(&shared_so);
            if shared_path.exists() {
                // Output to the jniLibs directory (one level up from OUT_DIR)
                let out_dir = std::env::var("OUT_DIR").unwrap();
                // Also copy to a known location that Gradle cargoNdkBuild uses
                let jni_dir = format!("{}/../../../jniLibs/arm64-v8a", out_dir);
                let _ = std::fs::create_dir_all(&jni_dir);
                let _ = std::fs::copy(shared_path, format!("{jni_dir}/libc++_shared.so"));
                println!("cargo:warning=Copied libc++_shared.so to jniLibs");
            }
        }
        println!("cargo:rustc-link-lib=c++_shared");

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
