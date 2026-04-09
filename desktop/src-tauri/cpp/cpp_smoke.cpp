// cpp_smoke.cpp — Step E.2 minimal stub: std::atomic only, no thread/mutex.
//
// Linked via cpp_link_stdlib("c++_shared") so the resulting .so still carries
// a NEEDED entry for libc++_shared.so exactly like the Oboe build would.
//
// Same extern "C" export as E.4 so the linker CAN pull the symbol in if it
// chooses to, but since Rust never calls it, it'll typically be dead-stripped.
// The diagnostic value is in the build.rs link directives this compile
// produces, not in the file's actual code being linked.

#include <atomic>

namespace {
    std::atomic<int> g_counter{0};
}

extern "C" int wzp_cpp_smoke(void) {
    return g_counter.fetch_add(1);
}
