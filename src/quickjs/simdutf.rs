#![allow(non_snake_case, unused)]
//! Family: simdutf
//! Pure-Rust implementations of the simdutf__* UTF/base64 FFI surface.
//! No JSC involved — these are on Deno's hot path and implemented for real.

#[repr(C)]
pub struct FfiResult {
  error: i32,
  count: usize,
}

const SUCCESS: i32 = 0;
const HEADER_BITS: i32 = 1;
const TOO_SHORT: i32 = 2;
const TOO_LONG: i32 = 3;
const OVERLONG: i32 = 4;
const TOO_LARGE: i32 = 5;
const SURROGATE: i32 = 6;
const INVALID_BASE64_CHARACTER: i32 = 7;
const BASE64_INPUT_REMAINDER: i32 = 8;
const BASE64_EXTRA_BITS: i32 = 9;
const OUTPUT_BUFFER_TOO_SMALL: i32 = 10;

#[inline]
fn ok(count: usize) -> FfiResult {
  FfiResult {
    error: SUCCESS,
    count,
  }
}
#[inline]
fn err(error: i32, count: usize) -> FfiResult {
  FfiResult { error, count }
}

#[inline]
unsafe fn slice_u8<'a>(buf: *const u8, len: usize) -> &'a [u8] {
  if buf.is_null() || len == 0 {
    &[]
  } else {
    unsafe { core::slice::from_raw_parts(buf, len) }
  }
}
#[inline]
unsafe fn slice_u16<'a>(buf: *const u16, len: usize) -> &'a [u16] {
  if buf.is_null() || len == 0 {
    &[]
  } else {
    unsafe { core::slice::from_raw_parts(buf, len) }
  }
}
#[inline]
unsafe fn slice_u32<'a>(buf: *const u32, len: usize) -> &'a [u32] {
  if buf.is_null() || len == 0 {
    &[]
  } else {
    unsafe { core::slice::from_raw_parts(buf, len) }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_utf8(buf: *const u8, len: usize) -> bool {
  let s = unsafe { slice_u8(buf, len) };
  core::str::from_utf8(s).is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_utf8_with_errors(
  buf: *const u8,
  len: usize,
) -> FfiResult {
  let s = unsafe { slice_u8(buf, len) };
  match core::str::from_utf8(s) {
    Ok(_) => ok(len),
    Err(e) => {
      let pos = e.valid_up_to();
      if e.error_len().is_none() {
        err(TOO_SHORT, pos)
      } else {
        err(HEADER_BITS, pos)
      }
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_ascii(buf: *const u8, len: usize) -> bool {
  let s = unsafe { slice_u8(buf, len) };
  s.is_ascii()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_ascii_with_errors(
  buf: *const u8,
  len: usize,
) -> FfiResult {
  let s = unsafe { slice_u8(buf, len) };
  match s.iter().position(|&b| b >= 0x80) {
    None => ok(len),
    Some(pos) => err(TOO_LARGE, pos),
  }
}

fn validate_utf16_units(
  units: &[u16],
  to_native: impl Fn(u16) -> u16,
) -> Result<(), usize> {
  let mut i = 0;
  while i < units.len() {
    let u = to_native(units[i]);
    if (0xD800..=0xDBFF).contains(&u) {
      if i + 1 >= units.len() {
        return Err(i);
      }
      let lo = to_native(units[i + 1]);
      if !(0xDC00..=0xDFFF).contains(&lo) {
        return Err(i);
      }
      i += 2;
    } else if (0xDC00..=0xDFFF).contains(&u) {
      return Err(i);
    } else {
      i += 1;
    }
  }
  Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_utf16le(
  buf: *const u16,
  len: usize,
) -> bool {
  let s = unsafe { slice_u16(buf, len) };
  validate_utf16_units(s, u16::from_le).is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_utf16le_with_errors(
  buf: *const u16,
  len: usize,
) -> FfiResult {
  let s = unsafe { slice_u16(buf, len) };
  match validate_utf16_units(s, u16::from_le) {
    Ok(()) => ok(len),
    Err(pos) => err(SURROGATE, pos),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_utf16be(
  buf: *const u16,
  len: usize,
) -> bool {
  let s = unsafe { slice_u16(buf, len) };
  validate_utf16_units(s, u16::from_be).is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_utf16be_with_errors(
  buf: *const u16,
  len: usize,
) -> FfiResult {
  let s = unsafe { slice_u16(buf, len) };
  match validate_utf16_units(s, u16::from_be) {
    Ok(()) => ok(len),
    Err(pos) => err(SURROGATE, pos),
  }
}

#[inline]
fn valid_scalar(c: u32) -> bool {
  c <= 0x10FFFF && !(0xD800..=0xDFFF).contains(&c)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_utf32(buf: *const u32, len: usize) -> bool {
  let s = unsafe { slice_u32(buf, len) };
  s.iter().all(|&c| valid_scalar(c))
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__validate_utf32_with_errors(
  buf: *const u32,
  len: usize,
) -> FfiResult {
  let s = unsafe { slice_u32(buf, len) };
  match s.iter().position(|&c| !valid_scalar(c)) {
    None => ok(len),
    Some(pos) => {
      let c = s[pos];
      if (0xD800..=0xDFFF).contains(&c) {
        err(SURROGATE, pos)
      } else {
        err(TOO_LARGE, pos)
      }
    }
  }
}

fn decode_utf16_to_scalars(
  units: &[u16],
  to_native: impl Fn(u16) -> u16,
) -> Option<Vec<u32>> {
  let mut out = Vec::with_capacity(units.len());
  let mut i = 0;
  while i < units.len() {
    let u = to_native(units[i]) as u32;
    if (0xD800..=0xDBFF).contains(&u) {
      if i + 1 >= units.len() {
        return None;
      }
      let lo = to_native(units[i + 1]) as u32;
      if !(0xDC00..=0xDFFF).contains(&lo) {
        return None;
      }
      let c = 0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00);
      out.push(c);
      i += 2;
    } else if (0xDC00..=0xDFFF).contains(&u) {
      return None;
    } else {
      out.push(u);
      i += 1;
    }
  }
  Some(out)
}

#[inline]
fn encode_utf8(c: u32, out: *mut u8, off: usize) -> usize {
  unsafe {
    if c < 0x80 {
      *out.add(off) = c as u8;
      1
    } else if c < 0x800 {
      *out.add(off) = 0xC0 | (c >> 6) as u8;
      *out.add(off + 1) = 0x80 | (c & 0x3F) as u8;
      2
    } else if c < 0x10000 {
      *out.add(off) = 0xE0 | (c >> 12) as u8;
      *out.add(off + 1) = 0x80 | ((c >> 6) & 0x3F) as u8;
      *out.add(off + 2) = 0x80 | (c & 0x3F) as u8;
      3
    } else {
      *out.add(off) = 0xF0 | (c >> 18) as u8;
      *out.add(off + 1) = 0x80 | ((c >> 12) & 0x3F) as u8;
      *out.add(off + 2) = 0x80 | ((c >> 6) & 0x3F) as u8;
      *out.add(off + 3) = 0x80 | (c & 0x3F) as u8;
      4
    }
  }
}

#[inline]
fn utf8_len_of_scalar(c: u32) -> usize {
  if c < 0x80 {
    1
  } else if c < 0x800 {
    2
  } else if c < 0x10000 {
    3
  } else {
    4
  }
}

fn utf8_to_utf16(
  input: &[u8],
  output: *mut u16,
  to_unit: impl Fn(u16) -> u16,
) -> Option<usize> {
  let s = core::str::from_utf8(input).ok()?;
  let mut n = 0usize;
  unsafe {
    let mut buf = [0u16; 2];
    for ch in s.chars() {
      let encoded = ch.encode_utf16(&mut buf);
      for &u in encoded.iter() {
        *output.add(n) = to_unit(u);
        n += 1;
      }
    }
  }
  Some(n)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf8_to_utf16le(
  input: *const u8,
  length: usize,
  output: *mut u16,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  utf8_to_utf16(s, output, u16::to_le).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf8_to_utf16le_with_errors(
  input: *const u8,
  length: usize,
  output: *mut u16,
) -> FfiResult {
  let s = unsafe { slice_u8(input, length) };
  match core::str::from_utf8(s) {
    Ok(_) => {
      let n = utf8_to_utf16(s, output, u16::to_le).unwrap_or(0);
      ok(n)
    }
    Err(e) => {
      let pos = e.valid_up_to();
      if e.error_len().is_none() {
        err(TOO_SHORT, pos)
      } else {
        err(HEADER_BITS, pos)
      }
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_valid_utf8_to_utf16le(
  input: *const u8,
  length: usize,
  output: *mut u16,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  utf8_to_utf16(s, output, u16::to_le).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf8_to_utf16be(
  input: *const u8,
  length: usize,
  output: *mut u16,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  utf8_to_utf16(s, output, u16::to_be).unwrap_or(0)
}

fn utf16_to_utf8(
  input: &[u16],
  output: *mut u8,
  to_native: impl Fn(u16) -> u16,
) -> Option<usize> {
  let scalars = decode_utf16_to_scalars(input, to_native)?;
  let mut n = 0usize;
  for c in scalars {
    n += encode_utf8(c, output, n);
  }
  Some(n)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf16le_to_utf8(
  input: *const u16,
  length: usize,
  output: *mut u8,
) -> usize {
  let s = unsafe { slice_u16(input, length) };
  utf16_to_utf8(s, output, u16::from_le).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf16le_to_utf8_with_errors(
  input: *const u16,
  length: usize,
  output: *mut u8,
) -> FfiResult {
  let s = unsafe { slice_u16(input, length) };
  match validate_utf16_units(s, u16::from_le) {
    Ok(()) => ok(utf16_to_utf8(s, output, u16::from_le).unwrap_or(0)),
    Err(pos) => err(SURROGATE, pos),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_valid_utf16le_to_utf8(
  input: *const u16,
  length: usize,
  output: *mut u8,
) -> usize {
  let s = unsafe { slice_u16(input, length) };
  utf16_to_utf8(s, output, u16::from_le).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf16be_to_utf8(
  input: *const u16,
  length: usize,
  output: *mut u8,
) -> usize {
  let s = unsafe { slice_u16(input, length) };
  utf16_to_utf8(s, output, u16::from_be).unwrap_or(0)
}

fn utf8_to_latin1(
  input: &[u8],
  output: *mut u8,
) -> Result<usize, (i32, usize)> {
  let s = match core::str::from_utf8(input) {
    Ok(s) => s,
    Err(e) => {
      let pos = e.valid_up_to();
      let code = if e.error_len().is_none() {
        TOO_SHORT
      } else {
        HEADER_BITS
      };
      return Err((code, pos));
    }
  };
  let mut n = 0usize;

  for (byte_pos, ch) in s.char_indices() {
    let cp = ch as u32;
    if cp > 0xFF {
      return Err((TOO_LARGE, byte_pos));
    }
    unsafe { *output.add(n) = cp as u8 };
    n += 1;
  }
  Ok(n)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf8_to_latin1(
  input: *const u8,
  length: usize,
  output: *mut u8,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  utf8_to_latin1(s, output).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf8_to_latin1_with_errors(
  input: *const u8,
  length: usize,
  output: *mut u8,
) -> FfiResult {
  let s = unsafe { slice_u8(input, length) };
  match utf8_to_latin1(s, output) {
    Ok(n) => ok(n),
    Err((code, pos)) => err(code, pos),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_valid_utf8_to_latin1(
  input: *const u8,
  length: usize,
  output: *mut u8,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  utf8_to_latin1(s, output).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_latin1_to_utf8(
  input: *const u8,
  length: usize,
  output: *mut u8,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  let mut n = 0usize;
  unsafe {
    for &b in s {
      n += encode_utf8(b as u32, output, n);
    }
  }
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_latin1_to_utf16le(
  input: *const u8,
  length: usize,
  output: *mut u16,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  unsafe {
    for (i, &b) in s.iter().enumerate() {
      *output.add(i) = (b as u16).to_le();
    }
  }
  s.len()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf16le_to_latin1(
  input: *const u16,
  length: usize,
  output: *mut u8,
) -> usize {
  let s = unsafe { slice_u16(input, length) };
  unsafe {
    for (i, &u) in s.iter().enumerate() {
      let v = u16::from_le(u);
      if v > 0xFF {
        return 0;
      }
      *output.add(i) = v as u8;
    }
  }
  s.len()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf8_to_utf32(
  input: *const u8,
  length: usize,
  output: *mut u32,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  let st = match core::str::from_utf8(s) {
    Ok(st) => st,
    Err(_) => return 0,
  };
  let mut n = 0usize;
  unsafe {
    for ch in st.chars() {
      *output.add(n) = ch as u32;
      n += 1;
    }
  }
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__convert_utf32_to_utf8(
  input: *const u32,
  length: usize,
  output: *mut u8,
) -> usize {
  let s = unsafe { slice_u32(input, length) };

  if !s.iter().all(|&c| valid_scalar(c)) {
    return 0;
  }
  let mut n = 0usize;
  for &c in s {
    n += encode_utf8(c, output, n);
  }
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__utf8_length_from_utf16le(
  input: *const u16,
  length: usize,
) -> usize {
  let s = unsafe { slice_u16(input, length) };
  utf8_len_from_utf16(s, u16::from_le)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__utf8_length_from_utf16be(
  input: *const u16,
  length: usize,
) -> usize {
  let s = unsafe { slice_u16(input, length) };
  utf8_len_from_utf16(s, u16::from_be)
}

fn utf8_len_from_utf16(units: &[u16], to_native: impl Fn(u16) -> u16) -> usize {
  let mut total = 0usize;
  let mut i = 0;
  while i < units.len() {
    let u = to_native(units[i]) as u32;
    if (0xD800..=0xDBFF).contains(&u) && i + 1 < units.len() {
      let lo = to_native(units[i + 1]) as u32;
      if (0xDC00..=0xDFFF).contains(&lo) {
        total += 4;
        i += 2;
        continue;
      }
    }

    total += if u < 0x80 {
      1
    } else if u < 0x800 {
      2
    } else {
      3
    };
    i += 1;
  }
  total
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__utf16_length_from_utf8(
  input: *const u8,
  length: usize,
) -> usize {
  let s = unsafe { slice_u8(input, length) };

  let mut count = 0usize;
  for &b in s {
    if (b & 0xC0) != 0x80 {
      count += 1;
    }
    if b >= 0xF0 {
      count += 1;
    }
  }
  count
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__utf8_length_from_latin1(
  input: *const u8,
  length: usize,
) -> usize {
  let s = unsafe { slice_u8(input, length) };

  length + s.iter().filter(|&&b| b >= 0x80).count()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__latin1_length_from_utf8(
  input: *const u8,
  length: usize,
) -> usize {
  let s = unsafe { slice_u8(input, length) };

  s.iter().filter(|&&b| (b & 0xC0) != 0x80).count()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__utf32_length_from_utf8(
  input: *const u8,
  length: usize,
) -> usize {
  let s = unsafe { slice_u8(input, length) };

  s.iter().filter(|&&b| (b & 0xC0) != 0x80).count()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__utf8_length_from_utf32(
  input: *const u32,
  length: usize,
) -> usize {
  let s = unsafe { slice_u32(input, length) };
  s.iter().map(|&c| utf8_len_of_scalar(c)).sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__utf16_length_from_utf32(
  input: *const u32,
  length: usize,
) -> usize {
  let s = unsafe { slice_u32(input, length) };
  s.iter().map(|&c| if c >= 0x10000 { 2 } else { 1 }).sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__utf32_length_from_utf16le(
  input: *const u16,
  length: usize,
) -> usize {
  let s = unsafe { slice_u16(input, length) };

  let mut count = 0usize;
  let mut i = 0;
  while i < s.len() {
    let u = u16::from_le(s[i]) as u32;
    if (0xD800..=0xDBFF).contains(&u) && i + 1 < s.len() {
      let lo = u16::from_le(s[i + 1]) as u32;
      if (0xDC00..=0xDFFF).contains(&lo) {
        count += 1;
        i += 2;
        continue;
      }
    }
    count += 1;
    i += 1;
  }
  count
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__count_utf8(
  input: *const u8,
  length: usize,
) -> usize {
  let s = unsafe { slice_u8(input, length) };

  s.iter().filter(|&&b| (b & 0xC0) != 0x80).count()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__count_utf16le(
  input: *const u16,
  length: usize,
) -> usize {
  let s = unsafe { slice_u16(input, length) };

  s.iter()
    .filter(|&&u| !(0xDC00..=0xDFFF).contains(&(u16::from_le(u) as u32)))
    .count()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__count_utf16be(
  input: *const u16,
  length: usize,
) -> usize {
  let s = unsafe { slice_u16(input, length) };
  s.iter()
    .filter(|&&u| !(0xDC00..=0xDFFF).contains(&(u16::from_be(u) as u32)))
    .count()
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__detect_encodings(
  input: *const u8,
  length: usize,
) -> i32 {
  let s = unsafe { slice_u8(input, length) };
  let mut mask = 0i32;

  if length >= 2 && s[0] == 0xFF && s[1] == 0xFE {
    if length >= 4 && s[2] == 0x00 && s[3] == 0x00 {
      return 8;
    }
    return 2;
  }
  if length >= 2 && s[0] == 0xFE && s[1] == 0xFF {
    return 4;
  }
  if length >= 4 && s[0] == 0x00 && s[1] == 0x00 && s[2] == 0xFE && s[3] == 0xFF
  {
    return 16;
  }

  if core::str::from_utf8(s).is_ok() {
    mask |= 1;
  }
  if length % 2 == 0 {
    let u16s =
      unsafe { core::slice::from_raw_parts(input as *const u16, length / 2) };
    if validate_utf16_units(u16s, u16::from_le).is_ok() {
      mask |= 2;
    }
    if validate_utf16_units(u16s, u16::from_be).is_ok() {
      mask |= 4;
    }
  }
  if length % 4 == 0 {
    let u32s =
      unsafe { core::slice::from_raw_parts(input as *const u32, length / 4) };
    if u32s.iter().all(|&c| valid_scalar(u32::from_le(c))) {
      mask |= 8;
    }
    if u32s.iter().all(|&c| valid_scalar(u32::from_be(c))) {
      mask |= 16;
    }
  }
  mask
}

#[inline]
fn is_url_alphabet(options: u64) -> bool {
  options == 1 || options == 3
}
#[inline]
fn wants_padding(options: u64) -> bool {
  options == 0 || options == 3
}

const STD_ALPHABET: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_ALPHABET: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

#[inline]
fn b64_decode_value(c: u8, url: bool) -> Option<u8> {
  match c {
    b'A'..=b'Z' => Some(c - b'A'),
    b'a'..=b'z' => Some(c - b'a' + 26),
    b'0'..=b'9' => Some(c - b'0' + 52),
    b'+' => Some(62),
    b'/' => Some(63),
    b'-' if url => Some(62),
    b'_' if url => Some(63),

    b'-' => Some(62),
    b'_' => Some(63),
    _ => None,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__maximal_binary_length_from_base64(
  input: *const u8,
  length: usize,
) -> usize {
  let s = unsafe { slice_u8(input, length) };

  let sig = s
    .iter()
    .filter(|&&c| !c.is_ascii_whitespace() && c != b'=')
    .count();

  (sig / 4) * 3 + ((sig % 4) * 3 + 3) / 4
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__base64_to_binary(
  input: *const u8,
  length: usize,
  output: *mut u8,
  options: u64,
  last_chunk_options: u64,
) -> FfiResult {
  let s = unsafe { slice_u8(input, length) };
  let url = is_url_alphabet(options);

  let mut sextets: Vec<u8> = Vec::with_capacity(s.len());
  let mut padding_seen = false;
  let mut nonws = 0usize; // non-whitespace chars, including '=' padding
  for (idx, &c) in s.iter().enumerate() {
    if c.is_ascii_whitespace() {
      continue;
    }
    nonws += 1;
    if c == b'=' {
      padding_seen = true;
      continue;
    }
    if padding_seen {
      return err(INVALID_BASE64_CHARACTER, idx);
    }
    match b64_decode_value(c, url) {
      Some(v) => sextets.push(v),
      None => return err(INVALID_BASE64_CHARACTER, idx),
    }
  }

  // WHATWG forgiving-base64 padding validation (simdutf strips '=' up front, so
  // enforce it here else `atob` accepts what v8 rejects): the whitespace-
  // stripped length (counting '=') ≡1 mod 4 is invalid; and '=' padding is only
  // allowed when that length ≡0 mod 4, at most two of them.
  let pad_count = nonws - sextets.len();
  if nonws % 4 == 1 {
    return err(BASE64_INPUT_REMAINDER, nonws);
  }
  if pad_count > 0 && (nonws % 4 != 0 || pad_count > 2) {
    return err(INVALID_BASE64_CHARACTER, nonws.saturating_sub(1));
  }

  let rem = sextets.len() % 4;

  if rem == 1 {
    return err(BASE64_INPUT_REMAINDER, sextets.len());
  }
  if last_chunk_options == 1 && rem != 0 {
    return err(BASE64_INPUT_REMAINDER, sextets.len());
  }

  let full_groups = sextets.len() / 4;
  let mut written = 0usize;

  unsafe {
    for g in 0..full_groups {
      let i = g * 4;
      let n = ((sextets[i] as u32) << 18)
        | ((sextets[i + 1] as u32) << 12)
        | ((sextets[i + 2] as u32) << 6)
        | (sextets[i + 3] as u32);
      *output.add(written) = (n >> 16) as u8;
      *output.add(written + 1) = (n >> 8) as u8;
      *output.add(written + 2) = n as u8;
      written += 3;
    }

    let handle_partial =
      rem != 0 && last_chunk_options != 2 && last_chunk_options != 3;
    if handle_partial {
      let i = full_groups * 4;
      if rem == 2 {
        let n = ((sextets[i] as u32) << 18) | ((sextets[i + 1] as u32) << 12);
        if last_chunk_options == 1 && (sextets[i + 1] & 0x0F) != 0 {
          return err(BASE64_EXTRA_BITS, written);
        }
        *output.add(written) = (n >> 16) as u8;
        written += 1;
      } else if rem == 3 {
        let n = ((sextets[i] as u32) << 18)
          | ((sextets[i + 1] as u32) << 12)
          | ((sextets[i + 2] as u32) << 6);
        if last_chunk_options == 1 && (sextets[i + 2] & 0x03) != 0 {
          return err(BASE64_EXTRA_BITS, written);
        }
        *output.add(written) = (n >> 16) as u8;
        *output.add(written + 1) = (n >> 8) as u8;
        written += 2;
      }
    }
  }

  ok(written)
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__base64_length_from_binary(
  length: usize,
  options: u64,
) -> usize {
  if wants_padding(options) {
    ((length + 2) / 3) * 4
  } else {
    (length * 4 + 2) / 3
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn simdutf__binary_to_base64(
  input: *const u8,
  length: usize,
  output: *mut u8,
  options: u64,
) -> usize {
  let s = unsafe { slice_u8(input, length) };
  let alphabet: &[u8; 64] = if is_url_alphabet(options) {
    URL_ALPHABET
  } else {
    STD_ALPHABET
  };
  let pad = wants_padding(options);
  let mut w = 0usize;

  unsafe {
    let chunks = s.chunks_exact(3);
    let rem = chunks.remainder();
    for c in chunks {
      let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
      *output.add(w) = alphabet[((n >> 18) & 0x3F) as usize];
      *output.add(w + 1) = alphabet[((n >> 12) & 0x3F) as usize];
      *output.add(w + 2) = alphabet[((n >> 6) & 0x3F) as usize];
      *output.add(w + 3) = alphabet[(n & 0x3F) as usize];
      w += 4;
    }
    match rem.len() {
      1 => {
        let n = (rem[0] as u32) << 16;
        *output.add(w) = alphabet[((n >> 18) & 0x3F) as usize];
        *output.add(w + 1) = alphabet[((n >> 12) & 0x3F) as usize];
        w += 2;
        if pad {
          *output.add(w) = b'=';
          *output.add(w + 1) = b'=';
          w += 2;
        }
      }
      2 => {
        let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
        *output.add(w) = alphabet[((n >> 18) & 0x3F) as usize];
        *output.add(w + 1) = alphabet[((n >> 12) & 0x3F) as usize];
        *output.add(w + 2) = alphabet[((n >> 6) & 0x3F) as usize];
        w += 3;
        if pad {
          *output.add(w) = b'=';
          w += 1;
        }
      }
      _ => {}
    }
  }
  w
}
