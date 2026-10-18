//! Links the pre-compiled Windows resources (manifest, icon, version info) into the binary.
//!
//! `assets/radial.res` is committed so building needs nothing but Rust; regenerate it with
//! `tools/build-resources.ps1` after editing `assets/radial.rc`, the manifest or the icon.

fn main() {
    println!("cargo:rerun-if-changed=assets/radial.res");
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows-msvc") {
        let res =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("assets/radial.res");
        if res.exists() {
            // Only the binary: test executables keep their default manifest.
            println!("cargo:rustc-link-arg-bins={}", res.display());
        } else {
            println!("cargo:warning=assets/radial.res is missing: building without the manifest and icon");
        }
    }
}
