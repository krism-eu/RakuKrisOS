use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let lib = pkg_config::Config::new().probe("libsolv").expect("libsolv not found via pkg-config (dnf install libsolv-devel / equivalent)");
    // `pool_parserpmrichdep` (rich/boolean dependency parsing, e.g. `(A if
    // B)`) lives in libsolvext, not libsolv itself.
    let ext_lib = pkg_config::Config::new().probe("libsolvext").expect("libsolvext not found via pkg-config (dnf install libsolv-devel / equivalent)");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // By default we use the committed, pre-generated `bindings.rs` in
    // `generated/` instead of running bindgen at build time: bindgen needs
    // libclang (clang-devel) available, and on EL10 the current clang-devel
    // pulls a newer llvm-libs than the distro's own `rust` package is built
    // against, making the two BuildRequires mutually uninstallable.
    // libsolv's API surface here is small and stable, so a
    // regenerate-on-demand vendored copy (via the `regen-bindings` feature)
    // is both more portable and avoids a build-time libclang dependency.
    #[cfg(feature = "regen-bindings")]
    regen_bindings(&lib, &out_dir);
    #[cfg(not(feature = "regen-bindings"))]
    {
        let _ = &lib;
        fs::copy(manifest_dir.join("generated/bindings.rs"), out_dir.join("bindings.rs")).expect("failed to copy vendored bindings.rs");
    }

    // `generated/static_fns.c` is hand-maintained, NOT bindgen output: it
    // wraps just the `static inline` libsolv functions we actually call
    // (pool_id2solvable, queue_push2 — see sys.rs) in non-inline
    // `<name>__extern` symbols, since static inline functions have no
    // linkable symbol in libsolv.so itself. We deliberately don't let
    // bindgen's wrap_static_fns auto-generate this: it dumps a wrapper for
    // every static inline function libsolv's headers declare, even ones we
    // never call, and that set isn't part of libsolv's stable ABI — EL10
    // ships libsolv 0.7.33, whose hash.h doesn't declare `allochashtable`
    // (etc.) that a newer Fedora libsolv does, so a full vendored dump
    // failed to compile there with "implicit declaration of function
    // 'allochashtable'" even though we never call it.
    println!("cargo:rerun-if-changed=generated/static_fns.c");
    let mut build = cc::Build::new();
    build.file(manifest_dir.join("generated/static_fns.c"));
    build.include(&manifest_dir);
    for path in &lib.include_paths {
        build.include(path);
    }
    build.compile("rum_solv_inline_shims");

    for path in lib.link_paths.iter().chain(&ext_lib.link_paths) {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    // libsolvext must come first: it depends on libsolv's symbols.
    for name in ext_lib.libs.iter().chain(&lib.libs) {
        println!("cargo:rustc-link-lib={name}");
    }
}

#[cfg(feature = "regen-bindings")]
fn regen_bindings(lib: &pkg_config::Library, out_dir: &PathBuf) {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // bindgen has a long-standing limitation with libsolv's
    // `struct s_Foo;` (forward decl in pooltypes.h) + later
    // `typedef struct s_Foo { ... } Foo;` (full def in e.g. solvable.h)
    // idiom: it emits an opaque `{ _address: u8 }` stub instead of the
    // real fields, even though clang's own AST has the complete type.
    // Pool/Repo/Repodata/Solver/Stringpool/Repokey/KeyValue are all used
    // exclusively through libsolv's real (non-inline, linkable) accessor
    // functions, so staying opaque for those is fine. `Solvable` is the
    // one type C callers (and we) access fields on directly (name, arch,
    // evr, repo), so we blocklist bindgen's broken version of it here and
    // hand-write the struct in `sys.rs` instead — its layout is small,
    // stable, and part of libsolv's public ABI.
    //
    // wrap_static_fns is deliberately off: pool_id2solvable/queue_push2
    // (the only static inline functions we call) are hand-wrapped and
    // hand-declared in sys.rs/generated/static_fns.c instead, so this
    // step only needs to refresh the ordinary extern-fn/type bindings.
    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .layout_tests(false)
        .blocklist_type("s_Solvable")
        .blocklist_type("Solvable")
        .blocklist_function("pool_id2solvable")
        .blocklist_function("queue_push2")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for path in &lib.include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    let bindings = builder.generate().expect("failed to generate libsolv bindings");
    bindings.write_to_file(out_dir.join("bindings.rs")).expect("failed to write bindings.rs");

    fs::copy(out_dir.join("bindings.rs"), manifest_dir.join("generated/bindings.rs")).expect("failed to update vendored generated/bindings.rs");
}
