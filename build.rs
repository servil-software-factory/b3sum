//! Embed `b3sum`'s own build version (`X.Y.Z+<commit>[.dirty]`) from this
//! repository rather than the pinned `blake3-servil` library version.

mod build_version;

use std::env;
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let scratch = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let (name, watched) = build_version::build_version(&dir, &env::var("CARGO_PKG_VERSION").unwrap(), &scratch);
    println!("cargo::rustc-env=B3SUM_BUILD_VERSION={name}");
    println!("cargo::rerun-if-changed=build_version.rs");
    for path in watched {
        println!("cargo::rerun-if-changed={}", path.display());
    }
}
