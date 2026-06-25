//! Remaining QuickJS link stubs (tools/gen_qjs_shims.sh).
//!
//! Everything previously listed here has been implemented in the `fam_*`
//! modules. The two entries below are auto-generated placeholders for symbols
//! that have no real C-ABI declaration in the vendored crate (they are *consts*
//! / unused leftovers, not functions any caller invokes): keeping inert stubs
//! is harmless and avoids spurious missing-symbol errors. If a future caller
//! does reference them, they need real implementations.
#![allow(non_snake_case)]

#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapStatistics() {
  unimplemented!("v8__HeapStatistics")
}
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView_SIZE() {
  unimplemented!("v8__String__ValueView_SIZE")
}
