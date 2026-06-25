//! JavaScriptCore engine backend: the v8__* C-ABI implemented over JSC.
#![allow(non_snake_case)]

pub(crate) mod jsc_sys;
pub(crate) mod shim_arraybuffer;
pub(crate) mod shim_cli_extra;
pub(crate) mod shim_core;
pub(crate) mod shim_exception;
pub(crate) mod shim_function;
pub(crate) mod shim_impl;
pub(crate) mod shim_inspector;
pub(crate) mod shim_isolate;
pub(crate) mod shim_misc;
pub(crate) mod shim_module;
pub(crate) mod shim_object;
pub(crate) mod shim_primitive;
pub(crate) mod shim_property;
pub(crate) mod shim_serializer;
pub(crate) mod shim_simdutf;
pub(crate) mod shim_string;
pub(crate) mod shim_value;
pub(crate) mod shims;
