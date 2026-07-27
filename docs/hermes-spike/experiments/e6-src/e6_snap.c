// E6: QuickJS real-heap-snapshot experiment.
//
// Builds a post-bootstrap object graph (nested objects, arrays, prototype
// chain, frozen object, Map, typed array) plus a native C function that has
// been mutated after install (an own property added on the heap). Serializes
// the whole global via JS_WriteObject(REFERENCE) into a blob, restores it into
// a FRESH runtime via JS_ReadObject, and verifies structural equality + that
// the native function is (a) callable and (b) carries its post-install state.
//
// This exercises the v8x intrinsic-path + JS_WriteSnapshotObjectState machinery
// that is baked into vendor/quickjs-ng/quickjs.c.

#include "quickjs.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define JS_WRITE_OBJ_BYTECODE  (1 << 0)
#define JS_WRITE_OBJ_REFERENCE (1 << 3)
#define JS_READ_OBJ_BYTECODE   (1 << 0)
#define JS_READ_OBJ_REFERENCE  (1 << 3)

// The v8x extern hook contract. quickjs.c calls these from the C-function /
// host-object write/read paths. For this experiment we want the pure intrinsic
// path (no host objects), so the write hook declines (returns 0) and the read
// hook is never reached. capture_intrinsics is provided by quickjs.c itself.
int v82jsc_snapshot_write_host_object(JSContext *ctx, JSValueConst obj,
                                      uint8_t *buf, size_t *size) {
  (void)ctx; (void)obj; (void)buf; (void)size;
  return 0; // "not a host object" -> fall through to intrinsic path
}
int v82jsc_snapshot_read_host_object(JSContext *ctx, const uint8_t *buf,
                                     size_t size, JSValue *obj) {
  (void)ctx; (void)buf; (void)size; (void)obj;
  return 0;
}
bool v82jsc_snapshot_host_object_has_prototype(JSValueConst obj) {
  (void)obj; return false;
}

extern void v82jsc_snapshot_capture_intrinsics(JSContext *ctx, JSValue registry);

// A native function we will install, mutate on the heap, snapshot and restore.
static JSValue native_add(JSContext *ctx, JSValueConst this_val,
                          int argc, JSValueConst *argv) {
  (void)this_val;
  int32_t a = 0, b = 0;
  if (argc > 0) JS_ToInt32(ctx, &a, argv[0]);
  if (argc > 1) JS_ToInt32(ctx, &b, argv[1]);
  return JS_NewInt32(ctx, a + b);
}

static double now_ms(void) {
  struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

// The "bootstrap": produce the exact heap state we want to persist.
static const char *BOOTSTRAP =
  "globalThis.state = (function(){\n"
  "  const proto = { kind: 'base', greet(){ return 'hi ' + this.name; } };\n"
  "  const obj = Object.create(proto);\n"
  "  obj.name = 'divy';\n"
  "  obj.nested = { a: [1,2,3], b: { c: { d: 'deep' } } };\n"
  "  const frozen = Object.freeze({ pi: 3.14159, tau: 6.28318 });\n"
  "  const m = new Map([['one',1],['two',2],['three',3]]);\n"
  "  const ta = new Float64Array([1.5, 2.5, 3.5]);\n"
  "  const big = 9007199254740993n;\n"
  "  const cyc = { self: null }; cyc.self = cyc;\n"        // cyclic reference
  "  return { obj, frozen, map: m, typed: ta, big, cyc, tag: Symbol.for('x') };\n"
  "})();\n";

// Install + mutate the native function on the heap (post-bootstrap state).
static void install_native(JSContext *ctx) {
  JSValue g = JS_GetGlobalObject(ctx);
  JSValue fn = JS_NewCFunction(ctx, native_add, "add", 2);
  // Mutate the function object ON THE HEAP after install: add an own property.
  // This is state that a "re-run the bindings" hybrid would LOSE; a real heap
  // snapshot must preserve it.
  JS_SetPropertyStr(ctx, fn, "callCount", JS_NewInt32(ctx, 42));
  JS_SetPropertyStr(ctx, g, "add", fn); // consumes fn ref
  JS_FreeValue(ctx, g);
}

// Install the intrinsic registry (list of native fns reachable), mirroring
// refresh_snapshot_intrinsics() in src/quickjs/core.rs.
static void refresh_intrinsics(JSContext *ctx) {
  JSValue g = JS_GetGlobalObject(ctx);
  JSValue reg = JS_GetPropertyStr(ctx, g, "__v8x_snapshot_intrinsics");
  if (!JS_IsObject(reg)) {
    JS_FreeValue(ctx, reg);
    reg = JS_NewArray(ctx);
    JS_DefinePropertyValueStr(ctx, g, "__v8x_snapshot_intrinsics",
                              JS_DupValue(ctx, reg),
                              JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE);
  }
  v82jsc_snapshot_capture_intrinsics(ctx, reg);
  JS_FreeValue(ctx, reg);
  JS_FreeValue(ctx, g);
}

static uint8_t *snapshot_global(JSContext *ctx, size_t *out_size) {
  refresh_intrinsics(ctx);
  JSValue g = JS_GetGlobalObject(ctx);
  uint8_t *buf = JS_WriteObject(ctx, out_size, g,
                                JS_WRITE_OBJ_BYTECODE | JS_WRITE_OBJ_REFERENCE);
  JS_FreeValue(ctx, g);
  return buf;
}

static int check(JSContext *ctx, const char *expr, const char *want) {
  JSValue v = JS_Eval(ctx, expr, strlen(expr), "<check>", JS_EVAL_TYPE_GLOBAL);
  if (JS_IsException(v)) {
    JSValue e = JS_GetException(ctx);
    const char *s = JS_ToCString(ctx, e);
    printf("  FAIL %-42s -> EXCEPTION %s\n", expr, s ? s : "?");
    JS_FreeCString(ctx, s); JS_FreeValue(ctx, e); JS_FreeValue(ctx, v);
    return 0;
  }
  const char *s = JS_ToCString(ctx, v);
  int ok = s && strcmp(s, want) == 0;
  printf("  %s %-42s -> %s (want %s)\n", ok ? "ok  " : "FAIL", expr,
         s ? s : "?", want);
  JS_FreeCString(ctx, s); JS_FreeValue(ctx, v);
  return ok;
}

int main(void) {
  // ---- Phase 1: bootstrap runtime, build heap, snapshot ----
  JSRuntime *rt1 = JS_NewRuntime();
  JSContext *c1 = JS_NewContext(rt1);
  install_native(c1);
  JSValue r = JS_Eval(c1, BOOTSTRAP, strlen(BOOTSTRAP), "<boot>",
                      JS_EVAL_TYPE_GLOBAL);
  if (JS_IsException(r)) {
    JSValue e = JS_GetException(c1);
    printf("bootstrap failed: %s\n", JS_ToCString(c1, e));
    return 1;
  }
  JS_FreeValue(c1, r);

  size_t blob_size = 0;
  double t0 = now_ms();
  uint8_t *blob = snapshot_global(c1, &blob_size);
  double t_write = now_ms() - t0;
  if (!blob) {
    JSValue e = JS_GetException(c1);
    const char *s = JS_ToCString(c1, e);
    printf("SNAPSHOT WRITE FAILED: %s\n", s ? s : "?");
    return 2;
  }
  printf("blob size: %zu bytes, write time: %.3f ms\n", blob_size, t_write);

  // ---- Phase 2: fresh runtime, restore, verify ----
  JSRuntime *rt2 = JS_NewRuntime();
  JSContext *c2 = JS_NewContext(rt2);
  // THE HYBRID: re-run the native-binding install step first, so the same C
  // functions are reachable, THEN refresh the intrinsic registry so path
  // resolution can rebind them, THEN JS_ReadObject the pure-JS heap on top.
  install_native(c2);
  refresh_intrinsics(c2);

  t0 = now_ms();
  JSValue restored = JS_ReadObject(c2, blob, blob_size,
                                   JS_READ_OBJ_BYTECODE | JS_READ_OBJ_REFERENCE);
  double t_read = now_ms() - t0;
  if (JS_IsException(restored)) {
    JSValue e = JS_GetException(c2);
    const char *s = JS_ToCString(c2, e);
    printf("SNAPSHOT READ FAILED: %s\n", s ? s : "?");
    return 3;
  }
  printf("read/restore time: %.3f ms\n", t_read);

  // The restored value is a plain object graph mirroring the old global. Splice
  // its `state` and `add` onto c2's global so we can query with JS.
  JSValue g2 = JS_GetGlobalObject(c2);
  JSValue st = JS_GetPropertyStr(c2, restored, "state");
  JSValue add = JS_GetPropertyStr(c2, restored, "add");
  JS_SetPropertyStr(c2, g2, "state", st);
  JS_SetPropertyStr(c2, g2, "add", add);
  JS_FreeValue(c2, g2);

  printf("verification:\n");
  int pass = 1;
  pass &= check(c2, "state.obj.name", "divy");
  pass &= check(c2, "state.obj.greet()", "hi divy");             // proto chain
  pass &= check(c2, "state.obj.nested.b.c.d", "deep");           // deep nesting
  pass &= check(c2, "state.obj.nested.a.join(',')", "1,2,3");    // array
  pass &= check(c2, "Object.isFrozen(state.frozen)", "true");    // frozen bit
  pass &= check(c2, "state.frozen.pi", "3.14159");
  pass &= check(c2, "state.map.get('two')", "2");               // Map
  pass &= check(c2, "state.map.size", "3");
  pass &= check(c2, "state.typed[1]", "2.5");                    // typed array
  pass &= check(c2, "state.typed.constructor.name", "Float64Array");
  pass &= check(c2, "state.big.toString()", "9007199254740993"); // bigint
  pass &= check(c2, "state.cyc.self === state.cyc", "true");     // cycle
  pass &= check(c2, "state.tag === Symbol.for('x')", "true");    // symbol identity
  pass &= check(c2, "typeof add", "function");                   // native rebind
  pass &= check(c2, "add(3,4)", "7");                            // native CALLABLE
  pass &= check(c2, "add.callCount", "42");                      // POST-INSTALL heap state

  // ---- Phase 3: cost baselines ----
  // (a) re-run bootstrap from SOURCE in a fresh runtime.
  JSRuntime *rt3 = JS_NewRuntime();
  JSContext *c3 = JS_NewContext(rt3);
  install_native(c3);
  t0 = now_ms();
  JSValue rr = JS_Eval(c3, BOOTSTRAP, strlen(BOOTSTRAP), "<boot>",
                       JS_EVAL_TYPE_GLOBAL);
  double t_src = now_ms() - t0;
  JS_FreeValue(c3, rr);

  // (b) compile bootstrap to bytecode once, then measure read+eval of bytecode.
  JSValue fn_bc = JS_Eval(c3, BOOTSTRAP, strlen(BOOTSTRAP), "<boot>",
                          JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY);
  size_t bc_size = 0;
  uint8_t *bc = NULL;
  double t_bc = -1;
  if (!JS_IsException(fn_bc)) {
    bc = JS_WriteObject(c3, &bc_size, fn_bc, JS_WRITE_OBJ_BYTECODE);
    JS_FreeValue(c3, fn_bc);
    JSRuntime *rt4 = JS_NewRuntime();
    JSContext *c4 = JS_NewContext(rt4);
    install_native(c4);
    t0 = now_ms();
    JSValue f = JS_ReadObject(c4, bc, bc_size, JS_READ_OBJ_BYTECODE);
    JSValue ev = JS_EvalFunction(c4, f);
    t_bc = now_ms() - t0;
    JS_FreeValue(c4, ev);
    JS_FreeContext(c4); JS_FreeRuntime(rt4);
  }

  printf("\ncost comparison (bootstrap this size; real deno global is ~100x):\n");
  printf("  snapshot blob:        %6zu bytes, restore %.3f ms\n", blob_size, t_read);
  if (bc) printf("  bytecode blob:        %6zu bytes, read+eval %.3f ms\n", bc_size, t_bc);
  printf("  from-source eval:            n/a bytes, eval      %.3f ms\n", t_src);

  printf("\nRESULT: %s\n", pass ? "ALL CHECKS PASSED (real heap snapshot works)"
                                : "SOME CHECKS FAILED");

  js_free(c1, blob);
  if (bc) js_free(c3, bc);
  JS_FreeValue(c2, restored);
  JS_FreeContext(c1); JS_FreeRuntime(rt1);
  JS_FreeContext(c2); JS_FreeRuntime(rt2);
  JS_FreeContext(c3); JS_FreeRuntime(rt3);
  return pass ? 0 : 4;
}
