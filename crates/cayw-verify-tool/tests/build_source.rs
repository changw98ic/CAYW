//! Real offline Cargo counterexamples for the build-source boundary.
use cayw_verify_tool::provenance::build_receipt::{
    BuildContext, capture_receipt, snapshot_sources, verify_current_receipt, verify_receipt,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct CargoFixture {
    root: PathBuf,
    evidence: PathBuf,
}

impl CargoFixture {
    fn new(label: &str) -> Self {
        let parent = std::env::var_os("CAYW_BUILD_SOURCE_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let evidence = parent.join(format!(
            "{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&evidence).unwrap();
        let root = evidence.join("source");
        let package = root.join("crates/cayw-verify-tool");
        fs::create_dir_all(package.join("src")).unwrap();
        fs::create_dir_all(root.join("crates/cayw-runtime/src")).unwrap();
        fs::create_dir_all(root.join("crates/cayw-ipc/src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/cayw-verify-tool\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            package.join("Cargo.toml"),
            "[package]\nname = \"cayw-verify-tool\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\nfixture-feature = []\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"\nsha2 = \"0.10.9\"\nlibc = \"0.2\"\n\n[build-dependencies]\nserde_json = \"1\"\nsha2 = \"0.10.9\"\n",
        )
        .unwrap();
        fs::write(
            package.join("build.rs"),
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs")).unwrap(),
        )
        .unwrap();
        fs::write(package.join("src/lib.rs"), "pub fn marker() {}\n").unwrap();
        fs::write(
            package.join("src/main.rs"),
            "mod measurement;\nmod provenance;\nfn main() {\n if let Some(path) = std::env::args_os().nth(1) {\n  let receipt = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();\n  match provenance::build_receipt::verify_current_receipt(Some(&receipt)) {\n   Ok(result) => println!(\"{}\", serde_json::to_string(&result).unwrap()),\n   Err(error) => { eprintln!(\"{error}\"); std::process::exit(1); }\n  }\n } else { println!(\"{} {}\", include_str!(concat!(env!(\"OUT_DIR\"), \"/source-inputs.json\")), include_str!(\"asset with # dollar$.txt\")); }\n}\n",
        )
        .unwrap();
        fs::write(package.join("src/measurement.rs"),
            "use sha2::{Digest, Sha256}; pub fn sha256(bytes:&[u8])->String { format!(\"{:x}\",Sha256::digest(bytes)) }\n").unwrap();
        fs::write(
            package.join("src/provenance.rs"),
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provenance.rs")).unwrap(),
        )
        .unwrap();
        fs::write(
            package.join("src/build_receipt.rs"),
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/build_receipt.rs")).unwrap(),
        )
        .unwrap();
        fs::write(
            package.join("src/asset with # dollar$.txt"),
            "offline asset\n",
        )
        .unwrap();
        fs::write(root.join("crates/cayw-runtime/src/lib.rs"), "").unwrap();
        fs::write(root.join("crates/cayw-ipc/src/lib.rs"), "").unwrap();
        // Resolve only this tiny fixture, offline, before recording source bytes.
        let output = Command::new(env!("CARGO"))
            .args(["generate-lockfile", "--offline", "--manifest-path"])
            .arg(root.join("Cargo.toml"))
            .current_dir(&root)
            .output()
            .unwrap();
        fs::write(evidence.join("lock.stderr"), &output.stderr).unwrap();
        assert!(output.status.success());
        Self { root, evidence }
    }

    fn manifest(&self, root: &Path) -> PathBuf {
        let files = [
            "Cargo.toml",
            "Cargo.lock",
            "crates/cayw-verify-tool/build.rs",
            "crates/cayw-verify-tool/src/lib.rs",
            "crates/cayw-runtime/src/lib.rs",
            "crates/cayw-ipc/src/lib.rs",
        ];
        let path = self.root.join("source-manifest.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "schema":"cayw.build-source.v2", "root":root,
                "scope":"offline isolated build-source counterexample",
                "files":files.map(|path|json!({"path":path}))
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    fn build(&self, manifest: &Path) -> Output {
        let args = [
            "build",
            "--locked",
            "--offline",
            "--message-format=json",
            "--features",
            "fixture-feature",
            "--manifest-path",
        ];
        let target = self.evidence.join("target");
        fs::write(
            self.evidence.join("command.json"),
            serde_json::to_vec_pretty(&json!({
                "program":env!("CARGO"), "args":args,
                "manifest_path":self.root.join("Cargo.toml"),
                "target_dir":target, "source_manifest":manifest,
                "source":"offline synthetic Cargo fixture"
            }))
            .unwrap(),
        )
        .unwrap();
        let output = Command::new(env!("CARGO"))
            .args(args)
            .arg(self.root.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(target)
            .env("CAYW_SOURCE_MANIFEST_PATH", manifest)
            .current_dir(&self.root)
            .output()
            .unwrap();
        fs::write(self.evidence.join("cargo.jsonl"), &output.stdout).unwrap();
        fs::write(self.evidence.join("cargo.stderr"), &output.stderr).unwrap();
        fs::write(
            self.evidence.join("cargo-exit.json"),
            serde_json::to_vec(&json!({"exit_code":output.status.code()})).unwrap(),
        )
        .unwrap();
        output
    }

    fn context(&self) -> BuildContext {
        BuildContext {
            workspace_root: self.root.clone(),
            package_manifest: self.root.join("crates/cayw-verify-tool/Cargo.toml"),
            dep_info_base: self.root.clone(),
            target_directory: self.evidence.join("target"),
            target: env!("CAYW_VERIFY_TOOL_BUILD_TARGET").into(),
            rustc_version: env!("CAYW_VERIFY_TOOL_RUSTC_VERSION").into(),
            cargo_arguments: vec![
                "build".into(),
                "--locked".into(),
                "--offline".into(),
                "--message-format=json".into(),
                "--features".into(),
                "fixture-feature".into(),
            ],
        }
    }

    fn executable(&self) -> PathBuf {
        let text = fs::read_to_string(self.evidence.join("cargo.jsonl")).unwrap();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|message| {
                (message["reason"] == "compiler-artifact")
                    .then(|| message["executable"].as_str().map(PathBuf::from))
                    .flatten()
            })
            .expect("actual Cargo executable artifact")
    }
}

#[test]
fn actual_cargo_build_rejects_a_manifest_for_another_source_root() {
    let fixture = CargoFixture::new("wrong-root");
    let other = fixture.evidence.join("other-source");
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "crates/cayw-verify-tool/build.rs",
        "crates/cayw-verify-tool/src/lib.rs",
        "crates/cayw-runtime/src/lib.rs",
        "crates/cayw-ipc/src/lib.rs",
    ] {
        let path = other.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "bytes from a different source directory\n").unwrap();
    }
    let output = fixture.build(&fixture.manifest(&other));
    assert!(
        !output.status.success(),
        "Cargo accepted another source root; evidence: {}",
        fixture.evidence.display()
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("source root differs from actual Cargo workspace"),
        "unexpected build failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn built_receipt(
    label: &str,
    omit_main: bool,
) -> (
    CargoFixture,
    cayw_verify_tool::provenance::build_receipt::BuildReceipt,
) {
    let fixture = CargoFixture::new(label);
    eprintln!("build source fixture: {}", fixture.evidence.display());
    let manifest = fixture.manifest(&fixture.root);
    let mut paths = vec![
        PathBuf::from("Cargo.toml"),
        PathBuf::from("Cargo.lock"),
        PathBuf::from("source-manifest.json"),
        PathBuf::from("crates/cayw-verify-tool/Cargo.toml"),
        PathBuf::from("crates/cayw-verify-tool/build.rs"),
        PathBuf::from("crates/cayw-verify-tool/src/lib.rs"),
        PathBuf::from("crates/cayw-verify-tool/src/measurement.rs"),
        PathBuf::from("crates/cayw-verify-tool/src/provenance.rs"),
        PathBuf::from("crates/cayw-verify-tool/src/build_receipt.rs"),
        PathBuf::from("crates/cayw-verify-tool/src/asset with # dollar$.txt"),
        PathBuf::from("crates/cayw-runtime/src/lib.rs"),
        PathBuf::from("crates/cayw-ipc/src/lib.rs"),
    ];
    if !omit_main {
        paths.push(PathBuf::from("crates/cayw-verify-tool/src/main.rs"));
    }
    let before = snapshot_sources(&fixture.root, &paths).unwrap();
    let output = fixture.build(&manifest);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let executable = fixture.executable();
    let receipt = capture_receipt(
        fixture.context(),
        before,
        &fixture.evidence.join("cargo.jsonl"),
        &[fixture.evidence.join("target/debug/cayw-verify-tool.d")],
        &executable,
    )
    .unwrap();
    fs::write(
        fixture.evidence.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    (fixture, receipt)
}

#[test]
fn actual_cargo_artifacts_verify_known_inputs_and_keep_untraced_inputs_pending() {
    let (fixture, receipt) = built_receipt("known-inputs", false);
    let checked = verify_receipt(Some(&receipt), &receipt.executable.sha256).unwrap();
    assert_eq!(checked.status, "pending");
    assert!(checked.known_inputs_verified, "{checked:?}");
    assert!(checked.gaps.iter().any(|gap| gap.contains("build-script")));
    assert_eq!(
        receipt.selected_artifact["features"],
        json!(["fixture-feature"])
    );
    assert_eq!(receipt.selected_artifact["profile"]["test"], false);
    assert!(
        receipt
            .generated_inputs
            .iter()
            .any(|input| input.path.ends_with("source-inputs.json"))
    );
    assert!(
        receipt
            .after
            .files
            .iter()
            .any(|input| input.path.ends_with("Cargo.lock"))
    );
    assert!(receipt.context.target.contains("apple-darwin"));
    let mut wrong_artifact = receipt.clone();
    wrong_artifact.selected_artifact["features"] = json!([]);
    assert!(verify_receipt(Some(&wrong_artifact), &receipt.executable.sha256).is_err());
    let mut wrong_target = receipt.clone();
    wrong_target.context.target_directory = fixture.root.clone();
    assert!(verify_receipt(Some(&wrong_target), &receipt.executable.sha256).is_err());
    let mut missing_generated = receipt.clone();
    missing_generated.generated_inputs.clear();
    assert!(verify_receipt(Some(&missing_generated), &receipt.executable.sha256).is_err());
    assert!(
        receipt
            .dependencies
            .iter()
            .any(|file| file.path.ends_with("asset with # dollar$.txt"))
    );
    let consumer = Command::new(fixture.executable())
        .arg(fixture.evidence.join("receipt.json"))
        .output()
        .unwrap();
    fs::write(fixture.evidence.join("consumer.stdout"), &consumer.stdout).unwrap();
    fs::write(fixture.evidence.join("consumer.stderr"), &consumer.stderr).unwrap();
    assert!(
        consumer.status.success(),
        "{}",
        String::from_utf8_lossy(&consumer.stderr)
    );
    let consumed: Value = serde_json::from_slice(&consumer.stdout).unwrap();
    assert_eq!(consumed["status"], "pending");
    assert_eq!(consumed["known_inputs_verified"], true);
    assert_eq!(consumed["compiled_listed_inputs_verified"], true);
    assert_eq!(consumed["executable_sha256"], receipt.executable.sha256);
    let mut wrong_listed = receipt.clone();
    let matches_listed = |file: &&mut cayw_verify_tool::provenance::build_receipt::FileDigest| {
        file.path.ends_with("crates/cayw-verify-tool/src/lib.rs")
    };
    for snapshot in [&mut wrong_listed.before, &mut wrong_listed.after] {
        snapshot
            .files
            .iter_mut()
            .filter(matches_listed)
            .for_each(|file| file.sha256 = "a".repeat(64));
    }
    for dependency in wrong_listed.dependencies.iter_mut().filter(matches_listed) {
        dependency.sha256 = "a".repeat(64);
    }
    let altered_path = fixture.evidence.join("altered-listed.json");
    fs::write(&altered_path, serde_json::to_vec(&wrong_listed).unwrap()).unwrap();
    let altered = Command::new(fixture.executable())
        .arg(&altered_path)
        .output()
        .unwrap();
    fs::write(
        fixture.evidence.join("altered-listed.stderr"),
        &altered.stderr,
    )
    .unwrap();
    assert!(
        !altered.status.success(),
        "current program accepted source bytes different from its embedded build-time list"
    );
    let mut forged: Value = serde_json::to_value(&receipt).unwrap();
    forged["complete"] = json!(true);
    assert!(
        serde_json::from_value::<cayw_verify_tool::provenance::build_receipt::BuildReceipt>(forged)
            .is_err()
    );
    let mut wrong_context = receipt.clone();
    wrong_context.context.workspace_root = fixture.evidence.clone();
    assert!(verify_receipt(Some(&wrong_context), &receipt.executable.sha256).is_err());
    let mut wrong_package = receipt.clone();
    wrong_package.context.package_manifest = fixture.root.join("Cargo.toml");
    assert!(verify_receipt(Some(&wrong_package), &receipt.executable.sha256).is_err());
}

#[test]
fn actual_dep_info_finds_main_rs_omitted_from_source_snapshot() {
    let (_, receipt) = built_receipt("omitted-main", true);
    let checked = verify_receipt(Some(&receipt), &receipt.executable.sha256).unwrap();
    assert_eq!(checked.status, "pending");
    assert!(!checked.known_inputs_verified);
    assert!(
        checked.gaps.iter().any(|gap| gap.contains("src/main.rs")),
        "{checked:?}"
    );
}

#[test]
fn executable_mismatch_and_changed_source_bytes_are_rejected() {
    let (fixture, receipt) = built_receipt("mismatch", false);
    assert!(verify_receipt(Some(&receipt), &"f".repeat(64)).is_err());
    assert!(verify_current_receipt(Some(&receipt)).is_err());
    let before = snapshot_sources(
        &fixture.root,
        &[
            PathBuf::from("Cargo.toml"),
            PathBuf::from("Cargo.lock"),
            PathBuf::from("crates/cayw-verify-tool/Cargo.toml"),
            PathBuf::from("crates/cayw-verify-tool/src/main.rs"),
        ],
    )
    .unwrap();
    fs::write(
        fixture.root.join("crates/cayw-verify-tool/src/main.rs"),
        "fn main() { panic!(\"changed\"); }\n",
    )
    .unwrap();
    let captured = capture_receipt(
        fixture.context(),
        before,
        &fixture.evidence.join("cargo.jsonl"),
        &[fixture.evidence.join("target/debug/cayw-verify-tool.d")],
        &fixture.executable(),
    );
    assert!(
        captured.is_err(),
        "changed source bytes must invalidate the capture"
    );
    let mut damaged = receipt.clone();
    damaged.cargo_messages.push(b' ');
    assert!(verify_receipt(Some(&damaged), &receipt.executable.sha256).is_err());
}

#[test]
fn missing_receipt_cannot_verify_a_program() {
    let checked = verify_receipt(None, &"0".repeat(64)).unwrap();
    assert_eq!(checked.status, "pending");
    assert!(!checked.known_inputs_verified);
    assert!(!checked.gaps.is_empty());
}
