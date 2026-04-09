// cpp_smoke.cpp — Step E.1: the absolute minimum C++ file.
// No #include, no STL, no atomics, no thread, no mutex. Just one function.
// Still compiled as .cpp(true) with cpp_link_stdlib("c++_shared"), so the
// only delta vs the non-crashing baseline is:
//   1. cc::Build using cpp(true) (vs plain C in hello.c)
//   2. cargo:rustc-link-lib=c++_shared emitted by cc-rs (adds NEEDED
//      entry for libc++_shared.so in the final .so)
//
// If this crashes, we've conclusively proven the trigger is one of those
// two things — not any C++ code behavior.

extern "C" int wzp_cpp_hello(void) {
    return 42;
}
