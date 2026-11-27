//! Embed `b3sum`'s own build version (`X.Y.Z+<commit>[.dirty]`) from this
//! repository rather than the pinned `blake3-servil` library version, and
//! enforce the supported target contract at build time.

#[cfg(unix)]
mod build_version;

use std::env;

const SUPPORTED_TARGETS: &[&str] = &["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin"];

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if !SUPPORTED_TARGETS.contains(&target.as_str()) {
        panic!(
            "for {target}, use official BLAKE3 b3sum (https://github.com/BLAKE3-team/BLAKE3); \
             this standalone b3sum crate natively supports only {}",
            SUPPORTED_TARGETS.join(", ")
        );
    }
    #[cfg(not(unix))]
    panic!("building b3sum requires a Unix host; for other hosts, use official BLAKE3 b3sum");

    #[cfg(unix)]
    {
        let dir = std::path::PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let scratch = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap());
        let (name, watched) = build_version::build_version(&dir, &env::var("CARGO_PKG_VERSION").unwrap(), &scratch);
        println!("cargo::rustc-env=B3SUM_BUILD_VERSION={name}");
        println!("cargo::rerun-if-changed=build_version.rs");
        for path in watched {
            // Cargo's build-script stdout protocol is UTF-8 and line-delimited:
            // never emit a lossily-replaced or newline-containing path that could
            // silently watch the wrong target or inject a build directive.
            if let Some(s) = path.to_str()
                && !s.contains('\n')
                && !s.contains('\r')
            {
                println!("cargo::rerun-if-changed={s}");
            }
        }
    }
}
