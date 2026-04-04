fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("android") {
        // Real Oboe build for Android targets
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .file("cpp/oboe_bridge.cpp")
            .include("cpp")
            .compile("oboe_bridge");
    } else {
        // Stub for host builds / testing
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .file("cpp/oboe_stub.cpp")
            .include("cpp")
            .compile("oboe_bridge");
    }
}
