fn main() {
    // Use VERSION env var if set (from CI), otherwise fall back to Cargo.toml version
    let version =
        std::env::var("VERSION").unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").unwrap());
    println!("cargo:rustc-env=CARGO_PKG_VERSION={}", version);
    println!("cargo:rerun-if-env-changed=VERSION");
}
