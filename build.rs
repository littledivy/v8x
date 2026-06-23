use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

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
        build_quickjs(&manifest_dir);
    }
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

    // Build only if the libraries aren't already present.
    if !lib_dir.join("libJavaScriptCore.a").exists()
        && env::var_os("JSC_VENDOR_BUILD_DIR").is_none()
    {
        let status = std::process::Command::new(webkit.join("Tools/Scripts/build-jsc"))
            .args(["--jsc-only", "--release"])
            .current_dir(&webkit)
            .status();
        match status {
            Ok(s) if s.success() => {}
            other => panic!("WebKit JSCOnly build failed: {other:?}"),
        }
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // JSCOnly static libs (link order matters: JSC -> WTF -> bmalloc).
    println!("cargo:rustc-link-lib=static=JavaScriptCore");
    println!("cargo:rustc-link-lib=static=WTF");
    println!("cargo:rustc-link-lib=static=bmalloc");
    println!("cargo:rustc-link-lib=c++");
    #[cfg(target_os = "macos")]
    {
        // ICU + the CoreFoundation/Security frameworks WTF/JSC depend on.
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
    println!("cargo:rerun-if-changed={}", lib_dir.display());
}

#[allow(dead_code)]
fn build_quickjs(manifest_dir: &std::path::Path) {
    // Honor a prebuilt tree first.
    if let Some(dir) = env::var_os("QUICKJS_NG_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", PathBuf::from(dir).display());
        println!("cargo:rustc-link-lib=static=quickjs");
        return;
    }
    let qjs = manifest_dir.join("vendor/quickjs-ng");
    // The four core sources matching upstream CMake `qjs_sources`.
    let sources = ["quickjs.c", "libregexp.c", "libunicode.c", "cutils.c", "dtoa.c"];
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
        .flag_if_supported("-Wno-implicit-fallthrough")
        .flag_if_supported("-Wno-sign-compare")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .opt_level(2)
        .compile("quickjs");
    println!("cargo:rerun-if-changed={}", qjs.display());
}
