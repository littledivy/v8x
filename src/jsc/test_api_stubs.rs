//! Safe link-stubs for v8 C-ABI symbols referenced by the large rusty_v8
//! test targets (notably `test_api.rs`) that the JSC backend does not
//! implement yet. Each returns a benign default (null / 0 / false /
//! `Nothing`) so the targets LINK and the hundreds of tests that don't
//! touch these paths run. Tests that do exercise them fail gracefully
//! (they never crash the process). Promote individual stubs to real
//! implementations over time.
#![allow(non_snake_case, unused)]

use crate::MicrotasksPolicy;
use crate::support::{MaybeBool, int};
use crate::{
  Intrinsic, PropertyAttribute, RegExpCreationFlags, SideEffectType,
};
use std::os::raw::c_void;

#[unsafe(no_mangle)]
pub extern "C" fn icu_set_default_locale(_locale: *const c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__GetWireBytesRef(
  _this: *mut c_void,
  _length: *mut c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__SourceUrl(
  _this: *mut c_void,
  _length: *mut c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetMicrotaskQueue(
  _this: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetMicrotaskQueue(
  _this: *const c_void,
  _microtask_queue: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CpuProfiler__CollectSample(
  _isolate: *mut c_void,
  _trace_id: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CpuProfiler__UseDetailedSourcePositionsForProfiling(
  _isolate: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsObjectTemplate(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrivate(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Clear(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__IsEmpty(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__CaptureStackTrace(
  _context: *const c_void,
  _object: *const c_void,
) -> MaybeBool {
  MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__GetStackTrace(
  _exception: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ExternalOneByteStringResource__data(
  _this: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ExternalOneByteStringResource__length(
  _this: *const c_void,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__IsConstructCall(
  _this: *const c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetAccessorProperty(
  _this: *const c_void,
  _key: *const c_void,
  _getter: *const c_void,
  _setter: *const c_void,
  _attr: PropertyAttribute,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetName(_this: *const c_void) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptColumnNumber(
  _this: *const c_void,
) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptLineNumber(
  _this: *const c_void,
) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptOrigin(
  _this: *const c_void,
  _out: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__ScriptId(_this: *const c_void) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddMessageListener(
  _isolate: *mut c_void,
  _callback: *const c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__ClearKeptObjects(_isolate: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentHostDefinedOptions(
  _this: *mut c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetDataFromSnapshotOnce(
  _this: *mut c_void,
  _index: usize,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetMicrotasksPolicy(
  _isolate: *const c_void,
) -> MicrotasksPolicy {
  MicrotasksPolicy::Explicit
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__MemoryPressureNotification(
  _this: *mut c_void,
  _level: u8,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveGCEpilogueCallback(
  _isolate: *mut c_void,
  _callback: *const c_void,
  _data: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveGCPrologueCallback(
  _isolate: *mut c_void,
  _callback: *const c_void,
  _data: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetAllowAtomicsWait(
  _isolate: *mut c_void,
  _allow: bool,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetOOMErrorHandler(
  _isolate: *mut c_void,
  _callback: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPromiseHook(
  _isolate: *mut c_void,
  _hook: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetUseCounterCallback(
  _isolate: *mut c_void,
  _callback: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Clear(_this: *const c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Delete(
  _this: *const c_void,
  _context: *const c_void,
  _key: *const c_void,
) -> MaybeBool {
  MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Get(
  _this: *const c_void,
  _context: *const c_void,
  _key: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Has(
  _this: *const c_void,
  _context: *const c_void,
  _key: *const c_void,
) -> MaybeBool {
  MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__New(_isolate: *mut c_void) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Set(
  _this: *const c_void,
  _context: *const c_void,
  _key: *const c_void,
  _value: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__ErrorLevel(_this: *const c_void) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetEndColumn(_this: *const c_void) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetEndPosition(_this: *const c_void) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStartPosition(_this: *const c_void) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetWasmFunctionIndex(
  _this: *const c_void,
) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__IsOpaque(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__IsSharedCrossOrigin(
  _this: *const c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSourceTextModule(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__ScriptId(_this: *const c_void) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__InternalFieldCount(
  _this: *const c_void,
) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetImmutableProto(_this: *const c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNativeDataProperty(
  _this: *const c_void,
  _key: *const c_void,
  _getter: *const c_void,
  _setter: *const c_void,
  _data_or_null: *const c_void,
  _attr: PropertyAttribute,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetAlignedPointerFromInternalField(
  _this: *const c_void,
  _index: int,
  _tag: u16,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetInternalField(
  _this: *const c_void,
  _index: int,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__InternalFieldCount(_this: *const c_void) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetAccessor(
  _this: *const c_void,
  _context: *const c_void,
  _key: *const c_void,
  _getter: *const c_void,
  _setter: *const c_void,
  _data_or_null: *const c_void,
  _attr: PropertyAttribute,
) -> MaybeBool {
  MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetAlignedPointerInInternalField(
  _this: *const c_void,
  _index: int,
  _value: *const c_void,
  _tag: u16,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetInternalField(
  _this: *const c_void,
  _index: int,
  _data: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetLazyDataProperty(
  _this: *const c_void,
  _context: *const c_void,
  _key: *const c_void,
  _getter: *const c_void,
  _data_or_null: *const c_void,
  _attr: PropertyAttribute,
  _getter_side_effect_type: SideEffectType,
  _setter_side_effect_type: SideEffectType,
) -> MaybeBool {
  MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__Name(_this: *const c_void) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__HasHandler(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Data(
  _this: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__IsRevoked(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__New(
  _context: *const c_void,
  _target: *const c_void,
  _handler: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__Revoke(_this: *const c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__Exec(
  _this: *const c_void,
  _context: *const c_void,
  _subject: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__GetSource(_this: *const c_void) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__New(
  _context: *const c_void,
  _pattern: *const c_void,
  _flags: RegExpCreationFlags,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Get(
  _this: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetEmptyString(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ResourceName(
  _origin: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ScriptId(_origin: *const c_void) -> i32 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__SourceMapUrl(
  _origin: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Clear(_this: *const c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Delete(
  _this: *const c_void,
  _context: *const c_void,
  _key: *const c_void,
) -> MaybeBool {
  MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Has(
  _this: *const c_void,
  _context: *const c_void,
  _key: *const c_void,
) -> MaybeBool {
  MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__NewBackingStore__with_data(
  _data: *mut c_void,
  _byte_length: usize,
  _deleter: *const c_void,
  _deleter_data: *mut c_void,
) -> *mut c_void {
  std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_byte_length(
  _isolate: *mut c_void,
  _byte_length: usize,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Signature__New(
  _isolate: *mut c_void,
  _templ: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_isolate(
  _this: *mut c_void,
  _data: *const c_void,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptId(_this: *const c_void) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptNameOrSourceURL(
  _this: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptSource(
  _this: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptSourceMappingURL(
  _this: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsConstructor(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsWasm(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__CurrentScriptNameOrSourceURL(
  _isolate: *mut c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Concat(
  _isolate: *mut c_void,
  _left: *const c_void,
  _right: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsExternalOneByte(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsExternalTwoByte(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalTwoByteStatic(
  _isolate: *mut c_void,
  _buffer: *const c_void,
  _length: int,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetIntrinsicDataProperty(
  _this: *const c_void,
  _key: *const c_void,
  _intrinsic: Intrinsic,
  _attr: PropertyAttribute,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__StackTrace(
  _this: *const c_void,
  _context: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__GetHash(_this: *const c_void) -> u32 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToDetailString(
  _this: *const c_void,
  _context: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInt32(
  _this: *const c_void,
  _context: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToUint32(
  _this: *const c_void,
  _context: *const c_void,
) -> *const c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Abort(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Finish(
  _this: *mut c_void,
  _isolate: *mut c_void,
  _caching_callback: *const c_void,
  _resolution_callback: *const c_void,
  _resolution_data: *mut c_void,
  _drop_resolution_data: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__OnBytesReceived(
  _this: *mut c_void,
  _bytes: *const c_void,
  _size: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__SetUrl(
  _this: *mut c_void,
  _url: *const c_void,
  _length: usize,
) {
}
