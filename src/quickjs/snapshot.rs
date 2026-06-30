use super::core::{ctx_of, intern, iso_state, jsval_of};
use super::quickjs_sys::*;
use crate::{Context, Data, RealIsolate};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const SNAP_PREFIX: &[u8] = b"v8x-snapshot:";
const BASELINE_SRC: &[u8] = b"(function(){\
  var base=Object.create(null);\
  var names=Object.getOwnPropertyNames(globalThis);\
  for(var i=0;i<names.length;i++) base[names[i]]=1;\
  Object.defineProperty(globalThis,'__v8x_snapshot_baseline',{value:base,configurable:false,enumerable:false});\
})()\0";

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RawStartupDataAbi {
  pub(crate) data: *const c_char,
  pub(crate) raw_size: c_int,
}

#[derive(Clone, Default)]
pub(crate) struct ContextSnapshot {
  pub(crate) script: String,
  pub(crate) embedder_data: Vec<Option<String>>,
  pub(crate) context_data: Vec<Option<String>>,
}

#[derive(Clone, Default)]
pub(crate) struct SnapshotBlob {
  pub(crate) default_context: ContextSnapshot,
  pub(crate) contexts: Vec<ContextSnapshot>,
  pub(crate) isolate_data: Vec<Option<String>>,
}

struct SnapshotCreatorState {
  isolate: *mut RealIsolate,
  default_context: Option<ContextSnapshot>,
  default_context_key: Option<usize>,
  contexts: Vec<ContextSnapshot>,
  isolate_data: Vec<Option<String>>,
  context_data: HashMap<usize, Vec<Option<String>>>,
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
static BLOBS: OnceLock<Mutex<HashMap<usize, SnapshotBlob>>> = OnceLock::new();

fn blobs() -> &'static Mutex<HashMap<usize, SnapshotBlob>> {
  BLOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_blob_id(raw: *const c_void) -> Option<usize> {
  if raw.is_null() {
    return None;
  }
  let raw = unsafe { &*(raw as *const RawStartupDataAbi) };
  if raw.data.is_null() || raw.raw_size <= SNAP_PREFIX.len() as c_int {
    return None;
  }
  let bytes = unsafe {
    std::slice::from_raw_parts(raw.data as *const u8, raw.raw_size as usize)
  };
  if !bytes.starts_with(SNAP_PREFIX) {
    return None;
  }
  std::str::from_utf8(&bytes[SNAP_PREFIX.len()..])
    .ok()?
    .parse()
    .ok()
}

pub(crate) fn loaded_blob_from_params(params: *const c_void) -> Option<usize> {
  if params.is_null() {
    return None;
  }
  let params = unsafe {
    &*(params as *const crate::isolate_create_params::raw::CreateParams)
  };
  parse_blob_id(params.snapshot_blob as *const c_void)
}

pub(crate) fn get_blob(id: usize) -> Option<SnapshotBlob> {
  blobs().lock().ok()?.get(&id).cloned()
}

fn eval_to_string(ctx: *mut JSContext, src: &'static [u8]) -> Option<String> {
  let v = unsafe {
    JS_Eval(
      ctx,
      src.as_ptr() as *const c_char,
      src.len() - 1,
      c"<v8x snapshot>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return None;
  }
  let mut len = 0usize;
  let s = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  let out = if s.is_null() {
    None
  } else {
    Some(
      String::from_utf8_lossy(unsafe {
        std::slice::from_raw_parts(s as *const u8, len)
      })
      .into_owned(),
    )
  };
  if !s.is_null() {
    unsafe { JS_FreeCString(ctx, s) };
  }
  unsafe { JS_FreeValue(ctx, v) };
  out
}

pub(crate) fn mark_baseline(ctx: *mut JSContext) {
  let r = unsafe {
    JS_Eval(
      ctx,
      BASELINE_SRC.as_ptr() as *const c_char,
      BASELINE_SRC.len() - 1,
      c"<v8x snapshot baseline>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if r.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
  } else {
    unsafe { JS_FreeValue(ctx, r) };
  }
}

fn capture_global_script(ctx: *mut JSContext) -> String {
  const SRC: &[u8] = b"(function(){\
    var skip={Infinity:1,NaN:1,undefined:1,globalThis:1,console:1,Intl:1,WebAssembly:1,eval:1,Function:1};\
    var names=Object.getOwnPropertyNames(globalThis);\
    var out='';\
    for(var i=0;i<names.length;i++){\
      var k=names[i];\
      if(skip[k]) continue;\
      try {\
        var base=globalThis.__v8x_snapshot_baseline;\
        if(base&&base[k]) continue;\
        var d=Object.getOwnPropertyDescriptor(globalThis,k);\
        if(!d||!d.configurable) continue;\
        var v=globalThis[k];\
        if(typeof v==='function'||typeof v==='symbol'||typeof v==='undefined') continue;\
        var j=JSON.stringify(v);\
        if(j!==undefined) out+='globalThis['+JSON.stringify(k)+']='+j+';\\n';\
      } catch (_) {}\
    }\
    return out;\
  })()\0";
  eval_to_string(ctx, SRC).unwrap_or_default()
}

fn json_literal(ctx: *mut JSContext, value: JSValue) -> Option<String> {
  let s =
    unsafe { JS_JSONStringify(ctx, value, jsv_undefined(), jsv_undefined()) };
  if s.tag == JS_TAG_EXCEPTION || jsv_is_undefined(&s) {
    if s.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    }
    return None;
  }
  let mut len = 0usize;
  let cs = unsafe { JS_ToCStringLen(ctx, &mut len, s) };
  let out = if cs.is_null() {
    None
  } else {
    Some(
      String::from_utf8_lossy(unsafe {
        std::slice::from_raw_parts(cs as *const u8, len)
      })
      .into_owned(),
    )
  };
  if !cs.is_null() {
    unsafe { JS_FreeCString(ctx, cs) };
  }
  unsafe { JS_FreeValue(ctx, s) };
  out
}

fn eval_script(ctx: *mut JSContext, script: &str) {
  if script.is_empty() {
    return;
  }
  if let Ok(c) = CString::new(script) {
    let r = unsafe {
      JS_Eval(
        ctx,
        c.as_ptr(),
        script.len(),
        c"<v8x snapshot replay>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if r.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      unsafe { JS_FreeValue(ctx, r) };
    }
  }
}

fn materialize_json(ctx: *mut JSContext, json: &str) -> *const Data {
  let Ok(c) = CString::new(json) else {
    return ptr::null();
  };
  let v = unsafe {
    JS_ParseJSON(ctx, c.as_ptr(), json.len(), c"<v8x snapshot data>".as_ptr())
  };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Data>(v)
}

fn capture_context(
  ctx: *mut JSContext,
  context: *const Context,
) -> ContextSnapshot {
  ContextSnapshot {
    script: capture_global_script(ctx),
    embedder_data: super::misc::snapshot_embedder_data(ctx),
    context_data: if context.is_null() {
      Vec::new()
    } else {
      Vec::new()
    },
  }
}

pub(crate) fn replay_context(ctx: *mut JSContext, snap: &ContextSnapshot) {
  eval_script(ctx, &snap.script);
  super::misc::restore_embedder_data(ctx, &snap.embedder_data);
}

pub(crate) fn construct(buf: *mut c_void, params: *const c_void) {
  let iso = crate::quickjs::core::v8__Isolate__New(params);
  crate::quickjs::core::v8__Isolate__Enter(iso);
  let state = Box::new(SnapshotCreatorState {
    isolate: iso,
    default_context: None,
    default_context_key: None,
    contexts: Vec::new(),
    isolate_data: Vec::new(),
    context_data: HashMap::new(),
  });
  if !buf.is_null() {
    unsafe { *(buf as *mut *mut SnapshotCreatorState) = Box::into_raw(state) };
  }
}

pub(crate) fn destruct(this: *mut c_void) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(*(this as *mut *mut SnapshotCreatorState))) };
  }
}

fn state<'a>(this: *mut c_void) -> Option<&'a mut SnapshotCreatorState> {
  if this.is_null() {
    return None;
  }
  let p = unsafe { *(this as *mut *mut SnapshotCreatorState) };
  if p.is_null() {
    None
  } else {
    Some(unsafe { &mut *p })
  }
}

pub(crate) fn get_isolate(this: *const c_void) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  let p = unsafe { *(this as *const *mut SnapshotCreatorState) };
  if p.is_null() {
    ptr::null_mut()
  } else {
    unsafe { (*p).isolate as *mut c_void }
  }
}

pub(crate) fn set_default_context(this: *mut c_void, context: *const Context) {
  let Some(st) = state(this) else {
    return;
  };
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return;
  }
  let mut snap = capture_context(ctx, context);
  snap.context_data =
    st.context_data.remove(&(ctx as usize)).unwrap_or_default();
  st.default_context = Some(snap);
  st.default_context_key = Some(ctx as usize);
}

pub(crate) fn add_context(this: *mut c_void, context: *const Context) -> usize {
  let Some(st) = state(this) else {
    return 0;
  };
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return st.contexts.len();
  }
  let mut snap = capture_context(ctx, context);
  snap.context_data =
    st.context_data.remove(&(ctx as usize)).unwrap_or_default();
  let idx = st.contexts.len();
  st.contexts.push(snap);
  idx
}

pub(crate) fn add_isolate_data(this: *mut c_void, data: *const Data) -> usize {
  let Some(st) = state(this) else {
    return 0;
  };
  let ctx = iso_state(st.isolate)
    .contexts
    .last()
    .copied()
    .unwrap_or(iso_state(st.isolate).ctx);
  let idx = st.isolate_data.len();
  st.isolate_data.push(json_literal(ctx, jsval_of(data)));
  idx
}

pub(crate) fn add_context_data(
  this: *mut c_void,
  context: *const Context,
  data: *const Data,
) -> usize {
  let Some(st) = state(this) else {
    return 0;
  };
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return 0;
  }
  let slots = st.context_data.entry(ctx as usize).or_default();
  let idx = slots.len();
  slots.push(json_literal(ctx, jsval_of(data)));
  idx
}

pub(crate) fn create_blob(this: *mut c_void) -> RawStartupDataAbi {
  let Some(st) = state(this) else {
    return RawStartupDataAbi {
      data: ptr::null(),
      raw_size: 0,
    };
  };
  let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
  let mut default_context = st.default_context.clone().unwrap_or_default();
  if let Some(key) = st.default_context_key {
    if let Some(data) = st.context_data.remove(&key) {
      default_context.context_data = data;
    }
  }
  let blob = SnapshotBlob {
    default_context,
    contexts: st.contexts.clone(),
    isolate_data: st.isolate_data.clone(),
  };
  if let Ok(mut m) = blobs().lock() {
    m.insert(id, blob);
  }
  let bytes = format!("{}{}", std::str::from_utf8(SNAP_PREFIX).unwrap(), id);
  let Ok(c) = CString::new(bytes) else {
    return RawStartupDataAbi {
      data: ptr::null(),
      raw_size: 0,
    };
  };
  let raw_size = c.as_bytes().len() as c_int;
  RawStartupDataAbi {
    data: c.into_raw(),
    raw_size,
  }
}

pub(crate) fn startup_data_is_valid(this: *const c_void) -> bool {
  parse_blob_id(this).and_then(get_blob).is_some()
}

pub(crate) fn startup_data_delete(this: *const c_char) {
  if !this.is_null() {
    unsafe { drop(CString::from_raw(this as *mut c_char)) };
  }
}

pub(crate) fn context_data_once(
  context: *const Context,
  index: usize,
) -> *const Data {
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  super::misc::take_snapshot_context_data(ctx, index)
    .as_deref()
    .map(|json| materialize_json(ctx, json))
    .unwrap_or(ptr::null())
}

pub(crate) fn isolate_data_once(
  isolate: *mut RealIsolate,
  index: usize,
) -> *const Data {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let Some(Some(json)) =
    st.snapshot_isolate_data.get_mut(index).map(Option::take)
  else {
    return ptr::null();
  };
  materialize_json(st.contexts.last().copied().unwrap_or(st.ctx), &json)
}

pub(crate) fn validate_cow_startup_data(this: *const c_void) -> bool {
  parse_blob_id(this).is_some()
    || (!this.is_null()
      && unsafe { &*(this as *const RawStartupDataAbi) }.raw_size > 0)
}
