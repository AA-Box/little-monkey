// Little Monkey: the text-to-speech symbols this build deliberately does not
// link.
//
// Keyword spotting never synthesizes speech, and this build does not link
// eSpeak NG — GPL-3.0 — or piper_phonemize into an MIT binary. The linker still
// demands their symbols anyway: the Rust bindings declare externs across
// sherpa-onnx's whole C API, so `libsherpa-onnx-c-api.a`'s text-to-speech
// objects are pulled in, and those reference the phonemizer. Excluding the
// archives without answering for their symbols does not remove the dependency,
// it only moves the failure to link time — which is exactly what happened on
// Linux, where `lld` refuses undefined symbols that macOS had tolerated.
//
// So the symbols are answered here, and answered honestly: reaching this code
// means something asked for speech synthesis from a build that has none, which
// is a programming error rather than a condition to recover from. It aborts and
// says so, instead of returning a plausible-looking empty phonemization that
// would surface later as silence nobody can explain.
//
// Delete this file the moment the link list stops excluding the phonemizer, or
// upstream gains a build without it. A stub that outlives its reason is a trap.

#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>

namespace {

[[noreturn]] void little_monkey_no_tts_backend(const char *symbol) {
  std::fprintf(stderr,
               "little-monkey: %s was called, but this build links no "
               "text-to-speech backend. Keyword spotting must never reach it.\n",
               symbol);
  std::abort();
}

}  // namespace

// espeak-ng's C entry point. `extern "C"`, so the parameter types only have to
// be layout-compatible for a call that never returns; the symbol name is what
// the linker is looking for.
//
// Defined once. On Linux this file is compiled twice — see below — and a C
// symbol has one name in both passes, so the second pass must skip it or the
// link fails on a duplicate instead of a missing one.
#ifndef LITTLE_MONKEY_TTS_STUBS_NO_C_SYMBOLS
extern "C" int espeak_Initialize(int, int, const char *, int) {
  little_monkey_no_tts_backend("espeak_Initialize");
}
#endif

// libstdc++ has two incompatible `std::string` types, and which one a C++
// symbol mangles with depends on `_GLIBCXX_USE_CXX11_ABI` at *its* compile
// time. sherpa-onnx's published Linux archives are built for broad
// compatibility; this crate is built with whatever the runner defaults to.
// Guessing wrong leaves the symbol as undefined as it was before, which is
// exactly what the first attempt at this file did.
//
// So on Linux the build compiles this translation unit under both settings and
// links both. The two mangle to different names, so they cannot collide, and
// the one the archive does not reference is simply never called.
namespace piper {

// Incomplete by design: a reference parameter mangles from the class's name and
// namespace, not its definition, so this produces the symbol the archive wants
// without vendoring piper's headers.
struct eSpeakPhonemeConfig;

void phonemize_eSpeak(std::string, eSpeakPhonemeConfig &,
                      std::vector<std::vector<char32_t>> &) {
  little_monkey_no_tts_backend("piper::phonemize_eSpeak");
}

}  // namespace piper
