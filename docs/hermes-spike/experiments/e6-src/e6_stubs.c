// Minimal stubs for the v8x extern hooks that vendored quickjs.c references but
// that this standalone experiment does not exercise. Signatures only need to
// satisfy the linker; these code paths are not hit by the snapshot test.
#include "quickjs.h"
#include <stdint.h>
#include <stddef.h>

JSValue v82jsc_throw_type_error_not_a_function_at(JSContext *ctx, JSValueConst v) {
  (void)v; return JS_ThrowTypeError(ctx, "not a function");
}
int  v82jsc_locale_compare_utf32(JSContext *ctx, const uint32_t *a, size_t na,
                                 const uint32_t *b, size_t nb) {
  (void)ctx;(void)a;(void)na;(void)b;(void)nb; return 0;
}
void v82jsc_debugger_statement(JSContext *ctx) { (void)ctx; }
void v82jsc_coverage_hit(JSContext *ctx, int i){(void)ctx;(void)i;}
void v82jsc_coverage_function(JSContext *ctx, int i){(void)ctx;(void)i;}
void v82jsc_coverage_function_hit(JSContext *ctx, int i){(void)ctx;(void)i;}
void v82jsc_coverage_location(JSContext *ctx, int i){(void)ctx;(void)i;}
void v82jsc_coverage_range(JSContext *ctx, int i){(void)ctx;(void)i;}
void v82jsc_coverage_range_hit(JSContext *ctx, int i){(void)ctx;(void)i;}
