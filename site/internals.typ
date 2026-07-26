#import "./shim/html.typ": *

#set document(
  title: "internals — v8x",
  description: "How v8x implements the v8 C ABI on JavaScriptCore and QuickJS: vendored rusty_v8, ~570 shim symbols, and the linker as a todo list.",
)

#show: html-shim

= Internals

== The trick: keep the Rust, replace the C

rusty_v8 is two layers: a Rust API surface, and \~570 `extern "C"` bindings
(`v8__Isolate__New`, `v8__String__Utf8Length`, …) that a C++ file implements
against real V8. `v8x` vendors the Rust layer *verbatim* — only 3 files
diverge from upstream, enforced by a sync tool — and reimplements the C
layer on a different engine:

#table(
  columns: 2,
  [*layer*], [*where it lives*],
  [vendored rusty_v8 Rust source (unmodified API)], [`vendor/rusty_v8/`],
  [JSC backend: `v8__*` implemented on JavaScriptCore's C API], [`src/jsc/`],
  [QuickJS backend: `v8__*` implemented on quickjs-ng], [`src/quickjs/`],
)

Because the Rust surface is bit-for-bit the crates.io `v8` crate, everything
downstream — `deno_core`, Deno, your crate — compiles without a single
source change.

== Core object mapping (JSC)

#table(
  columns: 2,
  [*v8 concept*], [*JSC implementation*],
  [`v8::Local<T>`], [`JSValueRef` (tagged, GC-owned)],
  [`v8::Isolate`], [`JSContextGroup` + bookkeeping],
  [`v8::HandleScope`], [protect/unprotect bridge into JSC's GC],
  [current `Context`], [thread-local current-context stack],
)

QuickJS follows the same shape with `JSValue`/`JSRuntime`/`JSContext`, plus
reference-count discipline instead of protect/unprotect.

== The linker is the todo list

A missing `v8__*` symbol is a link error, not a runtime surprise. That makes
the linker a free completeness checker: point a real workload (Deno, a test
suite) at a backend, collect the undefined symbols, implement them. Stubs
are legal first moves — a stubbed symbol turns a build failure into a
failing test, which is progress the #link("hill-climb")[hill climb] can
measure.

== Engines are submodules + patches

WebKit and quickjs-ng are git submodules, patched from `patches/` at build
time. Engine fixes that upstream can take get PR'd upstream (several are
landed or open against quickjs-ng); the patch stack shrinks over time.

== Beyond the basics

Some subsystems don't map 1:1 and get real implementations rather than
shims:

- *Inspector / CDP* — `deno repl` works because the backend implements the
  V8-inspector protocol (CDP `Runtime` domain) for real, not as a stub.
- *Snapshots* — V8 startup snapshots have no JSC/QuickJS equivalent, so the
  QuickJS backend implements `SnapshotCreator` as a record/replay tape of
  engine operations.
- *napi* — native Node addons (like `@next/swc`) load and run through the
  same C ABI.
