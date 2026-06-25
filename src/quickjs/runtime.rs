//! Runtime / platform-level v8 C-ABI shims for the QuickJS backend.
//!
//! These cover V8/platform bring-up entry points that have no QuickJS analogue
//! and are safe to treat as inert (init/dispose/diagnostic registration).
//! Real, behaviour-bearing impls live in the per-domain modules; this file is for
//! the no-op-but-must-exist surface that Deno calls during boot.

#![allow(non_snake_case)]

use std::os::raw::c_void;

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFatalErrorHandler(_that: *const c_void) {}
