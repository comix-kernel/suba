fn main() {
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rerun-if-changed=src/linker.ld");
    println!("cargo:rustc-link-arg-bin=suba=-T{root}/src/linker.ld");
}
