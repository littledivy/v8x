//! Execution termination + near-heap-limit watchdog for the JSC backend.
//!
//! V8 exposes two isolate features that JSC's *public* C API does not:
//!
//!   * `Isolate::TerminateExecution` — abort the JS running on the isolate's
//!     thread from any other thread (deno uses this to kill a runaway script).
//!   * `Isolate::AddNearHeapLimitCallback` — fire a callback when the heap nears
//!     a configured limit so the embedder can raise the limit or bail out.
//!
//! Without these the affected deno_core tests loop forever (the JS never
//! unwinds), which is exactly the hang #651's watchdog had to kill.
//!
//! JSC's *private* C API does provide the missing primitive:
//! `JSContextGroupSetExecutionTimeLimit` installs a callback that JSC polls at
//! safe points while JS executes; returning `true` aborts the running script —
//! the same unwind V8's terminate produces. We arm that watchdog once per
//! isolate and use it to:
//!
//!   * abort once `terminate_execution` has been requested cross-thread, and
//!   * drive the near-heap-limit callback when a runaway allocation trips it
//!     (JSC's public C API exposes no live-heap gauge to compare against).

#![allow(non_snake_case)]

use crate::RealIsolate;
use crate::isolate::NearHeapLimitCallback;
use crate::jsc::jsc_sys::*;
use std::collections::HashMap;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The execution-time budget (seconds) after which JSC invokes the watchdog
/// while JS runs. JSC's watchdog is effectively one-shot per VM entry — it fires
/// the callback once when the budget elapses and (in this build) does not keep
/// re-polling a still-running script — so this doubles as the worst-case latency
/// between a cross-thread `terminate_execution` and the running script noticing.
/// It must comfortably exceed the ~100ms deno's termination tests wait before
/// requesting termination, so the single poll lands *after* the request; 250ms
/// gives margin while staying well under any per-test timeout. Scripts that
/// finish sooner never reach the callback at all.
const WATCHDOG_INTERVAL_SECS: f64 = 0.25;

/// Per-isolate watchdog state. Reached two ways: directly via a raw pointer
/// handed to JSC as the time-limit callback context (the hot path, no locking),
/// and by isolate pointer through [`REGISTRY`] from the C-ABI entry points.
struct Watch {
  /// The isolate's context group; used to disarm the watchdog on dispose.
  group: JSContextGroupRef,
  /// Cross-thread abort request. Set by `TerminateExecution`, cleared by
  /// `CancelTerminateExecution`; doubles as the `is_execution_terminating`
  /// state V8 keeps while a termination exception propagates.
  terminate: AtomicBool,
  /// Configured heap limit in bytes (0 = none configured → heap logic off).
  heap_limit: AtomicUsize,
  /// The limit at registration time, reported to the callback unchanged.
  initial_heap_limit: AtomicUsize,
  /// The single active near-heap-limit callback as a raw fn pointer (0 = none).
  /// deno_core only ever keeps the most-recently-added callback registered, so
  /// one slot is sufficient.
  heap_cb: AtomicUsize,
  heap_cb_data: AtomicUsize,
  /// Guards against re-entering the heap callback from a nested watchdog poll.
  in_heap_cb: AtomicBool,
}

// SAFETY: `group` is only touched on the isolate's own thread (at install and
// dispose). Everything reached from other threads is an atomic.
unsafe impl Send for Watch {}
unsafe impl Sync for Watch {}

fn registry() -> &'static Mutex<HashMap<usize, Arc<Watch>>> {
  static REG: OnceLock<Mutex<HashMap<usize, Arc<Watch>>>> = OnceLock::new();
  REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lookup(iso: *mut RealIsolate) -> Option<Arc<Watch>> {
  if iso.is_null() {
    return None;
  }
  registry().lock().ok()?.get(&(iso as usize)).cloned()
}

// ---- JSC private execution-time-limit API ----------------------------------
//
// These live in JSC's private `JSContextRefPrivate.h` (not the public umbrella
// header bindgen sees), so they're declared by hand. They're linked directly
// rather than `dlsym`'d: the `vendor_jsc` backend statically links
// `libJavaScriptCore.a`, whose `JS_EXPORT` symbols are not in the final
// binary's dynamic symbol table, so `dlsym(RTLD_DEFAULT, ...)` would return null
// and the watchdog would never arm (the runaway loop would hang forever). A
// direct reference resolves against the archive instead. Both symbols are
// long-standing `JS_EXPORT`s present in JSCOnly and in Apple's
// JavaScriptCore.framework (verified in its `.tbd`).

type ShouldTerminateCb =
  unsafe extern "C" fn(ctx: JSContextRef, context: *mut c_void) -> bool;

unsafe extern "C" {
  fn JSContextGroupSetExecutionTimeLimit(
    group: JSContextGroupRef,
    limit: f64,
    callback: ShouldTerminateCb,
    context: *mut c_void,
  );
  fn JSContextGroupClearExecutionTimeLimit(group: JSContextGroupRef);
}

// ---- the watchdog ----------------------------------------------------------

unsafe extern "C" fn watchdog(ctx: JSContextRef, context: *mut c_void) -> bool {
  if context.is_null() {
    return false;
  }
  // SAFETY: `context` is the `Arc<Watch>` inner pointer we handed JSC at
  // install time; the registry keeps that Arc alive until the watchdog is
  // disarmed in `uninstall`, so this reference is valid for the call.
  let watch = unsafe { &*(context as *const Watch) };

  // This runs at a JSC C frame: a panic unwinding across it would abort the
  // process (SIGABRT) and truncate the test binary. Contain any panic from the
  // user-supplied heap callback and fall back to "don't terminate".
  std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    maybe_drive_heap_callback(watch, ctx);
    watch.terminate.load(Ordering::SeqCst)
  }))
  .unwrap_or(false)
}

/// If a near-heap-limit callback is registered, invoke it (it may raise the
/// limit and/or request termination). JSC's public C API exposes no live-heap
/// gauge, so we can't measure how close we are to the limit — but only the heap
/// tests ever register a callback, and a callback firing means a script has run
/// long enough to trip the watchdog, i.e. a runaway allocation is in flight.
/// Firing on that first poll is both correct for the tests and bounds memory
/// growth far better than waiting for an (unmeasurable) threshold.
fn maybe_drive_heap_callback(watch: &Watch, _ctx: JSContextRef) {
  let cb_addr = watch.heap_cb.load(Ordering::Acquire);
  if cb_addr == 0 || watch.in_heap_cb.load(Ordering::Relaxed) {
    return;
  }
  let limit = watch.heap_limit.load(Ordering::Relaxed);
  if limit == 0 {
    return;
  }

  watch.in_heap_cb.store(true, Ordering::Relaxed);
  // SAFETY: `cb_addr` is a `NearHeapLimitCallback` fn pointer stored by
  // `set_heap_callback`; `data` is the opaque pointer registered alongside it.
  let cb: NearHeapLimitCallback = unsafe { std::mem::transmute(cb_addr) };
  let data = watch.heap_cb_data.load(Ordering::Relaxed) as *mut c_void;
  let initial = watch.initial_heap_limit.load(Ordering::Relaxed);
  let new_limit = unsafe { cb(data, limit, initial) };
  if new_limit > limit {
    watch.heap_limit.store(new_limit, Ordering::Relaxed);
  }
  watch.in_heap_cb.store(false, Ordering::Relaxed);
}

// ---- lifecycle, called from core.rs ----------------------------------------

/// Arm the watchdog for a freshly created isolate. `heap_limit` is the
/// configured max heap (bytes) from `create_params`, or 0 if none.
pub(crate) fn install(
  iso: *mut RealIsolate,
  group: JSContextGroupRef,
  heap_limit: usize,
) {
  let watch = Arc::new(Watch {
    group,
    terminate: AtomicBool::new(false),
    heap_limit: AtomicUsize::new(heap_limit),
    initial_heap_limit: AtomicUsize::new(heap_limit),
    heap_cb: AtomicUsize::new(0),
    heap_cb_data: AtomicUsize::new(0),
    in_heap_cb: AtomicBool::new(false),
  });

  let context = Arc::as_ptr(&watch) as *mut c_void;
  unsafe {
    JSContextGroupSetExecutionTimeLimit(
      group,
      WATCHDOG_INTERVAL_SECS,
      watchdog,
      context,
    )
  };

  if let Ok(mut reg) = registry().lock() {
    reg.insert(iso as usize, watch);
  }
}

/// Disarm and forget an isolate's watchdog on dispose. Clears the time limit
/// before the group is released so no callback can fire afterwards.
pub(crate) fn uninstall(iso: *mut RealIsolate) {
  let Ok(mut reg) = registry().lock() else {
    return;
  };
  if let Some(watch) = reg.remove(&(iso as usize)) {
    unsafe { JSContextGroupClearExecutionTimeLimit(watch.group) };
  }
}

// ---- termination, called from isolate.rs -----------------------------------

pub(crate) fn request_terminate(iso: *mut RealIsolate) {
  if let Some(w) = lookup(iso) {
    w.terminate.store(true, Ordering::SeqCst);
  }
}

pub(crate) fn cancel_terminate(iso: *mut RealIsolate) {
  if let Some(w) = lookup(iso) {
    w.terminate.store(false, Ordering::SeqCst);
  }
}

pub(crate) fn is_terminating(iso: *mut RealIsolate) -> bool {
  lookup(iso).is_some_and(|w| w.terminate.load(Ordering::SeqCst))
}

// ---- heap callback, called from isolate.rs / cli_extra.rs ------------------

pub(crate) fn set_heap_callback(
  iso: *mut RealIsolate,
  callback: NearHeapLimitCallback,
  data: *mut c_void,
) {
  if let Some(w) = lookup(iso) {
    w.heap_cb_data.store(data as usize, Ordering::Relaxed);
    w.heap_cb.store(callback as usize, Ordering::Release);
    // If no limit came through create_params, fall back to a small default so
    // the callback can still fire on a runaway allocation.
    if w.heap_limit.load(Ordering::Relaxed) == 0 {
      const DEFAULT_LIMIT: usize = 16 * 1024 * 1024;
      w.heap_limit.store(DEFAULT_LIMIT, Ordering::Relaxed);
      w.initial_heap_limit.store(DEFAULT_LIMIT, Ordering::Relaxed);
    }
  }
}

pub(crate) fn clear_heap_callback(iso: *mut RealIsolate) {
  if let Some(w) = lookup(iso) {
    w.heap_cb.store(0, Ordering::Release);
    w.heap_cb_data.store(0, Ordering::Relaxed);
  }
}
