// cpp_smoke.cpp — original Step E.1 minimal crashing C++ file.
// Compiled with cc::Build::new().cpp(true).cpp_link_stdlib("c++_shared").
// Never called from Rust (dead-stripped at link time). Used to validate
// the tauri.conf.json minSdkVersion=26 fix against the smallest variant
// that was reliably crashing with __init_tcb+4.

extern "C" int wzp_cpp_hello(void) {
    return 42;
}
