fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(rust_1_99_nightly)");
    if rustversion::cfg!(all(nightly, since(1.99), before(1.100))) {
        println!("cargo:rustc-cfg=rust_1_99_nightly");
    }
}
