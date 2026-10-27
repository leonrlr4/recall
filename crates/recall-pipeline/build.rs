//! Finds the sherpa-onnx shared libraries and bakes their location into the
//! crate's binaries.
//!
//! `sherpa-onnx-sys` emits its rpath with `cargo:rustc-link-arg`, which only
//! applies to its own targets, so binaries linking this crate would not find
//! `libsherpa-onnx-c-api.so` at run time without `LD_LIBRARY_PATH`. This
//! script adds the build-time library directory, `$ORIGIN` (where
//! `sherpa-onnx-sys` also copies the libraries) and `$ORIGIN/../lib/recall`
//! (the installed layout) as rpaths, and exports the build-time directory as
//! `RECALL_SHERPA_LIB_DIR` for `recall_pipeline::models::shared_library_dir`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const LIB: &str = "libsherpa-onnx-c-api.so";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let target_dir = target_dir(&out_dir);
    let prebuilt = target_dir.join("sherpa-onnx-prebuilt");

    match lib_dir(&out_dir, &prebuilt) {
        Some(dir) => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
            println!("cargo:rustc-env=RECALL_SHERPA_LIB_DIR={}", dir.display());
        }
        None => {
            println!(
                "cargo:warning={LIB} not found yet; building without its absolute rpath, \
                 $ORIGIN is still searched"
            );
            println!("cargo:rustc-env=RECALL_SHERPA_LIB_DIR=");
            // Run again on the next build, once sherpa-onnx-sys has fetched it.
            println!("cargo:rerun-if-changed={}", prebuilt.display());
        }
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/recall");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
}

/// The same target directory `sherpa-onnx-sys` downloads into.
fn target_dir(out_dir: &Path) -> PathBuf {
    if let Some(dir) = env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    out_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "target"))
        .unwrap_or(out_dir)
        .to_path_buf()
}

/// The directory holding [`LIB`]: `SHERPA_ONNX_LIB_DIR` when set, else the
/// prebuilt shared-library download, else a copy beside the build outputs.
fn lib_dir(out_dir: &Path, prebuilt: &Path) -> Option<PathBuf> {
    if let Some(dir) = env::var_os("SHERPA_ONNX_LIB_DIR") {
        return Some(PathBuf::from(dir));
    }
    let downloads = subdirs(prebuilt)
        .into_iter()
        .filter(|d| {
            d.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("shared"))
        })
        .map(|d| d.join("lib"));
    // OUT_DIR is <profile>/build/<pkg>-<hash>/out.
    let profile = out_dir.ancestors().nth(3);
    let sys_outs = profile
        .map(|p| subdirs(&p.join("build")))
        .unwrap_or_default()
        .into_iter()
        .filter(|d| {
            d.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("sherpa-onnx-sys-"))
        })
        .map(|d| d.join("out"));
    downloads
        .chain(sys_outs)
        .chain(profile.map(Path::to_path_buf))
        .find(|d| d.join(LIB).is_file())
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}
