// Copied from the JSC backend — pure-Rust / inert, engine-agnostic.
// Family: inspector  (v8_inspector::*)
//
// JSC has no V8 inspector / CDP backend. Every symbol here is implemented as an
// INERT no-op with a signature that EXACTLY matches the vendored extern
// declarations in src/inspector.rs, so that Deno links and runs fine as long as
// no debugger (`--inspect`) is attached. Where the crate wraps a returned raw
// pointer in `UniqueRef::from_raw(..)` (which would panic on null), we hand back
// a leaked, zeroed allocation instead of null so the happy path does not crash;
// the matching `__DELETE` frees it again.
#![allow(non_snake_case, unused)]

use crate::Context;
use crate::StackTrace;
use crate::Value;
use crate::isolate::RealIsolate;
use crate::support::Opaque;
use crate::support::UniquePtr;
use crate::support::int;

use std::ffi::c_void;
use std::mem::MaybeUninit;

// These types are defined (some `pub`, some private) in `crate::inspector`. We
// re-declare the layouts we need locally; at the C ABI level every one of these
// is passed/returned as an opaque pointer, so a local layout-compatible
// definition is interchangeable with the crate's own.
use crate::inspector::RawV8Inspector;
use crate::inspector::RawV8InspectorClient;
use crate::inspector::RawV8InspectorSession;
use crate::inspector::StringBuffer;
use crate::inspector::StringView;
use crate::inspector::V8InspectorClientTrustLevel;
use crate::inspector::V8StackTrace;

// `RawChannel` is private to the inspector module; redeclare a layout-compatible
// opaque type for our parameter signatures (pointer-only at the ABI level).
#[repr(C)]
pub struct RawChannel {
    _cxx_vtable: *const Opaque,
}

// A generic single-pointer-sized opaque heap object used to back the leaked
// allocations we hand out for `create`/`connect`. Layout matches the
// `{ _cxx_vtable }` shape of the real objects (one pointer field).
#[repr(C)]
struct InertObj {
    _cxx_vtable: *const Opaque,
}

#[inline]
fn alloc_inert<T>() -> *mut T {
    Box::into_raw(Box::new(InertObj {
        _cxx_vtable: std::ptr::null(),
    }))
    .cast::<T>()
}

#[inline]
unsafe fn free_inert<T>(p: *mut T) {
    if !p.is_null() {
        unsafe { drop(Box::from_raw(p.cast::<InertObj>())) };
    }
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__BASE__CONSTRUCT(
    buf: *mut MaybeUninit<RawChannel>,
) {
    // Zero-initialize the embedded vtable slot. The Rust-side `ChannelBase`
    // overrides individual methods elsewhere; for the inert path we just make
    // sure the memory is in a defined state.
    if !buf.is_null() {
        unsafe {
            (*buf).write(RawChannel {
                _cxx_vtable: std::ptr::null(),
            });
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__sendResponse(
    _this: *mut RawChannel,
    _call_id: int,
    _message: UniquePtr<StringBuffer>,
) {
    // TODO(v82jsc): no CDP transport; drop the message.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__sendNotification(
    _this: *mut RawChannel,
    _message: UniquePtr<StringBuffer>,
) {
    // TODO(v82jsc): no CDP transport; drop the notification.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__flushProtocolNotifications(
    _this: *mut RawChannel,
) {
    // TODO(v82jsc): nothing buffered.
}

// ---------------------------------------------------------------------------
// V8InspectorClient
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__BASE__CONSTRUCT(
    buf: *mut MaybeUninit<RawV8InspectorClient>,
) {
    if !buf.is_null() {
        unsafe {
            // RawV8InspectorClient is `{ _cxx_vtable: CxxVTable }` — one
            // pointer; zero it.
            std::ptr::write_bytes(buf, 0, 1);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__generateUniqueId(
    _this: *mut RawV8InspectorClient,
) -> i64 {
    // TODO(v82jsc): no inspector; ids are unused without a debugger.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__runMessageLoopOnPause(
    _this: *mut RawV8InspectorClient,
    _context_group_id: int,
) {
    // TODO(v82jsc): never pauses.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__quitMessageLoopOnPause(
    _this: *mut RawV8InspectorClient,
) {
    // TODO(v82jsc): never pauses.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__runIfWaitingForDebugger(
    _this: *mut RawV8InspectorClient,
    _context_group_id: int,
) {
    // TODO(v82jsc): never waits for a debugger.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__consoleAPIMessage(
    _this: *mut RawV8InspectorClient,
    _context_group_id: int,
    _level: int,
    _message: &StringView,
    _url: &StringView,
    _line_number: u32,
    _column_number: u32,
    _stack_trace: &mut V8StackTrace,
) {
    // TODO(v82jsc): console forwarding to the inspector is a no-op.
}

// ---------------------------------------------------------------------------
// V8InspectorSession
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__DELETE(
    this: *mut RawV8InspectorSession,
) {
    unsafe { free_inert(this) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__dispatchProtocolMessage(
    _session: *mut RawV8InspectorSession,
    _message: StringView,
) {
    // TODO(v82jsc): no CDP backend; ignore protocol messages.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__schedulePauseOnNextStatement(
    _session: *mut RawV8InspectorSession,
    _break_reason: StringView,
    _break_details: StringView,
) {
    // TODO(v82jsc): never pauses.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__cancelPauseOnNextStatement(
    _session: *mut RawV8InspectorSession,
) {
    // TODO(v82jsc): nothing scheduled.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__canDispatchMethod(
    _method: StringView,
) -> bool {
    // TODO(v82jsc): we dispatch nothing.
    false
}

// ---------------------------------------------------------------------------
// StringBuffer
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__DELETE(this: *mut StringBuffer) {
    unsafe { free_inert(this) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__string(
    _this: &StringBuffer,
) -> StringView<'_> {
    // TODO(v82jsc): inert StringBuffer holds no content.
    StringView::empty()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__create(
    _source: StringView,
) -> UniquePtr<StringBuffer> {
    // TODO(v82jsc): hand back a leaked inert StringBuffer so the wrapping
    // UniquePtr is non-null; freed via StringBuffer__DELETE.
    let p = alloc_inert::<StringBuffer>();
    unsafe { UniquePtr::from_raw(p) }
}

// ---------------------------------------------------------------------------
// V8Inspector
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__DELETE(this: *mut RawV8Inspector) {
    unsafe { free_inert(this) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__create(
    _isolate: *mut RealIsolate,
    _client: *mut RawV8InspectorClient,
) -> *mut RawV8Inspector {
    // The crate wraps this in `UniqueRef::from_raw` which panics on null, so we
    // return a leaked inert object (freed by V8Inspector__DELETE).
    alloc_inert::<RawV8Inspector>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__connect(
    _inspector: *mut RawV8Inspector,
    _context_group_id: int,
    _channel: *mut RawChannel,
    _state: StringView,
    _client_trust_level: V8InspectorClientTrustLevel,
) -> *mut RawV8InspectorSession {
    // Same non-null contract as `create`.
    alloc_inert::<RawV8InspectorSession>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextCreated(
    _this: *mut RawV8Inspector,
    _context: *const Context,
    _contextGroupId: int,
    _humanReadableName: StringView,
    _auxData: StringView,
) {
    // TODO(v82jsc): no inspector to notify.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextDestroyed(
    _this: *mut RawV8Inspector,
    _context: *const Context,
) {
    // TODO(v82jsc): no inspector to notify.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__idleStarted(
    _this: *mut RawV8Inspector,
) {
    // TODO(v82jsc): no-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__idleFinished(
    _this: *mut RawV8Inspector,
) {
    // TODO(v82jsc): no-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskScheduled(
    _this: *mut RawV8Inspector,
    _task_name: StringView,
    _task: *const c_void,
    _recurring: bool,
) {
    // TODO(v82jsc): async stack tracking is inspector-only.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskCanceled(
    _this: *mut RawV8Inspector,
    _task: *const c_void,
) {
    // TODO(v82jsc): no-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskStarted(
    _this: *mut RawV8Inspector,
    _task: *const c_void,
) {
    // TODO(v82jsc): no-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskFinished(
    _this: *mut RawV8Inspector,
    _task: *const c_void,
) {
    // TODO(v82jsc): no-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__allAsyncTasksCanceled(
    _this: *mut RawV8Inspector,
) {
    // TODO(v82jsc): no-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__exceptionThrown(
    _this: *mut RawV8Inspector,
    _context: *const Context,
    _message: StringView,
    _exception: *const Value,
    _detailed_message: StringView,
    _url: StringView,
    _line_number: u32,
    _column_number: u32,
    _stack_trace: *mut V8StackTrace,
    _script_id: int,
) -> u32 {
    // TODO(v82jsc): no inspector; return id 0 (no exception registered).
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__createStackTrace(
    _this: *mut RawV8Inspector,
    _stack_trace: *const StackTrace,
) -> *mut V8StackTrace {
    // Wrapped in `UniquePtr::from_raw` (null-tolerant), so returning null is
    // safe and means "no stack trace".
    // TODO(v82jsc): JSC has no V8StackTrace bridge.
    std::ptr::null_mut()
}

// ---------------------------------------------------------------------------
// V8StackTrace
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8StackTrace__DELETE(this: *mut V8StackTrace) {
    unsafe { free_inert(this) };
}
