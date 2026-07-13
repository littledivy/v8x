cmake_minimum_required(VERSION 3.14)
project(wamrvm C)

# Interpreter-only WAMR static lib for the v82jsc QuickJS backend's WebAssembly
# implementation. No AOT/JIT (avoids the LLVM dependency); fast interpreter on.
set(WAMR_ROOT_DIR ${CMAKE_CURRENT_LIST_DIR}/..)

if (NOT DEFINED WAMR_BUILD_PLATFORM)
  string(TOLOWER ${CMAKE_HOST_SYSTEM_NAME} WAMR_BUILD_PLATFORM)
endif ()

if (NOT DEFINED WAMR_BUILD_TARGET)
  if (CMAKE_SYSTEM_PROCESSOR MATCHES "^(arm64|aarch64)")
    set(WAMR_BUILD_TARGET "AARCH64")
  elseif (CMAKE_SIZEOF_VOID_P EQUAL 8)
    set(WAMR_BUILD_TARGET "X86_64")
  else ()
    set(WAMR_BUILD_TARGET "X86_32")
  endif ()
endif ()

set(WAMR_BUILD_INTERP 1)
set(WAMR_BUILD_FAST_INTERP 1)
set(WAMR_BUILD_AOT 0)
set(WAMR_BUILD_JIT 0)
set(WAMR_BUILD_LIBC_BUILTIN 1)
set(WAMR_BUILD_LIBC_WASI 0)
# MULTI_MODULE makes WAMR's loader try to resolve a wasm import's MODULE name
# (e.g. "./rs_lib.internal.js") as a nested wasm sub-module via a sub-module
# reader. Our embedding provides every import from JS (the WebAssembly.instantiate
# importObject), so submodule auto-loading must be OFF or load fails with
# "no sub module reader to load ./rs_lib.internal.js".
set(WAMR_BUILD_MULTI_MODULE 0)
set(WAMR_BUILD_REF_TYPES 1)
set(WAMR_BUILD_SIMD 1)
set(WAMR_BUILD_BULK_MEMORY 1)
set(WAMR_BUILD_DUMP_CALL_STACK 1)
# Hardware bound-check uses SIGSEGV/SIGBUS handlers + mmap tricks that conflict
# with deno/tokio's signal handling and macOS hardening (instant SIGKILL on
# `deno run`). Use software bounds checks instead.
set(WAMR_DISABLE_HW_BOUND_CHECK 1)
set(WAMR_DISABLE_STACK_HW_BOUND_CHECK 1)
set(WAMR_DISABLE_WRITE_GS_BASE 1)

include(${WAMR_ROOT_DIR}/build-scripts/runtime_lib.cmake)

add_library(vmlib STATIC ${WAMR_RUNTIME_LIB_SOURCE})
# WASM_API_EXTERN= so the wasm-c-api symbols are plain (no dllexport).
target_compile_definitions(vmlib PRIVATE WASM_API_EXTERN=)
