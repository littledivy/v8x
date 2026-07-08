#![allow(non_snake_case, unused)]

use crate::Context;
use crate::StackTrace;
use crate::Value;
use crate::isolate::RealIsolate;
use crate::quickjs::quickjs_sys::*;
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

#[repr(C)]
struct InspectorState {
  _cxx_vtable: *const Opaque,
  client: *mut RawV8InspectorClient,
  context_group_id: int,
  channel: *mut RawChannel,
}

thread_local! {
  static ACTIVE_INSPECTOR_CLIENT:
    std::cell::RefCell<Option<(*mut RawV8InspectorClient, int)>> =
      const { std::cell::RefCell::new(None) };
}

unsafe extern "C" {
  fn v8_inspector__V8InspectorClient__BASE__consoleAPIMessage(
    this: *mut RawV8InspectorClient,
    context_group_id: int,
    level: int,
    message: &StringView,
    url: &StringView,
    line_number: u32,
    column_number: u32,
    stack_trace: &mut V8StackTrace,
  );
  fn v8_inspector__V8Inspector__Channel__BASE__sendNotification(
    this: *mut RawChannel,
    message: UniquePtr<StringBuffer>,
  );
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

pub(crate) fn emit_console_api_message(level: int, message: String) {
  let Some((client, context_group_id)) =
    ACTIVE_INSPECTOR_CLIENT.with(|slot| *slot.borrow())
  else {
    return;
  };
  if client.is_null() {
    return;
  }

  let message_bytes = message.into_bytes();
  let message_view = StringView::from(message_bytes.as_slice());
  let url_bytes: [u8; 0] = [];
  let url_view = StringView::from(&url_bytes[..]);
  let stack_trace = alloc_inert::<V8StackTrace>();
  if stack_trace.is_null() {
    return;
  }
  unsafe {
    v8_inspector__V8InspectorClient__BASE__consoleAPIMessage(
      client,
      context_group_id,
      level,
      &message_view,
      &url_view,
      0,
      0,
      &mut *stack_trace,
    );
    free_inert(stack_trace);
  }
}

fn json_string(value: &str) -> String {
  let mut out = String::with_capacity(value.len() + 2);
  out.push('"');
  for ch in value.chars() {
    match ch {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      ch if ch.is_control() => {
        use std::fmt::Write;
        let _ = write!(out, "\\u{:04x}", ch as u32);
      }
      ch => out.push(ch),
    }
  }
  out.push('"');
  out
}

unsafe fn js_value_to_string(
  ctx: *mut JSContext,
  value: JSValue,
) -> Option<String> {
  let mut len = 0usize;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, value) };
  if cstr.is_null() {
    return None;
  }
  let text = unsafe {
    let bytes = std::slice::from_raw_parts(cstr as *const u8, len);
    String::from_utf8_lossy(bytes).into_owned()
  };
  unsafe { JS_FreeCString(ctx, cstr) };
  Some(text)
}

unsafe fn exception_description_and_message(
  exception: *const Value,
) -> (String, String) {
  let ctx = crate::quickjs::core::current_ctx();
  if ctx.is_null() || exception.is_null() {
    return ("Error".to_string(), String::new());
  }
  let value = crate::quickjs::core::jsval_of(exception);
  let description = unsafe { js_value_to_string(ctx, value) }
    .unwrap_or_else(|| "Error".to_string());
  let message_value =
    unsafe { JS_GetPropertyStr(ctx, value, c"message".as_ptr()) };
  let message = if message_value.tag == JS_TAG_EXCEPTION {
    String::new()
  } else {
    unsafe { js_value_to_string(ctx, message_value) }.unwrap_or_default()
  };
  unsafe { JS_FreeValue(ctx, message_value) };
  (description, message)
}

fn send_channel_notification(channel: *mut RawChannel, json: String) {
  if channel.is_null() {
    return;
  }
  let units: Vec<u16> = json.encode_utf16().collect();
  let buf = RealStringBuffer::boxed_from_utf16(units);
  let unique = unsafe { UniquePtr::from_raw(buf) };
  unsafe {
    v8_inspector__V8Inspector__Channel__BASE__sendNotification(channel, unique)
  };
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
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this.cast::<cdp::CdpSession>())) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__dispatchProtocolMessage(
  session: *mut RawV8InspectorSession,
  message: StringView,
) {
  if session.is_null() {
    return;
  }
  let sess = unsafe { &mut *(session.cast::<cdp::CdpSession>()) };
  let msg = string_view_to_string(&message);
  cdp::dispatch(sess, &msg);
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

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__DELETE(this: *mut RawV8Inspector) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this.cast::<InspectorState>())) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__create(
  _isolate: *mut RealIsolate,
  client: *mut RawV8InspectorClient,
) -> *mut RawV8Inspector {
  Box::into_raw(Box::new(InspectorState {
    _cxx_vtable: std::ptr::null(),
    client,
    context_group_id: 1,
    channel: std::ptr::null_mut(),
  }))
  .cast::<RawV8Inspector>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__connect(
  inspector: *mut RawV8Inspector,
  _context_group_id: int,
  channel: *mut RawChannel,
  _state: StringView,
  _client_trust_level: V8InspectorClientTrustLevel,
) -> *mut RawV8InspectorSession {
  if !inspector.is_null() {
    let state = unsafe { &mut *inspector.cast::<InspectorState>() };
    state.channel = channel;
  }
  let sess = Box::new(cdp::CdpSession::new(channel));
  Box::into_raw(sess).cast::<RawV8InspectorSession>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextCreated(
  this: *mut RawV8Inspector,
  _context: *const Context,
  contextGroupId: int,
  _humanReadableName: StringView,
  _auxData: StringView,
) {
  if this.is_null() {
    return;
  }
  let state = unsafe { &mut *this.cast::<InspectorState>() };
  state.context_group_id = contextGroupId;
  ACTIVE_INSPECTOR_CLIENT.with(|slot| {
    *slot.borrow_mut() = Some((state.client, contextGroupId));
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextDestroyed(
  this: *mut RawV8Inspector,
  _context: *const Context,
) {
  if this.is_null() {
    return;
  }
  let state = unsafe { &*this.cast::<InspectorState>() };
  ACTIVE_INSPECTOR_CLIENT.with(|slot| {
    let mut slot = slot.borrow_mut();
    if matches!(*slot, Some((client, _)) if client == state.client) {
      *slot = None;
    }
  });
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
  this: *mut RawV8Inspector,
  _context: *const Context,
  message: StringView,
  _exception: *const Value,
  _detailed_message: StringView,
  url: StringView,
  _line_number: u32,
  _column_number: u32,
  _stack_trace: *mut V8StackTrace,
  _script_id: int,
) -> u32 {
  if this.is_null() {
    return 0;
  }
  let state = unsafe { &*this.cast::<InspectorState>() };
  let text = string_view_to_string(&message);
  let url = string_view_to_string(&url);
  let (description, exception_message) =
    unsafe { exception_description_and_message(_exception) };
  let json = format!(
    "{{\"method\":\"Runtime.exceptionThrown\",\"params\":{{\"timestamp\":0,\"exceptionDetails\":{{\"exceptionId\":1,\"text\":{},\"lineNumber\":0,\"columnNumber\":0,\"scriptId\":\"1\",\"url\":{},\"exception\":{{\"type\":\"object\",\"subtype\":\"error\",\"className\":\"Error\",\"description\":{},\"objectId\":\"1.1.1\",\"preview\":{{\"type\":\"object\",\"subtype\":\"error\",\"description\":{},\"overflow\":false,\"properties\":[{{\"name\":\"stack\",\"type\":\"string\",\"value\":{}}},{{\"name\":\"message\",\"type\":\"string\",\"value\":{}}}]}}}},\"executionContextId\":1}}}}}}",
    json_string(&text),
    json_string(&url),
    json_string(&description),
    json_string(&description),
    json_string(&description),
    json_string(&exception_message),
  );
  send_channel_notification(state.channel, json);
  1
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

mod cdp {
  use super::RawChannel;
  use super::RealStringBuffer;
  use crate::inspector::StringBuffer;
  use crate::quickjs::core::current_ctx;
  use crate::quickjs::quickjs_sys::*;
  use crate::support::UniquePtr;
  use crate::support::int;
  use std::collections::HashMap;
  use std::ffi::{CStr, CString};
  use std::os::raw::c_char;

  unsafe extern "C" {
    fn v8_inspector__V8Inspector__Channel__BASE__sendResponse(
      this: *mut RawChannel,
      call_id: int,
      message: UniquePtr<StringBuffer>,
    );
    fn v8_inspector__V8Inspector__Channel__BASE__sendNotification(
      this: *mut RawChannel,
      message: UniquePtr<StringBuffer>,
    );
  }

  pub struct CdpSession {
    channel: *mut RawChannel,
    next_obj_id: u64,

    objects: HashMap<u64, JSValue>,
  }

  impl CdpSession {
    pub fn new(channel: *mut RawChannel) -> Self {
      CdpSession {
        channel,
        next_obj_id: 1,
        objects: HashMap::new(),
      }
    }
    fn retain(&mut self, ctx: *mut JSContext, v: JSValue) -> u64 {
      let id = self.next_obj_id;
      self.next_obj_id += 1;
      self.objects.insert(id, unsafe { JS_DupValue(ctx, v) });
      id
    }
  }

  impl Drop for CdpSession {
    fn drop(&mut self) {
      let ctx = current_ctx();
      if !ctx.is_null() {
        for (_, v) in self.objects.drain() {
          unsafe { JS_FreeValue(ctx, v) };
        }
      }
    }
  }

  unsafe fn set_str(ctx: *mut JSContext, obj: JSValue, key: &CStr, val: &str) {
    let v =
      unsafe { JS_NewStringLen(ctx, val.as_ptr() as *const c_char, val.len()) };
    unsafe { JS_SetPropertyStr(ctx, obj, key.as_ptr(), v) };
  }
  unsafe fn set_val(
    ctx: *mut JSContext,
    obj: JSValue,
    key: &CStr,
    val: JSValue,
  ) {
    unsafe { JS_SetPropertyStr(ctx, obj, key.as_ptr(), val) };
  }
  unsafe fn set_bool(ctx: *mut JSContext, obj: JSValue, key: &CStr, b: bool) {
    unsafe {
      JS_SetPropertyStr(ctx, obj, key.as_ptr(), JS_NewBool(ctx, b as i32))
    };
  }

  unsafe fn get_str(
    ctx: *mut JSContext,
    obj: JSValue,
    key: &CStr,
  ) -> Option<String> {
    let v = unsafe { JS_GetPropertyStr(ctx, obj, key.as_ptr()) };
    if jsv_is_exception(&v) {
      unsafe { drain_exc(ctx) };
      return None;
    }
    if !jsv_is_string(&v) {
      unsafe { JS_FreeValue(ctx, v) };
      return None;
    }
    let s = unsafe { cstr_value(ctx, v) };
    unsafe { JS_FreeValue(ctx, v) };
    s
  }

  unsafe fn get_int(
    ctx: *mut JSContext,
    obj: JSValue,
    key: &CStr,
  ) -> Option<i64> {
    let v = unsafe { JS_GetPropertyStr(ctx, obj, key.as_ptr()) };
    if jsv_is_exception(&v) {
      unsafe { drain_exc(ctx) };
      return None;
    }
    if jsv_is_undefined(&v) || jsv_is_null(&v) {
      unsafe { JS_FreeValue(ctx, v) };
      return None;
    }
    let mut out: i32 = 0;
    let rc = unsafe { JS_ToInt32(ctx, &mut out, v) };
    unsafe { JS_FreeValue(ctx, v) };
    if rc < 0 {
      unsafe { drain_exc(ctx) };
      None
    } else {
      Some(out as i64)
    }
  }

  unsafe fn cstr_value(ctx: *mut JSContext, v: JSValue) -> Option<String> {
    let p = unsafe { JS_ToCString(ctx, v) };
    if p.is_null() {
      unsafe { drain_exc(ctx) };
      return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { JS_FreeCString(ctx, p) };
    Some(s)
  }

  unsafe fn drain_exc(ctx: *mut JSContext) {
    if unsafe { JS_HasException(ctx) } {
      let e = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, e) };
    }
  }

  unsafe fn type_of(ctx: *mut JSContext, v: JSValue) -> &'static str {
    if jsv_is_undefined(&v) {
      "undefined"
    } else if jsv_is_null(&v) {
      "object"
    } else if jsv_is_bool(&v) {
      "boolean"
    } else if jsv_is_number(&v) {
      "number"
    } else if jsv_is_string(&v) {
      "string"
    } else if jsv_is_bigint(&v) {
      "bigint"
    } else if jsv_is_symbol(&v) {
      "symbol"
    } else if unsafe { JS_IsFunction(ctx, v) } {
      "function"
    } else {
      "object"
    }
  }

  unsafe fn remote_object(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    val: JSValue,
  ) -> JSValue {
    let ro = unsafe { JS_NewObject(ctx) };
    let t = unsafe { type_of(ctx, val) };
    unsafe { set_str(ctx, ro, c"type", t) };
    match t {
      "undefined" => {}
      "boolean" => unsafe {
        set_val(ctx, ro, c"value", JS_DupValue(ctx, val));
      },
      "number" => unsafe {
        set_val(ctx, ro, c"value", JS_DupValue(ctx, val));
        if let Some(d) = cstr_value(ctx, val) {
          set_str(ctx, ro, c"description", &d);
        }
      },
      "string" => unsafe {
        set_val(ctx, ro, c"value", JS_DupValue(ctx, val));
      },
      "bigint" => unsafe {
        if let Some(d) = cstr_value(ctx, val) {
          let disp = format!("{d}n");
          set_str(ctx, ro, c"unserializableValue", &disp);
          set_str(ctx, ro, c"description", &disp);
        }
      },
      "symbol" => unsafe {
        if let Some(d) = cstr_value(ctx, val) {
          set_str(ctx, ro, c"description", &d);
        }
        let id = sess.retain(ctx, val);
        set_str(ctx, ro, c"objectId", &id.to_string());
      },
      "function" => unsafe {
        set_str(ctx, ro, c"className", "Function");
        if let Some(d) = cstr_value(ctx, val) {
          set_str(ctx, ro, c"description", &d);
        }
        let id = sess.retain(ctx, val);
        set_str(ctx, ro, c"objectId", &id.to_string());
      },
      _ => unsafe {
        if jsv_is_null(&val) {
          set_str(ctx, ro, c"subtype", "null");
          set_val(ctx, ro, c"value", jsv_null());
        } else {
          let cname = ctor_name(ctx, val);
          set_str(ctx, ro, c"className", &cname);

          let is_err = is_error(ctx, val);
          let desc = if is_err {
            set_str(ctx, ro, c"subtype", "error");
            cstr_value(ctx, val).unwrap_or(cname.clone())
          } else {
            object_description(ctx, val, &cname)
          };
          set_str(ctx, ro, c"description", &desc);
          let id = sess.retain(ctx, val);
          set_str(ctx, ro, c"objectId", &id.to_string());
        }
      },
    }
    ro
  }

  unsafe fn is_error(ctx: *mut JSContext, val: JSValue) -> bool {
    let stack = unsafe { JS_GetPropertyStr(ctx, val, c"stack".as_ptr()) };
    let is = jsv_is_string(&stack);
    if jsv_is_exception(&stack) {
      unsafe { drain_exc(ctx) };
    }
    unsafe { JS_FreeValue(ctx, stack) };
    is
  }

  unsafe fn ctor_name(ctx: *mut JSContext, val: JSValue) -> String {
    let ctor = unsafe { JS_GetPropertyStr(ctx, val, c"constructor".as_ptr()) };
    if jsv_is_exception(&ctor) {
      unsafe { drain_exc(ctx) };
      return "Object".to_string();
    }
    if !jsv_is_object(&ctor) {
      unsafe { JS_FreeValue(ctx, ctor) };
      return "Object".to_string();
    }
    let name = unsafe { JS_GetPropertyStr(ctx, ctor, c"name".as_ptr()) };
    unsafe { JS_FreeValue(ctx, ctor) };
    if jsv_is_string(&name) {
      let s = unsafe { cstr_value(ctx, name) }
        .unwrap_or_else(|| "Object".to_string());
      unsafe { JS_FreeValue(ctx, name) };
      if s.is_empty() {
        "Object".to_string()
      } else {
        s
      }
    } else {
      unsafe { JS_FreeValue(ctx, name) };
      "Object".to_string()
    }
  }

  unsafe fn object_description(
    ctx: *mut JSContext,
    val: JSValue,
    cname: &str,
  ) -> String {
    let len = unsafe { JS_GetPropertyStr(ctx, val, c"length".as_ptr()) };
    let is_arrayish = jsv_is_number(&len);
    let mut n = 0i32;
    if is_arrayish {
      unsafe { JS_ToInt32(ctx, &mut n, len) };
    }
    unsafe { JS_FreeValue(ctx, len) };
    if cname == "Array" {
      format!("Array({n})")
    } else {
      cname.to_string()
    }
  }

  unsafe fn drain_jobs(ctx: *mut JSContext) {
    let rt = unsafe { JS_GetRuntime(ctx) };
    if rt.is_null() {
      return;
    }
    let mut pctx: *mut JSContext = std::ptr::null_mut();
    loop {
      let r = unsafe { JS_ExecutePendingJob(rt, &mut pctx) };
      if r == 0 {
        break;
      }
      if r < 0 {
        unsafe { drain_exc(pctx) };
      }
    }
  }

  unsafe fn await_value(ctx: *mut JSContext, v: JSValue) -> JSValue {
    if !jsv_is_object(&v) || !unsafe { JS_IsPromise(v) } {
      return v;
    }
    unsafe { drain_jobs(ctx) };
    let state = unsafe { JS_PromiseState(ctx, v) };

    let res = unsafe { JS_PromiseResult(ctx, v) };
    unsafe { JS_FreeValue(ctx, v) };
    if state == 2 {
      unsafe { JS_Throw(ctx, JS_DupValue(ctx, res)) };
      unsafe { JS_FreeValue(ctx, res) };
      return jsv_exception();
    }
    res
  }

  fn send(channel: *mut RawChannel, json: &str, call_id: Option<i32>) {
    let units: Vec<u16> = json.encode_utf16().collect();
    let buf = RealStringBuffer::boxed_from_utf16(units);
    let up = unsafe { UniquePtr::from_raw(buf) };
    unsafe {
      match call_id {
        Some(id) => v8_inspector__V8Inspector__Channel__BASE__sendResponse(
          channel, id, up,
        ),
        None => v8_inspector__V8Inspector__Channel__BASE__sendNotification(
          channel, up,
        ),
      }
    }
  }

  unsafe fn send_obj(
    sess: &CdpSession,
    ctx: *mut JSContext,
    obj: JSValue,
    call_id: Option<i32>,
  ) {
    let to_send = match call_id {
      Some(id) => {
        let env = unsafe { JS_NewObject(ctx) };
        unsafe { set_val(ctx, env, c"id", JS_NewInt32(ctx, id)) };
        unsafe { set_val(ctx, env, c"result", obj) };
        env
      }
      None => obj,
    };
    let s = unsafe {
      JS_JSONStringify(ctx, to_send, jsv_undefined(), jsv_undefined())
    };
    let json = if jsv_is_string(&s) {
      unsafe { cstr_value(ctx, s) }.unwrap_or_else(|| "{}".to_string())
    } else {
      unsafe { drain_exc(ctx) };
      "{}".to_string()
    };
    unsafe { JS_FreeValue(ctx, s) };
    unsafe { JS_FreeValue(ctx, to_send) };
    send(sess.channel, &json, call_id);
  }

  unsafe fn ack(sess: &CdpSession, ctx: *mut JSContext, call_id: i32) {
    let o = unsafe { JS_NewObject(ctx) };
    unsafe { send_obj(sess, ctx, o, Some(call_id)) };
  }

  unsafe fn send_eval_result(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    outcome: JSValue,
    call_id: i32,
  ) {
    let resp = unsafe { JS_NewObject(ctx) };
    if jsv_is_exception(&outcome) {
      let exc = unsafe { JS_GetException(ctx) };
      let ro = unsafe { remote_object(sess, ctx, exc) };
      unsafe { set_val(ctx, resp, c"result", ro) };
      let ed = unsafe { JS_NewObject(ctx) };
      let text = unsafe { cstr_value(ctx, exc) }
        .unwrap_or_else(|| "Uncaught".to_string());
      unsafe { set_str(ctx, ed, c"text", "Uncaught") };
      unsafe { set_val(ctx, ed, c"exceptionId", JS_NewInt32(ctx, 1)) };
      unsafe { set_val(ctx, ed, c"lineNumber", JS_NewInt32(ctx, 0)) };
      unsafe { set_val(ctx, ed, c"columnNumber", JS_NewInt32(ctx, 0)) };
      let exc_ro = unsafe { remote_object(sess, ctx, exc) };
      unsafe { set_val(ctx, ed, c"exception", exc_ro) };
      let _ = text;
      unsafe { set_val(ctx, resp, c"exceptionDetails", ed) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      let ro = unsafe { remote_object(sess, ctx, outcome) };
      unsafe { set_val(ctx, resp, c"result", ro) };
      unsafe { JS_FreeValue(ctx, outcome) };
    }
    unsafe { send_obj(sess, ctx, resp, Some(call_id)) };
  }

  unsafe fn handle_runtime_enable(
    sess: &CdpSession,
    ctx: *mut JSContext,
    call_id: i32,
  ) {
    unsafe { ack(sess, ctx, call_id) };
    let notif = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_str(ctx, notif, c"method", "Runtime.executionContextCreated")
    };
    let params = unsafe { JS_NewObject(ctx) };
    let context = unsafe { JS_NewObject(ctx) };
    unsafe { set_val(ctx, context, c"id", JS_NewInt32(ctx, 1)) };
    unsafe { set_str(ctx, context, c"origin", "") };
    unsafe { set_str(ctx, context, c"name", "repl") };
    unsafe { set_str(ctx, context, c"uniqueId", "1") };
    let aux = unsafe { JS_NewObject(ctx) };
    unsafe { set_bool(ctx, aux, c"isDefault", true) };
    unsafe { set_val(ctx, context, c"auxData", aux) };
    unsafe { set_val(ctx, params, c"context", context) };
    unsafe { set_val(ctx, notif, c"params", params) };
    unsafe { send_obj(sess, ctx, notif, None) };
  }

  unsafe fn handle_evaluate(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let expr =
      unsafe { get_str(ctx, params, c"expression") }.unwrap_or_default();
    let cexpr = CString::new(expr.as_str())
      .unwrap_or_else(|_| CString::new("undefined").unwrap());
    let mut val = unsafe {
      JS_Eval(
        ctx,
        cexpr.as_ptr(),
        expr.len(),
        c"<repl>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };

    let mut was_async = false;
    if jsv_is_exception(&val) && expr.contains("await") {
      unsafe { drain_exc(ctx) };

      was_async = true;
      val = unsafe {
        JS_Eval(
          ctx,
          cexpr.as_ptr(),
          expr.len(),
          c"<repl>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_ASYNC,
        )
      };
    }
    let mut val = unsafe { await_value(ctx, val) };

    if was_async && !jsv_is_exception(&val) && jsv_is_object(&val) {
      let inner = unsafe { JS_GetPropertyStr(ctx, val, c"value".as_ptr()) };
      if !jsv_is_exception(&inner) {
        unsafe { JS_FreeValue(ctx, val) };
        val = inner;
      } else {
        unsafe { drain_exc(ctx) };
      }
    }
    unsafe { send_eval_result(sess, ctx, val, call_id) };
  }

  unsafe fn handle_call_function_on(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let decl = unsafe { get_str(ctx, params, c"functionDeclaration") }
      .unwrap_or_default();
    let this_id = unsafe { get_str(ctx, params, c"objectId") }
      .and_then(|s| s.parse::<u64>().ok());
    let this_val = match this_id.and_then(|id| sess.objects.get(&id).copied()) {
      Some(v) => v,
      None => jsv_undefined(),
    };

    let src = format!("({decl})");
    let csrc = CString::new(src.as_str())
      .unwrap_or_else(|_| CString::new("(()=>{})").unwrap());
    let func = unsafe {
      JS_Eval(
        ctx,
        csrc.as_ptr(),
        src.len(),
        c"<repl-call>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if jsv_is_exception(&func) {
      unsafe { send_eval_result(sess, ctx, jsv_exception(), call_id) };
      return;
    }

    let mut args: Vec<JSValue> = Vec::new();
    let arr = unsafe { JS_GetPropertyStr(ctx, params, c"arguments".as_ptr()) };
    if jsv_is_object(&arr) {
      let len = unsafe { JS_GetPropertyStr(ctx, arr, c"length".as_ptr()) };
      let mut n = 0i32;
      unsafe { JS_ToInt32(ctx, &mut n, len) };
      unsafe { JS_FreeValue(ctx, len) };
      for i in 0..n.max(0) as u32 {
        let item = unsafe { JS_GetPropertyUint32(ctx, arr, i) };
        args.push(unsafe { call_arg_to_value(sess, ctx, item) });
        unsafe { JS_FreeValue(ctx, item) };
      }
    }
    unsafe { JS_FreeValue(ctx, arr) };
    let ret = unsafe {
      JS_Call(ctx, func, this_val, args.len() as i32, args.as_mut_ptr())
    };
    for a in &args {
      unsafe { JS_FreeValue(ctx, *a) };
    }
    unsafe { JS_FreeValue(ctx, func) };
    let ret = unsafe { await_value(ctx, ret) };
    unsafe { send_eval_result(sess, ctx, ret, call_id) };
  }

  unsafe fn call_arg_to_value(
    sess: &CdpSession,
    ctx: *mut JSContext,
    arg: JSValue,
  ) -> JSValue {
    if let Some(oid) = unsafe { get_str(ctx, arg, c"objectId") }
      .and_then(|s| s.parse::<u64>().ok())
    {
      if let Some(v) = sess.objects.get(&oid) {
        return unsafe { JS_DupValue(ctx, *v) };
      }
    }
    if let Some(u) = unsafe { get_str(ctx, arg, c"unserializableValue") } {
      let cu = CString::new(u.as_str())
        .unwrap_or_else(|_| CString::new("undefined").unwrap());
      let v = unsafe {
        JS_Eval(
          ctx,
          cu.as_ptr(),
          u.len(),
          c"<arg>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        )
      };
      if jsv_is_exception(&v) {
        unsafe { drain_exc(ctx) };
        return jsv_undefined();
      }
      return v;
    }

    let v = unsafe { JS_GetPropertyStr(ctx, arg, c"value".as_ptr()) };
    if jsv_is_exception(&v) {
      unsafe { drain_exc(ctx) };
      return jsv_undefined();
    }
    v
  }

  unsafe fn handle_compile_script(
    sess: &CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let expr =
      unsafe { get_str(ctx, params, c"expression") }.unwrap_or_default();
    let cexpr =
      CString::new(expr.as_str()).unwrap_or_else(|_| CString::new("").unwrap());
    let r = unsafe {
      JS_Eval(
        ctx,
        cexpr.as_ptr(),
        expr.len(),
        c"<compile>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY,
      )
    };
    let resp = unsafe { JS_NewObject(ctx) };
    if jsv_is_exception(&r) {
      let exc = unsafe { JS_GetException(ctx) };
      let ed = unsafe { JS_NewObject(ctx) };
      unsafe { set_str(ctx, ed, c"text", "SyntaxError") };
      unsafe { set_val(ctx, ed, c"exceptionId", JS_NewInt32(ctx, 1)) };
      unsafe { set_val(ctx, ed, c"lineNumber", JS_NewInt32(ctx, 0)) };
      unsafe { set_val(ctx, ed, c"columnNumber", JS_NewInt32(ctx, 0)) };
      unsafe { JS_FreeValue(ctx, exc) };
      unsafe { set_val(ctx, resp, c"exceptionDetails", ed) };
    } else {
      unsafe { JS_FreeValue(ctx, r) };
    }
    unsafe { send_obj(sess, ctx, resp, Some(call_id)) };
  }

  pub fn dispatch(sess: &mut CdpSession, message: &str) {
    let ctx = current_ctx();
    if ctx.is_null() {
      return;
    }
    let cmsg = match CString::new(message) {
      Ok(c) => c,
      Err(_) => return,
    };
    let parsed = unsafe {
      JS_ParseJSON(ctx, cmsg.as_ptr(), message.len(), c"<cdp>".as_ptr())
    };
    if jsv_is_exception(&parsed) {
      unsafe { drain_exc(ctx) };
      return;
    }
    let id = unsafe { get_int(ctx, parsed, c"id") }.unwrap_or(0) as i32;
    let method = unsafe { get_str(ctx, parsed, c"method") }.unwrap_or_default();
    let params = unsafe { JS_GetPropertyStr(ctx, parsed, c"params".as_ptr()) };

    match method.as_str() {
      "Runtime.enable" => unsafe { handle_runtime_enable(sess, ctx, id) },
      "Runtime.evaluate" => unsafe { handle_evaluate(sess, ctx, params, id) },
      "Runtime.callFunctionOn" => unsafe {
        handle_call_function_on(sess, ctx, params, id)
      },
      "Runtime.compileScript" => unsafe {
        handle_compile_script(sess, ctx, params, id)
      },
      "Runtime.globalLexicalScopeNames" => unsafe {
        let resp = JS_NewObject(ctx);
        set_val(ctx, resp, c"names", JS_NewArray(ctx));
        send_obj(sess, ctx, resp, Some(id));
      },
      "Runtime.releaseObject" => unsafe {
        if let Some(oid) =
          get_str(ctx, params, c"objectId").and_then(|s| s.parse::<u64>().ok())
        {
          if let Some(v) = sess.objects.remove(&oid) {
            JS_FreeValue(ctx, v);
          }
        }
        ack(sess, ctx, id);
      },

      _ => unsafe { ack(sess, ctx, id) },
    }

    unsafe { JS_FreeValue(ctx, params) };
    unsafe { JS_FreeValue(ctx, parsed) };
  }
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

fn string_view_to_string(sv: &StringView<'_>) -> String {
  if let Some(s) = sv.characters16() {
    String::from_utf16_lossy(s)
  } else if let Some(s) = sv.characters8() {
    s.iter().map(|&b| b as char).collect()
  } else {
    String::new()
  }
}
