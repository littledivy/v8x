`v8x` makes rusty_v8 engine agnostic.

```diff
-v8 = "0.155.0"
+v8x = { version = "0.155.0", features = ["jsc"] }
```

Support engines:
* JavaScriptCore
* QuickJS-NG

Swap the engine under Deno without touching `deno_core`:

```toml
# deno's workspace Cargo.toml
[patch.crates-io]
v8 = { package = "v8x", features = ["jsc"] }
```

```diff
- cargo build -p deno
+ cargo build -p deno --features hmr
```

`v8x` vendors the real `v8` crate's Rust source and implements the `v8__*` C ABI
on the chosen engine, so the swap is a drop-in — `deno_core` compiles unchanged.
