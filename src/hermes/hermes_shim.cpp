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

// ---- C9: TryCatch / exception surfacing -----------------------------------
//
// v8's TryCatch is a stack of exception-capture scopes on the isolate: only
// the INNERMOST live TryCatch observes an exception thrown while it is on
// top. We mirror that with a std::vector<TryCatchFrame> on the
// RuntimeWrapper; v8x_hermes_trycatch_push/pop manage it, and every JS-error
// boundary (Run, Function::Call, ThrowException, ...) captures into
// `tc_stack.back()` when the stack is non-empty. A caught jsi::JSError's
// embedded jsi::Value is moved into the handle table (a fresh slot) and the
// frame is marked caught; getMessage()/getStack() are read out of the
// JSError BEFORE it is destroyed (the C2 lifetime rule: the JSError, and the
// Runtime it references, must both still be alive when value()/getMessage()/
// getStack() are called - value() is captured via the handle-table push,
// which itself needs the still-alive Runtime).
struct TryCatchFrame {
  v8x_hermes_slot exception_slot = -1; // -1 = nothing caught (yet)
  bool has_caught = false;
  // Set once rethrow() has been called on this frame: a later reset() must
  // NOT clear has_caught (matches the vendored test's documented V8 quirk).
  bool rethrown = false;
  std::string message;
  std::string stack;
};

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
  // C11: lazily-created per-runtime internal-field state. Internal fields are
  // stored ON the object itself, in a hidden non-enumerable Symbol-keyed
  // property holding a JS Array of length internal_field_count (mirroring the
  // C4 identity-hash trick, a separate Symbol). Cached the same way as the
  // identity-hash infra so no per-call script compile happens on the hot path.
  std::unique_ptr<jsi::Value> internal_fields_symbol;
  std::unique_ptr<jsi::Value> get_own_property_descriptor_fn;
  // C9: the TryCatch scope stack. back() is the innermost live scope.
  std::vector<TryCatchFrame> tc_stack;
  // C10: pending exception left by a native FunctionCallback that threw (via
  // Isolate::ThrowException). -1 = no pending exception. Read and cleared by
  // the host-function trampoline, which re-throws it as a jsi::JSError so it
  // propagates through JSI like any JS-level throw.
  v8x_hermes_slot pending_callback_exception = -1;

  jsi::Runtime &runtime() { return *rt; }

  // Move a produced Value into a fresh slot, returning its index.
  v8x_hermes_slot push(jsi::Value &&v) {
    handles.emplace_back(std::move(v));
    return static_cast<v8x_hermes_slot>(handles.size() - 1);
  }

  // Capture a caught jsi::JSError into the innermost live TryCatch frame, if
  // any. No-op (exception silently dropped, like V8 with no TryCatch on the
  // stack reporting to the fatal-error handler) when tc_stack is empty - the
  // caller still returns its own null/empty-sentinel to signal failure.
  void capture_exception(const jsi::JSError &err) {
    if (tc_stack.empty()) {
      return;
    }
    TryCatchFrame &frame = tc_stack.back();
    try {
      // err.value() returns a `const jsi::Value&` bound to the JSError's own
      // shared_ptr<Value>; copy-construct a fresh Value from it (via the
      // Runtime, still alive here) into the handle table before the JSError
      // (and its value_ shared_ptr) is destroyed by the catch block.
      jsi::Value v(runtime(), err.value());
      frame.exception_slot = push(std::move(v));
    } catch (...) {
      frame.exception_slot = -1;
    }
    frame.has_caught = true;
    frame.message = err.getMessage();
    frame.stack = err.getStack();
  }
};

// A v8 `External` wraps an opaque embedder `void*` in a JS heap value. JSI has
// no native "external"/"foreign pointer" value, so we model it as a JSI
// HostObject that carries the pointer. Each External is a distinct JS object,
// so two Externals compare unequal by object identity (what the vendored
// `External` PartialEq via strictEquals/Data__EQ expects) and reading the
// pointer back is exact.
class ExternalHost : public jsi::HostObject {
public:
  explicit ExternalHost(void *ptr) : ptr_(ptr) {}
  void *ptr() const { return ptr_; }

private:
  void *ptr_;
};

} // namespace

// C10: native function callbacks. A v8 FunctionCallback is a C function pointer
// living on the Rust side; JSI invokes host functions through a
// std::function<jsi::Value(Runtime&, const Value& this, const Value* args,
// size_t count)>. The bridge below marshals the JSI-side call into handle-table
// slots and calls back into Rust (v8x_hermes_dispatch_callback), which
// constructs a v8 FunctionCallbackInfo, invokes the FunctionCallback, and
// hands back the slot the callback stored via ReturnValue. See
// docs/hermes-spike/experiments/C10-hermes-callbacks.md.
//
// The Rust dispatch trampoline. `callback_bits` is the FunctionCallback fn ptr
// reinterpreted as a uintptr_t. `this_slot`/`data_slot`/`new_target_slot` and
// each `arg_slots[i]` are handle-table indices. On return, `*ret_slot` holds
// the handle-table index of the callback's return value (or -1 for undefined),
// and `*threw` is set to 1 if the callback left a pending exception that the
// host function must surface as a jsi::JSError.
extern "C" int64_t v8x_hermes_dispatch_callback(
    void *rtw, uintptr_t callback_bits, v8x_hermes_slot this_slot,
    v8x_hermes_slot data_slot, const v8x_hermes_slot *arg_slots, size_t argc,
    int is_construct, v8x_hermes_slot new_target_slot, int *threw);

// C11: accessor getter/setter dispatch trampolines. Parallel to
// v8x_hermes_dispatch_callback but for property accessors: they build a
// PropertyCallbackInfo-shaped object, invoke the NamedGetter/SetterCallback,
// and (for the getter) hand back the return-value slot. `*threw` signals a
// pending exception the host function must re-throw. See
// docs/hermes-spike/experiments/C11-hermes-templates.md.
extern "C" int64_t v8x_hermes_dispatch_accessor_getter(
    void *rtw, uintptr_t getter_bits, v8x_hermes_slot key_slot,
    v8x_hermes_slot holder_slot, v8x_hermes_slot data_slot, int *threw);
extern "C" void v8x_hermes_dispatch_accessor_setter(
    void *rtw, uintptr_t setter_bits, v8x_hermes_slot key_slot,
    v8x_hermes_slot value_slot, v8x_hermes_slot holder_slot,
    v8x_hermes_slot data_slot, int *threw);

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

// Copy the Value in `src` into the EXISTING slot `dst` (overwrite
// handles[dst]). Used by EscapableHandleScope::escape: `reserve` (called
// before the child scope records its watermark) pushes a placeholder slot in
// the PARENT, and `escape` overwrites that reserved slot with the escaping
// value, so it survives the child scope's truncate-on-exit. Non-lossy for any
// value type. Returns 1 on success, 0 on error.
int v8x_hermes_set_slot(void *rtw, v8x_hermes_slot dst, v8x_hermes_slot src) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  if (dst < 0 || static_cast<size_t>(dst) >= w->handles.size() || src < 0 ||
      static_cast<size_t>(src) >= w->handles.size()) {
    return 0;
  }
  try {
    w->handles[static_cast<size_t>(dst)] =
        jsi::Value(w->runtime(), w->handles[static_cast<size_t>(src)]);
    return 1;
  } catch (...) {
    return 0;
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
  } catch (const jsi::JSError &err) {
    // Captured into the innermost live TryCatch frame (C9), while `w->rt` is
    // still alive (the C2 lifetime rule) - the JSError and its embedded
    // Value are destroyed at the end of this catch block.
    w->capture_exception(err);
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
  } catch (const jsi::JSError &err) {
    // Captured (C9), while `w->rt` is still alive (the C2 lifetime rule).
    w->capture_exception(err);
    return V8X_HERMES_NULL_SLOT;
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// ---- C6: Object / Array / Number / Integer / Boolean / Function ----------
//
// Widens the surface past the C3 hello-world path + C4 identity de-risking:
// object/array/primitive construction and read/write, and calling a JS
// function value. Every entry point follows the same shape as the ones
// above: operate on handle-table slots, wrap the JSI call in the C2
// catch-all, and return a sentinel (-1 slot, or an out-param `ok`/`*_or_err`
// int) instead of ever letting a C++ exception (including jsi::JSError)
// unwind across the extern "C" boundary.
//
// Object/Array property KEYS: the v8 C-ABI's Object::Get/Set/Has take a
// generic `Value` key (any JS value, not just a string), but JSI's
// Object::getProperty/setProperty/hasProperty are string- (or PropNameID-)
// keyed only, with no generic-Value-key overload in the C++ surface. So the
// key Value is coerced to a JS string via `Value::toString`, matching how
// v8 itself ultimately coerces non-Name property keys to strings (ordinary
// property access, not the Symbol-keyed path C4 uses internally via raw JS).

namespace {

// Helper: safely read `handles[slot]` by const reference, or nullptr if out
// of range. Never throws.
const jsi::Value *slot_ref(RuntimeWrapper *w, v8x_hermes_slot slot) {
  if (slot < 0 || static_cast<size_t>(slot) >= w->handles.size()) {
    return nullptr;
  }
  return &w->handles[static_cast<size_t>(slot)];
}

} // namespace

// jsi::Value::undefined() / jsi::Value::null(): static factories, no Runtime
// call needed. Pushed straight into the handle table. Returns a slot, or the
// null slot on error (out-of-memory in `handles.emplace_back`, in practice).
v8x_hermes_slot v8x_hermes_undefined(void *rtw) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    return w->push(jsi::Value::undefined());
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

v8x_hermes_slot v8x_hermes_null(void *rtw) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    return w->push(jsi::Value::null());
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// jsi::Object() (empty object). Returns a slot, or the null slot on error.
v8x_hermes_slot v8x_hermes_object_new(void *rtw) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::Object o(w->runtime());
    jsi::Value v(w->runtime(), o);
    return w->push(std::move(v));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// obj[key] where key is coerced to a JS string. Returns a slot with the
// property's value (undefined if absent), or the null slot on error (obj is
// not an object, or a bad slot).
v8x_hermes_slot v8x_hermes_object_get(void *rtw, v8x_hermes_slot obj_slot,
                                      v8x_hermes_slot key_slot) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *ov = slot_ref(w, obj_slot);
  const jsi::Value *kv = slot_ref(w, key_slot);
  if (ov == nullptr || kv == nullptr || !ov->isObject()) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    jsi::String key = kv->toString(w->runtime());
    jsi::Value result =
        ov->getObject(w->runtime()).getProperty(w->runtime(), key);
    return w->push(std::move(result));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// obj[key] = value, key coerced to a JS string. Returns 1 on success, 0 on
// error (obj is not an object, bad slot, or the set threw).
int v8x_hermes_object_set(void *rtw, v8x_hermes_slot obj_slot,
                          v8x_hermes_slot key_slot,
                          v8x_hermes_slot value_slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *ov = slot_ref(w, obj_slot);
  const jsi::Value *kv = slot_ref(w, key_slot);
  const jsi::Value *vv = slot_ref(w, value_slot);
  if (ov == nullptr || kv == nullptr || vv == nullptr || !ov->isObject()) {
    return 0;
  }
  try {
    jsi::String key = kv->toString(w->runtime());
    // getObject() returns a temporary Object handle; setProperty mutates the
    // underlying JS heap object it points at, which is what we want (the
    // handle itself is not stored anywhere further).
    jsi::Object obj = ov->getObject(w->runtime());
    obj.setProperty(w->runtime(), key, jsi::Value(w->runtime(), *vv));
    return 1;
  } catch (...) {
    return 0;
  }
}

// key in obj, key coerced to a JS string. Returns 1 (true), 0 (false), or -1
// on error (obj is not an object, or a bad slot).
int v8x_hermes_object_has(void *rtw, v8x_hermes_slot obj_slot,
                          v8x_hermes_slot key_slot) {
  if (rtw == nullptr) {
    return -1;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *ov = slot_ref(w, obj_slot);
  const jsi::Value *kv = slot_ref(w, key_slot);
  if (ov == nullptr || kv == nullptr || !ov->isObject()) {
    return -1;
  }
  try {
    jsi::String key = kv->toString(w->runtime());
    bool has = ov->getObject(w->runtime()).hasProperty(w->runtime(), key);
    return has ? 1 : 0;
  } catch (...) {
    return -1;
  }
}

// jsi::Array(runtime, length). Returns a slot, or the null slot on error.
v8x_hermes_slot v8x_hermes_array_new(void *rtw, int64_t length) {
  if (rtw == nullptr || length < 0) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::Array a(w->runtime(), static_cast<size_t>(length));
    jsi::Value v(w->runtime(), a);
    return w->push(std::move(v));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Array::length via runtime.size(Array). Returns the length, or -1 on error
// (not an array, or a bad slot).
int64_t v8x_hermes_array_length(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return -1;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return -1;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    if (!obj.isArray(w->runtime())) {
      return -1;
    }
    jsi::Array arr = obj.asArray(w->runtime());
    return static_cast<int64_t>(arr.size(w->runtime()));
  } catch (...) {
    return -1;
  }
}

// array[index]. Returns a slot with the element, or the null slot on error
// (not an array, out of range per JSI, or a bad slot).
v8x_hermes_slot v8x_hermes_array_get_index(void *rtw, v8x_hermes_slot slot,
                                           uint32_t index) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    if (!obj.isArray(w->runtime())) {
      return V8X_HERMES_NULL_SLOT;
    }
    jsi::Array arr = obj.asArray(w->runtime());
    jsi::Value result = arr.getValueAtIndex(w->runtime(), index);
    return w->push(std::move(result));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// array[index] = value. Returns 1 on success, 0 on error (not an array, or a
// bad slot).
int v8x_hermes_array_set_index(void *rtw, v8x_hermes_slot slot,
                               uint32_t index, v8x_hermes_slot value_slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  const jsi::Value *vv = slot_ref(w, value_slot);
  if (v == nullptr || vv == nullptr || !v->isObject()) {
    return 0;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    if (!obj.isArray(w->runtime())) {
      return 0;
    }
    jsi::Array arr = obj.asArray(w->runtime());
    arr.setValueAtIndex(w->runtime(), index, jsi::Value(w->runtime(), *vv));
    return 1;
  } catch (...) {
    return 0;
  }
}

// jsi::Value(double). Returns a slot, or the null slot on error.
v8x_hermes_slot v8x_hermes_number_new(void *rtw, double value) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::Value v(value);
    return w->push(std::move(v));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Value::getNumber(). Writes the number into *out and returns 1 on success,
// 0 on error (not a number, or a bad slot).
int v8x_hermes_number_value(void *rtw, v8x_hermes_slot slot, double *out) {
  if (rtw == nullptr || out == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isNumber()) {
    return 0;
  }
  try {
    *out = v->getNumber();
    return 1;
  } catch (...) {
    return 0;
  }
}

// jsi::Value(bool). Returns a slot, or the null slot on error.
v8x_hermes_slot v8x_hermes_boolean_new(void *rtw, int value) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::Value v(value != 0);
    return w->push(std::move(v));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Value::getBool(). Returns 1 (true), 0 (false), or -1 on error (not a
// bool, or a bad slot).
int v8x_hermes_boolean_value(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return -1;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isBool()) {
    return -1;
  }
  try {
    return v->getBool() ? 1 : 0;
  } catch (...) {
    return -1;
  }
}

// Function::call(runtime, recv, args, argc). `recv_slot` may be the null
// slot (Rust passes NULL_SLOT for a null/undefined receiver), in which case
// JSI's Function::call(runtime, args, count) (undefined `this`) is used
// instead of Function::callWithThis. Returns a slot with the call's result,
// or the null slot on error (fn_slot is not a function, a bad slot, or the
// call threw a jsi::JSError / any other C++ exception).
v8x_hermes_slot v8x_hermes_function_call(void *rtw, v8x_hermes_slot fn_slot,
                                         v8x_hermes_slot recv_slot,
                                         const v8x_hermes_slot *arg_slots,
                                         size_t argc, int *ok) {
  if (ok != nullptr) {
    *ok = 0;
  }
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *fv = slot_ref(w, fn_slot);
  if (fv == nullptr || !fv->isObject()) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    jsi::Object fnObj = fv->getObject(w->runtime());
    if (!fnObj.isFunction(w->runtime())) {
      return V8X_HERMES_NULL_SLOT;
    }
    jsi::Function fn = fnObj.getFunction(w->runtime());

    std::vector<jsi::Value> args;
    args.reserve(argc);
    for (size_t i = 0; i < argc; ++i) {
      const jsi::Value *av =
          arg_slots != nullptr ? slot_ref(w, arg_slots[i]) : nullptr;
      if (av == nullptr) {
        return V8X_HERMES_NULL_SLOT;
      }
      args.emplace_back(w->runtime(), *av);
    }

    jsi::Value result;
    const jsi::Value *rv = slot_ref(w, recv_slot);
    const jsi::Value *argv = args.data();
    if (rv != nullptr && rv->isObject()) {
      jsi::Object recv = rv->getObject(w->runtime());
      result = fn.callWithThis(w->runtime(), recv, argv,
                                static_cast<size_t>(argc));
    } else {
      result = fn.call(w->runtime(), argv, static_cast<size_t>(argc));
    }
    v8x_hermes_slot out = w->push(std::move(result));
    if (ok != nullptr) {
      *ok = 1;
    }
    return out;
  } catch (const jsi::JSError &err) {
    // Captured (C9), while `w->rt` is still alive (the C2 lifetime rule).
    w->capture_exception(err);
    return V8X_HERMES_NULL_SLOT;
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// ---- Value type predicates needed by the widened surface ------------------

int v8x_hermes_value_is_array(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return 0;
  }
  try {
    return v->getObject(w->runtime()).isArray(w->runtime()) ? 1 : 0;
  } catch (...) {
    return 0;
  }
}

int v8x_hermes_value_is_function(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return 0;
  }
  try {
    return v->getObject(w->runtime()).isFunction(w->runtime()) ? 1 : 0;
  } catch (...) {
    return 0;
  }
}

int v8x_hermes_value_is_number(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  return (v != nullptr && v->isNumber()) ? 1 : 0;
}

int v8x_hermes_value_is_boolean(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  return (v != nullptr && v->isBool()) ? 1 : 0;
}

// ---- External (v8::External): opaque embedder void* as a JS value ----------

// jsi::Object::createFromHostObject wrapping an ExternalHost that carries
// `ptr`. Returns a slot, or the null slot on error.
v8x_hermes_slot v8x_hermes_external_new(void *rtw, void *ptr) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::Object obj = jsi::Object::createFromHostObject(
        w->runtime(), std::make_shared<ExternalHost>(ptr));
    return w->push(jsi::Value(w->runtime(), obj));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Read the embedder void* back out of an External's ExternalHost. `found` (if
// non-null) is set to 1 when the slot really held an ExternalHost, 0 otherwise
// (so a genuine null pointer is distinguishable from a wrong-type slot).
void *v8x_hermes_external_value(void *rtw, v8x_hermes_slot slot, int *found) {
  if (found != nullptr) {
    *found = 0;
  }
  if (rtw == nullptr) {
    return nullptr;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return nullptr;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    if (!obj.isHostObject(w->runtime())) {
      return nullptr;
    }
    auto host = obj.getHostObject<ExternalHost>(w->runtime());
    if (host == nullptr) {
      return nullptr;
    }
    if (found != nullptr) {
      *found = 1;
    }
    return host->ptr();
  } catch (...) {
    return nullptr;
  }
}

// Is the Value in `slot` an External (a JSI HostObject carrying a pointer)?
int v8x_hermes_value_is_external(void *rtw, v8x_hermes_slot slot) {
  int found = 0;
  (void)v8x_hermes_external_value(rtw, slot, &found);
  return found;
}

// Is the Value in `slot` `undefined`?
int v8x_hermes_value_is_undefined(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  return (v != nullptr && v->isUndefined()) ? 1 : 0;
}

int v8x_hermes_value_is_null(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  return (v != nullptr && v->isNull()) ? 1 : 0;
}

// Uint32 coercion (v8's Value::Uint32Value). JSI has no ToUint32; a Number
// value is truncated to a uint32 the way ECMAScript ToUint32 does for finite
// numbers. Writes the result into *out; returns 1 on success, 0 on error
// (not a number, or a bad slot).
int v8x_hermes_uint32_value(void *rtw, v8x_hermes_slot slot, uint32_t *out) {
  if (rtw == nullptr || out == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isNumber()) {
    return 0;
  }
  try {
    double d = v->getNumber();
    // ECMAScript ToUint32: wrap into the 2^32 ring for finite numbers.
    *out = static_cast<uint32_t>(static_cast<int64_t>(d));
    return 1;
  } catch (...) {
    return 0;
  }
}

// ---- ArrayBuffer + TypedArray (C8) ----------------------------------------
//
// JSI exposes jsi::ArrayBuffer (a data() + size() view over a backing buffer)
// but only lets an embedder READ an existing one; it has no C++ factory to
// allocate a fresh ArrayBuffer of a given byte length. So a new ArrayBuffer is
// created by calling the JS `ArrayBuffer` constructor on the global with the
// byte length, then the backing bytes are reached through
// jsi::ArrayBuffer::data(). Typed arrays are likewise built by calling the JS
// `Uint8Array`/etc constructor with (arraybuffer, byteOffset, length). Every
// entry point keeps the C2 catch-all.

// new ArrayBuffer(byte_length). Returns a slot, or the null slot on error.
v8x_hermes_slot v8x_hermes_array_buffer_new(void *rtw, size_t byte_length) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::Function ctor = w->runtime()
                             .global()
                             .getPropertyAsFunction(w->runtime(), "ArrayBuffer");
    jsi::Value ab = ctor.callAsConstructor(
        w->runtime(),
        jsi::Value(static_cast<double>(byte_length)));
    return w->push(std::move(ab));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// ArrayBuffer.prototype.byteLength as a size_t. Returns SIZE_MAX on error so a
// genuine 0-length buffer is distinguishable from a bad slot.
size_t v8x_hermes_array_buffer_byte_length(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return SIZE_MAX;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return SIZE_MAX;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    if (!obj.isArrayBuffer(w->runtime())) {
      return SIZE_MAX;
    }
    return obj.getArrayBuffer(w->runtime()).size(w->runtime());
  } catch (...) {
    return SIZE_MAX;
  }
}

// Pointer to an ArrayBuffer's backing bytes (jsi::ArrayBuffer::data). Returns
// nullptr on error or for a non-ArrayBuffer slot.
void *v8x_hermes_array_buffer_data(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return nullptr;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return nullptr;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    if (!obj.isArrayBuffer(w->runtime())) {
      return nullptr;
    }
    return obj.getArrayBuffer(w->runtime()).data(w->runtime());
  } catch (...) {
    return nullptr;
  }
}

// new <ctor_name>(arraybuffer, byte_offset, length). `ctor_name` is a NUL-
// terminated JS global constructor name (e.g. "Uint8Array"). Returns a slot
// with the constructed typed array, or the null slot on error (bad slot, the
// buffer slot is not an ArrayBuffer, or the JS constructor threw).
v8x_hermes_slot v8x_hermes_typed_array_new(void *rtw, const char *ctor_name,
                                           v8x_hermes_slot buf_slot,
                                           size_t byte_offset, size_t length) {
  if (rtw == nullptr || ctor_name == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *bv = slot_ref(w, buf_slot);
  if (bv == nullptr || !bv->isObject()) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    jsi::Object buf = bv->getObject(w->runtime());
    if (!buf.isArrayBuffer(w->runtime())) {
      return V8X_HERMES_NULL_SLOT;
    }
    jsi::Function ctor =
        w->runtime().global().getPropertyAsFunction(w->runtime(), ctor_name);
    jsi::Value ta = ctor.callAsConstructor(
        w->runtime(), jsi::Value(w->runtime(), buf),
        jsi::Value(static_cast<double>(byte_offset)),
        jsi::Value(static_cast<double>(length)));
    return w->push(std::move(ta));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// TypedArray.prototype.length. Returns SIZE_MAX on error so a genuine 0-length
// typed array is distinguishable from a bad slot / non-object.
size_t v8x_hermes_typed_array_length(void *rtw, v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return SIZE_MAX;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return SIZE_MAX;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    jsi::Value len = obj.getProperty(w->runtime(), "length");
    if (!len.isNumber()) {
      return SIZE_MAX;
    }
    return static_cast<size_t>(len.getNumber());
  } catch (...) {
    return SIZE_MAX;
  }
}

// ---- TryCatch / exception surfacing (C9) ----------------------------------
//
// v8's TryCatch is a stack-discipline scope: CONSTRUCT pushes a frame,
// DESTRUCT pops it. While one or more frames are on the stack, a thrown
// jsi::JSError from Run/Function::Call/etc is captured into the INNERMOST
// frame (RuntimeWrapper::capture_exception, above). HasCaught/Exception/
// Message read the frame this v8x_hermes_slot-typed opaque handle refers to
// by INDEX (not top-of-stack, so a still-open outer frame can be queried
// after an inner one has already been popped/destructed - see the "rethrow
// and reset" vendored test, which keeps tc1 alive after tc2 is dropped).

// Push a new (empty) TryCatch frame. Returns its index in `tc_stack`, or -1
// on error (bad rtw). The index is what Rust stores in its raw::TryCatch
// buffer and passes back into every other v8x_hermes_trycatch_* call.
int64_t v8x_hermes_trycatch_push(void *rtw) {
  if (rtw == nullptr) {
    return -1;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    w->tc_stack.emplace_back();
    return static_cast<int64_t>(w->tc_stack.size() - 1);
  } catch (...) {
    return -1;
  }
}

// Pop the TryCatch stack down to (and including) `index`. Only ever called
// with `index == tc_stack.size() - 1` in practice (proper LIFO nesting, the
// same discipline HandleScope's watermark-truncate relies on), but tolerates
// a shallower stack defensively (a prior pop already covered it).
void v8x_hermes_trycatch_pop(void *rtw, int64_t index) {
  if (rtw == nullptr || index < 0) {
    return;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  if (static_cast<size_t>(index) >= w->tc_stack.size()) {
    return;
  }
  try {
    w->tc_stack.resize(static_cast<size_t>(index));
  } catch (...) {
  }
}

namespace {
// Safely fetch frame `index`, or nullptr if the wrapper/index is invalid.
TryCatchFrame *tc_frame(RuntimeWrapper *w, int64_t index) {
  if (w == nullptr || index < 0 ||
      static_cast<size_t>(index) >= w->tc_stack.size()) {
    return nullptr;
  }
  return &w->tc_stack[static_cast<size_t>(index)];
}
} // namespace

int v8x_hermes_trycatch_has_caught(void *rtw, int64_t index) {
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  TryCatchFrame *f = tc_frame(w, index);
  return (f != nullptr && f->has_caught) ? 1 : 0;
}

// The caught exception's Value, as a handle-table slot (or the null slot if
// nothing was caught / a bad index).
v8x_hermes_slot v8x_hermes_trycatch_exception(void *rtw, int64_t index) {
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  TryCatchFrame *f = tc_frame(w, index);
  if (f == nullptr || !f->has_caught) {
    return V8X_HERMES_NULL_SLOT;
  }
  return f->exception_slot;
}

// A synthetic v8::Message string ("Uncaught <ctor>: <message>" - matches the
// vendored test's exact expectation for `throw new Error('foo')`, "Uncaught
// Error: foo") pushed as a fresh handle-table String slot. Returns the null
// slot if nothing was caught. JSI's JSError::getMessage() already returns
// just the Error's own `.message` property (e.g. "foo"), and getStack()
// starts with "<ctor-name>: <message>\n    at ..." for a real Error object,
// or is empty for a non-Error thrown primitive (e.g. `throw 'bar'`) - so the
// "Uncaught " prefix is added here, and the ctor-qualified text is taken from
// the first line of the stack when available (falls back to the bare
// message, matching what `throw 'bar'` needs since it has no stack/ctor).
v8x_hermes_slot v8x_hermes_trycatch_message(void *rtw, int64_t index) {
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  TryCatchFrame *f = tc_frame(w, index);
  if (f == nullptr || !f->has_caught) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    std::string first_line = f->stack;
    size_t nl = first_line.find('\n');
    if (nl != std::string::npos) {
      first_line = first_line.substr(0, nl);
    }
    std::string text =
        "Uncaught " + (first_line.empty() ? f->message : first_line);
    jsi::String s = jsi::String::createFromUtf8(
        w->runtime(), reinterpret_cast<const uint8_t *>(text.data()),
        text.size());
    jsi::Value v(w->runtime(), s);
    return w->push(std::move(v));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// A synthetic stack-trace Value (the raw JSError stack string, or the
// message as a fallback for a non-Error throw with no stack). Returns the
// null slot if nothing was caught.
v8x_hermes_slot v8x_hermes_trycatch_stack_trace(void *rtw, int64_t index) {
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  TryCatchFrame *f = tc_frame(w, index);
  if (f == nullptr || !f->has_caught) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    const std::string &text = f->stack.empty() ? f->message : f->stack;
    jsi::String s = jsi::String::createFromUtf8(
        w->runtime(), reinterpret_cast<const uint8_t *>(text.data()),
        text.size());
    jsi::Value v(w->runtime(), s);
    return w->push(std::move(v));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Clears has_caught, UNLESS rethrow() was already called on this frame (a
// documented V8 quirk the vendored test exercises directly: reset() after
// rethrow() must leave has_caught() true). No-op on a bad index.
void v8x_hermes_trycatch_reset(void *rtw, int64_t index) {
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  TryCatchFrame *f = tc_frame(w, index);
  if (f == nullptr || f->rethrown) {
    return;
  }
  f->has_caught = false;
  f->exception_slot = -1;
  f->message.clear();
  f->stack.clear();
}

// Propagate this frame's caught exception to the next-outer live frame (the
// one immediately below `index` in tc_stack), mirroring v8's ReThrow: marks
// the outer frame caught with the SAME exception value, and marks THIS frame
// as rethrown (so a later reset() on it is a no-op, see above). Returns the
// exception's handle-table slot (what Rust's rethrow() surfaces as
// Some(value)), or the null slot if nothing was caught here.
v8x_hermes_slot v8x_hermes_trycatch_rethrow(void *rtw, int64_t index) {
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  TryCatchFrame *f = tc_frame(w, index);
  if (f == nullptr || !f->has_caught) {
    return V8X_HERMES_NULL_SLOT;
  }
  f->rethrown = true;
  if (index > 0) {
    TryCatchFrame *outer = tc_frame(w, index - 1);
    if (outer != nullptr) {
      outer->has_caught = true;
      outer->exception_slot = f->exception_slot;
      outer->message = f->message;
      outer->stack = f->stack;
    }
  }
  return f->exception_slot;
}

// ---- Isolate::ThrowException + Exception::* constructors (C9) ------------
//
// v8's ThrowException is the embedder throwing a value INTO the (about to
// resume) JS execution; the vendored TryCatch tests use it directly on a
// TryCatch-scoped isolate (no intervening JS call), so the simplification
// that matches those tests is: capture the thrown value straight into the
// innermost live TryCatch frame, exactly like a caught jsi::JSError would be
// (there is no separate "isolate-level pending exception" object distinct
// from a TryCatch frame in this model - see the doc comment above
// RuntimeWrapper::tc_stack). If no TryCatch is on the stack, the value is
// dropped (matches V8: an uncaught embedder throw with no handler runs the
// message/fatal-error path, which we do not model).

// Throw `value_slot` as the current exception. Returns 1 if a live TryCatch
// frame accepted it, 0 otherwise (bad rtw/slot, or no live frame - in V8
// terms, "uncaught").
int v8x_hermes_throw_exception(void *rtw, v8x_hermes_slot value_slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, value_slot);
  if (v == nullptr || w->tc_stack.empty()) {
    return 0;
  }
  try {
    TryCatchFrame &frame = w->tc_stack.back();
    frame.has_caught = true;
    frame.message.clear();
    frame.stack.clear();
    // If the thrown value is an Error-like object (has a string `.stack`
    // property - what `Exception::type_error`/`Error`/etc construct via the
    // real JS `Error` constructor, which auto-populates `.stack` as
    // "<ctor-name>: <message>\n    at ..."), capture it so
    // v8x_hermes_trycatch_message can build "Uncaught <ctor>: <message>" the
    // same way a real JS `throw` does. A plain non-Error value (e.g. `throw
    // 'bar'`) has no such property, so message/stack stay empty - matching
    // that case having no "<ctor>: text" line. MUST happen before the push()
    // below: `handles.push_back` can reallocate the vector, invalidating `v`
    // (a pointer into the OLD backing storage) - reading through it after a
    // reallocating push is a use-after-free.
    if (v->isObject()) {
      jsi::Object obj = v->getObject(w->runtime());
      jsi::Value stackVal = obj.getProperty(w->runtime(), "stack");
      if (stackVal.isString()) {
        frame.stack = stackVal.getString(w->runtime()).utf8(w->runtime());
      }
      jsi::Value msgVal = obj.getProperty(w->runtime(), "message");
      if (msgVal.isString()) {
        frame.message = msgVal.getString(w->runtime()).utf8(w->runtime());
      }
    }
    jsi::Value copy(w->runtime(), *v);
    frame.exception_slot = w->push(std::move(copy));
    return 1;
  } catch (...) {
    return 0;
  }
}

// new Error(message) / TypeError / RangeError / ReferenceError / SyntaxError,
// via the JS global constructor (JSI has no C++ Error-subtype factory).
// `ctor_name` is a NUL-terminated JS global constructor name. Returns a slot
// with the constructed (but NOT thrown) Error object, or the null slot on
// error.
v8x_hermes_slot v8x_hermes_exception_new(void *rtw, const char *ctor_name,
                                         v8x_hermes_slot message_slot) {
  if (rtw == nullptr || ctor_name == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *mv = slot_ref(w, message_slot);
  if (mv == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    jsi::Function ctor =
        w->runtime().global().getPropertyAsFunction(w->runtime(), ctor_name);
    jsi::Value err =
        ctor.callAsConstructor(w->runtime(), jsi::Value(w->runtime(), *mv));
    return w->push(std::move(err));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// ---- C11: ObjectTemplate internal fields + accessors ----------------------
//
// JSI objects have no native internal-field slots. Internal fields are modeled
// as a side table stored ON the object itself: a hidden, non-enumerable,
// Symbol-keyed property holding a JS Array of length internal_field_count,
// each slot initialized to `undefined`. This mirrors the C4 identity-hash
// trick (a separate, dedicated Symbol) and naturally survives being read back
// through any number of handle-table slots, since the storage is the object's
// own heap storage, not the (non-canonical) slot index.

namespace {

// Lazily create (once per runtime) the internal-fields Symbol and cache
// Object.getOwnPropertyDescriptor. Reuses the identity infra's
// define_property_fn (same Object.defineProperty), so ensure that is set up
// too. Returns false on any error.
bool ensure_internal_fields_infra(RuntimeWrapper *w) {
  if (w->internal_fields_symbol && w->get_own_property_descriptor_fn) {
    return true;
  }
  if (!ensure_identity_infra(w)) {
    return false;
  }
  try {
    jsi::Value setup = w->runtime().evaluateJavaScript(
        std::make_unique<jsi::StringBuffer>(
            "(function() { return ["
            "Symbol('v8x_internal_fields'),"
            "Object.getOwnPropertyDescriptor"
            "]; })()"),
        "v8x-internal-fields-setup.js");
    jsi::Array arr = setup.getObject(w->runtime()).asArray(w->runtime());
    jsi::Value sym = arr.getValueAtIndex(w->runtime(), 0);
    jsi::Value gopd = arr.getValueAtIndex(w->runtime(), 1);
    w->internal_fields_symbol =
        std::make_unique<jsi::Value>(w->runtime(), sym);
    w->get_own_property_descriptor_fn =
        std::make_unique<jsi::Value>(w->runtime(), gopd);
    return true;
  } catch (...) {
    return false;
  }
}

// Read the internal-fields Array off an object (via the hidden Symbol-keyed
// property), or return an invalid (undefined) Value if the object has none.
// `obj` must be a jsi::Object.
jsi::Value read_internal_fields_array(RuntimeWrapper *w, const jsi::Object &obj) {
  if (!ensure_internal_fields_infra(w)) {
    return jsi::Value::undefined();
  }
  jsi::Function gopd = w->get_own_property_descriptor_fn->getObject(w->runtime())
                           .getFunction(w->runtime());
  jsi::Symbol key = w->internal_fields_symbol->getSymbol(w->runtime());
  jsi::Value desc =
      gopd.call(w->runtime(), obj, jsi::Value(w->runtime(), key));
  if (!desc.isObject()) {
    return jsi::Value::undefined();
  }
  return desc.getObject(w->runtime()).getProperty(w->runtime(), "value");
}

// Install a fresh internal-fields Array of length `count` on `obj` (all slots
// undefined), via the hidden non-enumerable Symbol-keyed property. Returns
// false on error.
bool install_internal_fields(RuntimeWrapper *w, const jsi::Object &obj,
                             int64_t count) {
  if (count < 0 || !ensure_internal_fields_infra(w)) {
    return false;
  }
  try {
    jsi::Array fields(w->runtime(), static_cast<size_t>(count));
    for (int64_t i = 0; i < count; ++i) {
      fields.setValueAtIndex(w->runtime(), static_cast<size_t>(i),
                             jsi::Value::undefined());
    }
    jsi::Function defineProperty =
        w->define_property_fn->getObject(w->runtime()).getFunction(w->runtime());
    jsi::Symbol key = w->internal_fields_symbol->getSymbol(w->runtime());
    jsi::Object desc(w->runtime());
    desc.setProperty(w->runtime(), "value",
                     jsi::Value(w->runtime(), fields));
    desc.setProperty(w->runtime(), "enumerable", jsi::Value(false));
    desc.setProperty(w->runtime(), "writable", jsi::Value(false));
    desc.setProperty(w->runtime(), "configurable", jsi::Value(false));
    defineProperty.call(w->runtime(), obj, jsi::Value(w->runtime(), key),
                        std::move(desc));
    return true;
  } catch (...) {
    return false;
  }
}

} // namespace

extern "C" {

// Create a fresh object with `count` internal-field slots (all undefined),
// stored in a hidden Symbol-keyed Array. Returns a slot, or the null slot on
// error.
v8x_hermes_slot v8x_hermes_object_new_with_internal_fields(void *rtw,
                                                           int64_t count) {
  if (rtw == nullptr) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    jsi::Object o(w->runtime());
    if (count > 0 && !install_internal_fields(w, o, count)) {
      return V8X_HERMES_NULL_SLOT;
    }
    return w->push(jsi::Value(w->runtime(), o));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// Attach `count` internal-field slots to an EXISTING object (used by a
// constructor FunctionCallback path where the object is created by JSI's
// `new`, e.g. instance_template_with_internal_field). Idempotent-ish: if the
// object already has an internal-fields array, it is left as is. Returns 1 on
// success, 0 on error.
int v8x_hermes_object_ensure_internal_fields(void *rtw, v8x_hermes_slot slot,
                                             int64_t count) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return 0;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    jsi::Value existing = read_internal_fields_array(w, obj);
    if (existing.isObject()) {
      return 1; // already installed
    }
    return install_internal_fields(w, obj, count) ? 1 : 0;
  } catch (...) {
    return 0;
  }
}

// The number of internal fields on `slot` (the length of its hidden
// internal-fields Array), or 0 if it has none / is not an object.
int64_t v8x_hermes_object_internal_field_count(void *rtw,
                                               v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return 0;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    jsi::Value fields = read_internal_fields_array(w, obj);
    if (!fields.isObject()) {
      return 0;
    }
    jsi::Object fobj = fields.getObject(w->runtime());
    if (!fobj.isArray(w->runtime())) {
      return 0;
    }
    return static_cast<int64_t>(fobj.asArray(w->runtime()).size(w->runtime()));
  } catch (...) {
    return 0;
  }
}

// internal_fields[index] as a slot. Returns the null slot on error or an
// out-of-range index.
v8x_hermes_slot v8x_hermes_object_get_internal_field(void *rtw,
                                                     v8x_hermes_slot slot,
                                                     int64_t index) {
  if (rtw == nullptr || index < 0) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  if (v == nullptr || !v->isObject()) {
    return V8X_HERMES_NULL_SLOT;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    jsi::Value fields = read_internal_fields_array(w, obj);
    if (!fields.isObject()) {
      return V8X_HERMES_NULL_SLOT;
    }
    jsi::Object fobj = fields.getObject(w->runtime());
    if (!fobj.isArray(w->runtime())) {
      return V8X_HERMES_NULL_SLOT;
    }
    jsi::Array farr = fobj.asArray(w->runtime());
    if (static_cast<size_t>(index) >= farr.size(w->runtime())) {
      return V8X_HERMES_NULL_SLOT;
    }
    jsi::Value elem = farr.getValueAtIndex(w->runtime(),
                                           static_cast<size_t>(index));
    return w->push(std::move(elem));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

// internal_fields[index] = value. Returns 1 on success, 0 on error / an
// out-of-range index.
int v8x_hermes_object_set_internal_field(void *rtw, v8x_hermes_slot slot,
                                         int64_t index,
                                         v8x_hermes_slot value_slot) {
  if (rtw == nullptr || index < 0) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *v = slot_ref(w, slot);
  const jsi::Value *vv = slot_ref(w, value_slot);
  if (v == nullptr || vv == nullptr || !v->isObject()) {
    return 0;
  }
  try {
    jsi::Object obj = v->getObject(w->runtime());
    jsi::Value fields = read_internal_fields_array(w, obj);
    if (!fields.isObject()) {
      return 0;
    }
    jsi::Object fobj = fields.getObject(w->runtime());
    if (!fobj.isArray(w->runtime())) {
      return 0;
    }
    jsi::Array farr = fobj.asArray(w->runtime());
    if (static_cast<size_t>(index) >= farr.size(w->runtime())) {
      return 0;
    }
    farr.setValueAtIndex(w->runtime(), static_cast<size_t>(index),
                         jsi::Value(w->runtime(), *vv));
    return 1;
  } catch (...) {
    return 0;
  }
}

// Object.defineProperty(obj, key, {value, writable, enumerable, configurable})
// with the attribute bits taken from `attr` (v8 PropertyAttribute: bit0
// READ_ONLY -> writable:false, bit1 DONT_ENUM -> enumerable:false, bit2
// DONT_DELETE -> configurable:false). `key` is coerced to a JS string. Returns
// 1 on success, 0 on error.
int v8x_hermes_object_define_property(void *rtw, v8x_hermes_slot obj_slot,
                                      v8x_hermes_slot key_slot,
                                      v8x_hermes_slot value_slot, int attr) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *ov = slot_ref(w, obj_slot);
  const jsi::Value *kv = slot_ref(w, key_slot);
  const jsi::Value *vv = slot_ref(w, value_slot);
  if (ov == nullptr || kv == nullptr || vv == nullptr || !ov->isObject()) {
    return 0;
  }
  if (!ensure_identity_infra(w)) {
    return 0;
  }
  try {
    jsi::Object obj = ov->getObject(w->runtime());
    jsi::String key = kv->toString(w->runtime());
    jsi::Function defineProperty =
        w->define_property_fn->getObject(w->runtime()).getFunction(w->runtime());
    jsi::Object desc(w->runtime());
    desc.setProperty(w->runtime(), "value", jsi::Value(w->runtime(), *vv));
    desc.setProperty(w->runtime(), "writable", jsi::Value((attr & 1) == 0));
    desc.setProperty(w->runtime(), "enumerable", jsi::Value((attr & 2) == 0));
    desc.setProperty(w->runtime(), "configurable", jsi::Value((attr & 4) == 0));
    defineProperty.call(w->runtime(), obj,
                        jsi::Value(w->runtime(), key), std::move(desc));
    return 1;
  } catch (...) {
    return 0;
  }
}

// Object.defineProperty(obj, key, {get, set, enumerable, configurable}) where
// `get`/`set` are native accessor callbacks (v8 NamedGetter/SetterCallback fn
// ptrs, as uintptr_t). Either callback may be 0 (absent). `data_slot` is the
// accessor's optional associated data. `attr` gives enumerable/configurable
// (READ_ONLY is ignored for an accessor property, which has no writable bit).
// The get/set JSI host functions marshal (key, holder=this, data) into fresh
// slots and dispatch back into Rust. Returns 1 on success, 0 on error.
//
// `key` is a Name (string) held in key_slot; it is re-read for each invocation
// from a Runtime-owned copy captured in the host-function closures (NOT a
// handle-table slot, which a HandleScope exit would truncate away, exactly
// like C10's `data`). The holder is the receiver `thisVal` of the accessor
// call (holder == this for a plain own-accessor, no prototype chain).
int v8x_hermes_object_define_accessor(void *rtw, v8x_hermes_slot obj_slot,
                                      v8x_hermes_slot key_slot,
                                      uintptr_t getter_bits,
                                      uintptr_t setter_bits,
                                      v8x_hermes_slot data_slot, int attr) {
  if (rtw == nullptr || (getter_bits == 0 && setter_bits == 0)) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *ov = slot_ref(w, obj_slot);
  const jsi::Value *kv = slot_ref(w, key_slot);
  if (ov == nullptr || kv == nullptr || !ov->isObject()) {
    return 0;
  }
  if (!ensure_identity_infra(w)) {
    return 0;
  }
  try {
    jsi::Object obj = ov->getObject(w->runtime());
    // The property-key Name, copied into a Runtime-owned holder (outlives every
    // HandleScope, like C10's data).
    auto key_holder =
        std::make_shared<jsi::Value>(w->runtime(), *kv);
    // The accessor's associated data, likewise Runtime-owned.
    auto data_holder =
        std::make_shared<jsi::Value>(jsi::Value::undefined());
    const jsi::Value *dv = slot_ref(w, data_slot);
    if (dv != nullptr) {
      *data_holder = jsi::Value(w->runtime(), *dv);
    }

    jsi::Object desc(w->runtime());

    if (getter_bits != 0) {
      auto getFn = [w, getter_bits, key_holder, data_holder](
                       jsi::Runtime &rt, const jsi::Value &thisVal,
                       const jsi::Value *args, size_t count) -> jsi::Value {
        (void)args;
        (void)count;
        size_t watermark = w->handles.size();
        v8x_hermes_slot key_slot_local =
            w->push(jsi::Value(rt, *key_holder));
        v8x_hermes_slot holder_slot = w->push(jsi::Value(rt, thisVal));
        v8x_hermes_slot data_slot_local =
            w->push(jsi::Value(rt, *data_holder));

        int threw = 0;
        int64_t ret_slot = v8x_hermes_dispatch_accessor_getter(
            static_cast<void *>(w), getter_bits, key_slot_local, holder_slot,
            data_slot_local, &threw);

        jsi::Value result = jsi::Value::undefined();
        const jsi::Value *rv = slot_ref(w, ret_slot);
        if (rv != nullptr) {
          result = jsi::Value(rt, *rv);
        }
        bool have_exception = false;
        jsi::Value exception = jsi::Value::undefined();
        if (threw != 0) {
          const jsi::Value *ev = slot_ref(w, w->pending_callback_exception);
          if (ev != nullptr) {
            exception = jsi::Value(rt, *ev);
          }
          have_exception = true;
          w->pending_callback_exception = -1;
        }
        if (w->handles.size() > watermark) {
          w->handles.resize(watermark);
        }
        if (have_exception) {
          throw jsi::JSError(rt, std::move(exception));
        }
        return result;
      };
      jsi::Function getter = jsi::Function::createFromHostFunction(
          w->runtime(),
          jsi::PropNameID::forAscii(w->runtime(), "get"), 0,
          std::move(getFn));
      desc.setProperty(w->runtime(), "get", std::move(getter));
    }

    // A READ_ONLY accessor (attr bit0) drops the setter entirely: assignment
    // becomes a silent no-op in sloppy mode (matches v8's native-data-property
    // ReadOnly semantics, where the setter is not invoked on assignment).
    if (setter_bits != 0 && (attr & 1) == 0) {
      auto setFn = [w, setter_bits, key_holder, data_holder](
                       jsi::Runtime &rt, const jsi::Value &thisVal,
                       const jsi::Value *args, size_t count) -> jsi::Value {
        size_t watermark = w->handles.size();
        v8x_hermes_slot key_slot_local =
            w->push(jsi::Value(rt, *key_holder));
        v8x_hermes_slot value_slot_local = w->push(
            count > 0 ? jsi::Value(rt, args[0]) : jsi::Value::undefined());
        v8x_hermes_slot holder_slot = w->push(jsi::Value(rt, thisVal));
        v8x_hermes_slot data_slot_local =
            w->push(jsi::Value(rt, *data_holder));

        int threw = 0;
        v8x_hermes_dispatch_accessor_setter(
            static_cast<void *>(w), setter_bits, key_slot_local,
            value_slot_local, holder_slot, data_slot_local, &threw);

        bool have_exception = false;
        jsi::Value exception = jsi::Value::undefined();
        if (threw != 0) {
          const jsi::Value *ev = slot_ref(w, w->pending_callback_exception);
          if (ev != nullptr) {
            exception = jsi::Value(rt, *ev);
          }
          have_exception = true;
          w->pending_callback_exception = -1;
        }
        if (w->handles.size() > watermark) {
          w->handles.resize(watermark);
        }
        if (have_exception) {
          throw jsi::JSError(rt, std::move(exception));
        }
        return jsi::Value::undefined();
      };
      jsi::Function setter = jsi::Function::createFromHostFunction(
          w->runtime(),
          jsi::PropNameID::forAscii(w->runtime(), "set"), 1,
          std::move(setFn));
      desc.setProperty(w->runtime(), "set", std::move(setter));
    }

    desc.setProperty(w->runtime(), "enumerable", jsi::Value((attr & 2) == 0));
    desc.setProperty(w->runtime(), "configurable", jsi::Value((attr & 4) == 0));

    jsi::String key = kv->toString(w->runtime());
    jsi::Function defineProperty =
        w->define_property_fn->getObject(w->runtime()).getFunction(w->runtime());
    defineProperty.call(w->runtime(), obj, jsi::Value(w->runtime(), key),
                        std::move(desc));
    return 1;
  } catch (...) {
    return 0;
  }
}

// Object.defineProperty(obj, key, {get, set, enumerable, configurable}) where
// `get`/`set` are already-constructed JS functions (from FunctionTemplate::
// GetFunction), used by ObjectTemplate::SetAccessorProperty. Either function
// slot may be the null slot (absent). Returns 1 on success, 0 on error.
int v8x_hermes_object_define_accessor_fns(void *rtw, v8x_hermes_slot obj_slot,
                                          v8x_hermes_slot key_slot,
                                          v8x_hermes_slot getter_fn_slot,
                                          v8x_hermes_slot setter_fn_slot,
                                          int attr) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *ov = slot_ref(w, obj_slot);
  const jsi::Value *kv = slot_ref(w, key_slot);
  const jsi::Value *gv = slot_ref(w, getter_fn_slot);
  const jsi::Value *sv = slot_ref(w, setter_fn_slot);
  if (ov == nullptr || kv == nullptr || !ov->isObject()) {
    return 0;
  }
  if ((gv == nullptr || !gv->isObject()) &&
      (sv == nullptr || !sv->isObject())) {
    return 0;
  }
  if (!ensure_identity_infra(w)) {
    return 0;
  }
  try {
    jsi::Object obj = ov->getObject(w->runtime());
    jsi::Object desc(w->runtime());
    if (gv != nullptr && gv->isObject()) {
      desc.setProperty(w->runtime(), "get", jsi::Value(w->runtime(), *gv));
    }
    if (sv != nullptr && sv->isObject()) {
      desc.setProperty(w->runtime(), "set", jsi::Value(w->runtime(), *sv));
    }
    desc.setProperty(w->runtime(), "enumerable", jsi::Value((attr & 2) == 0));
    desc.setProperty(w->runtime(), "configurable", jsi::Value((attr & 4) == 0));
    jsi::String key = kv->toString(w->runtime());
    jsi::Function defineProperty =
        w->define_property_fn->getObject(w->runtime()).getFunction(w->runtime());
    defineProperty.call(w->runtime(), obj, jsi::Value(w->runtime(), key),
                        std::move(desc));
    return 1;
  } catch (...) {
    return 0;
  }
}

// Set `obj`'s prototype to `ctor.prototype` (so `obj.constructor` resolves to
// `ctor` and `obj.constructor.name` is the ctor's name). Used by
// ObjectTemplate::new_from_template to link an instance to its source
// FunctionTemplate's constructor. Returns 1 on success, 0 on error.
int v8x_hermes_set_prototype_from_ctor(void *rtw, v8x_hermes_slot obj_slot,
                                       v8x_hermes_slot ctor_slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *ov = slot_ref(w, obj_slot);
  const jsi::Value *cv = slot_ref(w, ctor_slot);
  if (ov == nullptr || cv == nullptr || !ov->isObject() || !cv->isObject()) {
    return 0;
  }
  try {
    jsi::Object obj = ov->getObject(w->runtime());
    jsi::Object ctor = cv->getObject(w->runtime());
    jsi::Value proto = ctor.getProperty(w->runtime(), "prototype");
    if (!proto.isObject()) {
      return 0;
    }
    jsi::Function setProto = w->runtime()
                                 .global()
                                 .getPropertyAsObject(w->runtime(), "Object")
                                 .getPropertyAsFunction(w->runtime(),
                                                        "setPrototypeOf");
    setProto.call(w->runtime(), obj, std::move(proto));
    return 1;
  } catch (...) {
    return 0;
  }
}

// Function.prototype.name / the `name` property of a JS function value. Used to
// implement FunctionTemplate::SetClassName by setting the constructed
// function's `.name` (via defineProperty, since `name` is non-writable). This
// entry point sets `fn.name = name_string` on an existing function slot.
// Returns 1 on success, 0 on error.
int v8x_hermes_function_set_name(void *rtw, v8x_hermes_slot fn_slot,
                                 v8x_hermes_slot name_slot) {
  if (rtw == nullptr) {
    return 0;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  const jsi::Value *fv = slot_ref(w, fn_slot);
  const jsi::Value *nv = slot_ref(w, name_slot);
  if (fv == nullptr || nv == nullptr || !fv->isObject()) {
    return 0;
  }
  if (!ensure_identity_infra(w)) {
    return 0;
  }
  try {
    jsi::Object fn = fv->getObject(w->runtime());
    jsi::Function defineProperty =
        w->define_property_fn->getObject(w->runtime()).getFunction(w->runtime());
    jsi::Object desc(w->runtime());
    desc.setProperty(w->runtime(), "value", jsi::Value(w->runtime(), *nv));
    desc.setProperty(w->runtime(), "writable", jsi::Value(false));
    desc.setProperty(w->runtime(), "enumerable", jsi::Value(false));
    desc.setProperty(w->runtime(), "configurable", jsi::Value(true));
    jsi::String nameKey = jsi::String::createFromAscii(w->runtime(), "name");
    defineProperty.call(w->runtime(), fn, jsi::Value(w->runtime(), nameKey),
                        std::move(desc));
    return 1;
  } catch (...) {
    return 0;
  }
}

} // extern "C"

// ---- C10: native function callbacks ---------------------------------------

// Record a pending exception (a handle-table slot) that a native
// FunctionCallback threw. The host-function trampoline reads and clears it,
// then re-throws it as a jsi::JSError so it propagates through JSI.
void v8x_hermes_set_pending_callback_exception(void *rtw,
                                               v8x_hermes_slot slot) {
  if (rtw == nullptr) {
    return;
  }
  static_cast<RuntimeWrapper *>(rtw)->pending_callback_exception = slot;
}

// Create a JS function backed by a native v8 FunctionCallback. `callback_bits`
// is the FunctionCallback fn ptr as a uintptr_t; `data_slot` is an optional
// handle-table slot for the callback's `data` (or -1 for none); `length` is
// the reported arity; `name` is an optional NUL-terminated function name.
// Returns a slot holding the new jsi::Function, or the null slot on error.
//
// The callback's `data` must outlive every HandleScope, so it is copied into a
// std::shared_ptr<jsi::Value> owned by the host function (NOT kept as a raw
// handle-table slot, which any HandleScope exit would truncate away). The
// shared_ptr is destroyed when JSI releases the host function, while the
// Runtime is still alive (JSI guarantees host-function teardown precedes
// Runtime teardown).
//
// C11: `instance_internal_field_count` doubles as a template marker. A value
// >= 0 means the function came from a FunctionTemplate: it is made
// constructable (see the wrapper below) and, on `new`, its receiver is given
// that many internal-field slots BEFORE the callback runs, so a constructor
// callback can `this.set_internal_field(...)` (matches
// instance_template_with_internal_field). A value < 0 (-1) means a plain
// Function::new: an ordinary non-constructable host function, unchanged from
// C10.
v8x_hermes_slot v8x_hermes_function_new(void *rtw, uintptr_t callback_bits,
                                        v8x_hermes_slot data_slot,
                                        int32_t length, const char *name,
                                        int64_t instance_internal_field_count) {
  if (rtw == nullptr || callback_bits == 0) {
    return V8X_HERMES_NULL_SLOT;
  }
  auto *w = static_cast<RuntimeWrapper *>(rtw);
  try {
    // Copy `data` into a Runtime-owned, HandleScope-independent holder.
    auto data = std::make_shared<jsi::Value>(jsi::Value::undefined());
    const jsi::Value *dv = slot_ref(w, data_slot);
    if (dv != nullptr) {
      *data = jsi::Value(w->runtime(), *dv);
    }

    std::string fn_name = (name != nullptr) ? std::string(name) : std::string();
    jsi::PropNameID propName = jsi::PropNameID::forUtf8(
        w->runtime(), fn_name.empty() ? std::string("anonymous") : fn_name);

    unsigned int paramCount =
        static_cast<unsigned int>(length < 0 ? 0 : length);
    int64_t ifc = instance_internal_field_count;

    auto hostFn = [w, callback_bits, data, ifc](
                      jsi::Runtime &rt, const jsi::Value &thisVal,
                      const jsi::Value *args, size_t count) -> jsi::Value {
      // Marshal `this`, `data`, and each arg into fresh handle-table slots.
      // Everything pushed here (plus anything the callback interns) is released
      // by truncating back to `watermark` afterwards, emulating v8's implicit
      // per-callback HandleScope.
      size_t watermark = w->handles.size();

      v8x_hermes_slot this_slot = w->push(jsi::Value(rt, thisVal));
      // Constructor path: give the new receiver its declared internal-field
      // slots before the callback runs (idempotent: no-op if already present).
      if (ifc > 0 && thisVal.isObject()) {
        v8x_hermes_object_ensure_internal_fields(static_cast<void *>(w),
                                                 this_slot, ifc);
      }
      v8x_hermes_slot data_slot_local = w->push(jsi::Value(rt, *data));

      std::vector<v8x_hermes_slot> arg_slots;
      arg_slots.reserve(count);
      for (size_t i = 0; i < count; ++i) {
        arg_slots.push_back(w->push(jsi::Value(rt, args[i])));
      }

      int threw = 0;
      int64_t ret_slot = v8x_hermes_dispatch_callback(
          static_cast<void *>(w), callback_bits, this_slot, data_slot_local,
          arg_slots.empty() ? nullptr : arg_slots.data(), count,
          /*is_construct=*/0, /*new_target_slot=*/V8X_HERMES_NULL_SLOT, &threw);

      // Materialize the result (and any pending exception) BEFORE truncating
      // the handle table: both live in slots we are about to release.
      jsi::Value result = jsi::Value::undefined();
      const jsi::Value *rv = slot_ref(w, ret_slot);
      if (rv != nullptr) {
        result = jsi::Value(rt, *rv);
      }

      bool have_exception = false;
      jsi::Value exception = jsi::Value::undefined();
      if (threw != 0) {
        const jsi::Value *ev =
            slot_ref(w, w->pending_callback_exception);
        if (ev != nullptr) {
          exception = jsi::Value(rt, *ev);
          have_exception = true;
        } else {
          have_exception = true; // threw with no value: still surface an error
        }
        w->pending_callback_exception = -1;
      }

      // Release everything the callback added.
      if (w->handles.size() > watermark) {
        w->handles.resize(watermark);
      }

      if (have_exception) {
        throw jsi::JSError(rt, std::move(exception));
      }
      return result;
    };

    jsi::Function fn = jsi::Function::createFromHostFunction(
        w->runtime(), propName, paramCount, std::move(hostFn));

    // A JSI host function (createFromHostFunction) is NOT constructable: `new
    // f()` throws "not a constructor". A v8 FunctionTemplate's function IS a
    // constructor. When this function came from a template
    // (instance_internal_field_count >= 0), wrap the host function in a real JS
    // function that forwards `this`/args to the host impl, so `new Ctor()`
    // works: the wrapper is an ordinary JS function (constructable), and inside
    // a `new` call its `this` is the freshly-constructed object, which is
    // passed through to the host impl (where the internal-field setup and the
    // constructor callback run). If the impl returns an object (v8 constructor
    // callbacks return `this`), that object becomes the `new` result.
    if (instance_internal_field_count >= 0) {
      jsi::Function wrapMaker =
          w->runtime()
              .global()
              .getPropertyAsFunction(w->runtime(), "Function")
              .callAsConstructor(w->runtime(),
                                 jsi::String::createFromAscii(w->runtime(),
                                                              "impl"),
                                 jsi::String::createFromAscii(
                                     w->runtime(),
                                     "return function(){ return "
                                     "impl.apply(this, arguments); };"))
              .getObject(w->runtime())
              .getFunction(w->runtime());
      jsi::Value wrapped = wrapMaker.call(w->runtime(),
                                          jsi::Value(w->runtime(), fn));
      return w->push(std::move(wrapped));
    }

    return w->push(jsi::Value(std::move(fn)));
  } catch (...) {
    return V8X_HERMES_NULL_SLOT;
  }
}

} // extern "C"
