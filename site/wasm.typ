#import "./shim/html.typ": *

#set document(
  title: "webassembly · v8x",
  description: "quickjs-ng has no Wasm surface, so v8x embeds WAMR behind V8's WebAssembly object API.",
)

#show: html-shim

#crumb(6, [webassembly])

= WebAssembly

The ABI reaches V8's WebAssembly objects too. `deno_core` expects
constructors and accessors for modules, instances, memories, tables, and
globals. quickjs-ng has no WebAssembly surface, so the QuickJS backend
embeds #link("https://github.com/bytecodealliance/wasm-micro-runtime")[WAMR]
and wraps its native objects in JavaScript objects. This is a second
ownership boundary:

```text
JS wrapper ──retains──▶ native WAMR object     (until engine collects wrapper)
wasm import call ──▶ QuickJS callback ──▶ same Rust trampoline as JS fns
memory.grow ──▶ JS ArrayBuffer view re-synced to new linear memory
```

Three rules keep it sound:

+ a wrapper must outlive its native object, never the reverse
+ imported functions reuse the #link("callbacks")[callback trampoline], so
  host calls behave the same from JavaScript and from Wasm
+ reference values crossing tables and globals keep their QuickJS
  ownership count

#next("gaps", [Closing gaps: forward, adapt, normalize, patch])
