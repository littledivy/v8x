# Hermes backend spike (overnight autonomous run)

**Directive (user): explore frontiers, truly innovate, do not accept limitations. AOT can make
this wonderful. Bias to BUILD + MEASURE real prototypes/experiments, chase the AOT-native-Deno
vision (tiny instant-start binary). Fully autonomous overnight.**

Goal: prototype a static **Hermes** backend for v8x (`engine_hermes`) that can run
Deno, and test whether AOT-compiling JS (Hermes bytecode / static-hermes /
Porffor) can replace the V8 startup snapshot.

Branch: `hermes-backend-spike`. Loop state in `.omc/hermes-loop/`.

## Status board (updated each cycle)
- [x] *** deno_core boots to the LAST step of new_inner on Hermes (past primordials, ops, 01_core.js, module graph); wall = ArrayBuffer BackingStore. rusty_v8 89/267. CEILING: async gens pervasive in full runtime ***
- [x] *** Hermes backend runs real JS: objects/arrays/numbers/functions (12/12 smoke tests), on the ratchet ***
- [x] *** SHIP: working QuickJS deno binary at ~/deno-quickjs/deno (TS+HTTP+npm verified) ***
- [x] C0 research: Hermes embedding API + AOT capabilities
- [x] C0 research: v8x integration surface (how to add engine_hermes)
- [x] C0 research: AOT-solves-snapshot feasibility (HBC / static-hermes / Porffor)
- [x] C1 scaffold: engine_hermes feature + src/hermes/ skeleton (build.rs untouched, not needed yet)
- [x] C2 FFI feasibility PROVEN: Rust -> extern "C" C++ -> real libhermes JSI
      evals "40 + 2" == 42, prints it (docs/hermes-spike/experiments/C2-hermes-ffi.md).
      Prebuilt macOS hermes.framework (facebook/hermes v0.11.0 release, 4.5MB
      vendored, no source build). `--features hermes,link_hermes`.
- [x] C3 implement: core v8__* hello-world path runs a REAL script THROUGH the
      hermes backend: v8x Rust surface (Isolate/HandleScope/Context/String/
      Script) -> our v8__* -> libhermes evaluateJavaScript -> "hello world" back
      in Rust (docs/hermes-spike/experiments/C3-hermes-helloworld.md).
- [x] C4 SOLVED object identity (strictEquals + hidden-symbol-id hash); 4 hermes tests pass
- [x] C5 AOT: Hermes HBC parse-free boot = 21x faster than source (202ms->9.3ms) THROUGH the backend
- [x] E6 AOT: REAL QuickJS heap snapshot WORKS (but no startup speedup - see log)
- [x] C6 widen surface: Object/Array/Number/Integer/Boolean/Function real + 6 smoke tests pass (12/12
      hermes tests total); registered hermes as 4th harness backend, rusty_v8 baseline honestly 0/16
      (14 targets link, 2 don't - ICU + TypedArray gaps, named not chased); gen_hermes_shims.sh gate
      + symbol-detection bugs fixed (docs/hermes-spike/experiments/C6-hermes-surface.md).
- [ ] C7 AOT E7: Static Hermes native AOT on real bootstrap chunk (push past 'untyped=no win')
- [ ] C8 AOT E8: AOT-native Deno north star - tiny native binary running a Deno program
- [ ] C9 AOT E9: hybrid AOT-bytecode + build-time-precomputed constant data (partial heap)

## Cycle log
(newest first)

### Cycle D7 - past async-gen wall to the last boot step + the VIABILITY crux (agent, opus) DONE
PART A (the honest crux): async generators are a ONE-OFF in deno_core's boot JS (only the primordials
%AsyncGenerator% prototype capture at 00_primordials.js:285 - reflects on the shape, never drives one) so
deno_core is NOT blocked. But PERVASIVE in the wider Deno runtime (ext/, loaded later): 16 async function*,
4 async *method(), 58 Symbol.asyncIterator; hot paths ext/net/01_net.js (for await over a listener) +
ext/web/09_file.js (Blob.stream()) + Node stream polyfills. Those are REAL suspend/resume async generators,
NOT source-transformable like the one primordials literal. => deno_core CORE boots with a 1-literal
workaround, but a FULL Deno runtime running user code that hits for-await-over-listener / Blob.stream / Node
streams WILL hit the Hermes async-gen compiler gap at RUNTIME. Deno-on-Hermes is not viable for a COMPLETE
runtime without Hermes gaining async generators (a compiler feature we cannot add) or a large per-site
rewrite of vendored Deno source. THIS IS THE CEILING.
PART B: rewrote the primordials async function* literal -> synthetic %AsyncGenerator% shape in the Hermes
compile path, then cleared 6 more walls: Function.length (async-op codegen), Object::with_prototype_and_
properties, Value::IsPromise (module-eval), value-Global durable pinning (C2 bug: stored object read back
non-object), builtin globalThis.console, UnboundModuleScript. Boot now runs op registration, the virtual ops
module, ALL of 01_core.js, and the builtin ES-module graph, stopping at JsRuntime::store_js_callbacks
(new_inner's LAST step). NEW WALL: v8__ArrayBuffer__NewBackingStore__with_data (null stub) - external-memory
BackingStore subsystem. Isolated 1+1 boot probe already passes, so close after BackingStores.
rusty_v8 86 -> 89 (3 fixes turned green). --check --rescue holds. Deno-checkout diagnostics reverted.
NEXT (D8): implement the external-memory BackingStore chain -> finishes store_js_callbacks -> likely
deno_core boots + runs 1+1 (a real 'deno_core runs on Hermes' milestone). THEN report the ceiling to the user.

### Cycle D6 - bump Hermes v0.11.0 -> 260318099.0.1 (HBC 99) (agent, opus) DONE - boot past intrinsics
Bumped to Hermes 260318099.0.1 (current HERMES_VERSION on RN main, HBC v99); hermesc+runtime both v99
(C5 HBC path stays matched). Sourcing was hard: facebook/hermes releases ship the macOS-HOST framework
only for v0.11.0; newer host frameworks live inside RN's Maven Central hermes-ios artifact
(destroot/Library/Frameworks/macosx/). Chose 260 not RN 0.75.4 because FinalizationRegistry did not land
in Hermes until March 2026 (absent from all RN-tagged builds); only 260 has all 6 intrinsics.
2 engine fixes: named GCConfig in runtime_new (empty heap name null-derefs in HadesGC under deno = SIGSEGV
before any JS); withMicrotaskQueue(true) (WeakRef/FinalizationRegistry need it). build.rs auto-detects the
renamed hermesvm.framework; is_hbc uses new IHermesRootAPI. ~3000-line JSI shim recompiled clean vs 2026 headers.
RE-VERIFY: 20/20 hermes lib tests (smoke+5 probes+HBC); rusty_v8 84->86 (newer Hermes conformant Intl/ICU
-> icu_collator + icu_date pass); baseline updated.
NEW BOOT RESULT: past the intrinsics wall - primordials.js no longer throws on globalThis[name]. Fails one
step deeper at COMPILE time: "285:40: async generators are unsupported" - line 285 is
Reflect.getPrototypeOf(async function* () {}). Hermes has generators/async/for-await but NOT async function*.
Wall moved from missing-intrinsics to an UNSUPPORTED SOURCE-LANGUAGE feature (Hermes compiler gap).
NEXT (D7): source-transform the one async function* in the boot script (it only captures %AsyncGenerator%
prototype) in the Script/CompileModule path (like D2), OR provide %AsyncGenerator% another way; then re-run
boot -> next wall. Honest risk: if Deno's runtime uses async generators for real elsewhere, this becomes pervasive.

### Cycle D4/D5 - REAL deno_core boot on Hermes (agent, opus) DONE - THE MILESTONE
An actual deno_core::JsRuntime::new RUNS on the Hermes backend. Path a: wired real deno_core (checkout
~/gh/deno-v8x-rebase, OUTSIDE this branch) - added a `hermes` feature to nathan's libs/deno_v8 facade
(re-exports v8x_backend::* like quickjs), repointed v8x_backend crates.io->local v82jsc path, workspace
v8 alias features ["simdutf","hermes"]. deno_core + hello_world example compile+link against Hermes.
Run with DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes.
RESULT (boots to X, fails at Y): JsRuntime::new runs through v8 platform init, isolate+context (deno_core
global ObjectTemplate), full string interning, Deno.core namespace, THEN executes deno_core's FIRST
bootstrap script ext:core/00_primordials.js which throws TypeError: target is not an object. Bisected:
primordials enumerates every intrinsic via globalThis[name]; Hermes v0.11.0 MISSING 6: AggregateError,
BigInt, BigInt64Array, BigUint64Array, FinalizationRegistry, WeakRef. ENGINE-completeness gap, NOT a
v8__* gap. Does not run 1+1 yet (primordials must finish).
5 native walls knocked down (src/hermes/): simdutf__* link (reuse quickjs impl), Platform__NewCustomPlatform,
Local__New (+ new v8x_hermes_slot_dup C++ primitive), String__NewExternalOneByteConst/OneByteStatic,
synthesized console on Context__GetExtrasBindingObject. rusty_v8 83->84 (context_get_extras_binding_object
green from the console fix). --check --rescue holds. Committed e2d7ac9.
NEXT (D6): BUMP vendored Hermes framework to a newer release (BigInt/WeakRef/FinalizationRegistry/
AggregateError all landed upstream post-v0.11.0) -> clears primordials in one move; then walls shift to
the ext:core/mod.js + synthetic ext:core/ops MODULE GRAPH (D2 module system supports the shape). Watch
JSI ABI / HBC-version breakage on the bump.

### Cycle D3 - op glue (salvaged from a crashed agent) DONE - 5/5 boot probes
The D3-D5 agent CRASHED on an API ConnectionRefused mid-work; salvaged its clean D3 op glue (its bad
mid-run baseline --update was discarded, restored to 83). Implemented v8__Context__GetExtrasBindingObject
(lazy per-context plain object, deno bootstrap reads built-ins onto it) + v8__Function__SetName/GetName
(JSI has no name setter -> define name property). New probe boot_op_external_roundtrip: bind one op via
External + FunctionTemplate, set_name, call from JS, + extras-binding stability -> 5/5 boot probes pass,
builds clean. rusty_v8 baseline held at 83 (ratchet run timed out at 2min - harness is slow; re-check in D4).
D4/D5 (wire real deno_core against hermes + attempt a JsRuntime boot) did NOT happen - agent died before
it + disk was tight (2.5G). Now freed 7G (cargo clean deno-v8x-rebase target; binary already at
~/deno-quickjs/deno) -> 9.6G free. Re-attempting the real boot next.

### Cycle D2 - ES modules on Hermes (agent: d2, opus) DONE - all 4 boot probes GREEN
boot_es_module_instantiate_evaluate GREEN => 4/4 boot probes pass. rusty_v8 81 -> 83
(script_compiler_source, module_evaluation). --update, no regressions, test_api still links.
JSI has NO ES-module API, so v8 module semantics modeled in Rust (src/hermes/modules.rs) on JSI:
- CompileModule = line-oriented source transform -> closure (function(__imports,__exports){...});
  imports -> `const x = __imports["spec"].x;` prologue; exports -> `__exports.x = ...`.
- InstantiateModule = walk import requests, call resolve cb, recursively instantiate, build namespaces.
- Evaluate = depth-first deps (V8 order), build __imports from dep namespaces, run closure, resolved
  promise (reuses D1). Synthetic modules = exports obj filled by native steps + SetSyntheticModuleExport.
- Module/ModuleRequest/FixedArray = Rust Box records, raw ptr = v8 Local; cross-scope JS values held in
  runtime-owned durable pins (v8x_hermes_pin, the C2 pattern).
Handles import "spec" / {a, b as c} / default / *as ns; export const/let/var/fn/class/default/{a,b as c}.
Boot-graph shape (source module importing named bindings from a SYNTHETIC module) supported end-to-end.
GAPS (documented): re-exports `export {x} from` + `export *` don't register a request; Location offsets 0;
no top-level await. Not needed for the boot graph shape.
IMPORTANT: probes green is a PROXY. The REAL test is booting an actual deno_core JsRuntime (full
bootstrap + real module graph + ops). Next: D3 op glue (2 stubs Context__GetExtrasBindingObject +
Function__SetName + verify External FunctionTemplate op roundtrip) -> D4 wire deno_core to build against
LOCAL v8x with hermes,link_hermes -> D5 attempt the real boot, report exact failures.

### Cycle D1 - Promises + microtask queue (agent: d1, opus) DONE - 2/3 boot walls cleared
boot_promise_resolver_roundtrip + boot_microtask_enqueue_and_checkpoint GREEN. rusty_v8 77 -> 81
(promise_resolved, promise_rejected, microtask_queue_new, set_promise_reject_callback). --update, no regressions.
Impl: Promise::Resolver::New/GetPromise/Resolve/Reject, Promise::State/Result/HasHandler/MarkAsHandled/
Then/Catch/Then2, Isolate::EnqueueMicrotask, real PerformMicrotaskCheckpoint, MicrotaskQueue::* - one
cached per-runtime JS helper. A v8 Resolver = the [promise,resolve,reject] array new Promise(...) returns;
state+result in a closure WeakMap keyed by the promise (first-write-wins, matches V8 sync settle);
HasHandler via a separate WeakSet only user then/catch mark.
CRASH FIX: Hermes InternalBytecode Promise polyfill schedules via a global setImmediate the bare JSI
global lacks (no microtask-queue RuntimeConfig here) -> installed a setImmediate FIFO + drainJobs drained
by the checkpoint; rejected-no-handler never aborts; drain capped vs re-enqueue hang.
REMAINING BOOT WALL: ES modules - v8__ScriptCompiler__CompileModule still null. D2.
Deferred (JSI cannot surface): promise-hook/reject callbacks, Auto-policy auto-flush on eval.

### Cycle D0 - deno-boot recon (agent: d0, opus) DONE => path is clear + measurable
FIRST WALLS (all null stubs on hermes): Promises, microtasks, ES modules. Proved via in-repo probe
`hermes_boot_probe` (src/hermes/mod.rs): boot_baseline_isolate_context_script OK; promise_resolver /
microtask_checkpoint / es_module_compile all FAIL (stubs). Run: cargo test --features hermes,link_hermes
--lib hermes_boot_probe -- --test-threads=1.
STRUCTURAL: deno_core boots FROM SOURCE (InitMode::New, no snapshot required) BUT loads ext:core/mod.js
as an ES MODULE + ext:core/ops as a SYNTHETIC module -> module subsystem is ON the minimal boot path,
not deferrable. External/op infra (External__New, FunctionCallbackInfo::Data, FunctionTemplate) is
ALREADY real on hermes = NOT the wall.
ROADMAP to boot: D1 Promises + MicrotaskQueue + PerformMicrotaskCheckpoint (hermes has native JS
Promises + rt.drainMicrotasks -> JSI host-fn work). D2 ES modules (ScriptCompiler__CompileModule,
Module__{InstantiateModule,Evaluate,GetModuleRequests,CreateSyntheticModule,SetSyntheticModuleExport,
GetModuleNamespace,GetStatus}) - the real headline, no clean JSI analogue, needs a modeling spike. D3
op glue (verify External FunctionTemplate roundtrip + 2 tiny stubs Context__GetExtrasBindingObject,
Function__SetName). D4 deno_core JsRuntime runs a script on hermes.
deno_core HARNESS not run yet: ~/gh/deno-v8x-rebase pulls v8x from crates.io (quickjs alias only, no
hermes/path dep) so ensureDenoV8Patch fails + full deno_core build won't link until Promises+modules
exist. Re-attempt after D1-D2. Probe (33ab561) is the interim measure.

### Cycle C12 - FunctionTemplate signatures + a TryCatch lifetime fix, ratchet 76 -> 77 (agent: c12) DONE (wind-down)
FunctionTemplate::Signature (v8__Signature__New + receiver check): each FnTemplate gets a process-global
template_id; instances stamped via hidden Symbol-keyed prop; signature-bearing fns walk the receiver
prototype chain for a matching stamp before running, else throw "TypeError: Illegal invocation". New
pass: function_template_signature. 15/15 internal smoke.
FIXED a load-bearing exception-lifetime bug (same class as C11 EscapableHandleScope): TryCatchFrame held
the exception as a raw handle-index that a later EscapableHandleScope exit (vendored eval() helper) could
truncate before the message was read. Now held as a Runtime-owned shared_ptr<jsi::Value>, fresh slot per read.
DEFERRED (clean, not half-committed per wind-down): named property interceptors - designed in full (ABI
shapes; Intercepted is #[repr(u32)]{kYes=0,kNo=1}, inverted from intuition) but not implemented. Notes in
C12 doc for the next cycle.
CRASHER FINDING (honest): the 4 remaining crashers (array_buffer_with_shared_backing_store + 3 cppgc_*)
abort via "panic in a fn that cannot unwind" inside the VENDORED support.rs extern C trampoline - cannot
be caught without editing vendored code (which the ratchet forbids). Prototyped catch_unwind, confirmed no
effect, reverted. Suite exactly as crash-stable as before; --rescue skips them cleanly. No new crashers.

### Cycle C11 - ObjectTemplate + property accessors, ratchet 61 -> 76 (agent: c11) DONE
ObjectTemplate, object internal fields, and native property accessors all work. +15 tests, --check
--rescue holds x2. Clusters: object_template (internal fields, PropertyAttribute, templated fn prop),
object_template_from_function_template (SetClassName + constructor.name), instance_template_with_
internal_field (new Ctor()), object_template_set_accessor (all 4 incl set_accessor_property),
object_set_accessor{,_with_setter,_with_setter_with_property,_with_data}, context_from_object_template,
+ 3 escapable_handle_scope* bonus.
~28 new v8__*. Templates = #[repr(C)] structs with a TemplateHeader{kind} tag; Data::Is*Template via
raw-ptr-vs-tagged-Local lowbit. Internal fields = hidden Symbol-keyed JS Array on the object (reuses
C4 identity), aligned-ptr fields via C7 External. Attributes via real Object.defineProperty. Native
accessors reuse C10 host-fn bridge + new PropertyCallbackInfo/PropCbInfo trampolines, exceptions via
C10 pending-exception path.
FIXED the C3 EscapableHandleScope::escape compromise: was string-only/lossy and reclaimed by child
scope truncate. Now reserves a parent slot at construction + overwrites on escape. This was the actual
blocker for the whole template cluster (vendored eval helper escapes its result) + unlocked 3 escapable
tests. Only the 4 pre-existing C8 crashers remain.
NEXT (C12): named/indexed property interceptors (JSI Proxy/HostObject + Intercepted enum);
function_template_signature; BackingStore/shared_ptr (last non-cppgc crasher).

### Cycle C10 - native function callbacks, ratchet 58 -> 61 (agent: c10, opus) DONE
A Rust/C v8 FunctionCallback is invoked when JS calls the fn: reads args/this/data, sets return value,
value flows back to JS (smoke test hermes_native_callback + 3 rusty_v8: function_builder_raw,
function_callback_info_parts, return_value). --check --rescue holds.
Impl: Function::New, FunctionTemplate::New + GetFunction (template = deferred Function::New); the
FunctionCallbackInfo bridge (JSI createFromHostFunction host lambda marshals this/data/args into
handle slots -> v8x_hermes_dispatch_callback builds a Rust CbInfo, invokes the callback ptr, returns
the ReturnValue slot; C++ copies result before truncating the per-callback scope; data held in
shared_ptr<jsi::Value> to survive scope truncation). All v8__FunctionCallbackInfo__* + ReturnValue__*
accessors matching the vendored layout; predicates IsInt32/Uint32/Null/True/False + Number/Int32Value.
Callback-throw routes Isolate::ThrowException -> per-isolate pending_exception -> jsi::JSError ->
surfaces via C9 TryCatch, never aborts. 13/13 smoke, quickjs clean, generator idempotent.
KNOWN (pre-existing, not C10): stub-only `--features hermes` build blocked by misc.rs:100 c_void error
(confirmed on HEAD). The real link_hermes backend the ratchet uses builds+links clean. FIX pending.
NEXT: ObjectTemplate + template instantiation (object_template*, internal fields, signatures) - reachable
on this cycle's template machinery; then property accessors (reuse callback bridge via PropertyCallbackInfo);
BackingStore/shared_ptr subsystem kills the last 3 cppgc_ crashers.

### Cycle C9 - TryCatch + exception surfacing, ratchet 33 -> 58 (agent: c9) DONE
throw/catch works: TryCatch has_caught/exception/message(synthesizes "Uncaught Error: foo")/stack_trace/
rethrow/reset (incl the V8 quirk that reset() after rethrow() keeps has_caught true); Isolate.
ThrowException; Exception::{Error,TypeError,RangeError,ReferenceError,SyntaxError}; Message::Get.
=> 58/267 rusty_v8 tests pass on hermes (was 33). Baseline --updated; --check --rescue holds x3.
Impl: per-runtime tc_stack of TryCatchFrame + capture_exception() sink wired into run/eval_buffer/
function_call; 12 v8__TryCatch__* + ThrowException + Exception ctors in core.rs/hermes_shim.cpp.
CRASH LANDMINES fixed: (1) latent Isolate Enter/Exit needed REAL nesting (ISO_STACK Vec push/pop) -
flat set/clear double-panic-SIGABRT'd on teardown when Exception::type_error's internal enter/exit
nested. (2) use-after-realloc in throw_exception (read slot before handles.push_back that can realloc).
(3) PRE-EXISTING vendored-infra artifact: shared PROCESS_LOCK RwLock poisons when an earlier test
panics mid-scope, cascading PoisonError onto ~200 later tests -> use run.mjs --rescue (already exists
for quickjs's identical issue). Any hermes CI wiring MUST pass --rescue; plain --check falsely reports
a regression against this baseline.
NEXT: native Function::new callbacks (unlocks microtask/accessor clusters), then BackingStore/shared_ptr
(kills the last known crasher array_buffer_with_shared_backing_store).

### Cycle C8 - unlock test_api, ratchet 10 -> 33 (agent: c8, opus) DONE
rv8_test_api (248 cases) + rv8_test_cppgc now LINK -> all 16/16 files link (was 14). 33 baselined (was
10): 10 file-level + 23 test_api cases (19 engine-independent crdtp_* inspector + cached_data_version_tag,
get_version, icu_set_common_data_fail, inspector_string_view, latin1_to_utf8). --check holds.
Implemented: ICU trio (src/hermes/misc.rs, pure Rust, no v8__ prefix so generator never stubs them) -
this alone unblocked cppgc link; 12 TypedArray ctors + ArrayBuffer New/ByteLength/Data + TypedArray
Length over jsi::ArrayBuffer via the JS global constructors (paste! names dodge the generator regex).
HARNESS HARDENING (important): gen_hermes_shims.sh now emits NULL-returning stubs, not unimplemented!().
A panic in an extern "C" fn cannot unwind and ABORTS the whole test binary; null-return (linking is
name-only) converts ~20 aborting stubs into graceful single-test FAILURES. Added real crash-guards:
V8__GetVersion (null -> SEGV in CStr::from_ptr), Context Get/SetMicrotaskQueue (null -> SEGV in &*ptr).
One remaining process-crasher left: array_buffer_with_shared_backing_store (needs BackingStore +
std::shared_ptr refcount subsystem) - harness recovery skips just that one cleanly.
No regressions (12/12 smoke, stub-hermes + quickjs clean, generator idempotent).
NEXT: TryCatch/exception surfacing (largest failing cluster - every tc_scope! test; v8x_hermes_run
already catches jsi::JSError, so surface its value into a pending-exception slot = contained plumbing),
then native Function::new callbacks, then the BackingStore subsystem (also kills the last crasher).

### Cycle C7 - first rusty_v8 tests GREEN on Hermes: 10/16 (agent: c7, opus) DONE
Up from 0/16. All genuinely green, baseline --updated, --check holds, no regressions (12/12 smoke +
quickjs clean). Passing: rv8_slots (7 of 9: context_slots, dropped_context_slots_on_kept_context,
slots_auto_boxing, slots_general_1/2, slots_layer1/2), test_api_flags::set_flags_from_string,
test_simple_external::test, test_single_threaded_default_platform.
Implemented (core.rs + hermes_shim.cpp): PerformMicrotaskCheckpoint(noop), SetFlagsFromString(honors
--use_strict via injected "use strict" directive), Context embedder-data slots (grow-on-demand Vec on
IsoState), Global__New/NewWeak(non-firing, over-retains = leak-not-UAF)/Reset, External as a JSI
HostObject carrying void* + Data__EQ via strictEquals, IsUndefined/IsExternal/Uint32Value.
6 remaining (legit skips): entropy_source (no JSI entropy hook), custom_platform +
platform_atomics_pump_message_loop (need SharedArrayBuffer async-wait + %-natives), external_deserialize
(snapshot subsystem), slots::dropped_context_slots (needs real GC weak-callback reclaim - our weak
never fires by design). rv8_test_api + rv8_test_cppgc still don't LINK (missing ICU trio
icu_get/set_default_locale + udata_setCommonData_77, and TypedArray ctor family).
HIGHEST-LEVERAGE NEXT: ICU trio + TypedArrays -> makes test_api LINK -> surfaces HUNDREDS of test_api
outcomes (big potential jump). Then TryCatch/exception surfacing.

### Cycle C6 - Hermes surface widened + on the ratchet (agent: c6) DONE
Now REAL through the backend: Object New/Get/Set/Has, Array New/Length + indexed get/set, Number/
Integer/Boolean New+Value, Function::Call (jsi::Function::call/callWithThis), Undefined/Null, and the
Is{Array,Function,Number,Boolean,String} predicates. 6 new smoke tests build objects/nested/arrays/
numeric+bool/Function.call and cross-check vs real JSON.stringify/Array.isArray/.reduce via Script::run.
=> 12/12 hermes tests pass (stable parallel + single-thread); quickjs + stub-hermes unaffected.
gen_hermes_shims.sh FIXED: 2 real bugs (regex matched mid-identifier -> wrong std__shared_ptr__v8__*
stub names; scan missed vendor/rusty_v8/src/scope/raw.rs one dir deep = the old '14 hand-appended
symbols' cause). Now idempotent (byte-identical shims.rs on re-run). Preserves link_hermes gates.
RATCHET: registered 4th backend 'hermes' (features hermes,link_hermes, os macos) in
tests/harness/config.json + empty baselines. node run.mjs rusty_v8 hermes --check clean.
BASELINE (honest, not --updated): 0 passing / 16 total. 14 targets LINK (0 pass - they exercise
slots/flags/entropy/snapshot/platform machinery not yet built); 2 don't link (rv8_test_api,
rv8_test_cppgc) on missing ICU syms (icu_get/set_default_locale, udata_setCommonData_77) + the 11
TypedArray ctor family for test_api.
Next: hill-climb - implement slots/flags/platform/external/entropy to pass the FIRST rusty_v8 tests on
Hermes, then --update the baseline. Plus AOT: measure HBC parse-free win on a REAL Deno bootstrap chunk.

### Cycle C5 - Hermes HBC parse-free AOT through the backend (agent: c5) DONE - the 'AOT wonderful' number
Same shim entry (v8x_hermes_eval_buffer) runs JS source OR hermesc-compiled HBC transparently (Hermes
sniffs the 8-byte magic). isHermesBytecode(source)=false, (hbc)=true verified (magic c6 1f bc 03..).
MEASURED (1.4MB bootstrap-shaped JS, 4000 tiny fn/obj/proto defs, 7 cold-runtime iters, medians):
  source parse+compile+run ~202 ms  vs  HBC run-only ~9.3 ms  =>  ~21x faster, ~193ms parse+compile
  recovered by AOT. HBC size 1.36x source.
hermesc from deprecated hermes-engine npm v0.11.0 (matches framework, HBC v84), 2.9MB, vendored to
vendor/hermes/bin/hermesc.
Bug found+fixed: OwnedBuffer needed a trailing zeroed byte (not counted in size()) - Hermes lexer does
a 1-byte OOB lookahead read that SIGSEGVs on raw vector buffers >~300KB (jsi::StringBuffer dodges it
via std::string NUL). First looked like an infinite hang under cargo test; a standalone C++ repro
revealed the real SIGSEGV.
SYNTHESIS (E6 + C5): the AOT-vs-snapshot question is now answered with data. On QuickJS a heap-snapshot
restore is NOT faster than re-running (no mmap fastpath). On Hermes, parse-free HBC boot is 21x faster
than source, and Hermes builtins are native C++ so ONLY the runtime/app bootstrap runs at boot. =>
parse-free AOT bytecode, not heap-snapshot, is the real startup lever. This IS 'AOT makes startup
wonderful', demonstrated. Maps directly to deno compile shipping HBC not source.
Next: measure on a REAL Deno bootstrap chunk; wire prepareJavaScript for cross-isolate sharing; -O vs plain.

### Cycle C4 - Hermes object identity SOLVED (agent: c4) DONE - deepest blocker cleared
Both identity-sensitive parts of the V8 C-ABI reroute through JSI primitives:
- v8__Value__StrictEquals/SameValue -> jsi::Value::strictEquals (shim v8x_hermes_strict_equals).
- v8__Object__GetIdentityHash -> lazily attach a HIDDEN non-enumerable Symbol-keyed prop (real JS
  Symbol + Object.defineProperty; no JSI-native symbol-prop API) holding a monotonic id. VERIFIED
  stable: same object via 2 independent slots -> hash 1 & 1; different object -> 2. Invisible to
  Object.keys/JSON/for-in (visible to getOwnPropertySymbols = correct JS, enumerable:false).
- 4 hermes tests pass; quickjs + stub-hermes still clean. Bonus fixes: v8__Value__IsObject/ToObject,
  a process-wide init_v8_once shared across hermes test modules (V8::initialize gates one global state
  machine; a 2nd module's private Once panicked).
Residual risks (documented, not hidden): (1) SameValue==StrictEquals, not exact for NaN/+0/-0 (JSI has
no bit-level float inspection). (2) no canonicalization (interned slot per obj) built - likely
unneeded since strictEquals+GetIdentityHash match V8's embedder identity contract, BUT unaudited
whether any Rust-side rusty_v8 code hashes raw Local pointers directly (bypassing GetIdentityHash).
(3) GetIdentityHash costs 1-2 real JS calls, unmeasured.
SHARP EDGE: tools/gen_hermes_shims.sh drops hand-added cfg(not(link_hermes)) gates on re-run - do NOT
blindly re-run it; hand-patch new stubs. NEEDS FIX before more shim regen.
NEXT: AOT flourish (run Hermes HBC bytecode through the backend, parse-free) + widen Object/Array +
register 4th backend in tests/harness/config.json (after auditing residual risk 2).

### Cycle C3 - Hermes backend runs hello world through v8 C-ABI (agent: c3, opus) DONE (headline)
A v8x smoke test drives the VENDORED rusty_v8 Rust surface: Isolate -> scope! -> Context -> String ->
Script::compile -> Script::run -> to_rust_string_lossy => "hello world", executed on real libhermes.
Source is compiled+run by OUR v8__Script__Compile/Run and read via the same String/Value path real V8
strings use (not the C2 standalone eval). 3 tests pass (C3 + both C2 smokes). No regressions on stub-
hermes or quickjs.
- Design: arena lives C++-SIDE (jsi::Value is move-only + Runtime-bound, cannot sit in a Rust arena
  like qjs). RuntimeWrapper owns unique_ptr<jsi::Runtime> rt (first-declared, last-destroyed per C2
  rule) + vector<jsi::Value> handle table. Local = table index handed to Rust as tagged ptr ((i<<1)|1).
  HandleScope = watermark; DESTRUCT truncates. Thread-local current iso/ctx; one ctx per runtime.
- ~30 v8__* made REAL in src/hermes/core.rs (Isolate lifecycle, HandleScope CONSTRUCT/DESTRUCT +
  EscapeSlot, Context, String NewFromUtf8/OneByte/Length/Utf8Length/WriteUtf8 + the ValueView quintet
  fast-path read, Script Compile/Run, Value ToString). Their stubs gated cfg(not(link_hermes)) so no
  dup symbols; stub build unchanged. C++ bridge: src/hermes/hermes_shim.cpp.
- Test cmd: cargo test --no-default-features --features hermes,link_hermes --lib hermes:: -- --nocapture
  (scope to hermes:: - bare run also builds vendored rv8_test_api = hundreds of stubbed syms, won't link).
- Known compromise: EscapeSlot__escape re-materializes the escaping value as a STRING (exact for the
  hello-world string, lossy for non-string Values). Clean fix = a handles_dup(rtw, slot) shim entry.
NEXT C4: de-risk OBJECT IDENTITY (the deepest C0 risk) - JSI hands out no raw ptr so two Locals to one
object differ. Intern same object twice, show tagged ptrs differ, wire jsi::Runtime::strictEquals,
demo a Set with one logical member. Gate before broad surface / rusty_v8 hill-climb.

### Cycle C2 - Hermes FFI PROOF (agent: c2, opus) DONE => GO CONFIRMED (breakthrough)
Rust -> extern "C" C++ shim -> real libhermes JSI evaluateJavaScript("40 + 2") -> asNumber -> 42
back in Rust. The C++-only-JSI blocker from C0 is BEATEN: author v8__* in C++ against JSI, export
extern "C", catch jsi::JSError at the boundary. Test asserts 42; a thrown JS error maps to a sentinel.
- libhermes: PREBUILT facebook/hermes v0.11.0 release asset hermes-runtime-darwin (universal
  hermes.framework + JSI/hermes headers), 4.5MB into vendor/hermes/. NO source/CMake/LLVM build, no
  disk risk. (npm hermes-engine is the WRONG artifact: hermesc + android .so only, no macOS host lib.)
- build.rs build_hermes: cc::Build cpp(true) std=c++17 compiles src/hermes/hermes_eval_shim.cpp,
  includes vendor/hermes/include, links framework=hermes + c++ + rpath; gated on link_hermes; honors
  HERMES_LIB_DIR/HERMES_INCLUDE_DIR. Run: cargo test --no-default-features --features hermes,link_hermes --lib hermes_smoke.
- Real JSI rules learned (carry into every impl): (1) the Runtime must OUTLIVE any caught jsi::JSError
  (its embedded Value dtor calls back into the Runtime) - declare rt in outer scope. (2) one
  HermesRuntime per thread. (3) link_hermes surfaced 14 more scope/platform stub symbols (added to
  shims.rs); quickjs + stub-hermes builds still clean, no regressions.
Deepest remaining risk unchanged: object IDENTITY (JSI hands out no raw ptr) - de-risk right after the
hello-world path.
Next C3: real backend = clone src/quickjs/ arena-handle shape (JSI Value is NaN-boxed struct like qjs
JSValue). Implement the 9-symbol hello-world path (Isolate/Context/HandleScope/String/Script) in C++
against JSI + expand the extern-C shim, so a v8x smoke test runs a real script THROUGH the hermes backend.

### DELIVERABLE - working QuickJS deno binary (01:21) SHIPPED
Built deno release --no-default-features --features quickjs on v8x 149.4.0-rc.1 (crates.io) in
~/gh/deno-v8x-rebase. 68M. VERIFIED fully working: JS builtins, async+setTimeout, Deno.readTextFile,
TypeScript, Deno.serve+fetch (200), npm import (change-case). Delivered ~/deno-quickjs/{deno,README.md}.

### Cycle - E6 REAL QuickJS heap snapshot (agent: e6) DONE - the 'AOT vs snapshot' answer
FRONTIER RESULT: a real post-bootstrap heap snapshot IS achievable on QuickJS and v8x already has
the machinery. JS_WriteObject + JS_WRITE_OBJ_REFERENCE serializes the whole reachable graph by value
(nested objs, prototypes, frozen bits, Map/Set, typed arrays, BigInt, cycles, Symbol identity all
round-trip). Native C fn pointers (unserializable) are solved by a SYMBOLIC reference-path registry
(__v8x_snapshot_intrinsics + patch quickjs-67-snapshot-native-function-state): writer emits a
property PATH to each native fn, re-resolved on read against the fresh runtime. Patch 67 also
serializes native object state -> a REAL heap snapshot, not just rebinding. Residual blocker: opaque
JS_SetOpaque C-state (sockets/napi) needs per-class hooks.
Hybrid (re-install natives -> refresh registry -> JS_ReadObject pure-JS on top) WORKS: prototype
docs/hermes-spike/experiments/e6-src/e6_snap.c, 16/16 checks incl a native add() callable after
restore carrying heap-added callCount=42.
MEASURED (arm64 standalone C): small graph 17.9KB blob / 0.08ms restore; 20k-obj graph: from-source
reboot 8.4ms vs snapshot restore 10.0ms, blob 1.46MB.
KEY INSIGHT that refines the whole idea: on QuickJS, restore is NOT faster than re-execution (no
mmap-and-fixup fast path like V8; JS_ReadObject re-allocs+re-hashes every node = same order of work
as running the code). So the heap snapshot's value is STATE CAPTURE (side effects, non-determinism,
expensive-to-recompute state), NOT startup latency. Corollary for the user's vision: for STARTUP,
parse-free AOT-bytecode boot is already ~as good as snapshot-restore on QuickJS -> AOT genuinely
'solves' the startup half without needing snapshots. Snapshots only earn their keep for stateful boot.
Next (E7): drive src/quickjs/snapshot.rs capture/replay across a REAL Deno bootstrap; test replacing
the replay-tape, isolating the load-bearing synthetic ext:core/ops module identity.

### Cycle 1 - scaffold engine_hermes, fix link failure (agent: executor) DONE
The prior commit (9d3d86a) already added Cargo.toml/src/lib.rs wiring and
src/hermes/{mod,misc,shims}.rs, but `cargo build --no-default-features
--features hermes` did not actually compile: rustc rejected it with "symbol
`v8__Platform__CustomPlatform__BASE__DROP` is already defined" (a
duplicate-symbol error, not a linker error - it happens during codegen of the
v8x crate itself, since both the generated stub and the real definition live
in the same crate). Root-caused and fixed tools/gen_hermes_shims.sh; after
the fix, `cargo build --no-default-features --features hermes` compiles and
links clean (0 errors, 0 warnings) with zero Hermes dependency. Pure-Rust
stub backend only, nothing runs real JavaScript yet.
- Cargo.toml / src/lib.rs / src/hermes/{mod,misc}.rs: already correct as
  committed. `engine_hermes`, `link_hermes` (unused so far), `hermes =
  ["engine_hermes"]` alias (deliberately does not pull in `link_hermes`);
  `#[cfg(feature="engine_hermes")] mod hermes;` in lib.rs next to the other
  backends, `V8X_ENGINE` returns `"hermes"`; misc.rs has 25 `cppgc__*` stubs
  with small real bodies backed by a raw pointer slot, since the
  Member/Persistent wrapper code in cppgc.rs treats them as plain data, not
  just link placeholders.
- src/hermes/shims.rs: regenerated, now 737 `v8__*`/`v8_inspector__*` stubs
  (down from the prior 764 - the 27 symbols below are excluded).
- tools/gen_hermes_shims.sh: adapted from tools/gen_qjs_shims.sh. Diverges in
  one important way: it sources symbol names directly from
  `vendor/rusty_v8/src/*.rs` extern decls (no test-build union.txt exists yet
  for hermes) and explicitly excludes symbols the vendored crate itself
  DEFINES with `#[unsafe(no_mangle)]` (the engine-independent
  CustomPlatform-task and Value(De)Serializer::Delegate/Inspector
  Channel/Client callback trampolines in platform.rs/value_serializer.rs/
  value_deserializer.rs/inspector.rs). Stubbing those again is a
  duplicate-symbol error at compile time, not a linker warning, since they
  live in the same crate.
- Confirmed empirically: Rust `extern "C"` FFI linking is name-only (no
  signature check across module/file boundaries), so no-arg
  `unimplemented!()` stub bodies link fine against any real declared
  signature. Same technique the QuickJS/JSC generators already rely on.
- No regression: `cargo check --no-default-features --features quickjs` still
  builds clean.
Next: C2, vendor a real Hermes static library and start replacing shims with
real JSI-backed implementations, starting with the 9-symbol hello-world path
noted in the C0 integration-surface log entry.

### Cycle 0 - Hermes embedding feasibility (agent: hermes-embed-aot) DONE => GO-WITH-CAVEATS
Verdict: technically feasible, hardest of the 3 backends. Decision: SPIKE it (not a commitment);
de-risk object identity before writing broad surface.
- Embedding: JSI is C++-only, NOT ABI-stable (vtables/STL/mangling). Experimental C ABI in
  API/hermes_abi/hermes_abi.h exists but under-documented/not production. => must write ~570 v8__*
  in C++ against JSI, export extern "C", translate C++ jsi::JSError at every boundary.
- Good news: JSI managed value = PointerValue* = a STABLE, refcounted, GC-updated slot (Hades moving
  GC rewrites the tagged ptr in place). That IS V8's handle indirection. Global/Persistent = natural
  fit; HandleScope = watermark pop; EscapableHandleScope::Escape = move slot to parent.
- BLOCKERS: (1) IDENTITY - JSI hands out no raw ptr; two handles to same JS object differ. Every
  V8 Value*/Object* identity/hash/Map/Set site must reroute to strictEquals OR canonicalize (intern
  one slot per object). Deepest, most invasive. (2) per-Local alloc + atomic refcount cost on hot
  paths. (3) C++-only boundary = most complex backend.
- Build: CMake+Ninja, vendors llvh (NO external LLVM), Intl OFF by default. Size ~8MB app contrib
  (> quickjs ~1MB, < jsc ~12MB). Linux/macOS first-class. PRIOR ART: rust-hermes/rusty_hermes +
  libhermes-sys build Hermes from source "following the rusty_v8 pattern" - use as reference/dep.
- AOT: HBC real+shipping (hermesc -emit-binary; prepareJavaScript sniffs source-vs-HBC magic;
  isHermesBytecode()). Static Hermes (shermes, native via lowering to C) = research branch, NOT
  shipping, needs types. RN 0.84 default = Hermes V1 = bytecode+small arm64 JIT, still not native.

## DECISION (end of C0)
Two overnight tracks:
- TRACK A (Hermes spike): C1 scaffold engine_hermes (link with stubs) -> C2 get static Hermes lib
  (reuse rusty_hermes/libhermes-sys machinery) -> C3 minimal C++ JSI shim for the 9-symbol hello
  world (isolate->context->run script->read string) = the feasibility proof -> later de-risk identity.
- TRACK B (AOT/snapshot): E5 QuickJS bytecode-boot experiment (IN-REPO, existing engine) - measure
  boot-from-bytecode vs boot-from-source for bootstrap-shaped JS; tests whether native-builtins+AOT
  makes snapshot unnecessary. Independent of Hermes.

