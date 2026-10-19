use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CAYW_SOURCE_MANIFEST_SHA256");
    let source = match env::var("CAYW_SOURCE_MANIFEST_SHA256") {
        Ok(value) => {
            assert!(
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "CAYW_SOURCE_MANIFEST_SHA256 must be an explicit 64-digit SHA-256 digest"
            );
            value.to_ascii_lowercase()
        }
        Err(env::VarError::NotPresent) => String::new(),
        Err(error) => panic!("invalid CAYW_SOURCE_MANIFEST_SHA256: {error}"),
    };
    let rustc = env::var_os("RUSTC").expect("Cargo supplies RUSTC");
    let output = Command::new(rustc)
        .arg("--version")
        .output()
        .expect("read the compiler version");
    assert!(output.status.success(), "rustc --version failed");
    let version = String::from_utf8(output.stdout).expect("rustc version is UTF-8");
    println!(
        "cargo:rustc-env=CAYW_HARNESS_RUSTC_VERSION={}",
        version.trim()
    );
    println!("cargo:rustc-env=CAYW_HARNESS_SOURCE_MANIFEST_SHA256={source}");
    for (input, output) in [
        ("TARGET", "CAYW_HARNESS_BUILD_TARGET"),
        ("PROFILE", "CAYW_HARNESS_BUILD_PROFILE"),
        ("OPT_LEVEL", "CAYW_HARNESS_OPT_LEVEL"),
    ] {
        println!("cargo:rustc-env={output}={}", env::var(input).expect(input));
    }
}
