// cpp_smoke.c — Step E.minus-1: same content as the crashing .cpp file
// but as plain C. No extern "C" linkage spec (that's C++-only syntax;
// in C every function has C linkage by default). If this crashes we
// know cc::Build is being wrongly accused — the trigger must be more
// general than C++ mode.

int wzp_cpp_hello(void) {
    return 42;
}
