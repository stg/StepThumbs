fn main() {
    let resource = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("assets")
        .join("app.res");
    println!("cargo:rerun-if-changed=assets/app.res");
    println!("cargo:rustc-link-arg={}", resource.display());
}
