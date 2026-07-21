use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(command: &mut Command, description: &str) {
    let status = command.status().unwrap_or_else(|error| {
        panic!("failed to start {description}: {error}");
    });
    assert!(status.success(), "{description} exited with {status}");
}

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/bootstrap.S");
    println!("cargo:rerun-if-changed=src/interrupts/stubs.S");
    println!("cargo:rerun-if-changed=include/sisyphus/driver.h");
    println!("cargo:rerun-if-changed=drivers/reference/reference_driver.c");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=AR");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        let linker_script = PathBuf::from(
            env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
        )
        .join("linker.ld");
        println!(
            "cargo:rustc-link-arg-bin=boulder=-T{}",
            linker_script.display()
        );
        println!("cargo:rustc-link-arg-bin=boulder=--gc-sections");
    }

    if env::var_os("CARGO_FEATURE_REFERENCE_DRIVER").is_none() {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let object = out_dir.join("reference_driver.o");
    let archive = out_dir.join("libreference_driver.a");
    let cc = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let ar = env::var_os("AR").unwrap_or_else(|| "ar".into());

    run(
        Command::new(cc)
            .arg("-std=c11")
            .arg("-ffreestanding")
            .arg("-fno-stack-protector")
            .arg("-fvisibility=hidden")
            .arg("-Wall")
            .arg("-Wextra")
            .arg("-Werror")
            .arg("-I")
            .arg(Path::new("include"))
            .arg("-c")
            .arg("drivers/reference/reference_driver.c")
            .arg("-o")
            .arg(&object),
        "C reference-driver compilation",
    );

    run(
        Command::new(ar).arg("crs").arg(&archive).arg(&object),
        "C reference-driver archive creation",
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=reference_driver");
}
