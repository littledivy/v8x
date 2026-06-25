// Family: "primitive" — QuickJS-ng backend.
// Number / Integer / Int32 / Uint32 / Boolean / Null / BigInt / Symbol /
// Private / Data predicates.
//
// Ports the JSC template (src/shim_primitive.rs) to QuickJS-ng. Numeric and
// boolean primitives use the direct JS_New* constructors; BigInt scalar
// round-trips use JS_NewBigInt64/JS_ToBigInt64 etc.; the words-based BigInt API
// (which QuickJS-ng's C ABI does not expose) is emulated via small evaluated JS
// snippets. Well-known symbols are read off the global `Symbol` object.
//
// Refcount discipline: every NEW handle is routed through `intern` (for an
// owned/+1 JSValue returned by a QuickJS C fn) or `intern_dup` (for a borrowed
// value we want to keep). Temporary JSValues we create and don't keep get
// JS_FreeValue.

#![allow(non_snake_case)]

use crate::qjs::quickjs_sys::*;
use crate::qjs::shim_core::{ctx_of, current_ctx, intern, iso_state, jsval_of};
use crate::support::int;
use crate::{
    BigInt, Boolean, Context, Data, Int32, Integer, Number, Primitive, Private, RealIsolate,
    String as V8String, Symbol, Uint32, Value,
};
use std::ffi::CString;
use std::ptr;

// QuickJS-ng C functions used here that are not declared in quickjs_sys.rs.
unsafe extern "C" {
    fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;
    fn JS_ToBigInt64(ctx: *mut JSContext, pres: *mut i64, val: JSValue) -> std::os::raw::c_int;
    fn JS_ToBigUint64(ctx: *mut JSContext, pres: *mut u64, val: JSValue) -> std::os::raw::c_int;
}

/// Context to root a new handle against: the innermost entered ctx, falling
/// back to the isolate's single context.
#[inline]
fn iso_ctx(isolate: *mut RealIsolate) -> *mut JSContext {
    if isolate.is_null() {
        return current_ctx();
    }
    let st = iso_state(isolate);
    st.contexts.last().copied().unwrap_or(st.ctx)
}

/// Evaluate `src` in `ctx` returning the owned (+1) result JSValue, or
/// `undefined` on failure / null ctx (after draining any pending exception).
unsafe fn eval(ctx: *mut JSContext, src: &str) -> JSValue {
    if ctx.is_null() {
        return jsv_undefined();
    }
    let Ok(c) = CString::new(src) else {
        return jsv_undefined();
    };
    let fname = c"<v82jsc>";
    let v = JS_Eval(
        ctx,
        c.as_ptr(),
        src.len(),
        fname.as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
    );
    if v.tag == JS_TAG_EXCEPTION {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        return jsv_undefined();
    }
    v
}

/// Decimal string of a JSValue (used for BigInt magnitude math via eval).
unsafe fn to_dec_string(ctx: *mut JSContext, v: JSValue) -> std::string::String {
    let mut len: usize = 0;
    let s = JS_ToCStringLen(ctx, &mut len, v);
    if s.is_null() {
        // Drain a possible exception from the failed coercion.
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        return std::string::String::new();
    }
    let bytes = std::slice::from_raw_parts(s as *const u8, len);
    let out = std::string::String::from_utf8_lossy(bytes).into_owned();
    JS_FreeCString(ctx, s);
    out
}

// ===================================================================
// Number / Integer / Int32 / Uint32
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__New(isolate: *mut RealIsolate, value: f64) -> *const Number {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JS_NewFloat64(ctx, value) };
    intern::<Number>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__Value(this: *const Number) -> f64 {
    let ctx = current_ctx();
    let v = jsval_of(this);
    if ctx.is_null() {
        // Fast path for already-numeric tagged values.
        return match v.tag {
            JS_TAG_INT => unsafe { v.u.int32 as f64 },
            JS_TAG_FLOAT64 => unsafe { v.u.float64 },
            _ => f64::NAN,
        };
    }
    let mut out: f64 = f64::NAN;
    unsafe { JS_ToFloat64(ctx, &mut out, v) };
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__New(isolate: *mut RealIsolate, value: i32) -> *const Integer {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JS_NewInt32(ctx, value) };
    intern::<Integer>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__NewFromUnsigned(
    isolate: *mut RealIsolate,
    value: u32,
) -> *const Integer {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    // JS_NewUint32 yields an INT tag when it fits in i32, else a FLOAT64.
    let v = unsafe { JS_NewUint32(ctx, value) };
    intern::<Integer>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__Value(this: *const Integer) -> i64 {
    let v = jsval_of(this);
    match v.tag {
        JS_TAG_INT => unsafe { v.u.int32 as i64 },
        JS_TAG_FLOAT64 => unsafe { v.u.float64 as i64 },
        _ => {
            let ctx = current_ctx();
            if ctx.is_null() {
                return 0;
            }
            let mut out: i64 = 0;
            unsafe { JS_ToInt64(ctx, &mut out, v) };
            out
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Int32__Value(this: *const Int32) -> i32 {
    let v = jsval_of(this);
    if v.tag == JS_TAG_INT {
        return unsafe { v.u.int32 };
    }
    let ctx = current_ctx();
    if ctx.is_null() {
        return unsafe {
            match v.tag {
                JS_TAG_FLOAT64 => v.u.float64 as i64 as i32,
                _ => 0,
            }
        };
    }
    let mut out: i32 = 0;
    unsafe { JS_ToInt32(ctx, &mut out, v) };
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Uint32__Value(this: *const Uint32) -> u32 {
    let v = jsval_of(this);
    if v.tag == JS_TAG_INT {
        return unsafe { v.u.int32 as u32 };
    }
    let ctx = current_ctx();
    if ctx.is_null() {
        return unsafe {
            match v.tag {
                JS_TAG_FLOAT64 => v.u.float64 as i64 as u32,
                _ => 0,
            }
        };
    }
    // ToInt32 then reinterpret bits as u32 — matches V8's Uint32::Value
    // (ToUint32 == ToInt32 modulo 2^32).
    let mut out: i32 = 0;
    unsafe { JS_ToInt32(ctx, &mut out, v) };
    out as u32
}

// ===================================================================
// Boolean / Null
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Boolean__New(isolate: *mut RealIsolate, value: bool) -> *const Boolean {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JS_NewBool(ctx, value as std::os::raw::c_int) };
    intern::<Boolean>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Null(isolate: *mut RealIsolate) -> *const Primitive {
    let _ = isolate;
    // `null` is a static tagged value with no refcount; intern moves it in.
    intern::<Primitive>(jsv_null())
}

// ===================================================================
// BigInt — scalar paths use the native JS_NewBigInt64 / JS_ToBig*64 ABI.
// The words-based paths have no C-ABI equivalent in QuickJS-ng, so they are
// emulated via evaluated BigInt arithmetic.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__New(isolate: *mut RealIsolate, value: i64) -> *const BigInt {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JS_NewBigInt64(ctx, value) };
    intern::<BigInt>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromUnsigned(
    isolate: *mut RealIsolate,
    value: u64,
) -> *const BigInt {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JS_NewBigUint64(ctx, value) };
    intern::<BigInt>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromWords(
    context: *const Context,
    sign_bit: int,
    word_count: int,
    words: *const u64,
) -> *const BigInt {
    let ctx = ctx_of(context);
    if ctx.is_null() || (word_count > 0 && words.is_null()) {
        return ptr::null();
    }
    // value = (-1)^sign_bit * sum(words[i] * 2^(64*i))
    let n = word_count.max(0) as usize;
    let mut expr = std::string::String::from("(");
    for i in 0..n {
        let w = unsafe { *words.add(i) };
        if i > 0 {
            expr.push('+');
        }
        expr.push_str(&format!("(BigInt(\"{w}\")<<{}n)", 64u64 * i as u64));
    }
    if n == 0 {
        expr.push_str("0n");
    }
    expr.push(')');
    if sign_bit != 0 {
        expr = format!("(-{expr})");
    }
    let v = unsafe { eval(ctx, &expr) };
    intern::<BigInt>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Uint64Value(this: *const BigInt, lossless: *mut bool) -> u64 {
    let ctx = current_ctx();
    let val = jsval_of(this);
    if ctx.is_null() {
        if !lossless.is_null() {
            unsafe { *lossless = false };
        }
        return 0;
    }
    let mut out: u64 = 0;
    // JS_ToBigUint64 returns the value truncated (wrapped) to 64 bits.
    let rc = unsafe { JS_ToBigUint64(ctx, &mut out, val) };
    if rc != 0 {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        if !lossless.is_null() {
            unsafe { *lossless = false };
        }
        return 0;
    }
    if !lossless.is_null() {
        // lossless iff the value is >= 0 and fits in 64 unsigned bits, i.e.
        // BigInt.asUintN(64, x) === x.
        unsafe { *lossless = bigint_eq_self(ctx, val, "BigInt.asUintN(64,__v)===__v") };
    }
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Int64Value(this: *const BigInt, lossless: *mut bool) -> i64 {
    let ctx = current_ctx();
    let val = jsval_of(this);
    if ctx.is_null() {
        if !lossless.is_null() {
            unsafe { *lossless = false };
        }
        return 0;
    }
    let mut out: i64 = 0;
    let rc = unsafe { JS_ToBigInt64(ctx, &mut out, val) };
    if rc != 0 {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        if !lossless.is_null() {
            unsafe { *lossless = false };
        }
        return 0;
    }
    if !lossless.is_null() {
        unsafe { *lossless = bigint_eq_self(ctx, val, "BigInt.asIntN(64,__v)===__v") };
    }
    out
}

/// Evaluate a boolean predicate of the form `<expr in __v>` against `val`
/// (a BigInt), by reconstructing the BigInt from its decimal string.
unsafe fn bigint_eq_self(ctx: *mut JSContext, val: JSValue, pred: &str) -> bool {
    let dec = to_dec_string(ctx, val);
    if dec.is_empty() {
        return false;
    }
    let src = format!("((__v)=>({pred}))(BigInt(\"{dec}\"))");
    let r = eval(ctx, &src);
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    b
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__WordCount(this: *const BigInt) -> int {
    let ctx = current_ctx();
    let val = jsval_of(this);
    if ctx.is_null() {
        return 0;
    }
    let dec = unsafe { to_dec_string(ctx, val) };
    if dec.is_empty() {
        return 0;
    }
    let src = format!(
        "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;let c=0;while(x>0n){{x>>=64n;c++;}}return c;}})()"
    );
    let r = unsafe { eval(ctx, &src) };
    let mut out: i32 = 0;
    unsafe { JS_ToInt32(ctx, &mut out, r) };
    unsafe { JS_FreeValue(ctx, r) };
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__ToWordsArray(
    this: *const BigInt,
    sign_bit: *mut int,
    word_count: *mut int,
    words: *mut u64,
) {
    let ctx = current_ctx();
    let val = jsval_of(this);
    let avail = if word_count.is_null() {
        0
    } else {
        unsafe { *word_count }.max(0) as usize
    };
    if ctx.is_null() {
        if !sign_bit.is_null() {
            unsafe { *sign_bit = 0 };
        }
        if !word_count.is_null() {
            unsafe { *word_count = 0 };
        }
        return;
    }
    let dec = unsafe { to_dec_string(ctx, val) };
    let neg = dec.starts_with('-');
    if !sign_bit.is_null() {
        unsafe { *sign_bit = if neg { 1 } else { 0 } };
    }
    // Total number of 64-bit words in the magnitude.
    let total_src = format!(
        "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;let c=0;while(x>0n){{x>>=64n;c++;}}return c;}})()"
    );
    let total = unsafe {
        let r = eval(ctx, &total_src);
        let mut t: i32 = 0;
        JS_ToInt32(ctx, &mut t, r);
        JS_FreeValue(ctx, r);
        t.max(0) as usize
    };
    let to_write = total.min(avail);
    for i in 0..to_write {
        let w = unsafe { bigint_word_at(ctx, &dec, i) };
        unsafe { *words.add(i) = w };
    }
    if !word_count.is_null() {
        unsafe { *word_count = total as int };
    }
}

/// Extract the i-th 64-bit word of |BigInt(dec)|.
unsafe fn bigint_word_at(ctx: *mut JSContext, dec: &str, i: usize) -> u64 {
    let shift = 64u64 * i as u64;
    let src = format!(
        "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;return BigInt.asUintN(64,x>>{shift}n).toString();}})()"
    );
    let r = eval(ctx, &src);
    if r.tag == JS_TAG_EXCEPTION {
        JS_FreeValue(ctx, r);
        return 0;
    }
    let s = to_dec_string(ctx, r);
    JS_FreeValue(ctx, r);
    s.parse::<u64>().unwrap_or(0)
}

// ===================================================================
// Private — QuickJS has no V8-style private symbols; use a registered
// Symbol.for in a namespaced key as a stable stand-in.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__ForApi(
    isolate: *mut RealIsolate,
    name: *const V8String,
) -> *const Private {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let desc = if name.is_null() {
        std::string::String::new()
    } else {
        unsafe { to_dec_string(ctx, jsval_of(name)) }
    };
    let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
    let v = unsafe { eval(ctx, &format!("Symbol.for(\"v82jsc_private:{escaped}\")")) };
    intern::<Private>(v)
}

/// `Private::New` — a fresh, unique private (unlike `ForApi`, NOT registered, so
/// two calls with the same name are distinct). Backed by a fresh unique Symbol.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__New(
    isolate: *mut RealIsolate,
    name: *const V8String,
) -> *const Private {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let desc = if name.is_null() {
        std::string::String::new()
    } else {
        unsafe { to_dec_string(ctx, jsval_of(name)) }
    };
    let Ok(cdesc) = CString::new(desc) else {
        return ptr::null();
    };
    // Fresh, non-global symbol → unique private key.
    let v = unsafe { JS_NewSymbol(ctx, cdesc.as_ptr(), 0) };
    if v.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<Private>(v)
}

// ===================================================================
// Symbol
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__For(
    isolate: *mut RealIsolate,
    description: *const V8String,
) -> *const Symbol {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let desc = if description.is_null() {
        std::string::String::new()
    } else {
        unsafe { to_dec_string(ctx, jsval_of(description)) }
    };
    let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
    let v = unsafe { eval(ctx, &format!("Symbol.for(\"{escaped}\")")) };
    intern::<Symbol>(v)
}

/// `Symbol(description)` — a fresh unique (non-registered) symbol.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__New(
    isolate: *mut RealIsolate,
    description: *const V8String,
) -> *const Symbol {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    // JS_NewSymbol takes a NUL-terminated C string description; build one from
    // the v8 String handle (or empty when none was supplied).
    let desc = if description.is_null() {
        std::string::String::new()
    } else {
        unsafe { to_dec_string(ctx, jsval_of(description)) }
    };
    let Ok(cdesc) = CString::new(desc) else {
        return ptr::null();
    };
    // is_global = 0 → a fresh, unregistered Symbol (V8 `Symbol::New` semantics).
    let v = unsafe { JS_NewSymbol(ctx, cdesc.as_ptr(), 0) };
    if v.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<Symbol>(v)
}

/// `symbol.description` — the symbol's description string, or `undefined`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__Description(
    this: *const Symbol,
    isolate: *mut RealIsolate,
) -> *const Value {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    // A symbol is not an object, so we can't JS_GetPropertyStr it directly. Read
    // `this.description` with the symbol bound as `this` via JS_EvalThis.
    unsafe {
        let desc = symbol_description(ctx, jsval_of(this));
        if desc.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return ptr::null();
        }
        intern::<Value>(desc)
    }
}

/// Invoke `Symbol.prototype.description`'s getter with `sym` as the receiver,
/// returning an owned (+1) result. Uses `JS_EvalThis` so the accessor sees the
/// symbol as `this`.
unsafe fn symbol_description(ctx: *mut JSContext, sym: JSValue) -> JSValue {
    const SRC: &[u8] = b"this.description\0";
    unsafe {
        JS_EvalThis(
            ctx,
            sym,
            SRC.as_ptr() as *const std::os::raw::c_char,
            SRC.len() - 1,
            c"<sym-desc>".as_ptr(),
            JS_EVAL_TYPE_GLOBAL,
        )
    }
}

/// Read a well-known symbol off the global `Symbol` constructor object,
/// e.g. `Symbol.iterator`. Returns an owned (+1) JSValue.
unsafe fn well_known(ctx: *mut JSContext, prop: &[u8]) -> JSValue {
    if ctx.is_null() {
        return jsv_undefined();
    }
    let global = JS_GetGlobalObject(ctx);
    let sym_ctor = JS_GetPropertyStr(ctx, global, c"Symbol".as_ptr());
    JS_FreeValue(ctx, global);
    if sym_ctor.tag == JS_TAG_EXCEPTION {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        return jsv_undefined();
    }
    // `prop` is a NUL-terminated byte string literal.
    let v = JS_GetPropertyStr(ctx, sym_ctor, prop.as_ptr() as *const std::os::raw::c_char);
    JS_FreeValue(ctx, sym_ctor);
    if v.tag == JS_TAG_EXCEPTION {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        return jsv_undefined();
    }
    v
}

macro_rules! well_known_symbol {
    ($fn_name:ident, $prop:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $fn_name(isolate: *mut RealIsolate) -> *const Symbol {
            let ctx = iso_ctx(isolate);
            if ctx.is_null() {
                return ptr::null();
            }
            let v = unsafe { well_known(ctx, $prop) };
            intern::<Symbol>(v)
        }
    };
}

well_known_symbol!(v8__Symbol__GetAsyncIterator, b"asyncIterator\0");
well_known_symbol!(v8__Symbol__GetHasInstance, b"hasInstance\0");
well_known_symbol!(v8__Symbol__GetIsConcatSpreadable, b"isConcatSpreadable\0");
well_known_symbol!(v8__Symbol__GetIterator, b"iterator\0");
well_known_symbol!(v8__Symbol__GetMatch, b"match\0");
well_known_symbol!(v8__Symbol__GetReplace, b"replace\0");
well_known_symbol!(v8__Symbol__GetSearch, b"search\0");
well_known_symbol!(v8__Symbol__GetSplit, b"split\0");
well_known_symbol!(v8__Symbol__GetToPrimitive, b"toPrimitive\0");
well_known_symbol!(v8__Symbol__GetToStringTag, b"toStringTag\0");
well_known_symbol!(v8__Symbol__GetUnscopables, b"unscopables\0");

// ===================================================================
// Data — identity / type predicates
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__EQ(this: *const Data, other: *const Data) -> bool {
    let a = jsval_of(this);
    let b = jsval_of(other);
    // Fast path for pointer-payload values (object/string/symbol/bigint/...):
    // identical tag + identical pointer ⇒ same underlying heap value. `ptr` is
    // the meaningful payload for every ref-counted tag.
    // Ref-counted tags are negative (JS_TAG_FIRST..=JS_TAG_OBJECT).
    if a.tag == b.tag && a.tag < 0 && unsafe { a.u.ptr == b.u.ptr } {
        return true;
    }
    let ctx = current_ctx();
    if ctx.is_null() {
        // No ctx for a strict-equal call; fall back to exact bit identity.
        return a.tag == b.tag && unsafe { a.u.ptr == b.u.ptr };
    }
    unsafe { JS_IsStrictEqual(ctx, a, b) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsValue(this: *const Data) -> bool {
    // Any non-null handle backs a JSValue in this backend. (Templates/modules
    // are also JSValues here — IsValue is permissive in V8 too for those that
    // are values; deno relies on the common case.)
    !this.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrimitive(this: *const Data) -> bool {
    if this.is_null() {
        return false;
    }
    let v = jsval_of(this);
    // Primitive iff not an object-tagged value (objects include functions,
    // arrays, etc.). Undefined/null/bool/int/float/string/symbol/bigint are
    // all primitives.
    v.tag != JS_TAG_OBJECT
        && v.tag != JS_TAG_MODULE
        && v.tag != JS_TAG_FUNCTION_BYTECODE
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsFunctionTemplate(this: *const Data) -> bool {
    // FunctionTemplates are modeled as objects carrying a marker property by
    // the template family; detect that marker here.
    has_marker(this, c"__v82jsc_function_template")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModule(this: *const Data) -> bool {
    if this.is_null() {
        return false;
    }
    jsval_of(this).tag == JS_TAG_MODULE
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModuleRequest(this: *const Data) -> bool {
    // ModuleRequest objects (built by the module family) carry a marker prop.
    has_marker(this, c"__v8jsc_module_request")
}

/// True iff `this` is an object carrying a truthy own property `key`.
fn has_marker(this: *const Data, key: &std::ffi::CStr) -> bool {
    if this.is_null() {
        return false;
    }
    let ctx = current_ctx();
    if ctx.is_null() {
        return false;
    }
    let v = jsval_of(this);
    if v.tag != JS_TAG_OBJECT {
        return false;
    }
    unsafe {
        let prop = JS_GetPropertyStr(ctx, v, key.as_ptr());
        if prop.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return false;
        }
        let b = JS_ToBool(ctx, prop) != 0;
        JS_FreeValue(ctx, prop);
        b
    }
}
