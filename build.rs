//! Tell the binary where DuckDB's library is.
//!
//! The build links DuckDB's release library dynamically (see `.cargo/config.toml`), and an MCP
//! client starts the binary with no LD_LIBRARY_PATH. libduckdb-sys emits an rpath, but a build
//! script's link arguments reach only its own package, so this one records it for ours: first
//! the directory the library was linked from, then the binary's own directory, so the two files
//! can be moved together.
//!
//! A release bundle sets BENCHY_BUNDLE=1 and keeps only the binary's own directory: the library
//! ships beside it, and an rpath into the build machine's target/ would have every installed copy
//! look first in a directory that does not exist there, or that someone else could create.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=DUCKDB_LIB_DIR");
    println!("cargo:rerun-if-env-changed=BENCHY_BUNDLE");
    let bundle = std::env::var("BENCHY_BUNDLE").is_ok_and(|v| v == "1");
    let linked = std::env::var("DUCKDB_LIB_DIR")
        .or_else(|_| std::env::var("DEP_DUCKDB_INCLUDE"))
        .ok()
        .filter(|_| !bundle)
        .filter(|dir| {
            ["libduckdb.so", "libduckdb.dylib"]
                .iter()
                .any(|lib| Path::new(dir).join(lib).is_file())
        });
    let beside = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") => "$ORIGIN",
        Ok("macos") => "@executable_path",
        _ => return,
    };
    for dir in linked.as_deref().into_iter().chain([beside]) {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}
