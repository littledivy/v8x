// Real-backend C++/JSI bridge for the Hermes engine spike (C3 hello-world path).
//
// Hermes has no C ABI, only a C++-only JSI. So the v8x backend authors the
// v8__* surface in Rust but routes every real JS operation through this
// extern "C" bridge, which owns a jsi::Runtime and a handle table of
// jsi::Value objects. Rust never sees a jsi::Value by value (it is move-only
// C++), only opaque slot indices into the table.
//
// Design (mirrors src/quickjs/core.rs arena model, with C++-side storage):
//   - A RuntimeWrapper owns the HermesRuntime plus a std::vector<jsi::Value>
//     handle table. The Runtime OUTLIVES every Value it produced (a C2 rule:
//     a jsi::Value's destructor calls back into its Runtime, and a caught
//     jsi::JSError embeds Values, so the Runtime must be destroyed last).
//   - A v8 Local is an index into that table (returned to Rust as a slot).
//   - A HandleScope is a watermark (the table length at scope entry). Scope
//     exit truncates the table back to the watermark, releasing those roots.
//   - Every entry point is wrapped in the C2 catch-all so no C++ exception
//     (including jsi::JSError) unwinds across extern "C" into Rust.
//
// See docs/hermes-spike/experiments/C3-hermes-helloworld.md.

#include <hermes/hermes.h>
#include <jsi/jsi.h>

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <string>
#include <vector>

using namespace facebook;

// A slot index into the handle table. -1 is the null/invalid slot; Rust maps
// it to a null v8 handle pointer.
typedef int64_t v8x_hermes_slot;
static const v8x_hermes_slot V8X_HERMES_NULL_SLOT = -1;

namespace {

struct RuntimeWrapper {
  // Declared first so it is destroyed LAST: every jsi::Value in `handles`
  // holds a PointerValue owned by this Runtime and must be torn down while the
  // Runtime is still alive (the C2 lifetime rule).
  std::unique_ptr<jsi::Runtime> rt;
  // The handle table. Each live v8 Local is an index into this vector. Growing
  // it never invalidates a slot index (unlike a pointer into the storage).
  std::vector<jsi::Value> handles;

  jsi::Runtime &runtime() { return *rt; }

  // Move a produced Value into a fresh slot, returning its index.
  v8x_hermes_slot push(jsi::Value &&v) {
    handles.emplace_back(std::move(v));
    return static_cast<v8x_hermes_slot>(handles.size() - 1);
  }
};

} // namespace

extern "C" {

// Create a HermesRuntime + empty handle table. Returns an opaque wrapper
// pointer, or nullptr if runtime creation threw.
void *v8x_hermes_runtime_new() {
  try {
    auto *w = new RuntimeWrapper();
    w->rt = facebook::hermes::makeHermesRuntime();
    return static_cast<void *>(w);
  } catch (...) {
    return nullptr;
  }
}

// Destroy the wrapper. Clears the handle table (Values) BEFORE the Runtime,
// because ~RuntimeWrapper destroys members in reverse declaration order (rt
// first would violate the lifetime rule), so we clear explicitly here to be
// certain the Values die while `rt` is still valid.
void v8x_hermes_runtime_free(void *rtw) {
  if (rtw == nullptr) {
    return;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    w->handles.clear();
  } catch (...) {
    // fall through to delete regardless
  }
  delete w;
}

// Current handle-table length: a HandleScope watermark.
size_t v8x_hermes_handles_len(void *rtw) {
  if (rtw == nullptr) {
    return 0;
  }
  return static_cast<RuntimeWrapper *>(rtw)->handles.size();
}

// Truncate the handle table back to `watermark`, releasing every slot created
// since the scope was entered. Values are destroyed while the Runtime is alive.
void v8x_hermes_handles_truncate(void *rtw, size_t watermark) {
  if (rtw == nullptr) {
    return;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  if (watermark >= w->handles.size()) {
    return;
  }
  try {
    w->handles.resize(watermark);
  } catch (...) {
  }
}

// global object as a slot.
v8x_hermes_slot v8x_hermes_global(void *rtw) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::Value g(w->runtime(), w->runtime().global());
    return w->push(std::move(g));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Make a JS string from UTF-8 bytes; returns a slot holding a String Value.
v8x_hermes_slot v8x_hermes_string_new_utf8(void *rtw, const char *data,
                                           size_t len) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::String s = jsi::String::createFromUtf8(
        w->runtime(), reinterpret_cast<const uint8_t *>(data), len);
    jsi::Value v(w->runtime(), s);
    return w->push(std::move(v));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Compile+run a source string held in `src_slot`, returning a slot with the
// result Value. On a JS error or any C++ throw, returns null slot and sets
// *ok to 0; on success *ok is 1. (The pending-exception plumbing is left for a
// later cycle; the hello-world path does not throw.)
v8x_hermes_slot v8x_hermes_run(void *rtw, v8x_hermes_slot src_slot, int *ok) {
  if (ok != nullptr) {
    *ok = 0;
  }
  if (rtw == nullptr || src_slot < 0) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  if (static_cast<size_t>(src_slot) >= w->handles.size()) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    // Read the source out of the slot as a std::string.
    const jsi::Value &sv = w->handles[static_cast<size_t>(src_slot)];
    if (!sv.isString()) {
      return V8X_HERMES_NULL_SLOT;
    }
    std::string source = sv.getString(w->runtime()).utf8(w->runtime());
    jsi::Value result = w->runtime().evaluateJavaScript(
        std::make_unique<jsi::StringBuffer>(source), "v8x.js");
    v8x_hermes_slot out = w->push(std::move(result));
    if (ok != nullptr) {
      *ok = 1;
    }
    return out;
  } catch (const jsi::JSError &) {
    // The JSError and its embedded Values are destroyed here, while `w->rt`
    // is still alive (the C2 lifetime rule).
    return V8X_HERMES_NULL_SLOT;
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Coerce the Value in `slot` to a JS string and copy its UTF-8 into `out`
// (capacity `cap`). Returns the full byte length of the UTF-8 (which may
// exceed `cap`, in which case the copy is truncated); returns SIZE_MAX on
// error. Does not NUL-terminate.
size_t v8x_hermes_value_to_utf8(void *rtw, v8x_hermes_slot slot, char *out,
                                size_t cap) {
  if (rtw == nullptr || slot < 0) {
    return static_cast<size_t>(-1);
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  if (static_cast<size_t>(slot) >= w->handles.size()) {
    return static_cast<size_t>(-1);
  }
  try {
    const jsi::Value &v = w->handles[static_cast<size_t>(slot)];
    // toString coerces any Value (number, bool, object) to a JS string, like
    // v8's String::Utf8Value on a non-string does.
    std::string s = v.toString(w->runtime()).utf8(w->runtime());
    size_t n = s.size();
    if (out != nullptr && cap > 0) {
      size_t copy = n < cap ? n : cap;
      std::memcpy(out, s.data(), copy);
    }
    return n;
  } catch (...) {
    return static_cast<size_t>(-1);
  }
}

// Is the Value in `slot` a string? (used by the write path to decide whether
// to coerce). Returns 1/0, or 0 on error.
int v8x_hermes_value_is_string(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr || slot < 0) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  if (static_cast<size_t>(slot) >= w->handles.size()) {
    return 0;
  }
  try {
    return w->handles[static_cast<size_t>(slot)].isString() ? 1 : 0;
  } catch (...) {
    return 0;
  }
}

} // extern "C"
