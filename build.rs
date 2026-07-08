use std::env;
use std::path::PathBuf;

/// Emit `$OUT_DIR/bc_embed.rs` defining `EMBEDDED_BC`. When `V82JSC_BC_BLOB`
/// points at a packed bytecode blob (see `module.rs` for the format), its bytes
/// are `include_bytes!`'d straight into the binary so startup reads compiled
/// module bytecode from memory instead of the on-disk cache. Absent the env
/// var, `EMBEDDED_BC` is empty and the disk cache path is used as before.
fn emit_bc_embed() {
  let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
  let dst = out_dir.join("bc_embed.rs");
  println!("cargo:rerun-if-env-changed=V82JSC_BC_BLOB");
  let body = match env::var_os("V82JSC_BC_BLOB") {
    Some(p) if PathBuf::from(&p).is_file() => {
      let p = PathBuf::from(&p);
      println!("cargo:rerun-if-changed={}", p.display());
      format!(
        "pub static EMBEDDED_BC: &[u8] = include_bytes!(r\"{}\");",
        p.display()
      )
    }
    _ => "pub static EMBEDDED_BC: &[u8] = &[];".to_string(),
  };
  std::fs::write(&dst, body).unwrap();
}

fn main() {
  let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

  // Init the pinned rusty_v8 submodule + apply our 2 patches BEFORE compile:
  // src/lib.rs `#[path]`-includes its modules, so the vendored Rust API surface
  // must be materialized for either backend. (Engines are set up separately,
  // only for the QuickJS path.)
  setup_vendor(&manifest_dir, "rusty_v8");

  // The vendored crate's `binding.rs` does
  // `include!(env!("RUSTY_V8_SRC_BINDING_PATH"))` to pull in the bindgen
  // output (extern decls + SIZE consts). We point it at the pre-generated
  // bindings for this target. The C ABI symbols are *defined* by our own
  // JSC-backed shim (linked below); only the declarations come from here.
  let simdutf = env::var("CARGO_FEATURE_SIMDUTF").is_ok();
  let gen_file = if simdutf {
    "gen/src_binding_simdutf_debug_aarch64-apple-darwin.rs"
  } else {
    "gen/src_binding_debug_aarch64-apple-darwin.rs"
  };
  let binding_path = manifest_dir.join(gen_file);
  println!(
    "cargo:rustc-env=RUSTY_V8_SRC_BINDING_PATH={}",
    binding_path.display()
  );
  println!("cargo:rerun-if-changed={}", binding_path.display());

  emit_bc_embed();

  // --- JSC backend: generate full FFI bindings from the SDK header. ---
  // `src/jsc_sys.rs` `include!`s the output, so the complete JavaScriptCore
  // C API is available without hand-written externs.
  if env::var_os("CARGO_FEATURE_ENGINE_JSC").is_some() {
    generate_jsc_bindings();
  }

  // --- JSC backend: vendored WebKit JSCOnly build, or system framework ---
  if env::var_os("CARGO_FEATURE_ENGINE_JSC").is_some()
    && env::var_os("CARGO_FEATURE_VENDOR_JSC").is_some()
  {
    build_vendored_jsc(&manifest_dir);
    return;
  }

  #[cfg(target_os = "macos")]
  if env::var_os("CARGO_FEATURE_ENGINE_JSC").is_some() {
    println!("cargo:rustc-link-lib=framework=JavaScriptCore");
    // `jsc_version_string` reads the JavaScriptCore bundle's version via
    // CoreFoundation (CFBundle*/CFString*/CFRelease). Those symbols are only
    // pulled in once a test target references `v8__V8__GetVersion` (the small
    // suites dead-strip them), so link CoreFoundation explicitly or the
    // `test_api` target fails to link on the system-framework backend.
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    // lld with -nodefaultlibs doesn't search the SDK, where macOS now keeps
    // the .tbd stubs for system libs like iconv (the .dylib files were moved
    // into the dyld shared cache). Add the SDK lib dir so `-liconv` resolves.
    if let Ok(out) = std::process::Command::new("xcrun")
      .args(["--show-sdk-path"])
      .output()
    {
      if let Ok(sdk) = String::from_utf8(out.stdout) {
        let sdk = sdk.trim();
        if !sdk.is_empty() {
          println!("cargo:rustc-link-search=native={sdk}/usr/lib");
        }
      }
    }
  }

  // --- QuickJS-ng backend: compile + statically link the vendored sources ---
  if env::var_os("CARGO_FEATURE_LINK_QUICKJS").is_some() {
    // Init the pinned quickjs-ng + WAMR submodules and apply our patches
    // (idempotent). Skipped when both engines are driven from prebuilt trees.
    if env::var_os("QUICKJS_NG_LIB_DIR").is_none()
      || env::var_os("WAMR_LIB_DIR").is_none()
    {
      setup_vendor(&manifest_dir, "quickjs");
    }
    build_quickjs(&manifest_dir);
    // WebAssembly engine: build the vendored WAMR (interpreter-only) static
    // lib and link it; the WebAssembly.* JS API is implemented over its
    // wasm-c-api in src/quickjs/wasm.rs.
    build_wamr(&manifest_dir);
  }
}

/// Init the pinned quickjs-ng + WAMR submodules and apply our patch files on top
/// (see tools/setup_vendor.sh). Idempotent; runs before either engine compiles so
/// a fresh checkout builds without a manual submodule dance.
fn setup_vendor(manifest_dir: &std::path::Path, mode: &str) {
  let status = std::process::Command::new("bash")
    .arg(manifest_dir.join("tools/setup_vendor.sh"))
    .arg(mode)
    .current_dir(manifest_dir)
    .status();
  match status {
    Ok(s) if s.success() => {}
    other => panic!(
      "tools/setup_vendor.sh (submodule init + patches) failed: {other:?}"
    ),
  }
  println!("cargo:rerun-if-changed=tools/setup_vendor.sh");
  println!("cargo:rerun-if-changed=patches");
}

/// Build the vendored wasm-micro-runtime (WAMR) as an interpreter-only static
/// library via CMake and link it. Backs the QuickJS backend's `WebAssembly`.
fn build_wamr(manifest_dir: &std::path::Path) {
  if let Some(dir) = env::var_os("WAMR_LIB_DIR") {
    println!(
      "cargo:rustc-link-search=native={}",
      PathBuf::from(dir).display()
    );
    println!("cargo:rustc-link-lib=static=vmlib");
    return;
  }
  let src = manifest_dir.join("vendor/wamr/v82jsc");
  let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("wamr-build");
  // Wipe any stale cmake cache so flag changes (notably the HW-bound-check
  // disable) always take effect.
  let _ = std::fs::remove_dir_all(&out);
  std::fs::create_dir_all(&out).unwrap();
  let cmake = |args: &[&str]| {
    let status = std::process::Command::new("cmake")
      .args(args)
      .current_dir(&out)
      .status()
      .expect("cmake not found — needed to build WAMR");
    assert!(status.success(), "cmake step failed: {args:?}");
  };
  cmake(&[
    "-DCMAKE_BUILD_TYPE=Release",
    "-DCMAKE_POLICY_VERSION_MINIMUM=3.5",
    // WAMR's hardware bound-check installs SIGSEGV/SIGBUS handlers that fight
    // Rust's stack-overflow guard (instant abort). Force software checks.
    "-DWAMR_DISABLE_HW_BOUND_CHECK=1",
    "-DWAMR_DISABLE_STACK_HW_BOUND_CHECK=1",
    src.to_str().unwrap(),
  ]);
  cmake(&["--build", ".", "-j", "4"]);
  println!("cargo:rustc-link-search=native={}", out.display());
  println!("cargo:rustc-link-lib=static=vmlib");
  println!("cargo:rerun-if-changed={}", src.display());
}

/// Build JavaScriptCore from the vendored WebKit (JSCOnly port) and link it.
/// Override the build with `JSC_VENDOR_BUILD_DIR` pointing at a prebuilt
/// `WebKitBuild/JSCOnly/Release` (containing `lib/`).
fn build_vendored_jsc(manifest_dir: &std::path::Path) {
  let webkit = manifest_dir.join("vendor/webkit");
  let build_dir = env::var_os("JSC_VENDOR_BUILD_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| webkit.join("WebKitBuild/JSCOnly/Release"));
  let lib_dir = build_dir.join("lib");

  // Bundled = STATIC: the JSCOnly port with -DENABLE_STATIC_JSC=ON emits
  // libJavaScriptCore.a + libWTF.a + libbmalloc.a; we link them into the
  // binary so it's self-contained (no dylib, no rpath).
  let jsc_a = lib_dir.join("libJavaScriptCore.a");
  let prebuilt =
    jsc_a.exists() || env::var_os("JSC_VENDOR_BUILD_DIR").is_some();
  if prebuilt {
    // A PREBUILT lib archive is in place (CI downloads the WebKit static-lib
    // release — see .github/workflows/webkit-release.yml). Still apply the
    // source patches so the glue (native_modules.cpp) compiles against the
    // patched headers; skip the multi-hour build.
    let _ = &webkit;
    let status = std::process::Command::new("bash")
      .arg(manifest_dir.join("tools/setup_webkit.sh"))
      .arg("--patches-only")
      .current_dir(manifest_dir)
      .status();
    match status {
      Ok(s) if s.success() => {}
      other => panic!("tools/setup_webkit.sh --patches-only failed: {other:?}"),
    }
  } else {
    // tools/setup_webkit.sh inits the pinned submodule, applies the patches,
    // and runs the static JSCOnly build — everything needed for a fresh tree.
    let _ = &webkit;
    let status = std::process::Command::new("bash")
      .arg(manifest_dir.join("tools/setup_webkit.sh"))
      .current_dir(manifest_dir)
      .status();
    match status {
      Ok(s) if s.success() => {}
      other => {
        panic!("tools/setup_webkit.sh (WebKit JSC build) failed: {other:?}")
      }
    }
  }

  // Compile the native ES-module glue (src/jsc/native_modules.cpp) against the
  // vendored WebKit private headers and link it. Replaces the rewrite_es_module
  // string rewriter with real JSModuleRecords.
  build_native_modules_glue(manifest_dir, &webkit, &build_dir);

  println!("cargo:rustc-link-search=native={}", lib_dir.display());
  // JavaScriptCore + JavaScriptCoreJIT are split cmake targets; link both as
  // normal static libs (like the `jsc` CLI does) and repeat to satisfy their
  // cyclic references. NOTE: the offlineasm LLInt/IPInt assembly objects in
  // these archives have MH_SUBSECTIONS_VIA_SYMBOLS cleared by tools/
  // setup_webkit.sh so the deno binary's `-Wl,-dead_strip` cannot strip the
  // computed-jump-only WASM opcode handlers (else WASM runs garbage). See the
  // comment there.
  println!("cargo:rustc-link-lib=static=JavaScriptCore");
  let jit_a = lib_dir.join("libJavaScriptCoreJIT.a");
  if jit_a.exists() {
    println!("cargo:rustc-link-lib=static=JavaScriptCoreJIT");
    // second pass: JIT <-> core have cyclic references
    println!("cargo:rustc-link-lib=static=JavaScriptCore");
    println!("cargo:rustc-link-lib=static=JavaScriptCoreJIT");
  }
  println!("cargo:rustc-link-lib=static=WTF");
  println!("cargo:rustc-link-lib=static=bmalloc");
  println!("cargo:rustc-link-lib=c++");
  println!("cargo:rerun-if-changed={}", jsc_a.display());

  #[cfg(target_os = "macos")]
  {
    // ICU + the system frameworks WTF/JSC depend on.
    println!("cargo:rustc-link-lib=icucore");
    for fw in ["CoreFoundation", "Foundation", "Security"] {
      println!("cargo:rustc-link-lib=framework={fw}");
    }
    if let Ok(out) = std::process::Command::new("xcrun")
      .args(["--show-sdk-path"])
      .output()
    {
      if let Ok(sdk) = String::from_utf8(out.stdout) {
        println!("cargo:rustc-link-search=native={}/usr/lib", sdk.trim());
      }
    }
  }
}

/// Compile `src/jsc/native_modules.cpp` (the native JSModuleRecord glue) against
/// the vendored WebKit's private + derived headers, archive it, and link it. The
/// glue exposes `v82jsc_module_*` C functions the JSC module shims call.
///
/// We mirror the include set JSC's own unified sources use (extracted from
/// `compile_commands.json`): the JavaScriptCore.hmap header map resolves the
/// unprefixed parser internals (`ModuleAnalyzer.h`, `Nodes.h`, ...) that aren't
/// in PrivateHeaders. config.h is included first by the .cpp itself, so no PCH
/// is needed. Apple clang (via `xcrun`) — NOT a PATH `clang++` which may be a
/// mismatched LLVM that mishandles the SDK headers.
fn build_native_modules_glue(
  manifest_dir: &std::path::Path,
  webkit: &std::path::Path,
  build_dir: &std::path::Path,
) {
  // Glue translation units, archived together: native_modules.cpp (the
  // JSModuleRecord module system), bytecode.cpp (JSC bytecode cache), and
  // introspect.cpp (Proxy handler / Promise state / iterator preview).
  let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
  let archive = out_dir.join("libv82jsc_native_modules.a");
  let units = ["native_modules.cpp", "bytecode.cpp", "introspect.cpp"];

  let sdk = String::from_utf8(
    std::process::Command::new("xcrun")
      .args(["--show-sdk-path"])
      .output()
      .expect("xcrun --show-sdk-path failed")
      .stdout,
  )
  .expect("sdk path not utf8");
  let sdk = sdk.trim();

  let b = build_dir.to_str().unwrap();
  let inc = |p: &str| format!("-I{b}/{p}");
  let mut objs: Vec<PathBuf> = Vec::new();
  for unit in units {
    let src = manifest_dir.join("src/jsc").join(unit);
    let obj = out_dir.join(unit).with_extension("o");
    objs.push(obj.clone());
    let status = std::process::Command::new("xcrun")
      .args(["clang++", "-c"])
      .arg(&src)
      .arg("-o")
      .arg(&obj)
      .args([
        "-DBUILDING_JSCONLY__",
        "-DBUILDING_JavaScriptCore",
        "-DBUILDING_WEBKIT=1",
        "-DBUILDING_WITH_CMAKE=1",
        "-DHAVE_CONFIG_H=1",
        "-DPAS_BMALLOC=1",
        "-DSTATICALLY_LINKED_WITH_WTF",
        "-DSTATICALLY_LINKED_WITH_bmalloc",
        "-DU_DISABLE_RENAMING=1",
        "-D_LIBCPP_HARDENING_MODE=_LIBCPP_HARDENING_MODE_EXTENSIVE",
        "-DNDEBUG",
      ])
      .arg(inc("JavaScriptCore/Headers"))
      .arg(inc("JavaScriptCore/PrivateHeaders"))
      .arg(format!("-I{b}"))
      .arg(inc("HeaderMaps/JavaScriptCore.hmap"))
      .arg(inc("JavaScriptCore/PrivateHeaders/JavaScriptCore"))
      .arg(format!("-I{}/Source/JavaScriptCore", webkit.display()))
      .arg(inc("JavaScriptCore/DerivedSources"))
      .arg(inc("JavaScriptCore/DerivedSources/inspector"))
      .arg(inc("JavaScriptCore/DerivedSources/runtime"))
      .arg(inc("JavaScriptCore/DerivedSources/yarr"))
      .arg(inc("WTF/Headers"))
      .arg(inc("bmalloc/Headers"))
      .arg(inc("bmalloc/PrivateHeaders"))
      .args(["-isystem", &format!("{b}/ICU/Headers")])
      .args([
        "-std=c++2b",
        "-O3",
        "-fno-exceptions",
        "-fno-rtti",
        "-fvisibility=hidden",
        "-fvisibility-inlines-hidden",
        "-fPIC",
        "-ffp-contract=off",
        "-fno-slp-vectorize",
        "-arch",
        "arm64",
        "-Wno-everything",
      ])
      .args(["-isysroot", sdk])
      .status()
      .expect("xcrun clang++ not found — needed to build the JSC glue");
    assert!(status.success(), "{unit} compile failed");
    println!("cargo:rerun-if-changed={}", src.display());
  }

  // Archive the objects so the linker pulls them on demand (the Rust shims
  // reference the v82jsc_* symbols).
  let _ = std::fs::remove_file(&archive);
  let mut ar = std::process::Command::new("ar");
  ar.arg("crs").arg(&archive);
  for o in &objs {
    ar.arg(o);
  }
  assert!(
    ar.status().expect("ar failed").success(),
    "ar archiving failed"
  );

  println!("cargo:rustc-link-search=native={}", out_dir.display());
  println!("cargo:rustc-link-lib=static=v82jsc_native_modules");
}

/// Run bindgen over the SDK's JavaScriptCore C API umbrella header to produce
/// the complete set of declarations (`JSValueRef`, `JSEvaluateScript`,
/// `JSType`/`kJSType*`, `JSTypedArrayType`/`kJSTypedArrayType*`, ...). The
/// generated names are identical to the C names, so `src/jsc_sys.rs` just
/// `include!`s the output and the shim code keeps compiling.
fn generate_jsc_bindings() {
  let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
  let out_path = out_dir.join("jsc_bindings.rs");

  let sdk = String::from_utf8(
    std::process::Command::new("xcrun")
      .args(["--show-sdk-path"])
      .output()
      .expect("xcrun --show-sdk-path failed")
      .stdout,
  )
  .expect("sdk path not utf8");
  let sdk = sdk.trim();
  let frameworks = format!("{sdk}/System/Library/Frameworks");
  let header =
    format!("{frameworks}/JavaScriptCore.framework/Headers/JavaScript.h");

  // bindgen locates libclang via the `clang-sys` crate. On a stock Xcode
  // install it lives in the toolchain lib dir; point LIBCLANG_PATH there if
  // it isn't already set so the build works out of the box.
  if env::var_os("LIBCLANG_PATH").is_none() {
    if let Ok(out) = std::process::Command::new("xcrun")
      .args(["--find", "clang"])
      .output()
    {
      if let Ok(clang) = String::from_utf8(out.stdout) {
        // .../usr/bin/clang -> .../usr/lib
        if let Some(libdir) = PathBuf::from(clang.trim())
          .parent()
          .and_then(|p| p.parent())
          .map(|p| p.join("lib"))
        {
          if libdir.join("libclang.dylib").exists() {
            unsafe { env::set_var("LIBCLANG_PATH", &libdir) };
          }
        }
      }
    }
  }

  let bindings = bindgen::Builder::default()
    .header(&header)
    .clang_arg("-isysroot")
    .clang_arg(sdk)
    .clang_arg(format!("-F{frameworks}"))
    .allowlist_function("JS.*")
    .allowlist_type("JS.*|Opaque.*")
    .allowlist_var("kJS.*")
    .generate()
    .expect("bindgen failed to generate JavaScriptCore bindings");

  // Edition 2024 requires `extern` blocks to be `unsafe extern`. bindgen 0.70
  // still emits bare `extern "C" {`, so rewrite the block headers. (Function
  // pointer typedefs already use `unsafe extern "C" fn(...)` and are skipped
  // because the pattern below only matches a block-opening brace.)
  let src = bindings
    .to_string()
    .replace("extern \"C\" {", "unsafe extern \"C\" {");

  std::fs::write(&out_path, src).expect("failed to write jsc_bindings.rs");

  println!("cargo:rerun-if-changed={header}");
}

#[allow(dead_code)]
fn build_quickjs(manifest_dir: &std::path::Path) {
  // Honor a prebuilt tree first.
  if let Some(dir) = env::var_os("QUICKJS_NG_LIB_DIR") {
    println!(
      "cargo:rustc-link-search=native={}",
      PathBuf::from(dir).display()
    );
    println!("cargo:rustc-link-lib=static=quickjs");
    return;
  }
  let qjs = manifest_dir.join("vendor/quickjs-ng");
  // The four core sources matching upstream CMake `qjs_sources`.
  let sources = [
    "quickjs.c",
    "libregexp.c",
    "libunicode.c",
    "cutils.c",
    "dtoa.c",
  ];
  let mut build = cc::Build::new();
  build.include(&qjs);
  for s in sources {
    let p = qjs.join(s);
    if p.exists() {
      build.file(p);
    }
  }
  build
    .define("_GNU_SOURCE", None)
    // Real QuickJS ships with NDEBUG; it also drops the JS_FreeRuntime
    // gc_obj_list assert so a (temporary) refcount leak doesn't abort.
    .define("NDEBUG", None)
    .flag_if_supported("-Wno-implicit-fallthrough")
    .flag_if_supported("-Wno-sign-compare")
    .flag_if_supported("-Wno-unused-parameter")
    .flag_if_supported("-Wno-unused-but-set-variable")
    .flag_if_supported("-Wno-unused-variable")
    .opt_level(2)
    .compile("quickjs");
  println!("cargo:rerun-if-changed={}", qjs.display());
}
