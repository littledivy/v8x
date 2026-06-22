//! Raw FFI bindings to Apple's JavaScriptCore C API.
//!
//! Only the subset needed by the v82jsc surface is declared. Types are opaque
//! pointers, matching `<JavaScriptCore/JSBase.h>`.
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int};

// Opaque JSC struct types. We never construct these; we only hold pointers.
pub enum OpaqueJSContextGroup {}
pub enum OpaqueJSContext {}
pub enum OpaqueJSString {}
pub enum OpaqueJSValue {}
pub enum OpaqueJSClass {}
pub enum OpaqueJSPropertyNameArray {}

pub type JSContextGroupRef = *const OpaqueJSContextGroup;
pub type JSGlobalContextRef = *mut OpaqueJSContext;
pub type JSContextRef = *const OpaqueJSContext;
pub type JSStringRef = *mut OpaqueJSString;
pub type JSValueRef = *const OpaqueJSValue;
pub type JSObjectRef = *mut OpaqueJSValue;
pub type JSClassRef = *mut OpaqueJSClass;

/// `JSType` enum from JSValueRef.h
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JSType {
    Undefined = 0,
    Null = 1,
    Boolean = 2,
    Number = 3,
    String = 4,
    Object = 5,
    Symbol = 6,
    BigInt = 7,
}

unsafe extern "C" {
    // --- Context group (≈ Isolate / VM heap) ---
    pub fn JSContextGroupCreate() -> JSContextGroupRef;
    pub fn JSContextGroupRetain(group: JSContextGroupRef) -> JSContextGroupRef;
    pub fn JSContextGroupRelease(group: JSContextGroupRef);

    // --- Global context (≈ Context) ---
    pub fn JSGlobalContextCreateInGroup(
        group: JSContextGroupRef,
        global_object_class: JSClassRef,
    ) -> JSGlobalContextRef;
    pub fn JSGlobalContextRetain(ctx: JSGlobalContextRef) -> JSGlobalContextRef;
    pub fn JSGlobalContextRelease(ctx: JSGlobalContextRef);
    pub fn JSContextGetGlobalObject(ctx: JSContextRef) -> JSObjectRef;
    pub fn JSContextGetGroup(ctx: JSContextRef) -> JSContextGroupRef;
    pub fn JSContextGetGlobalContext(ctx: JSContextRef) -> JSGlobalContextRef;

    // --- Strings ---
    pub fn JSStringCreateWithUTF8CString(string: *const c_char) -> JSStringRef;
    pub fn JSStringRetain(string: JSStringRef) -> JSStringRef;
    pub fn JSStringRelease(string: JSStringRef);
    pub fn JSStringGetLength(string: JSStringRef) -> usize;
    pub fn JSStringGetMaximumUTF8CStringSize(string: JSStringRef) -> usize;
    pub fn JSStringGetUTF8CString(
        string: JSStringRef,
        buffer: *mut c_char,
        buffer_size: usize,
    ) -> usize;

    // --- Eval & syntax ---
    pub fn JSEvaluateScript(
        ctx: JSContextRef,
        script: JSStringRef,
        this_object: JSObjectRef,
        source_url: JSStringRef,
        starting_line_number: c_int,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSCheckScriptSyntax(
        ctx: JSContextRef,
        script: JSStringRef,
        source_url: JSStringRef,
        starting_line_number: c_int,
        exception: *mut JSValueRef,
    ) -> bool;

    // --- Value creation ---
    pub fn JSValueMakeUndefined(ctx: JSContextRef) -> JSValueRef;
    pub fn JSValueMakeNull(ctx: JSContextRef) -> JSValueRef;
    pub fn JSValueMakeBoolean(ctx: JSContextRef, boolean: bool) -> JSValueRef;
    pub fn JSValueMakeNumber(ctx: JSContextRef, number: f64) -> JSValueRef;
    pub fn JSValueMakeString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;

    // --- Value inspection ---
    pub fn JSValueGetType(ctx: JSContextRef, value: JSValueRef) -> JSType;
    pub fn JSValueIsUndefined(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsNull(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsBoolean(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsNumber(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsString(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsObject(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsArray(ctx: JSContextRef, value: JSValueRef) -> bool;

    // --- Value conversion ---
    pub fn JSValueToBoolean(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueToNumber(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> f64;
    pub fn JSValueToStringCopy(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSStringRef;

    // --- GC rooting ---
    pub fn JSValueProtect(ctx: JSContextRef, value: JSValueRef);
    pub fn JSValueUnprotect(ctx: JSContextRef, value: JSValueRef);
    pub fn JSGarbageCollect(ctx: JSContextRef);
}
