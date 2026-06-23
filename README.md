`v8x` makes rusty_v8 engine agnostic.

```diff
-v8 = "0.155.0"
+v8x = { version = "0.155.0", features = ["jsc"] }
```

Support engines:
* JavaScriptCore
* QuickJS-NG

Deno on JSC: ✅ runs — `deno eval`/`run`, `Deno.serve` HTTP, TypeScript, Web Crypto, top-level await. HTTP throughput on par with stock V8 deno; startup slower (JSC has no V8-style heap snapshot). System framework or vendored WebKit (`vendor_jsc`).

Deno on QuickJS-NG: 🚧 in progress — engine linked, `1 + 1` evaluates through the real v8 API; C-ABI shims being ported from [denoland/deno#34033](https://github.com/denoland/deno/pull/34033).
