use std::process::Command;

fn main() {
    // Get git hash at build time
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();

    let hash = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => "unknown".to_string(),
    };

    println!("cargo:rustc-env=WZP_BUILD_HASH={hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
