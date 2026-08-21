//! Smoke test for the porffor backend: drives the real rusty_v8 API and checks
//! that values round-trip through porffor's heap.
//!
//! Only meaningful with `--features porffor,link_porffor`; without a linked
//! porffor object every symbol is a null-returning stub.
#![cfg(feature = "link_porffor")]

/// Porffor compiles a single global heap per binary, so only one isolate may
/// be live at a time process-wide. Tests therefore serialize on this lock
/// rather than depending on `--test-threads=1` being remembered.
static PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
  use std::sync::Once;
  static START: Once = Once::new();
  START.call_once(|| {
    v8::V8::initialize_platform(
      v8::new_default_platform(0, false).make_shared(),
    );
    v8::V8::initialize();
  });
  PROCESS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn primitives_round_trip() {
  let _guard = setup();
  let isolate = &mut v8::Isolate::new(Default::default());
  v8::scope!(let scope, isolate);

  let n = v8::Number::new(scope, 42.5);
  assert!(n.is_number(), "Number::new produced a number");

  let b = v8::Boolean::new(scope, true);
  assert!(b.is_boolean(), "Boolean::new produced a boolean");
}

#[test]
fn strings_round_trip() {
  let _guard = setup();
  let isolate = &mut v8::Isolate::new(Default::default());
  v8::scope!(let scope, isolate);

  let s = v8::String::new(scope, "hello porffor").unwrap();
  assert!(s.is_string(), "String::new produced a string");
  assert_eq!(s.length(), 13, "length in code units");

  // the bytes must actually come back out of porffor's heap
  assert_eq!(s.to_rust_string_lossy(scope), "hello porffor");
}

#[test]
fn non_ascii_strings_round_trip() {
  let _guard = setup();
  let isolate = &mut v8::Isolate::new(Default::default());
  v8::scope!(let scope, isolate);

  // forces the utf16 representation (porffor type 67) rather than bytestring
  let s = v8::String::new(scope, "héllo → 🌍").unwrap();
  assert_eq!(s.to_rust_string_lossy(scope), "héllo → 🌍");
}

#[test]
fn objects_round_trip_through_the_heap() {
  let _guard = setup();
  let isolate = &mut v8::Isolate::new(Default::default());
  v8::scope!(let scope, isolate);
  let context = v8::Context::new(scope, Default::default());
  let scope = &mut v8::ContextScope::new(scope, context);

  let obj = v8::Object::new(scope);
  assert!(obj.is_object());

  let key = v8::String::new(scope, "answer").unwrap();
  let val = v8::Number::new(scope, 42.0);
  obj.set(scope, key.into(), val.into()).unwrap();

  let got = obj.get(scope, key.into()).unwrap();
  assert!(got.is_number(), "read back a number");
  assert_eq!(got.number_value(scope).unwrap(), 42.0);

  // a key that was never set
  let missing = v8::String::new(scope, "nope").unwrap();
  assert!(obj.get(scope, missing.into()).unwrap().is_undefined());
}

#[test]
fn values_survive_a_collection() {
  let _guard = setup();
  let isolate = &mut v8::Isolate::new(Default::default());
  v8::scope!(let scope, isolate);

  let s = v8::String::new(scope, "must survive").unwrap();

  // churn, then collect: the handle is an arena slot registered with porffor's
  // collector as a root, so this must still read back
  for _ in 0..20_000 {
    let _ = v8::String::new(scope, "garbage garbage garbage");
  }
  scope.request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);

  assert_eq!(s.to_rust_string_lossy(scope), "must survive");
}
