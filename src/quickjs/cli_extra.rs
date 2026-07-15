//! Extra symbols the full deno cli references beyond deno_core (QuickJS backend).
#![allow(non_snake_case)]
use std::os::raw::c_char;
use std::sync::{Mutex, OnceLock};

// Process-wide default locale (BCP47 language tag). QuickJS has no ICU default
// locale, so initialize it from the standard POSIX locale environment and
// remember any later `icu_set_default_locale` override.
fn locale_store() -> &'static Mutex<String> {
  static LOCALE: OnceLock<Mutex<String>> = OnceLock::new();
  LOCALE.get_or_init(|| Mutex::new(default_locale_from_environment()))
}

fn canonicalize_locale_id(locale: &str) -> Option<String> {
  let locale = locale.split(['.', '@']).next().unwrap_or(locale);
  if locale.eq_ignore_ascii_case("C") || locale.eq_ignore_ascii_case("POSIX") {
    return Some("en-US".to_string());
  }
  let locale = locale.replace('_', "-");
  icu_locale::Locale::try_from_str(&locale)
    .ok()
    .map(|locale| locale.to_string())
}

fn default_locale_from_environment() -> String {
  ["LC_ALL", "LC_MESSAGES", "LANG"]
    .into_iter()
    .filter_map(|name| std::env::var(name).ok())
    .find_map(|locale| {
      (!locale.is_empty())
        .then(|| canonicalize_locale_id(&locale))
        .flatten()
    })
    .unwrap_or_else(|| "en-US".to_string())
}

// Canonicalize a locale id to a BCP47 language tag the way ICU's
// `uloc_toLanguageTag` does, e.g. "nb_NO" -> "nb-NO".
pub(crate) fn set_default_locale_str(s: &str) {
  let Some(tag) = canonicalize_locale_id(s) else {
    return;
  };
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

#[cfg(test)]
mod tests {
  use super::canonicalize_locale_id;

  #[test]
  fn canonicalizes_icu_and_posix_locale_ids() {
    assert_eq!(canonicalize_locale_id("pl_PL"), Some("pl-PL".into()));
    assert_eq!(
      canonicalize_locale_id("sr_Latn_RS.UTF-8@calendar=gregorian"),
      Some("sr-Latn-RS".into())
    );
    assert_eq!(canonicalize_locale_id("C"), Some("en-US".into()));
    assert_eq!(canonicalize_locale_id("not a locale"), None);
  }
}
