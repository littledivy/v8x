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

use crate::inspector::RawV8Inspector;
use crate::inspector::RawV8InspectorClient;
use crate::inspector::RawV8InspectorSession;
use crate::inspector::StringBuffer;
use crate::inspector::StringView;
use crate::inspector::V8InspectorClientTrustLevel;
use crate::inspector::V8StackTrace;

#[repr(C)]
pub struct RawChannel {
  _cxx_vtable: *const Opaque,
}

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

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__BASE__CONSTRUCT(
  buf: *mut MaybeUninit<RawChannel>,
) {
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
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__sendNotification(
  _this: *mut RawChannel,
  _message: UniquePtr<StringBuffer>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__flushProtocolNotifications(
  _this: *mut RawChannel,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__BASE__CONSTRUCT(
  buf: *mut MaybeUninit<RawV8InspectorClient>,
) {
  if !buf.is_null() {
    unsafe {
      std::ptr::write_bytes(buf, 0, 1);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__generateUniqueId(
  _this: *mut RawV8InspectorClient,
) -> i64 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__runMessageLoopOnPause(
  _this: *mut RawV8InspectorClient,
  _context_group_id: int,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__quitMessageLoopOnPause(
  _this: *mut RawV8InspectorClient,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__runIfWaitingForDebugger(
  _this: *mut RawV8InspectorClient,
  _context_group_id: int,
) {
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
}

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
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__schedulePauseOnNextStatement(
  _session: *mut RawV8InspectorSession,
  _break_reason: StringView,
  _break_details: StringView,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__cancelPauseOnNextStatement(
  _session: *mut RawV8InspectorSession,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__canDispatchMethod(
  method: StringView,
) -> bool {
  // A method "Domain.command" is dispatchable iff its domain is one of the
  // protocol domains V8InspectorSession serves built-in. Mirror V8's set.
  let s: String = if let Some(b) = method.characters8() {
    b.iter().map(|&c| c as char).collect()
  } else if let Some(u) = method.characters16() {
    String::from_utf16_lossy(u)
  } else {
    String::new()
  };
  let domain = s.split('.').next().unwrap_or("");
  matches!(
    domain,
    "Runtime" | "Debugger" | "Profiler" | "HeapProfiler" | "Console" | "Schema"
  )
}

/// A real `StringBuffer` backing store. V8's `StringBuffer::create` copies the
/// source `StringView` into an owned buffer that outlives it; the previous stub
/// dropped the contents, so `string()` reported an empty view (rusty_v8's
/// `inspector_string_buffer` test). Mirrors the quickjs backend.
#[repr(C)]
struct RealStringBuffer {
  _vtable: *const Opaque,
  units: Vec<u16>,
}

impl RealStringBuffer {
  fn boxed_from_utf16(units: Vec<u16>) -> *mut StringBuffer {
    Box::into_raw(Box::new(RealStringBuffer {
      _vtable: std::ptr::null(),
      units,
    }))
    .cast::<StringBuffer>()
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__DELETE(this: *mut StringBuffer) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this.cast::<RealStringBuffer>())) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__string(
  this: &StringBuffer,
) -> StringView<'_> {
  let rb =
    unsafe { &*(this as *const StringBuffer as *const RealStringBuffer) };
  StringView::from(rb.units.as_slice())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__create(
  source: StringView,
) -> UniquePtr<StringBuffer> {
  let units = string_view_to_utf16(&source);
  unsafe { UniquePtr::from_raw(RealStringBuffer::boxed_from_utf16(units)) }
}

fn string_view_to_utf16(sv: &StringView<'_>) -> Vec<u16> {
  if let Some(s) = sv.characters16() {
    s.to_vec()
  } else if let Some(s) = sv.characters8() {
    s.iter().map(|&b| b as u16).collect()
  } else {
    Vec::new()
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__DELETE(this: *mut RawV8Inspector) {
  unsafe { free_inert(this) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__create(
  _isolate: *mut RealIsolate,
  _client: *mut RawV8InspectorClient,
) -> *mut RawV8Inspector {
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
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextDestroyed(
  _this: *mut RawV8Inspector,
  _context: *const Context,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__idleStarted(
  _this: *mut RawV8Inspector,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__idleFinished(
  _this: *mut RawV8Inspector,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskScheduled(
  _this: *mut RawV8Inspector,
  _task_name: StringView,
  _task: *const c_void,
  _recurring: bool,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskCanceled(
  _this: *mut RawV8Inspector,
  _task: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskStarted(
  _this: *mut RawV8Inspector,
  _task: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskFinished(
  _this: *mut RawV8Inspector,
  _task: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__allAsyncTasksCanceled(
  _this: *mut RawV8Inspector,
) {
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
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__createStackTrace(
  _this: *mut RawV8Inspector,
  _stack_trace: *const StackTrace,
) -> *mut V8StackTrace {
  std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8StackTrace__DELETE(this: *mut V8StackTrace) {
  unsafe { free_inert(this) };
}
