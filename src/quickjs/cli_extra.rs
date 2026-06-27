//! Extra symbols the full deno cli references beyond deno_core (QuickJS backend).
#![allow(non_snake_case)]
use std::os::raw::c_char;
use std::sync::{Mutex, OnceLock};

// Process-wide default locale (BCP47 language tag). QuickJS has no ICU, so we
// just remember whatever `icu_set_default_locale` was last handed and report it
// back from `icu_get_default_locale`. Defaults to "en-US".
fn locale_store() -> &'static Mutex<String> {
  static LOCALE: OnceLock<Mutex<String>> = OnceLock::new();
  LOCALE.get_or_init(|| Mutex::new("en-US".to_string()))
}

// Canonicalize a locale id to a BCP47-ish language tag the way ICU's
// `uloc_toLanguageTag` does for simple ids, e.g. "nb_NO" -> "nb-NO".
pub(crate) fn set_default_locale_str(s: &str) {
  let tag = s.replace('_', "-");
  if let Ok(mut g) = locale_store().lock() {
    *g = tag;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn icu_get_default_locale(
  output: *mut c_char,
  output_len: usize,
) -> usize {
  let guard = locale_store().lock();
  let loc: Vec<u8> = match guard {
    Ok(g) => g.as_bytes().to_vec(),
    Err(_) => b"en-US".to_vec(),
  };
  if output.is_null() || output_len == 0 {
    return loc.len();
  }
  let n = loc.len().min(output_len.saturating_sub(1));
  unsafe {
    std::ptr::copy_nonoverlapping(loc.as_ptr() as *const c_char, output, n);
    *output.add(n) = 0;
  }
  n
}
