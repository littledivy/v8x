# gen/ — pre-generated rusty_v8 bindings

Unmodified upstream release assets from
https://github.com/denoland/rusty_v8/releases/tag/v149.4.0 (the bindgen output
rusty_v8's `binding.rs` includes via `RUSTY_V8_SRC_BINDING_PATH`; build.rs
picks one per target). The C ABI symbols these declare are *defined* by our
engine shims — only the declarations come from here.

One file per OS family is enough: upstream publishes per-arch,
debug/release, and simdutf variants, but within one OS family they are all
byte-identical (verified by diffing the v149.4.0 assets). What actually varies
across OS families:

- mangled C++ `link_name`s: Itanium with a leading underscore on Apple,
  plain Itanium on Linux (and other ELF targets), MSVC mangling on Windows;
- enum repr types: `c_int` on MSVC, `c_uint` elsewhere.

| file | serves |
|---|---|
| `src_binding_debug_aarch64-apple-darwin.rs` | all Apple targets |
| `src_binding_release_x86_64-pc-windows-msvc.rs` | all windows-msvc targets (upstream ships no debug variant for Windows; debug and release are identical on every target that has both) |
| `src_binding_debug_x86_64-unknown-linux-gnu.rs` | everything else (linux-gnu/musl, BSDs, android, windows-gnu) |

The Windows file is normalized to LF line endings; otherwise, to re-verify a
file, download the same-named asset and diff.

Bumping the rusty_v8 pin: replace these with the new release's assets (one
per OS family) and re-check the identical-within-family claim.
