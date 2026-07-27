# C11: ObjectTemplate + internal fields + native accessors, ratchet 61 -> 76

**Result: ObjectTemplate, object internal fields, and native property accessors
work.** An `ObjectTemplate` records its `Set` properties, internal-field count,
and accessors as a Rust-owned struct; `NewInstance` materializes a real JSI
object with those properties (nested FunctionTemplates instantiated to real
functions), internal-field slots, and `Object.defineProperty`-installed
accessors. Native property accessors (`ObjectTemplate::set_accessor*` and
`Object::set_accessor`) bridge through the same C10 host-function machinery,
now with a `PropertyCallbackInfo`-shaped `PropCbInfo`. Internal fields live in a
hidden Symbol-keyed Array on the object itself (the C4 identity-hash trick,
a second Symbol). The Hermes rusty_v8 baseline moves from 61 to **76 passing
tests** (`tests/status/baselines/hermes/rusty_v8.txt`), and `--check --rescue`
holds deterministically across two independent re-runs (identical 76-test pass
set both times).

## What "templates work" means concretely

The nine target-cluster tests that now pass:

```
rv8_test_api::object_template                          - ObjectTemplate::new,
    set_internal_field_count, set_with_attr(FunctionTemplate value, attr),
    new_instance, get/set_internal_field, define_own_property with attribute
    bits verified via Object.getOwnPropertyDescriptor, calling the templated
    function property g.f().
rv8_test_api::object_template_from_function_template   - new_from_template,
    FunctionTemplate::set_class_name, g.constructor.name == "fortytwo".
rv8_test_api::instance_template_with_internal_field    - FunctionTemplate::
    instance_template, internal field set inside a constructor callback,
    `new Ctor()` from JS.
rv8_test_api::object_template_set_accessor             - set_accessor,
    set_accessor_with_setter, set_accessor_with_configuration (data),
    AND set_accessor_property (FunctionTemplate getter/setter pair).
rv8_test_api::object_set_accessor                      - Object::set_accessor
    (accessor registered directly on an Object, not via a template).
rv8_test_api::object_set_accessor_with_setter          - getter + setter.
rv8_test_api::object_set_accessor_with_setter_with_property - READ_ONLY
    accessor (setter dropped, assignment is a no-op).
rv8_test_api::object_set_accessor_with_data            - accessor data arg.
rv8_test_api::context_from_object_template             - Context::New's
    global_template applied onto the context's global object.
```

Six more tests pass as a side effect of the EscapableHandleScope fix below and
poison-cascade clearing, all stable across both runs:
`escapable_handle_scope`, `escapable_handle_scope_can_escape_only_once`,
`escapable_handle_scope_from_isolate`, `allow_javascript_execution_scope`,
`cpu_profiler_bindings`, `set_idle`.

An internal smoke test (`hermes_object_template_basic`) also proves the round
trip end to end: an ObjectTemplate with one internal field and a `key` accessor
that returns the field, plus a FunctionTemplate constructor that sets an
internal field on its receiver invoked via `new`.

## Design implemented

### Templates as Rust-owned structs with a shared header

Hermes has no template concept (same as C10's FunctionTemplate). `FnTemplate`
and `ObjTemplate` are `#[repr(C)]` structs leaked via `Box::into_raw`; both
begin with a shared `TemplateHeader { kind: u8 }` so `v8__Template__Set` (whose
`this` is the abstract `Template` base) can dispatch on the concrete kind. A
template pointer is a raw `Box::into_raw` allocation (always even), whereas a
tagged Local always has its low bit set (`slot_ptr`'s `(i<<1)|1`), so
`data_is_template_ptr(p) = !p.is_null() && (p as usize) & 1 == 0` cleanly tells
an untyped `*const Data` value apart. This backs the real `Data::
IsFunctionTemplate`/`IsObjectTemplate`/`IsValue` predicates, and lets
`Template::Set`'s stored value distinguish a nested template from a handle-table
Local at instantiation time (a FunctionTemplate value is instantiated to a real
function via `GetFunction` when the property is applied).

`#[repr(C)]` on both structs is load-bearing: without it Rust reorders fields
and `header` is not at offset 0, so `template_kind` reads a garbage kind byte
(this was the first bug, caught via the internal smoke test: an ObjTemplate
read as kind 0/FN, so its properties were never applied).

### Internal fields as a hidden Symbol-keyed Array on the object

JSI objects have no native internal-field slots. Internal fields are stored ON
the object itself, in a hidden non-enumerable Symbol-keyed property holding a JS
Array of length `internal_field_count`, each slot initialized to `undefined`.
This mirrors the C4 identity-hash trick (a second, dedicated Symbol
`v8x_internal_fields`, plus a cached `Object.getOwnPropertyDescriptor`), and
naturally survives being read back through any number of handle-table slots
because the storage is the object's own heap storage. `InternalFieldCount`/`Get`
/`SetInternalField` read/write that Array. `Get/SetAlignedPointerInInternalField`
store the pointer as a C7 `External` (a JSI HostObject) in an internal-field
slot and unwrap it back on read.

Internal fields are set up at object-creation time in `NewInstance`
(`v8x_hermes_object_new_with_internal_fields`) and, for a template-instantiated
constructor, on the freshly-constructed receiver inside the host-function
trampoline before the callback runs (`v8x_hermes_object_ensure_internal_fields`)
so a constructor callback can `this.set_internal_field(...)`.

### Property attributes via a real defineProperty

`object_template` reads back `configurable`/`enumerable`/`writable` via
`Object.getOwnPropertyDescriptor`, so a plain property set is not enough. Stored
`Template::Set` properties and `define_own_property` route through
`v8x_hermes_object_define_property`, which calls `Object.defineProperty(obj, key,
{value, writable: !(attr & READ_ONLY), enumerable: !(attr & DONT_ENUM),
configurable: !(attr & DONT_DELETE)})`. The PropertyAttribute bit values
(READ_ONLY=1, DONT_ENUM=2, DONT_DELETE=4) come from
`vendor/rusty_v8/src/property_attribute.rs`.

### Native property accessors reuse the C10 bridge

`ObjectTemplate::set_accessor*` and `Object::set_accessor` install a JS accessor
`Object.defineProperty(obj, key, {get, set, enumerable, configurable})` whose
get/set are real JSI `createFromHostFunction` host functions, the same shape as
C10's `v8x_hermes_function_new`, but dispatching through new trampolines
`v8x_hermes_dispatch_accessor_getter`/`_setter`. Each builds a
`PropertyCallbackInfo`-shaped `PropCbInfo { isolate, holder, data, return_slot }`
and invokes the accessor callback (`NamedGetterCallbackForAccessor` /
`NamedSetterCallbackForAccessor`; linked name-only, so declared as raw-pointer
fn types and transmuted from bits). `key`/`holder`/`data` are marshaled into
fresh handle-table slots per invocation with C10's watermark/truncate
discipline; `data` and the property-`key` Name are copied into Runtime-owned
`shared_ptr<jsi::Value>` holders that outlive every HandleScope (same reasoning
as C10's data capture). `holder` is the accessor call's receiver `thisVal`
(holder == this for a plain own-accessor, no prototype chain). The
`PropertyCallbackInfo::GetReturnValue`/`Holder`/`Data`/`GetIsolate` accessors
read from `PropCbInfo`; `ShouldThrowOnError` is always `false` (every vendored
test in this cluster asserts `!args.should_throw_on_error()`, so `false` is
exactly correct, not a cop-out). Exceptions route through the existing C10
`pending_exception`/`pending_callback_exception` plumbing (a shared
`surface_pending_exception` helper), not a second mechanism.

A READ_ONLY accessor (`object_set_accessor_with_setter_with_property`) drops the
setter entirely, so assignment is a silent no-op in sloppy mode, matching v8's
native-data-property ReadOnly semantics.

`set_accessor_property` (the FunctionTemplate getter/setter pair) instantiates
each FunctionTemplate to a real function at NewInstance and installs a JS
`{get, set}` accessor from those functions.

### Constructors: host functions made constructable

A JSI host function (`createFromHostFunction`) is not constructable (`new f()`
throws). A v8 FunctionTemplate's function is a constructor. When a function comes
from a FunctionTemplate (`instance_internal_field_count >= 0`, the last param of
`v8x_hermes_function_new` doubling as a template marker), the host function is
wrapped in an ordinary JS function `function(){ return impl.apply(this,
arguments); }` built via `new Function("impl", ...)`. That wrapper is
constructable; inside a `new` call its `this` is the fresh object, forwarded to
the host impl (where internal-field setup and the constructor callback run), and
the callback's returned `this` object becomes the `new` result. A plain
`Function::new` (marker `-1`) stays a non-constructable host function, unchanged
from C10.

### SetClassName and new_from_template constructor linkage

`SetClassName` records a class-name String slot; `GetFunction` sets the
function's `.name` via `defineProperty` (non-writable). `ObjectTemplate::
new_from_template` records the source FunctionTemplate; `NewInstance` links the
instance to that constructor with `Object.setPrototypeOf(obj, ctor.prototype)`,
so `instance.constructor.name` resolves to the class name.

### Context global_template

`v8__Context__New`'s previously-ignored `templ` param now applies the supplied
global ObjectTemplate's `Set` properties and accessors onto the context's global
object at context-creation time (the Hermes context IS the isolate, one global).

## The EscapableHandleScope fix this cycle found (load-bearing)

The vendored `eval` test helper wraps every eval in an `EscapableHandleScope`
and returns `scope.escape(value)`. The C3 `EscapeSlot__escape` re-interned the
escaping value via `value_to_utf8` into a slot ABOVE the child scope's watermark
- which was both string-only/lossy AND reclaimed by the child scope's
truncate-on-exit, so any escape of a value produced an empty string. No prior
baselined test actually depended on escape's return value, so this was dormant;
`object_template` (whose descriptor-check reads the escaped block-completion
string through `eval`) was the first test to depend on it, and the whole
template cluster failed on it before templates were even reached.

The vendored `EscapableHandleScope` construction runs `raw::EscapeSlot::new`
(which calls `reserve`) BEFORE `HandleScope::init` records the child watermark.
So `reserve` now pushes an `undefined` placeholder slot in the PARENT (below the
child watermark, surviving the child truncate) and returns its index; `escape`
overwrites that reserved slot with a copy of the escaping value
(`v8x_hermes_set_slot`) and returns a handle to it. This is non-lossy for any
value type. Fixing it also newly passed the three `escapable_handle_scope*`
tests directly.

## Process-crash landmines neutralized

- **No unwinding across `extern "C"`.** The two new accessor trampolines are the
  only places a Rust accessor callback runs; they return plain slot sentinels
  and never let a panic cross into C++. The C++ getter/setter host-function
  lambdas wrap their JSI calls and only throw a controlled `jsi::JSError` (the
  pending exception), the same discipline as C10's function trampoline.
- **Handle-table pointer invalidation avoided.** Every new `w->push(...)` call
  site was audited for the C9 use-after-realloc class: `v8x_hermes_set_slot` and
  `install_internal_fields` copy the source `jsi::Value` BEFORE any push/assign
  that can reallocate `handles`; the accessor lambdas read the return/exception
  slots and copy them out BEFORE truncating back to the watermark, exactly like
  C10.
- No new process-crashers: across the two full `--rescue` runs the only crashers
  are the four PRE-EXISTING ones (`array_buffer_with_shared_backing_store` from
  C8, plus `cppgc_cell`/`cppgc_object_wrap8`/`cppgc_object_wrap16`), cleanly
  skipped by the harness, not masking any pass.

## No regressions

- Internal hermes smoke tests: 14/14 pass (13 prior + the new
  `hermes_object_template_basic`)
  (`cargo test --no-default-features --features hermes,link_hermes --lib hermes::`).
- QuickJS build (`cargo check --no-default-features --features quickjs`):
  unaffected (only `src/hermes/*` and the auto-generated `shims.rs` changed).
- `gen_hermes_shims.sh` re-run: idempotent (byte-identical `shims.rs` on a second
  run); it auto-detects the 28 newly-real symbols in core.rs and drops their
  stubs. No hand-written gate needed (there were zero gated stubs to preserve;
  the stub-only `--features hermes` build remains blocked ONLY by the
  pre-existing, unrelated `misc.rs` `typed_array_new_stub!` `c_void` error,
  same as C9/C10 noted, out of scope). The real backend (`link_hermes`), the one
  the ratchet runs, builds and links clean.
- Baseline `tests/status/baselines/hermes/rusty_v8.txt` 61 -> 76; `--update
  --rescue` then `--check --rescue` holds ("OK: ratchet holds (76 baselined)")
  deterministically across two independent runs. Plain `--update`/`--check`
  WITHOUT `--rescue` still false-regresses on the shared PROCESS_LOCK poison
  artifact (the C9 finding, confirmed again: a bare `--update` clobbered the
  baseline to 34 before it was restored and re-run with `--rescue`); any future
  CI wiring for hermes must pass `--rescue`.

## Recommended next target (C12)

**Named/indexed property interceptors** (`ObjectTemplate::set_named_property_
handler`, `object_template_set_named_property_handler`,
`context_with_object_template`): the `NamedGetter/Setter/Query/Deleter/
Enumerator/Definer/Descriptor` interceptor family, which returns an `Intercepted`
enum (unlike the accessor callbacks that have no such return). It reuses this
cycle's `PropertyCallbackInfo` bridge but needs a JSI `Proxy`-based object (Hermes
exposes `Proxy`) or a HostObject to route arbitrary property access through the
interceptor, which is a larger design step. The standing **BackingStore +
`std::shared_ptr` refcount** subsystem (neutralizes the last non-cppgc
process-crasher, `array_buffer_with_shared_backing_store`) remains open from
C8. Smaller wins nearby: `function_template_signature` (needs `Signature` + a
receiver-type check) and `object_template_immutable_proto` (needs enforcing an
immutable prototype on the global, honestly left stubbed here: the
`SetImmutableProto` flag is recorded but not enforced, since JSI has no
`preventExtensions`-on-proto hook and a `Proxy` global is a larger change).
