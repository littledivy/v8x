// Feasibility-proof shim for the Hermes backend spike.
//
// This is the single hardest, least de-risked part of the whole
// engine_hermes idea: Hermes has no C ABI, only a C++-only JSI. So every
// v8__* symbol the vendored rusty_v8 surface wants must ultimately be
// authored in C++ against jsi::Runtime and exported extern "C". This file
// proves that boundary works end to end with a real libhermes: it creates a
// HermesRuntime, evaluates a source string, reads a number back, and hands
// it to Rust as a plain int32_t.
//
// It is deliberately minimal. It is NOT part of the v8__* surface; it is the
// go/no-go proof that the surface CAN be built. See
// docs/hermes-spike/experiments/C2-hermes-ffi.md.

#include <hermes/hermes.h>
#include <jsi/jsi.h>

#include <cstdint>
#include <memory>
#include <string>

using namespace facebook;

// Sentinels distinguish failure classes without needing an out-param yet.
// Real backend code will surface jsi::JSError as a v8 exception instead.
static const int32_t V8X_HERMES_NOT_A_NUMBER = -1000;
static const int32_t V8X_HERMES_JS_ERROR = -2000;
static const int32_t V8X_HERMES_CPP_ERROR = -3000;

extern "C" int32_t v8x_hermes_smoke_eval(const char *src) {
  // The runtime must OUTLIVE any jsi::JSError produced by the eval. A JSError
  // embeds a jsi::Value whose PointerValue destructor calls back into the
  // owning Runtime; if the Runtime is destroyed first, that destructor
  // dereferences a freed vtable and crashes (EXC_BAD_ACCESS in
  // jsi::Value::~Value, observed on the JSError catch path). So `rt` is
  // declared in the OUTER scope here, and the try/catch that may surface a
  // JSError sits INSIDE its lifetime. Every C++ exception is caught before it
  // can unwind into Rust across the extern "C" boundary; that try/catch is the
  // contract every real v8__* shim will replicate.
  std::unique_ptr<jsi::Runtime> rt;
  try {
    rt = facebook::hermes::makeHermesRuntime();
  } catch (...) {
    return V8X_HERMES_CPP_ERROR;
  }
  try {
    jsi::Value v = rt->evaluateJavaScript(
        std::make_unique<jsi::StringBuffer>(std::string(src)), "smoke.js");
    if (v.isNumber()) {
      return static_cast<int32_t>(v.asNumber());
    }
    return V8X_HERMES_NOT_A_NUMBER;
  } catch (const jsi::JSError &) {
    // The JSError (and its embedded Value) is destroyed here, at the end of
    // this catch, while `rt` is still alive in the enclosing scope.
    return V8X_HERMES_JS_ERROR;
  } catch (...) {
    return V8X_HERMES_CPP_ERROR;
  }
}
