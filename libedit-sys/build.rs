use std::env;
use std::path::PathBuf;

fn main() {
    // docs.rs builds in a sandbox without system libraries. Skip link
    // directives — rustdoc doesn't actually link a binary.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    // Support non-standard install locations via environment variables.
    //   LIBEDIT_INCLUDE_DIR  -- path containing histedit.h
    //   LIBEDIT_LIB_DIR      -- path containing libedit.so / libedit.dylib / edit.lib
    //
    // If unset, we rely on the default system include/link paths, which works
    // out of the box on macOS (system or Homebrew), Debian/Ubuntu, Fedora, etc.
    if let Some(inc) = env::var_os("LIBEDIT_INCLUDE_DIR") {
        let path = PathBuf::from(inc);
        // Tell libclang (via bindgen) where to find <histedit.h>.
        println!("cargo:BINDGEN_EXTRA_CLANG_ARGS=-I{}", path.display());
    }
    if let Some(lib) = env::var_os("LIBEDIT_LIB_DIR") {
        println!(
            "cargo:rustc-link-search=native={}",
            PathBuf::from(lib).display()
        );
    }

    // Tell cargo to link against libedit.
    if cfg!(feature = "static") {
        println!("cargo:rustc-link-lib=static=edit");
        // libedit depends on a termcap/curses implementation for terminal
        // capabilities. Link it dynamically unless the consumer provides a
        // static libncurses.a in LIBEDIT_LIB_DIR.
        println!("cargo:rustc-link-lib=ncurses");
    } else {
        println!("cargo:rustc-link-lib=edit");
    }

    // Generate bindings only when the `bindgen` feature is enabled.
    generate_bindings();
}

#[cfg(feature = "bindgen")]
fn generate_bindings() {
    let builder = bindgen_dep::Builder::default()
        .header_contents("wrapper.h", "#include <histedit.h>")
        .allowlist_function("el_.*")
        .allowlist_function("history_.*")
        .allowlist_function("history")
        .allowlist_function("tok_.*")
        .allowlist_var("EL_.*")
        .allowlist_var("H_.*")
        .allowlist_var("CC_.*")
        .allowlist_type("EditLine")
        .allowlist_type("History")
        .allowlist_type("Tokenizer")
        .allowlist_type("HistEvent")
        .allowlist_type("LineInfo")
        .allowlist_type("LineInfoW")
        .allowlist_type("el_.*")
        .allowlist_type("hist.*")
        .allowlist_type("HistEventW")
        // `FILE` is only used behind a pointer (`*mut FILE`). Block bindgen's
        // platform-specific sized definition and supply our own zero-sized
        // opaque struct, so the type is identical on every target and cannot
        // be misused by value or via `size_of`.
        .blocklist_type("FILE")
        .raw_line("#[repr(C)]")
        .raw_line("#[derive(Debug, Copy, Clone)]")
        .raw_line("pub struct FILE { _unused: [u8; 0] }")
        .derive_debug(true)
        .derive_default(true)
        .generate_comments(false)
        .layout_tests(false);

    let bindings = builder.generate().expect(
        "Unable to generate libedit bindings.\n\
                 Make sure libedit-dev (or equivalent) is installed.\n\
                 For non-standard paths, set LIBEDIT_INCLUDE_DIR and LIBEDIT_LIB_DIR.",
    );

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

#[cfg(not(feature = "bindgen"))]
fn generate_bindings() {
    // Pre-generated bindings are used -- nothing to do.
    // The lib.rs includes src/bindings.rs directly.
}
