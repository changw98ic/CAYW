use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CAYW_SOURCE_MANIFEST_PATH");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let binding = match env::var_os("CAYW_SOURCE_MANIFEST_PATH") {
        None => json!({"status":"unbound","files":[],"scope":"no source manifest provided"}),
        Some(path) => {
            println!("cargo:rerun-if-changed={}", Path::new(&path).display());
            let manifest: Value =
                serde_json::from_slice(&fs::read(path).expect("source manifest readable"))
                    .expect("source manifest JSON");
            assert_eq!(manifest["schema"], "cayw.build-source.v2");
            let root = Path::new(manifest["root"].as_str().expect("source root"))
                .canonicalize()
                .expect("source root exists");
            let package_root =
                PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo package root"))
                    .canonicalize()
                    .expect("actual Cargo package exists");
            let workspace_root = package_root
                .parent()
                .and_then(Path::parent)
                .expect("verifyTool lives under workspace/crates")
                .canonicalize()
                .expect("actual Cargo workspace exists");
            assert_eq!(
                root, workspace_root,
                "source root differs from actual Cargo workspace"
            );
            assert_eq!(
                root.join("crates/cayw-verify-tool")
                    .canonicalize()
                    .expect("verifyTool package exists"),
                package_root,
                "source package differs from actual Cargo package"
            );
            let mut files = vec![];
            let mut names = std::collections::BTreeSet::new();
            for row in manifest["files"]
                .as_array()
                .expect("explicit source input files")
            {
                let name = row["path"].as_str().expect("input path");
                let relative = Path::new(name);
                assert!(
                    !relative.as_os_str().is_empty()
                        && relative
                            .components()
                            .all(|c| matches!(c, Component::Normal(_))),
                    "invalid source path"
                );
                assert!(names.insert(name.to_owned()), "duplicate source input");
                let path = root
                    .join(relative)
                    .canonicalize()
                    .expect("source input exists");
                assert!(path.starts_with(&root), "source input escapes root");
                println!("cargo:rerun-if-changed={}", path.display());
                let bytes = fs::read(path).expect("source input readable");
                // Hash the actual bytes; a caller's expected digest cannot establish this fact.
                files.push(json!({"path":name,"bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}));
            }
            for required in [
                "Cargo.toml",
                "Cargo.lock",
                "crates/cayw-verify-tool/build.rs",
                "crates/cayw-verify-tool/src/lib.rs",
                "crates/cayw-runtime/src/lib.rs",
                "crates/cayw-ipc/src/lib.rs",
            ] {
                assert!(
                    names.contains(required),
                    "required product input absent: {required}"
                );
            }
            files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            json!({"status":"build_verified_listed_inputs","scope":manifest["scope"],"files":files,
                "root":root,"package_root":package_root,
                "closure_status":"listed inputs measured; validate Cargo dep-info before acceptance"})
        }
    };
    let bytes = serde_json::to_vec(&binding).unwrap();
    let digest = if binding["status"] == "unbound" {
        String::new()
    } else {
        format!("{:x}", Sha256::digest(&bytes))
    };
    fs::write(out.join("source-inputs.json"), bytes).unwrap();
    let rustc = env::var_os("RUSTC").expect("Cargo supplies RUSTC");
    let output = Command::new(rustc)
        .arg("--version")
        .output()
        .expect("compiler version");
    assert!(output.status.success(), "rustc --version failed");
    println!(
        "cargo:rustc-env=CAYW_VERIFY_TOOL_RUSTC_VERSION={}",
        String::from_utf8(output.stdout).unwrap().trim()
    );
    println!("cargo:rustc-env=CAYW_VERIFY_TOOL_SOURCE_MANIFEST_SHA256={digest}");
    for (input, output) in [
        ("TARGET", "CAYW_VERIFY_TOOL_BUILD_TARGET"),
        ("PROFILE", "CAYW_VERIFY_TOOL_BUILD_PROFILE"),
        ("OPT_LEVEL", "CAYW_VERIFY_TOOL_OPT_LEVEL"),
    ] {
        println!("cargo:rustc-env={output}={}", env::var(input).expect(input));
    }
}
