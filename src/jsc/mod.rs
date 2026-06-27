//! JavaScriptCore engine backend: the v8__* C-ABI implemented over JSC.
#![allow(non_snake_case)]

pub(crate) mod arraybuffer;
pub(crate) mod cli_extra;
pub(crate) mod core;
pub(crate) mod exception;
pub(crate) mod function;
pub(crate) mod init;
pub(crate) mod inspector;
pub(crate) mod introspect;
pub(crate) mod isolate;
pub(crate) mod jsc_sys;
pub(crate) mod misc;
pub(crate) mod module;
pub(crate) mod object;
pub(crate) mod primitive;
pub(crate) mod property;
pub(crate) mod serializer;
pub(crate) mod shims;
pub(crate) mod simdutf;
pub(crate) mod string;
// Safe link-stubs for v8 C-ABI symbols `test_api.rs` references but JSC doesn't
// implement yet, so the large test targets link and run. See the module docs.
pub(crate) mod test_api_stubs;
pub(crate) mod value;
