//! Fails the build early when the web app has not been built, instead of
//! serving an empty UI.

use std::path::Path;

fn main() {
    let index = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist/index.html");
    println!("cargo:rerun-if-changed={}", index.display());
    if !index.is_file() {
        panic!(
            "{} is missing; build the web app first: cd web && npm run build",
            index.display()
        );
    }
}
