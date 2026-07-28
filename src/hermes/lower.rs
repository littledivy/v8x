//! E1: async-generator source-to-source lowering for the Hermes backend.
//!
//! Hermes' compiler accepts every primitive an async generator downlevels to
//! (regular `function*`, `async`/`await`, `Symbol.asyncIterator`, `yield*`, and
//! native `for await` consumption) but rejects exactly ONE construct at parse
//! time: the async-generator *declaration* syntax. Both spellings fail with
//! `error: async generators are unsupported`:
//!
//! ```text
//!   async function* ag() { ... }        // async generator function
//!   const o = { async *m() { ... } };   // async generator method (object/class)
//! ```
//!
//! deno_core / the full Deno runtime declare async generators; without lowering,
//! any source containing one is unparseable by Hermes and the whole compile unit
//! is lost. This module rewrites those declarations into the standard ES2017
//! downlevel (a regular `function*` wrapped by runtime helpers, `await x` ->
//! `yield _awaitAsyncGenerator(x)`, `yield x` -> `yield yield ...`, `yield* it`
//! -> `yield* _asyncGeneratorDelegate(_asyncIterator(it))`), then PREPENDS the
//! helper definitions once per unit. `for await` is left native (Hermes runs it).
//!
//! ## Why a real parser (oxc), not regex
//!
//! The body rewrite (`await`/`yield`/`yield*` inside a generator, but NOT inside
//! nested non-async-generator functions) is not safely expressible as text
//! substitution. This uses `oxc`'s parser + its `es2018` async-generator
//! transform + codegen. oxc is pulled in only under `engine_hermes` (Cargo
//! feature `dep:oxc`), so no other backend pays for it.
//!
//! ## The one oxc 0.90 defect we work around
//!
//! oxc's `for await` downlevel drops the loop body when it is a single
//! *unbraced* statement (`for await (const x of it) f(x);` loses `f(x)`; it only
//! preserves `Statement::BlockStatement` bodies). Since Hermes supports
//! `for await` natively we do not *want* oxc to touch it, but oxc's async
//! generator pass and its `for await` pass are a single monolithic transform. So
//! a pre-pass wraps every non-block `for await` body in a block before the
//! transform runs, which sidesteps the defect. See the `BraceForAwait` visitor.
//!
//! ## Robustness limits (documented, honest)
//!
//! - The transform runs oxc's own ES2018 async-generator lowering, which is
//!   Babel-parity and handles function declarations, function expressions,
//!   object methods, class methods, `yield`, `yield await`, `yield*`, and
//!   `for await` in generator/async bodies. It is far more robust than the
//!   previous D7 hand-rewrite (which only removed one specific literal).
//! - It parses as a *script* by default; a source-text ES module still flows
//!   through `transform_module` in modules.rs first (which strips import/export
//!   into a closure body), and the closure source is then lowered here, so
//!   module bodies are covered by the same pass.
//! - TypeScript / JSX are not enabled (the Hermes backend only ever sees plain
//!   JS at this boundary).
//! - If oxc fails to PARSE the input (a real syntax error unrelated to async
//!   generators), we return the source unchanged and let Hermes surface the
//!   error honestly rather than masking it.

use std::borrow::Cow;

use oxc::allocator::Allocator;
use oxc::ast::AstBuilder;
use oxc::ast::ast::{ForOfStatement, Statement};
use oxc::ast_visit::{walk_mut::walk_for_of_statement, VisitMut};
use oxc::codegen::Codegen;
use oxc::parser::Parser;
use oxc::span::{GetSpan, SourceType};
use oxc::transformer::{HelperLoaderMode, TransformOptions, Transformer};
use std::path::Path;

/// The `babelHelpers` runtime the lowered code calls (oxc External helper mode
/// emits `babelHelpers.wrapAsyncGenerator(...)` etc). These are the canonical
/// `@oxc-project/runtime` / Babel helper implementations, inlined into one
/// self-contained object so the compiled unit needs no module import. Only the
/// four helpers oxc's async-generator + for-await downlevel references are
/// included: `awaitAsyncGenerator`, `wrapAsyncGenerator`, `asyncIterator`,
/// `asyncGeneratorDelegate`. Prepended at most once per compile unit (idempotent
/// via the `babelHelpers` guard `if` below).
const BABEL_HELPERS: &str = r#";if (typeof globalThis.babelHelpers === "undefined") { globalThis.babelHelpers = (function () {
  function OverloadYield(e, d) { this.v = e; this.k = d; }
  function _awaitAsyncGenerator(e) { return new OverloadYield(e, 0); }
  function _wrapAsyncGenerator(e) {
    var f = function () { return new AsyncGenerator(e.apply(this, arguments)); };
    // Intrinsic identity: under real V8, reflecting the prototype of an
    // async-generator function yields %AsyncGeneratorFunction.prototype%
    // (a.k.a. %AsyncGenerator%), whose `.prototype` is %AsyncGeneratorPrototype%
    // (next/return/throw/asyncIterator). deno_core's ext:core/00_primordials.js
    // captures exactly that and then reads `.prototype` off it, so give every
    // lowered async-generator function that prototype chain instead of the bare
    // Function.prototype.
    Object.setPrototypeOf(f, AsyncGeneratorFunctionPrototype);
    return f;
  }
  function AsyncGenerator(e) {
    var r, t;
    function resume(r2, t2) {
      try {
        var n = e[r2](t2), o = n.value, u = o instanceof OverloadYield;
        Promise.resolve(u ? o.v : o).then(function (t3) {
          if (u) {
            var i = "return" === r2 ? "return" : "next";
            if (!o.k || t3.done) return resume(i, t3);
            t3 = e[i](t3).value;
          }
          settle(n.done ? "return" : "normal", t3);
        }, function (e2) { resume("throw", e2); });
      } catch (e2) { settle("throw", e2); }
    }
    function settle(e2, n) {
      switch (e2) {
        case "return": r.resolve({ value: n, done: true }); break;
        case "throw": r.reject(n); break;
        default: r.resolve({ value: n, done: false });
      }
      (r = r.next) ? resume(r.key, r.arg) : t = null;
    }
    this._invoke = function (e2, n) {
      return new Promise(function (o, u) {
        var i = { key: e2, arg: n, resolve: o, reject: u, next: null };
        t ? t = t.next = i : (r = t = i, resume(e2, n));
      });
    };
    if ("function" != typeof e["return"]) this["return"] = undefined;
  }
  AsyncGenerator.prototype["function" == typeof Symbol && Symbol.asyncIterator || "@@asyncIterator"] = function () { return this; };
  AsyncGenerator.prototype.next = function (e) { return this._invoke("next", e); };
  AsyncGenerator.prototype["throw"] = function (e) { return this._invoke("throw", e); };
  AsyncGenerator.prototype["return"] = function (e) { return this._invoke("return", e); };
  // %AsyncGeneratorFunction.prototype% (= %AsyncGenerator%): the object that
  // reflecting the prototype of an async-generator function returns under real
  // V8. Its own `prototype` property is %AsyncGeneratorPrototype% (the instance
  // proto, AsyncGenerator.prototype above), and it carries the standard
  // `Symbol.toStringTag`. Every lowered async-generator function's [[Prototype]]
  // is set to this object (see _wrapAsyncGenerator) so primordials' identity
  // capture and its subsequent `.prototype` read both succeed.
  // Its own [[Prototype]] is %Function.prototype% (as in real V8), so a lowered
  // async-generator function whose [[Prototype]] is set to this object still
  // reaches Function.prototype.apply / call / bind through the chain (oxc emits
  // `_ag.apply(this, arguments)` on the wrapper).
  var AsyncGeneratorFunctionPrototype = Object.create(Function.prototype);
  Object.defineProperty(AsyncGeneratorFunctionPrototype, "prototype", {
    value: AsyncGenerator.prototype, writable: false, enumerable: false, configurable: true
  });
  if ("function" == typeof Symbol && Symbol.toStringTag) {
    Object.defineProperty(AsyncGeneratorFunctionPrototype, Symbol.toStringTag, {
      value: "AsyncGeneratorFunction", writable: false, enumerable: false, configurable: true
    });
  }
  // Reciprocal `constructor` link mirrors V8's %AsyncGeneratorPrototype%.constructor.
  Object.defineProperty(AsyncGenerator.prototype, "constructor", {
    value: AsyncGeneratorFunctionPrototype, writable: false, enumerable: false, configurable: true
  });
  function _asyncIterator(r) {
    var n, t, o, e = 2;
    for ("undefined" != typeof Symbol && (t = Symbol.asyncIterator, o = Symbol.iterator); e--;) {
      if (t && null != (n = r[t])) return n.call(r);
      if (o && null != (n = r[o])) return new AsyncFromSyncIterator(n.call(r));
      t = "@@asyncIterator", o = "@@iterator";
    }
    throw new TypeError("Object is not async iterable");
  }
  function AsyncFromSyncIterator(r) {
    function AsyncFromSyncIteratorContinuation(r2) {
      if (Object(r2) !== r2) return Promise.reject(new TypeError(r2 + " is not an object."));
      var n = r2.done;
      return Promise.resolve(r2.value).then(function (r3) { return { value: r3, done: n }; });
    }
    return AsyncFromSyncIterator = function (r2) { this.s = r2; this.n = r2.next; },
      AsyncFromSyncIterator.prototype = {
        s: null, n: null,
        next: function () { return AsyncFromSyncIteratorContinuation(this.n.apply(this.s, arguments)); },
        "return": function (r2) { var n = this.s["return"]; return void 0 === n ? Promise.resolve({ value: r2, done: true }) : AsyncFromSyncIteratorContinuation(n.apply(this.s, arguments)); },
        "throw": function (r2) { var n = this.s["throw"]; return void 0 === n ? Promise.reject(r2) : AsyncFromSyncIteratorContinuation(n.apply(this.s, arguments)); }
      }, new AsyncFromSyncIterator(r);
  }
  function _asyncGeneratorDelegate(t) {
    var e = {}, n = false;
    function pump(e2, r) {
      return n = true, r = new Promise(function (n2) { n2(t[e2](r)); }), { done: false, value: new OverloadYield(r, 1) };
    }
    return e["undefined" != typeof Symbol && Symbol.iterator || "@@iterator"] = function () { return this; },
      e.next = function (t2) { return n ? (n = false, t2) : pump("next", t2); },
      "function" == typeof t["throw"] && (e["throw"] = function (t2) { if (n) throw n = false, t2; return pump("throw", t2); }),
      "function" == typeof t["return"] && (e["return"] = function (t2) { return n ? (n = false, t2) : pump("return", t2); }), e;
  }
  return {
    awaitAsyncGenerator: _awaitAsyncGenerator,
    wrapAsyncGenerator: _wrapAsyncGenerator,
    asyncIterator: _asyncIterator,
    asyncGeneratorDelegate: _asyncGeneratorDelegate
  };
})(); }
"#;

/// Cheap prefilter: does `src` textually contain an async-generator declaration?
/// Returns false for the overwhelmingly common case (no async generators), so
/// those sources skip the parse+transform entirely and are returned unchanged.
///
/// The gate matches the two producer spellings Hermes rejects:
///   * `async function*` (with any whitespace between the keywords / before `*`)
///   * `async *name(` in an object/class body (an async generator *method*)
/// A false positive here only costs a redundant parse (the transform is a no-op
/// if no async generator is actually present); a false negative would leave an
/// unparseable-by-Hermes construct, so the scan is deliberately permissive.
pub fn contains_async_generator(src: &str) -> bool {
  let bytes = src.as_bytes();
  let mut i = 0usize;
  while let Some(rel) = src[i..].find("async") {
    let start = i + rel;
    // Must be a standalone `async` keyword (not e.g. `myasync`).
    let prev_ok = start == 0
      || !is_ident_char(bytes[start - 1]);
    let after = start + "async".len();
    if prev_ok {
      // Skip whitespace after `async`.
      let mut j = after;
      while j < bytes.len() && (bytes[j] as char).is_whitespace() {
        j += 1;
      }
      if j < bytes.len() {
        // `async function*` (optionally `async  function *`)
        if src[j..].starts_with("function") {
          let mut k = j + "function".len();
          while k < bytes.len() && (bytes[k] as char).is_whitespace() {
            k += 1;
          }
          if k < bytes.len() && bytes[k] == b'*' {
            return true;
          }
        }
        // `async *m(` — an async generator method: `async` then `*`.
        if bytes[j] == b'*' {
          return true;
        }
      }
    }
    i = after;
  }
  false
}

#[inline]
fn is_ident_char(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Pre-transform visitor: wrap every non-block `for await` body in a
/// `BlockStatement`, working around oxc 0.90's for-await downlevel dropping
/// unbraced single-statement bodies. Non-await `for-of` is left untouched.
struct BraceForAwait<'a> {
  ast: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for BraceForAwait<'a> {
  fn visit_for_of_statement(&mut self, it: &mut ForOfStatement<'a>) {
    if it.r#await && !matches!(it.body, Statement::BlockStatement(_)) {
      let span = it.body.span();
      let placeholder =
        Statement::EmptyStatement(self.ast.alloc_empty_statement(span));
      let inner = std::mem::replace(&mut it.body, placeholder);
      let mut v = self.ast.vec();
      v.push(inner);
      it.body = self.ast.statement_block(span, v);
    }
    walk_for_of_statement(self, it);
  }
}

/// Lower every `async function*` / `async *method` in `src` into the ES2017
/// downlevel Hermes accepts, prepending the `babelHelpers` runtime. Returns the
/// source UNCHANGED (borrowed) when it contains no async-generator declaration
/// (the common case), or when oxc fails to parse it (a real syntax error is left
/// for Hermes to report, not masked).
pub fn lower_async_generators(src: &str) -> Cow<'_, str> {
  if !contains_async_generator(src) {
    return Cow::Borrowed(src);
  }

  let allocator = Allocator::default();
  let source_type = SourceType::default(); // plain JS, script
  let ret = Parser::new(&allocator, src, source_type).parse();
  if ret.panicked || !ret.errors.is_empty() {
    // A genuine parse error unrelated to async generators (or a parser panic):
    // do not mask it. Return unchanged and let Hermes surface the real error.
    return Cow::Borrowed(src);
  }
  let mut program = ret.program;

  // Work around the oxc for-await unbraced-body defect before transforming.
  {
    let ast = AstBuilder::new(&allocator);
    let mut norm = BraceForAwait { ast };
    norm.visit_program(&mut program);
  }

  // Enable ONLY the ES2018 async-generator pass. Using `ESTarget::ES2017` here
  // would enable every downlevel below ES2017 — including the ES2022
  // class-properties / private-field pass, which rewrites `#x` fields into
  // `babelHelpers.classPrivateFieldInitSpec(...)` / `classPrivateFieldSet2(...)`
  // calls. Those helpers are NOT in our four-helper `BABEL_HELPERS` object, so
  // any class WITH a private field AND an async-generator method in the same
  // compile unit (e.g. ext/net's `01_net.js`: the `Conn`/`Listener` classes use
  // `#rid` fields and the file also has an `async *[Symbol.asyncIterator]()`)
  // hits `babelHelpers.classPrivateFieldInitSpec` === undefined at construction
  // ("undefined is not a function"). Hermes supports private fields, class
  // properties, object spread, optional chaining, etc. NATIVELY, so we must not
  // downlevel them — only the async-generator declaration syntax Hermes rejects.
  // Build all-off options and switch on the single pass we need.
  let mut options = TransformOptions::default();
  options.env.es2018.async_generator_functions = true;
  options.helper_loader.mode = HelperLoaderMode::External;

  let scoping = oxc::semantic::SemanticBuilder::new()
    .build(&program)
    .semantic
    .into_scoping();
  let _ = Transformer::new(&allocator, Path::new("hermes-lower.js"), &options)
    .build_with_scoping(scoping, &mut program);

  let transformed = Codegen::new().build(&program).code;

  let mut out = String::with_capacity(BABEL_HELPERS.len() + transformed.len());
  out.push_str(BABEL_HELPERS);
  out.push_str(&transformed);
  Cow::Owned(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn prefilter_negatives() {
    assert!(!contains_async_generator("function* g(){ yield 1; }"));
    assert!(!contains_async_generator("async function f(){ await x; }"));
    assert!(!contains_async_generator("for await (const x of it) {}"));
    assert!(!contains_async_generator("const myasyncthing = 1;"));
    assert!(!contains_async_generator("1 + 1"));
  }

  #[test]
  fn prefilter_positives() {
    assert!(contains_async_generator("async function* ag(){ yield 1; }"));
    assert!(contains_async_generator("async  function  * ag(){}"));
    assert!(contains_async_generator("const o = { async *m(){ yield 1; } };"));
    assert!(contains_async_generator("class C { async *m(){} }"));
  }

  #[test]
  fn noop_when_absent() {
    let src = "function* g(){ yield 1; } for await (const x of it) { f(x); }";
    let out = lower_async_generators(src);
    assert!(matches!(out, Cow::Borrowed(_)), "unchanged source must borrow");
    assert_eq!(out, src);
  }

  #[test]
  fn lowers_and_removes_async_generator_syntax() {
    let src = "async function* ag(){ yield 1; yield await Promise.resolve(2); }";
    let out = lower_async_generators(src);
    assert!(matches!(out, Cow::Owned(_)), "async-gen source must be rewritten");
    // The rejected declaration syntax must be gone from the output.
    assert!(!out.contains("async function*"), "async function* must be lowered away");
    assert!(out.contains("babelHelpers"), "helper prelude must be prepended");
    assert!(out.contains("wrapAsyncGenerator"), "wrap helper must be referenced");
  }

  #[test]
  fn private_fields_survive_alongside_async_generator() {
    // E7 regression: ext/net's `01_net.js` has classes (`Conn`, `Listener`)
    // whose constructors use native private fields (`#rid`) AND the file also
    // contains an `async *[Symbol.asyncIterator]()`. Targeting `ESTarget::ES2017`
    // (all downlevels <= 2017) also enabled the ES2022 class-properties pass,
    // which rewrote `#x` fields into `babelHelpers.classPrivateFieldInitSpec(...)`
    // / `classPrivateFieldSet2(...)` — helpers NOT provided by our four-helper
    // BABEL_HELPERS object. Every such class then threw "undefined is not a
    // function" at construction. Hermes supports private fields natively, so the
    // async-generator pass must be the ONLY transform: private fields stay `#x`,
    // and the ctor's own references survive intact.
    let src = r#"(function (__bootstrap) {
"use strict"; return ((function () {
  const { primordials } = __bootstrap;
  const { ObjectDefineProperty, PromiseResolve } = primordials;
  class C {
    #v = 0;
    constructor(x) {
      ObjectDefineProperty(this, "tag", { value: x });
      this.#v = x;
    }
    async *[Symbol.asyncIterator]() { yield await PromiseResolve(1); }
  }
  return { C };
})());
})"#;
    let out = lower_async_generators(src);
    // The ctor's own references must survive the round-trip untouched.
    assert!(out.contains("ObjectDefineProperty(this"), "ctor ref must survive");
    // Private fields must be left NATIVE (Hermes supports them); they must NOT
    // be downleveled into the (unprovided) classPrivateField* babel helpers.
    assert!(
      !out.contains("classPrivateField"),
      "private fields must not be downleveled into missing babel helpers"
    );
    assert!(out.contains("#v"), "native private field syntax must survive");
    // The async generator itself MUST still be lowered away.
    assert!(!out.contains("async *"), "async generator must be lowered");
    assert!(out.contains("wrapAsyncGenerator"), "AG helper must be used");
  }

  #[test]
  fn wraps_before_lowering_preserves_top_level_return() {
    // deno_core's `load_ext_script` compiles `"use strict"; return (<IIFE>);`
    // as a function BODY. The Hermes CompileFunction shim must wrap that in a
    // function before lowering, or oxc rejects the top-level `return` and the
    // async-generator syntax survives. This asserts the lowering handles the
    // wrapped shape (function body with a top-level return + async generator).
    let wrapped =
      "(function (__bootstrap) {\n\"use strict\"; return (async function* ag(){ yield 1; });\n})";
    let out = lower_async_generators(wrapped);
    assert!(matches!(out, Cow::Owned(_)), "wrapped async-gen must be rewritten");
    assert!(!out.contains("async function*"), "async function* must be lowered away");
  }
}
