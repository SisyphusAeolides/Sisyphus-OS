use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("none") {
        return;
    }

    let linker_script = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    )
    .join("linker.ld");
    println!(
        "cargo:rustc-link-arg-bin=crest=-T{}",
        linker_script.display()
    );
    println!("cargo:rustc-link-arg-bin=crest=--no-pie");
    println!("cargo:rustc-link-arg-bin=crest=--no-dynamic-linker");
    println!("cargo:rustc-link-arg-bin=crest=--gc-sections");
}
