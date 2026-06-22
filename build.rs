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

    // Link Apple's system JavaScriptCore. Our shim translates the v8__* C ABI
    // onto its API.
    #[cfg(target_os = "macos")]
    {
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
}
