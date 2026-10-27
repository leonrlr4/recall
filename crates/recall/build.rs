//! Bakes the rpaths that find the sherpa-onnx shared libraries into the
//! `recall` binary (and this package's test binaries), so they run without
//! `LD_LIBRARY_PATH`.
//!
//! `recall-pipeline`'s build script finds the libraries but its link
//! arguments apply only to its own targets; it passes the directory here as
//! `DEP_RECALL_SHERPA_LIB_DIR`. `$ORIGIN/../lib/recall` is the installed
//! layout and `$ORIGIN` is where `sherpa-onnx-sys` copies the libraries
//! beside the build outputs.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DEP_RECALL_SHERPA_LIB_DIR");
    match env::var("DEP_RECALL_SHERPA_LIB_DIR") {
        Ok(dir) if !dir.is_empty() => println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}"),
        _ => println!(
            "cargo:warning=the sherpa-onnx library directory is unknown; \
             only $ORIGIN rpaths are set"
        ),
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/recall");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
}
