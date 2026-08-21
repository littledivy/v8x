//! Porffor engine backend for the v8 C-ABI compat layer.
//!
//! ## What makes this backend different
//!
//! JSC, QuickJS and Hermes are all interpreters: hand them a source string at
//! runtime and they run it. Porffor is an **ahead-of-time compiler** — it
//! lowers JS to C and hands that to a C compiler, with nothing interpreted or
//! JIT-compiled. There is no runtime `eval`, so `v8__Script__Compile` cannot
//! mean "compile this string now". It can only resolve source that was already
//! compiled into the binary, which is why this backend targets flattened
//! bundles (a `celld` Wrangler bundle, a `deno compile` graph) rather than
//! arbitrary runtime input.
//!
//! ## Handle representation
//!
//! Same problem QuickJS has, same solution. A porffor `jsval` is
//! `{ f64 val; i32 type; }` — 16 bytes, not a pointer — so it cannot itself be
//! a `Local<T>` (which the vendored source treats as `*const T`). Handles are
//! therefore arena slots and the slot's address is the v8 handle.
//!
//! Where QuickJS's arena discipline is about **refcounts** (each slot owns
//! exactly one), porffor's is about **GC roots**: porffor has a tracing
//! collector, so every live slot must be reachable as a root. The arena lives
//! on the porffor side (`porf_embed_scope_enter` / `porf_embed_handle_new`),
//! registered with the collector in whole blocks rather than per handle, so
//! entering and leaving a scope costs no per-handle bookkeeping.
//!
//! ## Build shape
//!
//! `engine_porffor` alone gives a pure-Rust stub backend that links with zero
//! porffor dependency (same scaffold pattern as the Hermes spike). Adding
//! `link_porffor` makes build.rs run `porf c --lib` over `seed.js` and link the
//! resulting object, at which point `core` provides real implementations.
//!
//! Because porffor only emits the builtins a compilation actually references,
//! **`seed.js` is the ABI contract**: whatever it mentions is what this backend
//! can reach.

// The real backend (isolate / context / handle scope / primitives / strings /
// objects / functions) needs the porffor runtime object to link against, so it
// only exists under `link_porffor`. Without it the generated stubs in `shims`
// cover those symbols instead.
#[cfg(feature = "link_porffor")]
mod core;
#[cfg(feature = "link_porffor")]
mod init;

// v8::ArrayBuffer::Allocator. The QuickJS backend's implementation has zero
// engine coupling (plain calloc/free plus the Rust allocator vtable), so reuse
// it rather than keeping a second copy in step.
#[cfg(feature = "link_porffor")]
#[path = "../quickjs/allocator.rs"]
mod allocator;
mod misc;
mod shims;

// Pure-Rust simdutf__* C-ABI surface (UTF validation/conversion/base64) that
// the vendored rusty_v8 `simdutf` binding references. It is engine-independent,
// so reuse the implementation authored for the quickjs backend rather than
// re-stubbing it.
#[cfg(feature = "simdutf")]
#[path = "../quickjs/simdutf.rs"]
mod simdutf;
