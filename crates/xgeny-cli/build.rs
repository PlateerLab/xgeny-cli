use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};

const EXPECTED_RUSTC_VERSION: &str = "rustc 1.98.0 (88d9e12ae 2026-08-18)";
const EXPECTED_RUST_LIBRARY_COPYRIGHT_BYTES: usize = 1_512_520;
const EXPECTED_RUST_LIBRARY_COPYRIGHT_SHA256: [u8; 32] = [
    0x68, 0x12, 0x95, 0x00, 0xb6, 0x16, 0xd5, 0x83, 0x86, 0x29, 0xe6, 0x8f, 0x55, 0xff, 0x3a, 0xed,
    0x5e, 0x09, 0x6d, 0xac, 0xf6, 0x0c, 0xe9, 0xeb, 0x41, 0xbb, 0xe5, 0x99, 0xa5, 0x63, 0xaf, 0xa6,
];
const EXPECTED_MUSL_COPYRIGHT_SHA256: [u8; 32] = [
    0xf9, 0xbc, 0x44, 0x23, 0x73, 0x23, 0x50, 0xeb, 0x0b, 0x3f, 0x7e, 0xd7, 0xe9, 0x1d, 0x53, 0x02,
    0x98, 0x47, 0x6f, 0x8f, 0xec, 0x0c, 0x6c, 0x42, 0x7a, 0x1c, 0x04, 0xad, 0xe2, 0x26, 0x55, 0xaf,
];
const EXPECTED_LLVM_LIBUNWIND_LICENSE_SHA256: [u8; 32] = [
    0xb5, 0xef, 0xeb, 0xca, 0xca, 0x80, 0x87, 0x92, 0x34, 0x09, 0x8e, 0x52, 0xd1, 0x72, 0x5e, 0x6d,
    0x9e, 0xb8, 0xfb, 0x96, 0xa1, 0x9f, 0xce, 0x62, 0x5d, 0x39, 0x18, 0x4b, 0x70, 0x5f, 0x7b, 0x6d,
];

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTC");

    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let version_output = Command::new(&rustc)
        .arg("--version")
        .output()
        .expect("rustc must be executable to validate the audited toolchain");
    assert!(
        version_output.status.success(),
        "rustc --version must succeed to validate the audited toolchain"
    );
    let version = std::str::from_utf8(&version_output.stdout)
        .expect("rustc version must be UTF-8")
        .trim();
    assert_eq!(
        version, EXPECTED_RUSTC_VERSION,
        "the build must use the audited Rust 1.98.0 compiler"
    );

    let output = Command::new(&rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc must be executable to locate its license notices");
    assert!(
        output.status.success(),
        "rustc --print sysroot must succeed to embed license notices"
    );

    let sysroot = std::str::from_utf8(&output.stdout)
        .expect("rustc sysroot must be UTF-8")
        .trim();
    let source = PathBuf::from(sysroot)
        .join("share")
        .join("doc")
        .join("rust")
        .join("COPYRIGHT-library.html");
    println!("cargo:rerun-if-changed={}", source.display());

    let notices = audited_file(
        &source,
        EXPECTED_RUST_LIBRARY_COPYRIGHT_BYTES,
        EXPECTED_RUST_LIBRARY_COPYRIGHT_SHA256,
        "Rust 1.98.0 standard-library notices",
    );

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"));
    for (path, expected_size, expected_digest, label) in [
        (
            manifest_dir.join("licenses/musl-1.2.5-COPYRIGHT"),
            6_204,
            EXPECTED_MUSL_COPYRIGHT_SHA256,
            "musl 1.2.5 notices",
        ),
        (
            manifest_dir.join("licenses/llvm-libunwind-52ed14f-LICENSE.TXT"),
            16_706,
            EXPECTED_LLVM_LIBUNWIND_LICENSE_SHA256,
            "LLVM libunwind notices",
        ),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
        let _ = audited_file(&path, expected_size, expected_digest, label);
    }

    let destination = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set"))
        .join("RUST_COPYRIGHT_LIBRARY.html");
    fs::write(&destination, notices).unwrap_or_else(|error| {
        panic!(
            "failed to stage Rust library notices at {}: {error}",
            destination.display()
        )
    });
}

fn audited_file(
    source: &std::path::Path,
    expected_size: usize,
    expected_digest: [u8; 32],
    label: &str,
) -> Vec<u8> {
    let contents = fs::read(source)
        .unwrap_or_else(|error| panic!("failed to read {label} at {}: {error}", source.display()));
    assert_eq!(
        contents.len(),
        expected_size,
        "{label} length differs from the audited document"
    );
    let observed_digest: [u8; 32] = Sha256::digest(&contents).into();
    assert_eq!(
        observed_digest, expected_digest,
        "{label} differs from the audited document"
    );
    contents
}
