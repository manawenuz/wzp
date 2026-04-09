// cpp_smoke.cpp — minimal C++ test that exercises the libc++_shared
// features Oboe uses (std::thread, std::mutex, std::atomic) without being
// Oboe itself.
//
// Built via cc::Build::new().cpp(true).cpp_link_stdlib("c++_shared") and
// replaces the full Oboe bridge compile during the Step E bisection of
// the __init_tcb+4 crash. The function is `extern "C"` and exported so
// the linker can't dead-code-eliminate it — the std::thread /
// std::lock_guard / std::atomic::fetch_add uses pull in libc++'s
// bindings to bionic pthread, matching what Oboe would force.
//
// The function is NEVER called from Rust. If we crash anyway, the trigger
// is just *linking* this code in. If it launches cleanly, Oboe itself
// (size, static ctors, specific headers) is the culprit.

#include <atomic>
#include <mutex>
#include <thread>

namespace {
    std::atomic<int> g_counter{0};
    std::mutex g_mutex;
}

extern "C" int wzp_cpp_smoke(void) {
    std::lock_guard<std::mutex> lock(g_mutex);
    std::thread t([]() { g_counter.fetch_add(1); });
    t.join();
    return g_counter.load();
}
