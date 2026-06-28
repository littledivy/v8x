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
//!   * drive the near-heap-limit callback when the live heap (read via the
//!     private `JSGetMemoryUsageStatistics`) crosses the configured limit.
//!
//! Every private symbol is resolved through `dlsym` instead of being linked
//! directly: if a particular JSC build doesn't export one, the feature simply
//! degrades to a no-op rather than failing the link — a link failure would zero
//! the entire test binary and regress the baseline.

#![allow(non_snake_case)]

use crate::RealIsolate;
use crate::isolate::NearHeapLimitCallback;
use crate::jsc::jsc_sys::*;
use std::collections::HashMap;
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// How often (seconds) JSC polls the watchdog while JS runs. Small enough that
/// a cross-thread terminate unwinds promptly and a runaway allocation is caught
/// long before it can exhaust memory, large enough not to burden normal scripts
/// (which finish in well under this and never reach the callback).
const WATCHDOG_INTERVAL_SECS: f64 = 0.01;

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

// ---- dlsym'd JSC private API -----------------------------------------------

const RTLD_DEFAULT: *mut c_void = (-2isize) as *mut c_void;

unsafe extern "C" {
  fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

fn resolve(name: &[u8]) -> *mut c_void {
  debug_assert_eq!(name.last(), Some(&0), "symbol name must be NUL-terminated");
  unsafe { dlsym(RTLD_DEFAULT, name.as_ptr() as *const c_char) }
}

type ShouldTerminateCb =
  unsafe extern "C" fn(ctx: JSContextRef, context: *mut c_void) -> bool;
type SetTimeLimitFn = unsafe extern "C" fn(
  group: JSContextGroupRef,
  limit: f64,
  callback: ShouldTerminateCb,
  context: *mut c_void,
);
type ClearTimeLimitFn = unsafe extern "C" fn(group: JSContextGroupRef);
type MemStatsFn = unsafe extern "C" fn(ctx: JSContextRef) -> JSValueRef;

fn set_time_limit_fn() -> Option<SetTimeLimitFn> {
  static F: OnceLock<Option<SetTimeLimitFn>> = OnceLock::new();
  *F.get_or_init(|| {
    let p = resolve(b"JSContextGroupSetExecutionTimeLimit\0");
    (!p.is_null()).then(|| unsafe { std::mem::transmute(p) })
  })
}

fn clear_time_limit_fn() -> Option<ClearTimeLimitFn> {
  static F: OnceLock<Option<ClearTimeLimitFn>> = OnceLock::new();
  *F.get_or_init(|| {
    let p = resolve(b"JSContextGroupClearExecutionTimeLimit\0");
    (!p.is_null()).then(|| unsafe { std::mem::transmute(p) })
  })
}

fn mem_stats_fn() -> Option<MemStatsFn> {
  static F: OnceLock<Option<MemStatsFn>> = OnceLock::new();
  *F.get_or_init(|| {
    let p = resolve(b"JSGetMemoryUsageStatistics\0");
    (!p.is_null()).then(|| unsafe { std::mem::transmute(p) })
  })
}

/// Best-effort live heap size in bytes, or 0 if it can't be measured (the
/// `JSGetMemoryUsageStatistics` symbol is absent on this JSC build).
fn heap_size(ctx: JSContextRef) -> usize {
  let Some(stats) = mem_stats_fn() else {
    return 0;
  };
  if ctx.is_null() {
    return 0;
  }
  unsafe {
    let stats_val = stats(ctx);
    if stats_val.is_null() {
      return 0;
    }
    let mut exc: JSValueRef = ptr::null();
    let obj = JSValueToObject(ctx, stats_val, &mut exc);
    if obj.is_null() {
      return 0;
    }
    let key =
      JSStringCreateWithUTF8CString(b"heapSize\0".as_ptr() as *const c_char);
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    if v.is_null() {
      return 0;
    }
    let n = JSValueToNumber(ctx, v, ptr::null_mut());
    if n.is_finite() && n >= 0.0 {
      n as usize
    } else {
      0
    }
  }
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

  maybe_drive_heap_callback(watch, ctx);

  watch.terminate.load(Ordering::SeqCst)
}

/// If a near-heap-limit callback is registered and the live heap has crossed
/// the configured limit, invoke it (it may raise the limit and/or request
/// termination). When the heap can't be measured we still fire — only the heap
/// tests register a callback, and reaching the watchdog there already means a
/// tight allocation loop is in flight.
fn maybe_drive_heap_callback(watch: &Watch, ctx: JSContextRef) {
  let cb_addr = watch.heap_cb.load(Ordering::Acquire);
  if cb_addr == 0 || watch.in_heap_cb.load(Ordering::Relaxed) {
    return;
  }
  let limit = watch.heap_limit.load(Ordering::Relaxed);
  if limit == 0 {
    return;
  }
  let live = heap_size(ctx);
  let near = if live > 0 { live >= limit } else { true };
  if !near {
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

  if let Some(set) = set_time_limit_fn() {
    let context = Arc::as_ptr(&watch) as *mut c_void;
    unsafe { set(group, WATCHDOG_INTERVAL_SECS, watchdog, context) };
  }

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
    if let Some(clear) = clear_time_limit_fn() {
      unsafe { clear(watch.group) };
    }
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
