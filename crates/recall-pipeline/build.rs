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

/// The sherpa-onnx release whose prebuilt libraries are linked. Must equal
/// the `sherpa-onnx` version pinned (with `=`) in this crate's Cargo.toml,
/// which `main` checks, because `target/sherpa-onnx-prebuilt/` keeps the
/// downloads of earlier versions after an upgrade.
const SHERPA_VERSION: &str = "2.0.1";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    check_pinned_version();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let target_dir = target_dir(&out_dir);
    let prebuilt = target_dir.join("sherpa-onnx-prebuilt");
    // Run again whenever the downloads change, e.g. once sherpa-onnx-sys has
    // fetched them on a first build.
    println!("cargo:rerun-if-changed={}", prebuilt.display());

    match lib_dir(&out_dir, &prebuilt) {
        Some(dir) => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
            println!("cargo:rustc-env=RECALL_SHERPA_LIB_DIR={}", dir.display());
        }
        None => {
            println!(
                "cargo:warning={LIB} {SHERPA_VERSION} not found yet; building without its \
                 absolute rpath, $ORIGIN is still searched"
            );
            println!("cargo:rustc-env=RECALL_SHERPA_LIB_DIR=");
        }
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/recall");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
}

/// Panics unless Cargo.toml pins `sherpa-onnx` to [`SHERPA_VERSION`].
fn check_pinned_version() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let toml = fs::read_to_string(manifest.join("Cargo.toml")).expect("reading Cargo.toml");
    let pinned = format!("\"={SHERPA_VERSION}\"");
    let ok = toml
        .lines()
        .find(|l| l.trim_start().starts_with("sherpa-onnx ="))
        .is_some_and(|l| l.contains(&pinned));
    assert!(
        ok,
        "Cargo.toml must pin sherpa-onnx to {pinned}; update SHERPA_VERSION in build.rs with it"
    );
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
/// [`SHERPA_VERSION`] shared-library download, else the copy
/// `sherpa-onnx-sys` puts beside the build outputs on every build.
fn lib_dir(out_dir: &Path, prebuilt: &Path) -> Option<PathBuf> {
    if let Some(dir) = env::var_os("SHERPA_ONNX_LIB_DIR") {
        return Some(PathBuf::from(dir));
    }
    let prefix = format!("sherpa-onnx-v{SHERPA_VERSION}-");
    let downloads = fs::read_dir(prebuilt)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|d| {
            d.file_name().is_some_and(|n| {
                let n = n.to_string_lossy();
                n.starts_with(&prefix) && n.contains("shared")
            })
        })
        .map(|d| d.join("lib"));
    // OUT_DIR is <profile>/build/<pkg>-<hash>/out.
    let profile = out_dir.ancestors().nth(3).map(Path::to_path_buf);
    downloads.chain(profile).find(|d| d.join(LIB).is_file())
}
