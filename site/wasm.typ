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
ownership boundary, and three rules keep it sound:

+ a JavaScript wrapper retains its native WAMR object until the engine
  collects the wrapper, never the reverse
+ imported functions cross from Wasm into a QuickJS callback and then the
  same #link("callbacks")[trampoline] as ordinary functions, so host calls
  behave the same from JavaScript and from Wasm
+ when linear memory grows, the JavaScript `ArrayBuffer` view is re-synced
  to the new memory; reference values crossing tables and globals keep
  their QuickJS ownership count

#next("gaps", [Closing gaps: forward, adapt, normalize, patch])
