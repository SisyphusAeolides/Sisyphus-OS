use std::env;
use std::fs;
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
    println!("cargo:rerun-if-changed=include/sisyphus/gpu.h");
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

        let workspace = PathBuf::from(
            env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
        )
        .join("../..");
        let push_image = workspace.join("target/x86_64-sisyphus-user/release/push");
        println!("cargo:rerun-if-changed={}", push_image.display());
        let bytes = fs::read(&push_image).unwrap_or_else(|error| {
            panic!(
                "failed to read {}: {error}; run `cargo user-push` before building Boulder",
                push_image.display()
            )
        });
        assert!(
            !bytes.is_empty() && bytes.len() <= 1024 * 1024,
            "Push image must be between 1 byte and 1 MiB"
        );
        let entry_file_offset = elf_entry_file_offset(&bytes);
        let digest = sha256(&bytes);
        let mut encoded = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write as _;
            write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        println!("cargo:rustc-env=SISYPHUS_PUSH_SHA256={encoded}");
        println!("cargo:rustc-env=SISYPHUS_PUSH_BYTES={}", bytes.len());
        println!("cargo:rustc-env=SISYPHUS_PUSH_ENTRY_FILE_OFFSET={entry_file_offset}");
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

fn elf_entry_file_offset(bytes: &[u8]) -> usize {
    assert!(
        bytes.get(..4) == Some(b"\x7fELF") && bytes.get(4) == Some(&2) && bytes.get(5) == Some(&1),
        "Push image must be a little-endian ELF64 artifact"
    );
    let entry = read_u64(bytes, 24);
    let program_offset = usize::try_from(read_u64(bytes, 32)).expect("program table offset");
    let program_entry_size = usize::from(read_u16(bytes, 54));
    let program_count = usize::from(read_u16(bytes, 56));
    assert!(program_entry_size >= 56, "invalid Push program header size");
    for index in 0..program_count {
        let offset = program_offset
            .checked_add(
                index
                    .checked_mul(program_entry_size)
                    .expect("program table index"),
            )
            .expect("program table offset");
        let header = bytes
            .get(offset..offset + 56)
            .expect("Push program header outside artifact");
        let kind = u32::from_le_bytes(header[0..4].try_into().expect("program type"));
        let flags = u32::from_le_bytes(header[4..8].try_into().expect("program flags"));
        let file_offset = u64::from_le_bytes(header[8..16].try_into().expect("file offset"));
        let virtual_address =
            u64::from_le_bytes(header[16..24].try_into().expect("virtual address"));
        let file_size = u64::from_le_bytes(header[32..40].try_into().expect("file size"));
        let memory_size = u64::from_le_bytes(header[40..48].try_into().expect("memory size"));
        let Some(segment_end) = virtual_address.checked_add(memory_size) else {
            continue;
        };
        if kind != 1 || flags & 1 == 0 || entry < virtual_address || entry >= segment_end {
            continue;
        }
        let within_segment = entry - virtual_address;
        assert!(
            within_segment < file_size,
            "Push entry is not backed by executable file bytes"
        );
        return usize::try_from(
            file_offset
                .checked_add(within_segment)
                .expect("Push entry file offset overflow"),
        )
        .expect("Push entry file offset does not fit usize");
    }
    panic!("Push entry is outside an executable load segment");
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("truncated Push ELF field"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("truncated Push ELF field"),
    )
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut state = [
        0x6a09_e667_u32,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let bit_length = (bytes.len() as u64).wrapping_mul(8);
    let padded_length = (bytes.len() + 9).div_ceil(64) * 64;
    let mut padded = vec![0_u8; padded_length];
    padded[..bytes.len()].copy_from_slice(bytes);
    padded[bytes.len()] = 0x80;
    padded[padded_length - 8..].copy_from_slice(&bit_length.to_be_bytes());
    for chunk in padded.chunks_exact(64) {
        let block: &[u8; 64] = chunk.try_into().expect("exact SHA-256 block");
        compress_sha256(&mut state, block);
    }
    let mut digest = [0_u8; 32];
    for (word, output) in state.iter().zip(digest.chunks_exact_mut(4)) {
        output.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn compress_sha256(state: &mut [u32; 8], block: &[u8; 64]) {
    const ROUND: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut words = [0_u32; 64];
    for (word, bytes) in words.iter_mut().take(16).zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes(bytes.try_into().expect("four-byte SHA-256 word"));
    }
    for index in 16..64 {
        let s0 = words[index - 15].rotate_right(7)
            ^ words[index - 15].rotate_right(18)
            ^ (words[index - 15] >> 3);
        let s1 = words[index - 2].rotate_right(17)
            ^ words[index - 2].rotate_right(19)
            ^ (words[index - 2] >> 10);
        words[index] = words[index - 16]
            .wrapping_add(s0)
            .wrapping_add(words[index - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let choice = (e & f) ^ ((!e) & g);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let first = h
            .wrapping_add(sum1)
            .wrapping_add(choice)
            .wrapping_add(ROUND[index])
            .wrapping_add(words[index]);
        let second = sum0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(first);
        d = c;
        c = b;
        b = a;
        a = first.wrapping_add(second);
    }
    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}
