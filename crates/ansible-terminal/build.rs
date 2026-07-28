use std::env;
use std::path::PathBuf;

/// Locate the libghostty-vt install tree.
///
/// Order: explicit override, then the vendored tree that
/// `scripts/build-libghostty-vt.sh` produces.
fn locate() -> Option<PathBuf> {
    if let Ok(dir) = env::var("LIBGHOSTTY_VT_DIR") {
        return Some(PathBuf::from(dir));
    }
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/libghostty-vt")
        .canonicalize()
        .ok()?;
    vendored.join("include/ghostty/vt.h").exists().then_some(vendored)
}

fn main() {
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_DIR");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(have_libghostty_vt)");

    let Some(prefix) = locate() else {
        // Not an error: the contract, snapshot, and event types build and test
        // without the native library, so `cargo test` stays useful before
        // anyone has run the vendor script.
        println!(
            "cargo:warning=libghostty-vt not found; building without the native backend. \
             Run scripts/build-libghostty-vt.sh or set LIBGHOSTTY_VT_DIR."
        );
        return;
    };

    let include = prefix.join("include");
    let lib = prefix.join("lib");
    println!("cargo:rerun-if-changed={}", include.join("ghostty/vt.h").display());

    // Prefer the static archive so the harness needs no LD_LIBRARY_PATH.
    if lib.join("libghostty-vt.a").exists() {
        println!("cargo:rustc-link-search=native={}", lib.display());
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    } else {
        println!("cargo:rustc-link-search=native={}", lib.display());
        println!("cargo:rustc-link-lib=dylib=ghostty-vt");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    }

    let bindings = bindgen::Builder::default()
        .header(include.join("ghostty/vt.h").to_string_lossy())
        .clang_arg(format!("-I{}", include.display()))
        .allowlist_item("[Gg]hostty.*")
        .allowlist_item("GHOSTTY_.*")
        // The upstream enums are `enum : <int type>` and already sized, so
        // newtypes keep the exact ABI. `is_global` puts the variants at module
        // scope, matching how the C names read at the call site.
        .default_enum_style(bindgen::EnumVariation::NewType { is_bitfield: false, is_global: true })
        .derive_default(true)
        .derive_debug(true)
        .prepend_enum_name(false)
        .layout_tests(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("failed to generate libghostty-vt bindings");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    bindings.write_to_file(out.join("libghostty_vt.rs")).expect("write bindings");

    println!("cargo:rustc-cfg=have_libghostty_vt");
}
