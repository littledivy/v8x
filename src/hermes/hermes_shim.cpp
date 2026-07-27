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
  // Lazily-created per-runtime identity-hash state (C4). See
  // v8x_hermes_get_identity_hash below for the design.
  std::unique_ptr<jsi::Value> identity_symbol;
  std::unique_ptr<jsi::Value> define_property_fn;
  int64_t next_identity_id = 1;

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

// Is the Value in `slot` a JS object? Needed so Rust can safely
// `Local<Value>::try_cast::<Object>()` (the vendored TryFrom checks
// `Value::is_object()` first). Returns 1/0, or 0 on error.
int v8x_hermes_value_is_object(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr || slot < 0) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  if (static_cast<size_t>(slot) >= w->handles.size()) {
    return 0;
  }
  try {
    return w->handles[static_cast<size_t>(slot)].isObject() ? 1 : 0;
  } catch (...) {
    return 0;
  }
}

// ---- C4: object identity (strict equality + stable identity hash) --------
//
// JSI hands out no raw pointer: two Locals (handle-table slots) obtained for
// the SAME JS object are different slot indices, so slot-pointer equality
// (what a naive port of V8's Value*/Object* identity would use) is WRONG.
// v8__Value__StrictEquals/SameValue and v8__Object__GetIdentityHash must
// instead go through the JS object's own identity, which JSI does expose via
// jsi::Runtime::strictEquals(const Value&, const Value&) - this is exact
// SameValueZero-ish JS `===` semantics over the underlying heap object, not
// slot identity. See docs/hermes-spike/experiments/C4-hermes-identity.md.

// jsi::Runtime::strictEquals(a, b): true iff `a === b` in JS semantics
// (compares underlying object identity for objects, not slot identity).
// Returns 1 (true), 0 (false), or -1 on error/invalid slot.
int v8x_hermes_strict_equals(void *rtw, v8x_hermes_slot a, v8x_hermes_slot b) {
  if (rtw == nullptr || a < 0 || b < 0) {
    return -1;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  size_t sa = static_cast<size_t>(a), sb = static_cast<size_t>(b);
  if (sa >= w->handles.size() || sb >= w->handles.size()) {
    return -1;
  }
  try {
    bool eq = jsi::Value::strictEquals(
        w->runtime(), w->handles[sa], w->handles[sb]);
    return eq ? 1 : 0;
  } catch (...) {
    return -1;
  }
}

namespace {

// Lazily create (once per runtime) a real JS Symbol used as the hidden
// identity-id property key, and cache a reference to Object.defineProperty.
// A JSI PropNameID is always string-keyed (PropNameID::forUtf8/forAscii);
// there is no Symbol-keyed Object::setProperty overload in the JSI C++
// surface. So the hidden slot is installed the same way native embedders
// (e.g. Hermes/RN itself) install non-enumerable hidden state: through the
// real JS-level Object.defineProperty, keyed by a Symbol that is never
// exposed to the interned handle table Rust sees, so nothing in the normal
// v8__* surface can ever hand a JS caller this Symbol value.
bool ensure_identity_infra(RuntimeWrapper *w) {
  if (w->identity_symbol && w->define_property_fn) {
    return true;
  }
  try {
    jsi::Value setup = w->runtime().evaluateJavaScript(
        std::make_unique<jsi::StringBuffer>(
            "(function() { return ["
            "Symbol('v8x_identity_id'),"
            "Object.defineProperty"
            "]; })()"),
        "v8x-identity-setup.js");
    jsi::Array arr = setup.getObject(w->runtime()).asArray(w->runtime());
    jsi::Value sym = arr.getValueAtIndex(w->runtime(), 0);
    jsi::Value defProp = arr.getValueAtIndex(w->runtime(), 1);
    w->identity_symbol =
        std::make_unique<jsi::Value>(w->runtime(), sym);
    w->define_property_fn =
        std::make_unique<jsi::Value>(w->runtime(), defProp);
    return true;
  } catch (...) {
    return false;
  }
}

} // namespace

// Stable per-object identity hash (the crux of C4). JSI has no built-in
// identity hash, so this uses the standard embedder trick: lazily attach a
// HIDDEN, non-enumerable property keyed by a well-known per-runtime Symbol,
// holding a monotonically-increasing integer id. On first call for an
// object, assign+store the next id; on later calls, read back the same id.
// Getting the SAME object through two different slots yields the SAME hash,
// because the id lives on the object's own heap storage (JSI's Symbol-keyed
// property), not on the (per-call, non-canonical) slot.
//
// The hidden id is invisible to Object.keys/JSON.stringify/for-in because
// (a) it is Symbol-keyed (Symbol-keyed properties are never enumerated by
// Object.keys, JSON.stringify, or for-in - only Object.getOwnPropertySymbols
// would surface it) and (b) it is additionally installed non-enumerable via
// Object.defineProperty, so even Object.getOwnPropertySymbols + a manual
// enumerable check would show enumerable:false.
//
// Returns the hash (>=1 to match v8's "never 0" contract), or -1 on error
// (non-object Value, or any C++ throw).
int64_t v8x_hermes_get_identity_hash(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr || slot < 0) {
    return -1;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  if (static_cast<size_t>(slot) >= w->handles.size()) {
    return -1;
  }
  try {
    const jsi::Value &v = w->handles[static_cast<size_t>(slot)];
    if (!v.isObject()) {
      return -1;
    }
    if (!ensure_identity_infra(w)) {
      return -1;
    }
    jsi::Object obj = v.getObject(w->runtime());
    jsi::Symbol key = w->identity_symbol->getSymbol(w->runtime());

    // Read back an existing id via a small JS helper so we can do a
    // Symbol-keyed lookup (JSI's Object::getProperty has no Symbol-keyed
    // overload either); Object.defineProperty(obj, sym, {...}) is called
    // directly through the cached Function so INSTALLING the id needs no
    // extra per-call script compile.
    jsi::Function hasOwn = w->runtime()
                                .global()
                                .getPropertyAsObject(w->runtime(), "Object")
                                .getPropertyAsFunction(
                                    w->runtime(), "getOwnPropertyDescriptor");
    jsi::Value existing =
        hasOwn.call(w->runtime(), obj, jsi::Value(w->runtime(), key));
    if (existing.isObject()) {
      jsi::Value id =
          existing.getObject(w->runtime()).getProperty(w->runtime(), "value");
      if (id.isNumber()) {
        return static_cast<int64_t>(id.getNumber());
      }
    }

    int64_t new_id = w->next_identity_id++;
    jsi::Function defineProperty =
        w->define_property_fn->getObject(w->runtime()).getFunction(w->runtime());
    jsi::Object desc(w->runtime());
    desc.setProperty(w->runtime(), "value", jsi::Value(static_cast<double>(new_id)));
    desc.setProperty(w->runtime(), "enumerable", jsi::Value(false));
    desc.setProperty(w->runtime(), "writable", jsi::Value(false));
    desc.setProperty(w->runtime(), "configurable", jsi::Value(false));
    defineProperty.call(w->runtime(), obj, jsi::Value(w->runtime(), key),
                        std::move(desc));
    return new_id;
  } catch (...) {
    return -1;
  }
}

// ---- C5: parse-free AOT execution (source OR precompiled HBC) ------------
//
// HermesRuntime::evaluateJavaScript already sniffs its input: the first 8
// bytes are checked against the Hermes Bytecode (HBC) magic
// (HermesRuntime::isHermesBytecode), and if they match, Hermes runs the
// buffer directly as bytecode, skipping the parser/compiler entirely. If they
// don't match, it is treated as UTF-8 JS source and parsed+compiled as usual.
// This entry point hands Hermes a raw byte buffer (owned by Rust, copied
// here) instead of assuming it is a `std::string` of source text, so the
// SAME function runs plain JS source or a hermesc-compiled .hbc file
// transparently. See docs/hermes-spike/experiments/C5-hermes-hbc-aot.md.

namespace {

// A jsi::Buffer over an owned byte copy (the Rust slice may not outlive the
// call, and HermesRuntime keeps referencing the Buffer while a
// PreparedJavaScript derived from it is alive).
//
// LOAD-BEARING: one extra NUL byte is always appended after the payload
// (not counted in size()). Hermes' JS lexer (like most hand-rolled C++
// lexers, e.g. V8's own ScannerStream) reads a one-byte lookahead past the
// last real character and expects a NUL sentinel there instead of relying on
// size()/bounds-checking every single access; jsi::StringBuffer gets this
// for free because std::string::data() has been guaranteed NUL-terminated
// since C++11. A raw std::vector<uint8_t> built from (data, data+len) has NO
// such guarantee, so the byte after the last real one is uninitialized
// heap - reading it is technically fine (it is still one past a valid
// std::vector allocation's used range only if capacity > size; otherwise it
// is a real out-of-bounds read). Empirically this crashed (SIGSEGV inside
// the Hermes lexer/parser) on a ~340KB source buffer but not smaller ones,
// consistent with an out-of-bounds read that only sometimes lands on an
// unmapped page depending on heap layout. Confirmed fixed by reserving one
// extra byte and zeroing it. HBC buffers do not need this (bytecode has a
// fixed-size header/footer, no lexer lookahead), but appending it
// unconditionally is harmless there too.
class OwnedBuffer : public jsi::Buffer {
public:
  OwnedBuffer(const uint8_t *data, size_t len) : bytes_(len + 1) {
    std::memcpy(bytes_.data(), data, len);
    bytes_[len] = 0;
  }
  size_t size() const override { return bytes_.size() - 1; }
  const uint8_t *data() const override { return bytes_.data(); }

private:
  std::vector<uint8_t> bytes_;
};

} // namespace

// Is `data[0..len)` recognized as Hermes bytecode (checks the 8-byte HBC
// magic + header)? Returns 1/0. Static on HermesRuntime, needs no runtime
// instance.
int v8x_hermes_is_hbc(const uint8_t *data, size_t len) {
  if (data == nullptr) {
    return 0;
  }
  return facebook::hermes::HermesRuntime::isHermesBytecode(data, len) ? 1 : 0;
}

// Evaluate a raw byte buffer that is EITHER JS source OR Hermes bytecode
// (HBC): Hermes itself sniffs which one it is (see isHermesBytecode above)
// and skips parsing/compiling for HBC. Returns a slot with the result Value,
// or the null slot on any error (JSI throws JSIException/JSError for bad
// input on either path). *ok is set to 1 on success, 0 otherwise, mirroring
// v8x_hermes_run's contract.
v8x_hermes_slot v8x_hermes_eval_buffer(void *rtw, const uint8_t *data,
                                       size_t len, const char *source_url,
                                       int *ok) {
  if (ok != nullptr) {
    *ok = 0;
  }
  if (rtw == nullptr || data == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    auto buffer = std::make_shared<OwnedBuffer>(data, len);
    std::string url = source_url != nullptr ? source_url : "v8x-buffer.js";
    jsi::Value result = w->runtime().evaluateJavaScript(buffer, url);
    v8x_hermes_slot out = w->push(std::move(result));
    if (ok != nullptr) {
      *ok = 1;
    }
    return out;
  } catch (const jsi::JSError &) {
    // Destroyed here, while `w->rt` is still alive (the C2 lifetime rule).
    return V8X_HERMES_NULL_SLOT;
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

} // extern "C"
