#![cfg(feature = "engine_quickjs")]

const MB: usize = 1024 * 1024;
const GB: usize = 1024 * MB;

#[test]
fn system_memory_heap_limits_match_v8_defaults() {
  let params = v8::CreateParams::default()
    .heap_limits_from_system_memory((2 * GB) as u64, 0);

  assert_eq!(params.max_old_generation_size_in_bytes(), GB);
  assert_eq!(params.max_young_generation_size_in_bytes(), 48 * MB);
  assert_eq!(params.code_range_size_in_bytes(), 0);
}
