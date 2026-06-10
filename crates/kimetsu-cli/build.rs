fn main() {
    let base = if std::env::var_os("CARGO_FEATURE_EMBEDDINGS").is_some() {
        "embeddings"
    } else {
        "lean"
    };
    let mut flavor = String::from(base);
    if std::env::var_os("CARGO_FEATURE_PI").is_some() {
        flavor.push_str(", +pi");
    }
    if std::env::var_os("CARGO_FEATURE_OPENCLAW").is_some() {
        flavor.push_str(", +openclaw");
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    println!("cargo:rustc-env=KIMETSU_VERSION_DISPLAY={version} ({flavor})");
    println!("cargo:rerun-if-changed=build.rs");
}
