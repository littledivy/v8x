//! AUTO-GENERATED Hermes link stubs (tools/gen_hermes_shims.sh).
//!
//! Cycle 0/1 scaffold: every v8__*/v8_inspector__* symbol the vendored
//! rusty_v8 surface declares, stubbed as an unimplemented!() no-arg
//! function so the crate links with zero Hermes dependency. Real
//! implementations land incrementally in later cycles, same pattern as
//! the QuickJS backend (see tools/gen_qjs_shims.sh).
#![allow(non_snake_case)]

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__use_count() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__use_count() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__AllowJavascriptExecutionScope__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__AllowJavascriptExecutionScope__DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New_with_elements() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewRustAllocator() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Detach() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsDetachable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__SetDetachKey() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__WasDetached() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
// v8__ArrayBufferView__{Buffer,Buffer__Data,ByteLength,ByteOffset,CopyContents,
// HasBuffer} are implemented in core.rs (real impl, E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__GetContents() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Int64Value() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromUnsigned() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromWords() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__ToWordsArray() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Uint64Value() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__WordCount() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__BUILD_NUMBER() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CFunction() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CFunctionInfo() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__GetWireBytesRef() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__SourceUrl() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__AllowCodeGenerationFromStrings() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__FromSnapshot() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetContinuationPreservedEmbedderData() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetDataFromSnapshotOnce() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetEmbedderData() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetSecurityToken() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetContinuationPreservedEmbedderData() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetEmbedderData() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetPromiseHooks() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetSecurityToken() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__UseDefaultSecurityToken() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context_IsCodeGenerationFromStringsAllowed() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Create() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Terminate() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CpuProfiler__CollectSample() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__CpuProfiler__UseDetailedSourcePositionsForProfiling() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsBigInt() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsBoolean() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsContext() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsFixedArray() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModule() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModuleRequest() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsName() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsNumber() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrimitive() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrivate() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsString() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsSymbol() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__DataView__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ValueOf() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Clear() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Get() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__IsEmpty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Set() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal_SIZE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__CaptureStackTrace() -> *const ::std::ffi::c_void { ::std::ptr::null() }
// v8__Exception__CreateMessage is implemented in core.rs (real impl).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__GetStackTrace() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ExternalOneByteStringResource__data() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ExternalOneByteStringResource__length() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__FastOneByteString() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__CreateCodeCache() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptColumnNumber() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptLineNumber() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptOrigin() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__NewInstance() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__ScriptId() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__Inherit() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__ReadOnlyPrototype() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__RemovePrototype() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetAccessorProperty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__GCCallbackFlags() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__GCType() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapCodeStatistics() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapProfiler__TakeHeapSnapshot() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapSpaceStatistics() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapStatistics() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__Run() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Int32__Value() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Intercepted() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCEpilogueCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCPrologueCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddMessageListener() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddMessageListenerWithErrorLevel() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddNearHeapLimitCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AdjustAmountOfExternalAllocatedMemory() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CancelTerminateExecution() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__ClearKeptObjects() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__DateTimeConfigurationChangeNotification() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCppHeap() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentHostDefinedOptions() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetDataFromSnapshotOnce() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetEnteredOrMicrotaskContext() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapCodeAndMetadataStatistics() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapSpaceStatistics() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapStatistics() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetMicrotasksPolicy() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__HasPendingBackgroundTasks() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__IsExecutionTerminating() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__LowMemoryNotification() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__MemoryPressureNotification() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__NumberOfHeapSpaces() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveGCEpilogueCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveGCPrologueCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveNearHeapLimitCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestGarbageCollectionForTesting() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestInterrupt() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetAllowAtomicsWait() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetAllowWasmCodeGenerationCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetCaptureStackTraceForUncaughtExceptions() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostCreateShadowRealmContextCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleDynamicallyCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleWithPhaseDynamicallyCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostInitializeImportMetaObjectCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetIdle() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetMicrotasksPolicy() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetOOMErrorHandler() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPrepareStackTraceCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPromiseHook() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPromiseRejectCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetUseCounterCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetWasmAsyncResolvePromiseCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetWasmStreamingCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__TerminateExecution() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__UseCounterFeature() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Parse() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Stringify() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__MAJOR_VERSION() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__As__Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Clear() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Delete() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Get() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Has() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Set() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Size() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__ErrorLevel() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetEndColumn() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetEndPosition() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetLineNumber() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetScriptResourceName() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetSourceLine() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStackTrace() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStartColumn() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStartPosition() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetWasmFunctionIndex() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__IsOpaque() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__IsSharedCrossOrigin() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__MINOR_VERSION() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleCachingInterface__GetWireBytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleCachingInterface__SetCachedCompiledModuleBytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleImportPhase() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Name__GetIdentityHash() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__CreateDataProperty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DefineProperty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Delete() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DeleteIndex() -> *const ::std::ffi::c_void { ::std::ptr::null() }
// v8__Object__DeletePrivate is implemented in core.rs (real impl, E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetConstructorName() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetCreationContext() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetOwnPropertyDescriptor() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetOwnPropertyNames() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
// v8__Object__GetPrivate is implemented in core.rs (real impl, E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPropertyAttributes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPropertyNames() -> *const ::std::ffi::c_void { ::std::ptr::null() }
// v8__Object__GetPrototype is implemented in core.rs (real impl).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetRealNamedProperty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetRealNamedPropertyAttributes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetWithReceiver() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasIndex() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasOwnProperty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
// v8__Object__HasPrivate is implemented in core.rs (real impl, E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasRealNamedProperty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__IsApiWrapper() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__PreviewEntries() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetIntegrityLevel() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetLazyDataProperty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
// v8__Object__SetPrivate is implemented in core.rs (real impl, E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetPrototype() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetWithReceiver() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Unwrap() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Wrap() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetIndexedPropertyHandler() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNamedPropertyHandler() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PATCH_LEVEL() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__PumpMessageLoop() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__RunIdleTasks() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Get() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Length() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Set() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
// v8__Private__ForApi / v8__Private__Name / v8__Private__New are implemented in
// core.rs (real impl, E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetEvent() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetPromise() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetValue() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__configurable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__CONSTRUCT__Get_Set() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__CONSTRUCT__Value() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__CONSTRUCT__Value_Writable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__enumerable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__get() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_configurable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_enumerable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_get() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_set() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_value() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_writable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__set() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__set_configurable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__set_enumerable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__value() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__writable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetHandler() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetTarget() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__IsRevoked() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__Revoke() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__Exec() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__GetSource() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__code_range_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__ConfigureDefaults() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__ConfigureDefaultsFromHeapSize() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__initial_old_generation_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__initial_young_generation_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__max_old_generation_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__max_young_generation_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_code_range_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_initial_old_generation_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_initial_young_generation_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_max_old_generation_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_max_young_generation_size_in_bytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_stack_limit() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__stack_limit() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__GetUnboundScript() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__NEW() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedDataVersionTag() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Compile() -> *const ::std::ffi::c_void { ::std::ptr::null() }
// v8__ScriptCompiler__CompileFunction is implemented in modules.rs (real impl).
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileUnboundScript() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin_SIZE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrModule__GetResourceName() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrModule__HostDefinedOptions() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Add() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__As__Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Clear() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Delete() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Has() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Size() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__ByteLength() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__GetBackingStore() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_backing_store() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_byte_length() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__NewBackingStore__with_byte_length() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__NewBackingStore__with_data() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddContext() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_context() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_isolate() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CreateBlob() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__GetIsolate() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__SetDefaultContext() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetColumn() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetFunctionName() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetLineNumber() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptId() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptName() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptNameOrSourceURL() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptSource() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptSourceMappingURL() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsConstructor() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsEval() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsUserJavaScript() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsWasm() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__CurrentScriptNameOrSourceURL() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__CurrentStackTrace() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__GetFrame() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__GetFrameCount() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__CanBeRehashed() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__data__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__IsValid() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Concat() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ContainsOnlyOneByte() -> *const ::std::ffi::c_void { ::std::ptr::null() }
// v8__String__Empty is implemented in core.rs (real impl).
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__GetExternalStringResource() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__GetExternalStringResourceBase() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsExternal() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsExternalOneByte() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsExternalTwoByte() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsOneByte() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__kMaxLength() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByte() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalTwoByte() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalTwoByteStatic() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromTwoByte() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView_SIZE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Write_v2() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__WriteOneByte_v2() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__Description() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__For() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__ForApi() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetAsyncIterator() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetHasInstance() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetIsConcatSpreadable() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetIterator() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetMatch() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetReplace() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetSearch() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetSplit() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetToPrimitive() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetToStringTag() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__GetUnscopables() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__New() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__Run() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetIntrinsicDataProperty() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Get() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Reset() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference_SIZE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__TYPED_ARRAY_MAX_SIZE_IN_HEAP() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__TypedArray__kMaxByteLength() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Uint32__Value() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__CreateCodeCache() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__BindToCurrentContext() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__CreateCodeCache() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__GetSourceMappingURL() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__GetSourceURL() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__IsSandboxEnabled() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetEntropySource() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFatalErrorHandler() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromCommandLine() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__GetHash() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__InstanceOf() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArgumentsObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
// v8__Value__IsArrayBuffer / IsArrayBufferView implemented in core.rs (E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsAsyncFunction() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt64Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigIntObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigUint64Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBooleanObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsDataView() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsDate() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat16Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat32Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat64Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsGeneratorFunction() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsGeneratorObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt16Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt8Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsMap() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsMapIterator() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsModuleNamespaceObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsName() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNativeError() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNullOrUndefined() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumberObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsProxy() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsRegExp() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSet() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSetGeneratorObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSetIterator() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSharedArrayBuffer() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsStringObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSymbol() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSymbolObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
// v8__Value__IsTypedArray implemented in core.rs (E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint16Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32Array() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
// v8__Value__IsUint8Array implemented in core.rs (E4).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8ClampedArray() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmMemoryObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmModuleObject() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWeakMap() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWeakSet() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBigInt() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBoolean() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToDetailString() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInt32() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToNumber() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToUint32() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__TypeOf() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__Delegate__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__GetWireFormatVersion() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadDouble() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadHeader() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadRawBytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadUint32() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadUint64() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadValue() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__SetSupportsLegacyWireFormat() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferArrayBuffer() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferSharedArrayBuffer() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__Delegate__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__Release() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__SetTreatArrayBufferViewsAsHostObjects() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__TransferArrayBuffer() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteDouble() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteHeader() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteRawBytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint32() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint64() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteValue() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__VERSION_STRING() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmMemoryObject__Buffer() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Abort() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Finish() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__NEW() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__OnBytesReceived() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__SetHasCompiledModuleBytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__SetMoreFunctionsCanBeSerializedCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__SetUrl() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__Compile() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__FromCompiledModule() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__GetCompiledModule() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Abort() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Finish() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__OnBytesReceived() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__SetHasCompiledModuleBytes() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__SetUrl() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__shared_ptr_DESTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Unpack() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetIsolate() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetParameter() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__SetSecondPassCallback() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__create() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__string() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__allAsyncTasksCanceled() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskCanceled() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskFinished() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskScheduled() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskStarted() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__BASE__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__flushProtocolNotifications() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__sendNotification() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__sendResponse() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__connect() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextCreated() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextDestroyed() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__create() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__createStackTrace() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__exceptionThrown() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__idleFinished() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__idleStarted() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__BASE__CONSTRUCT() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__consoleAPIMessage() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__generateUniqueId() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__quitMessageLoopOnPause() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__runIfWaitingForDebugger() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__runMessageLoopOnPause() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__cancelPauseOnNextStatement() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__canDispatchMethod() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__dispatchProtocolMessage() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__schedulePauseOnNextStatement() -> *const ::std::ffi::c_void { ::std::ptr::null() }
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8StackTrace__DELETE() -> *const ::std::ffi::c_void { ::std::ptr::null() }
