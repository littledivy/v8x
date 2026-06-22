// Family: "primitive" — Number/Integer/Int32/Uint32/Boolean/BigInt/Symbol/Name/Private/Data
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::support::int;
use crate::{
    BigInt, Boolean, Context, Data, Int32, Integer, Number, Primitive, Private, RealIsolate,
    String as V8String, Symbol, Value,
};
use crate::shim_core::{ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval};
use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr;

// Extra JSC C functions not declared in jsc_sys.rs.
unsafe extern "C" {
    fn JSValueIsStrictEqual(ctx: JSContextRef, a: JSValueRef, b: JSValueRef) -> bool;
    fn JSValueToObject(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
}

// Evaluate a JS source string in `ctx` and return the resulting JSValueRef
// (or null on failure / empty ctx).
#[inline]
unsafe fn eval(ctx: JSContextRef, src: &str) -> JSValueRef {
    if ctx.is_null() {
        return ptr::null();
    }
    let Ok(c) = CString::new(src) else {
        return ptr::null();
    };
    let s = JSStringCreateWithUTF8CString(c.as_ptr());
    if s.is_null() {
        return ptr::null();
    }
    let mut exc: JSValueRef = ptr::null();
    let v = JSEvaluateScript(ctx, s, ptr::null_mut(), ptr::null_mut(), 0, &mut exc);
    JSStringRelease(s);
    if !exc.is_null() {
        return ptr::null();
    }
    v
}

// ===================================================================
// Number / Integer / Int32 / Uint32
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__New(isolate: *mut RealIsolate, value: f64) -> *const Number {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeNumber(ctx, value) };
    intern_ctx::<Number>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__Value(this: *const Number) -> f64 {
    let ctx = current_ctx();
    if ctx.is_null() {
        return 0.0;
    }
    let mut exc: JSValueRef = ptr::null();
    unsafe { JSValueToNumber(ctx, jsval(this), &mut exc) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__New(isolate: *mut RealIsolate, value: i32) -> *const Integer {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeNumber(ctx, value as f64) };
    intern_ctx::<Integer>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__NewFromUnsigned(
    isolate: *mut RealIsolate,
    value: u32,
) -> *const Integer {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeNumber(ctx, value as f64) };
    intern_ctx::<Integer>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__Value(this: *const Integer) -> i64 {
    let ctx = current_ctx();
    if ctx.is_null() {
        return 0;
    }
    let mut exc: JSValueRef = ptr::null();
    let n = unsafe { JSValueToNumber(ctx, jsval(this), &mut exc) };
    n as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Int32__Value(this: *const Int32) -> i32 {
    let ctx = current_ctx();
    if ctx.is_null() {
        return 0;
    }
    let mut exc: JSValueRef = ptr::null();
    let n = unsafe { JSValueToNumber(ctx, jsval(this), &mut exc) };
    n as i64 as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Uint32__Value(this: *const crate::Uint32) -> u32 {
    let ctx = current_ctx();
    if ctx.is_null() {
        return 0;
    }
    let mut exc: JSValueRef = ptr::null();
    let n = unsafe { JSValueToNumber(ctx, jsval(this), &mut exc) };
    n as i64 as u32
}

// ===================================================================
// Boolean / Null
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Boolean__New(isolate: *mut RealIsolate, value: bool) -> *const Boolean {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeBoolean(ctx, value) };
    intern_ctx::<Boolean>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Null(isolate: *mut RealIsolate) -> *const Primitive {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeNull(ctx) };
    intern_ctx::<Primitive>(ctx, v)
}

// ===================================================================
// BigInt — JSC has a BigInt JSType but no direct C creation API.
// Construct/inspect via evaluated JS (BigInt(...), value coercion).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__New(isolate: *mut RealIsolate, value: i64) -> *const BigInt {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    let v = unsafe { eval(ctx, &format!("BigInt(\"{value}\")")) };
    intern_ctx::<BigInt>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromUnsigned(
    isolate: *mut RealIsolate,
    value: u64,
) -> *const BigInt {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    let v = unsafe { eval(ctx, &format!("BigInt(\"{value}\")")) };
    intern_ctx::<BigInt>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromWords(
    context: *const Context,
    sign_bit: int,
    word_count: int,
    words: *const u64,
) -> *const BigInt {
    let ctx = ctx_of(context) as JSContextRef;
    if ctx.is_null() || (word_count > 0 && words.is_null()) {
        return ptr::null();
    }
    // value = (-1)^sign_bit * sum(words[i] * 2^(64*i))
    let mut expr = std::string::String::from("(");
    for i in 0..word_count.max(0) as usize {
        let w = unsafe { *words.add(i) };
        if i > 0 {
            expr.push('+');
        }
        // shift by 64*i bits
        expr.push_str(&format!("(BigInt(\"{w}\")<<{}n)", 64u64 * i as u64));
    }
    if word_count <= 0 {
        expr.push_str("0n");
    }
    expr.push(')');
    if sign_bit != 0 {
        expr = format!("(-{expr})");
    }
    let v = unsafe { eval(ctx, &expr) };
    intern_ctx::<BigInt>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Uint64Value(this: *const BigInt, lossless: *mut bool) -> u64 {
    let ctx = current_ctx();
    let val = jsval(this);
    if ctx.is_null() || val.is_null() {
        if !lossless.is_null() {
            unsafe { *lossless = false };
        }
        return 0;
    }
    // BigInt.asUintN(64, x) gives the wrapped value; compare to detect loss.
    unsafe {
        // truncated unsigned 64-bit value
        let truncated = bigint_to_u64(ctx, val);
        if !lossless.is_null() {
            // lossless iff x === BigInt.asUintN(64, x) and x >= 0
            let chk = format!(
                "((__v)=>(__v>=0n && __v===BigInt.asUintN(64,__v)))",
            );
            *lossless = bigint_predicate(ctx, val, &chk);
        }
        truncated
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Int64Value(this: *const BigInt, lossless: *mut bool) -> i64 {
    let ctx = current_ctx();
    let val = jsval(this);
    if ctx.is_null() || val.is_null() {
        if !lossless.is_null() {
            unsafe { *lossless = false };
        }
        return 0;
    }
    unsafe {
        let truncated = bigint_to_u64(ctx, val) as i64;
        if !lossless.is_null() {
            let chk = "((__v)=>(__v===BigInt.asIntN(64,__v)))";
            *lossless = bigint_predicate(ctx, val, chk);
        }
        truncated
    }
}

// Helper: evaluate `(fn)(value)` where the result is a boolean.
unsafe fn bigint_predicate(ctx: JSContextRef, val: JSValueRef, func_src: &str) -> bool {
    // Build: (func_src)(<val coerced via globalThis temp>)
    // We cannot easily inline the bigint literal, so stash it on globalThis.
    let stash = "globalThis.__v82jsc_bi";
    // store value
    if !stash_value(ctx, stash, val) {
        return false;
    }
    let src = format!("({func_src})({stash})");
    let r = eval(ctx, &src);
    if r.is_null() {
        return false;
    }
    JSValueToBoolean(ctx, r)
}

// Stash a JSValueRef onto a global path by round-tripping through a property.
// We can't set it from Rust directly without object API, so we use the fact
// that the value is already a handle: write it via a closure capturing nothing.
// Instead, encode the bigint as a decimal string and rebuild it.
unsafe fn stash_value(ctx: JSContextRef, path: &str, val: JSValueRef) -> bool {
    // Convert val (a BigInt) to its decimal string, then assign reconstructed.
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, val, &mut exc);
    if s.is_null() || !exc.is_null() {
        return false;
    }
    let dec = jsstring_to_string(s);
    JSStringRelease(s);
    // dec is like "12345" (String() of a BigInt has no trailing n)
    let src = format!("{path}=BigInt(\"{dec}\");true");
    let r = eval(ctx, &src);
    !r.is_null() && JSValueToBoolean(ctx, r)
}

// Truncate a BigInt to an unsigned 64-bit integer via two 32-bit halves.
unsafe fn bigint_to_u64(ctx: JSContextRef, val: JSValueRef) -> u64 {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, val, &mut exc);
    if s.is_null() || !exc.is_null() {
        return 0;
    }
    let dec = jsstring_to_string(s);
    JSStringRelease(s);
    // Reconstruct: BigInt.asUintN(64, x), then split into hi/lo 32-bit numbers.
    let lo_src = format!("Number(BigInt.asUintN(32,BigInt(\"{dec}\")))");
    let hi_src = format!("Number(BigInt.asUintN(32,BigInt(\"{dec}\")>>32n))");
    let lo = eval(ctx, &lo_src);
    let hi = eval(ctx, &hi_src);
    if lo.is_null() || hi.is_null() {
        return 0;
    }
    let lo_n = JSValueToNumber(ctx, lo, &mut exc) as u64;
    let hi_n = JSValueToNumber(ctx, hi, &mut exc) as u64;
    (hi_n << 32) | (lo_n & 0xFFFF_FFFF)
}

unsafe fn jsstring_to_string(s: JSStringRef) -> std::string::String {
    let cap = JSStringGetMaximumUTF8CStringSize(s);
    if cap == 0 {
        return std::string::String::new();
    }
    let mut buf = vec![0u8; cap];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut c_char, cap);
    if n == 0 {
        return std::string::String::new();
    }
    // n includes the trailing NUL.
    buf.truncate(n.saturating_sub(1));
    std::string::String::from_utf8_lossy(&buf).into_owned()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__WordCount(this: *const BigInt) -> int {
    let ctx = current_ctx();
    let val = jsval(this);
    if ctx.is_null() || val.is_null() {
        return 0;
    }
    // Number of 64-bit words needed for the magnitude.
    let dec = unsafe {
        let mut exc: JSValueRef = ptr::null();
        let s = JSValueToStringCopy(ctx, val, &mut exc);
        if s.is_null() || !exc.is_null() {
            return 0;
        }
        let d = jsstring_to_string(s);
        JSStringRelease(s);
        d
    };
    let src = format!(
        "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;let c=0;while(x>0n){{x>>=64n;c++;}}return c;}})()"
    );
    let r = unsafe { eval(ctx, &src) };
    if r.is_null() {
        return 0;
    }
    let mut exc: JSValueRef = ptr::null();
    let n = unsafe { JSValueToNumber(ctx, r, &mut exc) };
    n as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__ToWordsArray(
    this: *const BigInt,
    sign_bit: *mut int,
    word_count: *mut int,
    words: *mut u64,
) {
    let ctx = current_ctx();
    let val = jsval(this);
    let avail = if word_count.is_null() {
        0
    } else {
        unsafe { *word_count }.max(0) as usize
    };
    if ctx.is_null() || val.is_null() {
        if !sign_bit.is_null() {
            unsafe { *sign_bit = 0 };
        }
        if !word_count.is_null() {
            unsafe { *word_count = 0 };
        }
        return;
    }
    let dec = unsafe {
        let mut exc: JSValueRef = ptr::null();
        let s = JSValueToStringCopy(ctx, val, &mut exc);
        if s.is_null() || !exc.is_null() {
            if !sign_bit.is_null() {
                *sign_bit = 0;
            }
            if !word_count.is_null() {
                *word_count = 0;
            }
            return;
        }
        let d = jsstring_to_string(s);
        JSStringRelease(s);
        d
    };
    let neg = dec.starts_with('-');
    if !sign_bit.is_null() {
        unsafe { *sign_bit = if neg { 1 } else { 0 } };
    }
    // total words for magnitude
    let total_src = format!(
        "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;let c=0;while(x>0n){{x>>=64n;c++;}}return c;}})()"
    );
    let total = unsafe {
        let r = eval(ctx, &total_src);
        if r.is_null() {
            0usize
        } else {
            let mut exc: JSValueRef = ptr::null();
            JSValueToNumber(ctx, r, &mut exc) as usize
        }
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

// Extract the i-th 64-bit word of |BigInt(dec)|.
unsafe fn bigint_word_at(ctx: JSContextRef, dec: &str, i: usize) -> u64 {
    let shift = 64u64 * i as u64;
    let lo_src = format!(
        "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;return Number(BigInt.asUintN(32,x>>{shift}n));}})()"
    );
    let hi_src = format!(
        "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;return Number(BigInt.asUintN(32,x>>{}n));}})()",
        shift + 32
    );
    let lo = eval(ctx, &lo_src);
    let hi = eval(ctx, &hi_src);
    if lo.is_null() || hi.is_null() {
        return 0;
    }
    let mut exc: JSValueRef = ptr::null();
    let lo_n = JSValueToNumber(ctx, lo, &mut exc) as u64;
    let hi_n = JSValueToNumber(ctx, hi, &mut exc) as u64;
    (hi_n << 32) | (lo_n & 0xFFFF_FFFF)
}

// ===================================================================
// Private — JSC has no private symbols; use a unique JS Symbol as a stand-in.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__ForApi(
    isolate: *mut RealIsolate,
    name: *const V8String,
) -> *const Private {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let desc = if name.is_null() {
        std::string::String::new()
    } else {
        unsafe { jsvalue_to_desc(ctx, jsval(name)) }
    };
    // Registry-style: reuse Symbol.for so the same name yields the same private.
    let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
    let v = unsafe { eval(ctx, &format!("Symbol.for(\"v82jsc_private:{escaped}\")")) };
    intern_ctx::<Private>(ctx, v)
}

unsafe fn jsvalue_to_desc(ctx: JSContextRef, v: JSValueRef) -> std::string::String {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, v, &mut exc);
    if s.is_null() || !exc.is_null() {
        return std::string::String::new();
    }
    let d = jsstring_to_string(s);
    JSStringRelease(s);
    d
}

// ===================================================================
// Symbol
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__For(
    isolate: *mut RealIsolate,
    description: *const V8String,
) -> *const Symbol {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let desc = if description.is_null() {
        std::string::String::new()
    } else {
        unsafe { jsvalue_to_desc(ctx, jsval(description)) }
    };
    let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
    let v = unsafe { eval(ctx, &format!("Symbol.for(\"{escaped}\")")) };
    intern_ctx::<Symbol>(ctx, v)
}

macro_rules! well_known_symbol {
    ($fn_name:ident, $js:literal) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $fn_name(isolate: *mut RealIsolate) -> *const Symbol {
            let st = iso_state(isolate);
            let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
            if ctx.is_null() {
                return ptr::null();
            }
            let v = unsafe { eval(ctx, $js) };
            intern_ctx::<Symbol>(ctx, v)
        }
    };
}

well_known_symbol!(v8__Symbol__GetAsyncIterator, "Symbol.asyncIterator");
well_known_symbol!(v8__Symbol__GetHasInstance, "Symbol.hasInstance");
well_known_symbol!(v8__Symbol__GetIsConcatSpreadable, "Symbol.isConcatSpreadable");
well_known_symbol!(v8__Symbol__GetIterator, "Symbol.iterator");
well_known_symbol!(v8__Symbol__GetMatch, "Symbol.match");
well_known_symbol!(v8__Symbol__GetReplace, "Symbol.replace");
well_known_symbol!(v8__Symbol__GetSearch, "Symbol.search");
well_known_symbol!(v8__Symbol__GetSplit, "Symbol.split");
well_known_symbol!(v8__Symbol__GetToPrimitive, "Symbol.toPrimitive");
well_known_symbol!(v8__Symbol__GetToStringTag, "Symbol.toStringTag");
well_known_symbol!(v8__Symbol__GetUnscopables, "Symbol.unscopables");

// ===================================================================
// Data — identity/type predicates
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__EQ(this: *const Data, other: *const Data) -> bool {
    let ctx = current_ctx();
    let a = jsval(this);
    let b = jsval(other);
    if a == b {
        return true;
    }
    if ctx.is_null() || a.is_null() || b.is_null() {
        return false;
    }
    unsafe { JSValueIsStrictEqual(ctx, a, b) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsValue(this: *const Data) -> bool {
    // Any non-null JS value qualifies; templates/modules are not JSValues here.
    !this.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrimitive(this: *const Data) -> bool {
    let ctx = current_ctx();
    let v = jsval(this);
    if ctx.is_null() || v.is_null() {
        return false;
    }
    // Primitive iff not an object (object includes functions/arrays).
    !unsafe { JSValueIsObject(ctx, v) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsFunctionTemplate(this: *const Data) -> bool {
    // JSC has no FunctionTemplate concept in this shim layer.
    // TODO(v82jsc): wire up once templates are modeled.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModule(this: *const Data) -> bool {
    // TODO(v82jsc): modules are not represented as JSValues here.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModuleRequest(this: *const Data) -> bool {
    // TODO(v82jsc): module requests are not represented as JSValues here.
    false
}
