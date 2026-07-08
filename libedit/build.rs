use std::env;
use std::path::PathBuf;

fn main() {
    // Support non-standard install locations via LIBEDIT_INCLUDE_DIR,
    // consistent with the libedit-sys build.rs.
    let mut cc = cc::Build::new();
    cc.file("src/shim.c");

    if let Some(inc) = env::var_os("LIBEDIT_INCLUDE_DIR") {
        cc.include(PathBuf::from(inc));
    }

    cc.compile("edit_shim");

    // Re-run if inputs change.
    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-env-changed=LIBEDIT_INCLUDE_DIR");
}
