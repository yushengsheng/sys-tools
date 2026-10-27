fn main() {
    println!("cargo:rerun-if-changed=assets/app.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let manifest = std::path::Path::new("assets/app.manifest").canonicalize().unwrap();
        println!("cargo:rustc-link-arg-bin=systools=/MANIFESTUAC:level='requireAdministrator'");
        println!("cargo:rustc-link-arg-bin=systools=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=systools=/MANIFESTINPUT:{}", manifest.display());
    }
}

