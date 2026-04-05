use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.contains("android") {
        // On Android, try to build with Oboe. If Oboe is not available,
        // fall back to the stub (audio will need to be provided via JNI).
        let oboe_dir = fetch_oboe();
        match oboe_dir {
            Some(oboe_path) => {
                println!("cargo:warning=Building with Oboe from {:?}", oboe_path);
                cc::Build::new()
                    .cpp(true)
                    .std("c++17")
                    .cpp_link_stdlib(Some("c++_static"))
                    .file("cpp/oboe_bridge.cpp")
                    .include("cpp")
                    .include(oboe_path.join("include"))
                    .define("WZP_HAS_OBOE", None)
                    .compile("oboe_bridge");
            }
            None => {
                println!("cargo:warning=Oboe not found, building with stub");
                cc::Build::new()
                    .cpp(true)
                    .std("c++17")
                    .cpp_link_stdlib(Some("c++_static"))
                    .file("cpp/oboe_stub.cpp")
                    .include("cpp")
                    .compile("oboe_bridge");
            }
        }
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

/// Try to find or fetch Oboe headers.
/// Returns the path to the Oboe source root (containing include/ directory).
fn fetch_oboe() -> Option<PathBuf> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let oboe_dir = out_dir.join("oboe");

    // Check if already fetched
    if oboe_dir.join("include").join("oboe").join("Oboe.h").exists() {
        return Some(oboe_dir);
    }

    // Try to clone Oboe from GitHub
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
