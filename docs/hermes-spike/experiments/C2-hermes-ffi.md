# C2: Hermes FFI feasibility proof (the go/no-go)

**Result: YES.** Rust evaluates JavaScript on a real Hermes runtime, through an
extern "C" C++ shim linking a real libhermes, and reads the number back. The
test asserts `40 + 2 == 42` and prints it. A thrown JS error is caught at the
boundary and mapped to a sentinel. This de-risks the single hardest part of the
whole engine_hermes idea: Hermes has no C ABI, only a C++-only JSI, so every
`v8__*` symbol must eventually be authored in C++ against `jsi::Runtime` and
exported `extern "C"`. That boundary now provably works end to end.

## How libhermes was obtained (prebuilt, no source build)

- Source: `facebook/hermes` GitHub release **v0.11.0**, asset
  `hermes-runtime-darwin-v0.11.0.tar.gz` (462 MB download, mostly dSYM debug
  symbols and iOS device/simulator slices).
- The tarball ships a **native macOS `hermes.framework`** at
  `destroot/Library/Frameworks/macosx/hermes.framework` plus the full JSI +
  hermes headers at `destroot/include`. Only those two were extracted.
- Vendored into the repo at `vendor/hermes/` (4.5 MB total):
  - `hermes.framework` — universal dylib (arm64 + x86_64), 4.3 MB.
  - `include/{jsi,hermes}` — 260 KB of headers.
  - `HERMES_VERSION` — `v0.11.0` provenance stamp.
- No CMake/LLVM source build was needed, so the 7.6 GB free-disk constraint was
  never in danger. (The `hermes-engine` npm package was checked first: it ships
  `hermesc` and Android `.so` runtimes but **no** macOS-host linkable library,
  so it is the wrong artifact for host embedding. The `hermes-runtime-darwin`
  release asset is the right one.)

## The shim (src/hermes/hermes_eval_shim.cpp)

```
extern "C" int32_t v8x_hermes_smoke_eval(const char* src)
```

makeHermesRuntime -> evaluateJavaScript(src) -> asNumber -> return as int32.
Every C++ exception is caught before it can unwind into Rust. A `jsi::JSError`
maps to sentinel `-2000`, a non-number result to `-1000`, any other C++ throw
to `-3000`.

Rust side (`src/hermes/mod.rs`, gated on `link_hermes`): an `unsafe extern "C"`
declaration, a safe `smoke_eval(&str) -> i32` wrapper (CString marshalling),
and two `#[cfg(all(test, feature = "link_hermes"))]` unit tests.

## Load-bearing correctness finding: Runtime must outlive JSError

The first working shim declared the `std::unique_ptr<jsi::Runtime>` **inside**
the `try` block. Evaluating a throwing script then crashed with `EXC_BAD_ACCESS`
(NULL vtable deref) in `facebook::jsi::Value::~Value()`, on the JSError catch
path (`__cxa_decrement_exception_refcount -> JSError::~JSError`). Root cause: a
`jsi::JSError` embeds a `jsi::Value` whose `PointerValue` destructor calls back
into the owning `Runtime`; when the runtime unique_ptr is scoped inside the
`try`, it is destroyed **before** the in-flight exception object's destructor
runs, so that destructor dereferences a freed runtime.

Fix: declare `rt` in an outer scope so it strictly outlives any JSError
produced by the eval. This is not a shim quirk; it is a JSI lifetime rule that
every future `v8__*` exception-translation site must honor (the caught JS error
and its Values must be destroyed while their Runtime is still alive, on the
creating thread).

Secondary observation: two live `HermesRuntime`s on separate threads
simultaneously can crash (Hermes uses thread-local runtime state). The unit
tests are safe because each drops its runtime promptly; a real multi-isolate
backend must pin one runtime per thread or serialize creation.

## Build wiring (build.rs `fn build_hermes`)

Gated on `CARGO_FEATURE_LINK_HERMES`. Modeled on the WAMR/QuickJS `cc` glue:

- `cc::Build::new().cpp(true).std("c++17").file(shim).include(inc_dir)
  .compile("v8x_hermes_shim")`.
- Link flags emitted:
  - `cargo:rustc-link-search=framework=<vendor/hermes or HERMES_LIB_DIR>`
  - `cargo:rustc-link-lib=framework=hermes`
  - `cargo:rustc-link-lib=c++`
  - `cargo:rustc-link-arg=-Wl,-rpath,<framework dir>` (the framework is a dylib;
    the rpath lets the test binary find it at run time without
    `DYLD_FRAMEWORK_PATH`).
- Honors `HERMES_LIB_DIR` (dir containing `hermes.framework`) and
  `HERMES_INCLUDE_DIR` (dir containing `jsi/` and `hermes/`) overrides.
- macOS-only for now (panics on other targets); the prebuilt is a macOS
  framework. A Linux path would link `hermes-runtime-android`'s host build or a
  from-source static lib.

## Feature layout

`hermes = ["engine_hermes"]` stays a pure-Rust stub build (unchanged, zero
native dep). The real Hermes link is opt-in via the extra `link_hermes` flag:

```
cargo test --no-default-features --features hermes,link_hermes --lib hermes_smoke -- --nocapture
```

(Deliberately kept separate so the cheap stub scaffold build is never coupled to
having libhermes present.) `link_hermes` also required 14 scope/platform/
allocator stub symbols the generator had missed
(`v8__HandleScope__CONSTRUCT`, `v8__Context__Enter/Exit`, `v8__EscapeSlot__
reserve`, `v8__TryCatch__CONSTRUCT`, the `std__shared_ptr__v8__Platform__*` and
`__ArrayBuffer__Allocator__*` helpers); they are referenced by the lib-test
binary where the plain rlib build dead-strips them. Appended to
`src/hermes/shims.rs`.

## Verified output

```
running 2 tests
test hermes::tests::hermes_smoke_catches_js_error ... hermes eval throwing = -2000
ok
test hermes::tests::hermes_smoke_eval_40_plus_2 ... hermes eval "40 + 2" = 42
ok
test result: ok. 2 passed; 0 failed
```

No regressions: `cargo build --features hermes` (stub) and
`cargo build --features quickjs` (default backend) both still compile clean.

## Recommended next step (C3)

The boundary is proven; build the real backend as a clone of `src/quickjs/`'s
arena-handle shape (Hermes JSI `Value` is a 16-byte struct like QuickJS's
`JSValue`, not JSC pointer-punning). Start with the 9-symbol hello-world path
(Isolate__New/Enter, HandleScope__CONSTRUCT, Context__New, String__NewFromUtf8,
Script__Compile, Script__Run, read-string) authored in C++ against JSI and
exported extern "C", each wrapped in the same catch-all boundary this shim
established. Carry forward the two hard rules this experiment surfaced: (1) the
Runtime must outlive every JSI Value and JSError derived from it, and (2) one
runtime per thread. The deepest remaining risk, unchanged from C0, is object
identity: JSI hands out no raw pointer, so every V8 `Value*`/`Object*` identity,
hash, Map/Set, and Global slot must reroute to `strictEquals` or canonicalize to
one interned slot per object. That is the next thing to de-risk after the
hello-world path links.
```
