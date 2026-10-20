//! Post-build checks of public Cargo artifacts and dep-info, never a completeness attestation.
//!
//! Capture requires an immutable source snapshot before invoking Cargo, a fresh target
//! directory, the successful build's stdout, and its public `.d` files. The receipt
//! retains observations so a reader can check them without the original source tree.
//! These records are not signed: consistency does not establish authenticity, trace
//! arbitrary build-script/proc-macro/native reads, or prove files stayed unchanged
//! between the two source measurements. Consequently every valid result is pending.
use crate::measurement::sha256;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
};

const SCHEMA: &str = "cayw.build-receipt.v1";
const MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDigest {
    /// Absolute spelling used by Cargo/the caller, retained for raw-record matching.
    pub path: PathBuf,
    /// Physical path observed at capture; aliases are not counted as different inputs.
    pub canonical_path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSnapshot {
    pub root: PathBuf,
    pub files: Vec<FileDigest>,
}

/// Explicit non-secret invocation facts. Cargo JSON does not authenticate these facts.
/// Do not put credentials or unrestricted environment/configuration dumps in this type.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildContext {
    pub workspace_root: PathBuf,
    pub package_manifest: PathBuf,
    /// Base for relative paths when build.dep-info-basedir is used.
    pub dep_info_base: PathBuf,
    pub target_directory: PathBuf,
    pub target: String,
    pub rustc_version: String,
    pub cargo_arguments: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepInfo {
    pub file: FileDigest,
    pub contents: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildReceipt {
    pub schema: String,
    pub context: BuildContext,
    pub before: SourceSnapshot,
    pub after: SourceSnapshot,
    pub cargo_messages: Vec<u8>,
    pub cargo_messages_sha256: String,
    pub dep_infos: Vec<DepInfo>,
    pub compiler_artifacts: Vec<Value>,
    pub build_scripts: Vec<Value>,
    pub selected_artifact: Value,
    pub dependencies: Vec<FileDigest>,
    pub generated_inputs: Vec<FileDigest>,
    pub artifacts: Vec<FileDigest>,
    pub executable: FileDigest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiptVerification {
    /// Always pending: this API does not establish all compiler/build-tool inputs.
    pub status: String,
    /// Only the supplied source snapshot, public dep-info and artifact relationships.
    pub known_inputs_verified: bool,
    /// Checked against a caller-supplied build binding, or the actual embedded one
    /// when using verify_current_receipt. It still does not establish completeness.
    pub compiled_listed_inputs_verified: bool,
    pub executable_sha256: Option<String>,
    pub gaps: Vec<String>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn absolute(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(invalid(format!(
            "expected absolute path without '..': {}",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

fn digest(path: &Path) -> io::Result<FileDigest> {
    absolute(path)?;
    if !fs::metadata(path)
        .map_err(|error| invalid(format!("input {}: {error}", path.display())))?
        .is_file()
    {
        return Err(invalid(format!("not a regular file: {}", path.display())));
    }
    let contents =
        fs::read(path).map_err(|error| invalid(format!("input {}: {error}", path.display())))?;
    Ok(FileDigest {
        path: path.into(),
        canonical_path: path.canonicalize()?,
        bytes: contents.len() as u64,
        sha256: sha256(&contents),
    })
}

fn read_record(path: &Path) -> io::Result<Vec<u8>> {
    let meta = fs::metadata(path)?;
    if !meta.is_file() || meta.len() > MAX_RECORD_BYTES {
        return Err(invalid("build record is not a bounded regular file"));
    }
    let contents = fs::read(path)?;
    if contents.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("build record too large"));
    }
    Ok(contents)
}

/// Measure only explicitly selected source paths, rejecting paths outside the root.
/// A caller-supplied list is not a claim that these are all compiler inputs.
pub fn snapshot_sources(root: &Path, paths: &[PathBuf]) -> io::Result<SourceSnapshot> {
    let supplied_root = absolute(root)?;
    let root = supplied_root.canonicalize()?;
    let mut files = BTreeMap::new();
    for path in paths {
        if path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(invalid("source path contains '..'"));
        }
        let file = digest(&if path.is_absolute() {
            path.clone()
        } else {
            supplied_root.join(path)
        })?;
        if !file.canonical_path.starts_with(&root) {
            return Err(invalid("source path escapes source root"));
        }
        if files.insert(file.canonical_path.clone(), file).is_some() {
            return Err(invalid("duplicate source input"));
        }
    }
    Ok(SourceSnapshot {
        root,
        files: files.into_values().collect(),
    })
}

struct Messages {
    artifacts: Vec<Value>,
    scripts: Vec<Value>,
}

fn messages(bytes: &[u8]) -> io::Result<Messages> {
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("Cargo messages too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("Cargo messages are not UTF-8"))?;
    let mut artifacts = vec![];
    let mut scripts = vec![];
    let mut finished = false;
    for line in text
        .lines()
        .filter(|line| line.trim_start().starts_with('{'))
    {
        let value: Value =
            serde_json::from_str(line).map_err(|_| invalid("malformed Cargo JSON"))?;
        match value["reason"].as_str() {
            Some("compiler-artifact") if !finished => {
                if value["package_id"].as_str().is_none_or(str::is_empty)
                    || value["manifest_path"].as_str().is_none()
                    || !value["target"].is_object()
                    || !value["profile"].is_object()
                    || !value["features"].is_array()
                    || !value["filenames"].is_array()
                    || !value["fresh"].is_boolean()
                {
                    return Err(invalid("incomplete Cargo compiler-artifact"));
                }
                artifacts.push(value);
            }
            Some("build-script-executed") if !finished => scripts.push(value),
            Some("build-finished") if !finished && value["success"] == true => finished = true,
            Some("compiler-artifact" | "build-script-executed" | "build-finished") => {
                return Err(invalid(
                    "failed, repeated or out-of-order Cargo build finish",
                ));
            }
            _ => {}
        }
    }
    if !finished || artifacts.is_empty() {
        return Err(invalid("Cargo build did not finish successfully"));
    }
    Ok(Messages { artifacts, scripts })
}

/// Parse public Makefile dep-info including escaped spaces, '#', '\\', '$$'
/// and line continuations. Environment comments are retained raw, not treated as paths.
fn dep_rules(bytes: &[u8], base: &Path) -> io::Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("dep-info is not UTF-8"))?
        .replace("\\\r\n", "")
        .replace("\\\n", "");
    let mut targets = BTreeSet::new();
    let mut dependencies = BTreeSet::new();
    for line in text.lines() {
        let mut words = vec![];
        let mut word = String::new();
        let mut separator = None;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '#' if word.is_empty() => break,
                '#' => word.push('#'),
                '\\' => word.push(
                    chars
                        .next()
                        .ok_or_else(|| invalid("unfinished dep-info escape"))?,
                ),
                '$' if chars.peek() == Some(&'$') => {
                    chars.next();
                    word.push('$');
                }
                ':' if separator.is_none() => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    separator = Some(words.len());
                }
                ch if ch.is_whitespace() => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                ch => word.push(ch),
            }
        }
        if !word.is_empty() {
            words.push(word);
        }
        if words.is_empty() {
            continue;
        }
        let split = separator.ok_or_else(|| invalid("dep-info rule has no target separator"))?;
        if split == 0 {
            return Err(invalid("dep-info rule has no target"));
        }
        for (i, word) in words.iter().enumerate() {
            let path = Path::new(word);
            let path = absolute(&if path.is_absolute() {
                path.into()
            } else {
                base.join(path)
            })?;
            if i < split {
                targets.insert(path);
            } else {
                dependencies.insert(path);
            }
        }
    }
    if targets.is_empty() || dependencies.is_empty() {
        return Err(invalid("dep-info contains no dependency-bearing rule"));
    }
    Ok((targets, dependencies))
}

/// Capture after Cargo has exited successfully, while its sources/artifacts still exist.
/// The caller must retain the Cargo exit status separately; stdout alone cannot prove it.
/// Only explicit files are read; this API never scans configuration or the environment.
pub fn capture_receipt(
    mut context: BuildContext,
    before: SourceSnapshot,
    cargo_json_path: &Path,
    dep_info_paths: &[PathBuf],
    executable: &Path,
) -> io::Result<BuildReceipt> {
    let root = absolute(&context.workspace_root)?.canonicalize()?;
    if root != before.root || !context.package_manifest.canonicalize()?.starts_with(&root) {
        return Err(invalid(
            "receipt source root differs from actual package/source snapshot",
        ));
    }
    context.workspace_root = root.clone();
    context.package_manifest = context.package_manifest.canonicalize()?;
    context.dep_info_base = absolute(&context.dep_info_base)?.canonicalize()?;
    context.target_directory = absolute(&context.target_directory)?.canonicalize()?;
    let after = snapshot_sources(
        &root,
        &before
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>(),
    )?;
    if before != after {
        return Err(invalid(
            "source bytes changed between pre-build and post-build snapshots",
        ));
    }
    let cargo_messages = read_record(cargo_json_path)?;
    let parsed = messages(&cargo_messages)?;
    let executable = digest(executable)?;
    let selected = parsed
        .artifacts
        .iter()
        .filter(|artifact| {
            artifact["executable"].as_str().is_some_and(|path| {
                Path::new(path)
                    .canonicalize()
                    .is_ok_and(|path| path == executable.canonical_path)
            })
        })
        .collect::<Vec<_>>();
    if selected.len() != 1 {
        return Err(invalid(
            "executable is not one unique Cargo compiler artifact",
        ));
    }
    let selected_artifact = selected[0].clone();
    if Path::new(selected_artifact["manifest_path"].as_str().unwrap()).canonicalize()?
        != context.package_manifest.canonicalize()?
    {
        return Err(invalid("executable belongs to a different Cargo package"));
    }
    let mut dep_infos = vec![];
    let mut dependency_paths = BTreeSet::new();
    for path in dep_info_paths {
        let contents = read_record(path)?;
        dependency_paths.extend(dep_rules(&contents, &context.dep_info_base)?.1);
        dep_infos.push(DepInfo {
            file: digest(path)?,
            contents,
        });
    }
    let dependencies = dependency_paths
        .iter()
        .map(|path| digest(path))
        .collect::<io::Result<Vec<_>>>()?;
    let out_dirs = parsed
        .scripts
        .iter()
        .filter_map(|script| script["out_dir"].as_str())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let generated_inputs = dependencies
        .iter()
        .filter(|file| out_dirs.iter().any(|dir| file.path.starts_with(dir)))
        .cloned()
        .collect();
    let mut artifact_paths = BTreeSet::new();
    for artifact in &parsed.artifacts {
        for path in artifact["filenames"].as_array().unwrap() {
            artifact_paths.insert(PathBuf::from(
                path.as_str()
                    .ok_or_else(|| invalid("artifact path is not a string"))?,
            ));
        }
        if let Some(path) = artifact["executable"].as_str() {
            artifact_paths.insert(PathBuf::from(path));
        }
    }
    let artifacts = artifact_paths
        .iter()
        .map(|path| digest(path))
        .collect::<io::Result<Vec<_>>>()?;
    let receipt = BuildReceipt {
        schema: SCHEMA.into(),
        context,
        before,
        after,
        cargo_messages_sha256: sha256(&cargo_messages),
        cargo_messages,
        dep_infos,
        compiler_artifacts: parsed.artifacts,
        build_scripts: parsed.scripts,
        selected_artifact,
        dependencies,
        generated_inputs,
        artifacts,
        executable,
    };
    verify_receipt(Some(&receipt), &receipt.executable.sha256)?;
    Ok(receipt)
}

fn validate_files(files: &[FileDigest]) -> io::Result<()> {
    let mut paths = BTreeSet::new();
    let mut canonical: BTreeMap<&Path, (&str, u64)> = BTreeMap::new();
    for file in files {
        absolute(&file.path)?;
        absolute(&file.canonical_path)?;
        if file.sha256.len() != 64
            || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || !paths.insert(&file.path)
        {
            return Err(invalid("invalid or duplicate file digest"));
        }
        let identity = (file.sha256.as_str(), file.bytes);
        if canonical
            .insert(&file.canonical_path, identity)
            .is_some_and(|old| old != identity)
        {
            return Err(invalid(
                "one physical file has contradictory byte identities",
            ));
        }
    }
    Ok(())
}

fn find_file<'a>(files: &'a [FileDigest], path: &Path) -> Option<&'a FileDigest> {
    files
        .iter()
        .find(|file| file.path == path || file.canonical_path == path)
}

fn same_file(left: &FileDigest, right: &FileDigest) -> bool {
    left.canonical_path == right.canonical_path
        && left.sha256 == right.sha256
        && left.bytes == right.bytes
}

/// Recheck the portable observations and bind them to the executable identity recorded
/// by the report. This does not consult live source files, trust a `complete` flag, or
/// upgrade missing/untraced inputs to a complete build. Contradictions return Err.
pub fn verify_receipt(
    receipt: Option<&BuildReceipt>,
    expected_executable_sha256: &str,
) -> io::Result<ReceiptVerification> {
    let Some(receipt) = receipt else {
        return Ok(ReceiptVerification {
            status: "pending".into(),
            known_inputs_verified: false,
            compiled_listed_inputs_verified: false,
            executable_sha256: None,
            gaps: vec!["build receipt absent".into()],
        });
    };
    if receipt.schema != SCHEMA {
        return Err(invalid("unknown build receipt schema"));
    }
    if receipt.cargo_messages_sha256 != sha256(&receipt.cargo_messages) {
        return Err(invalid("Cargo message digest mismatch"));
    }
    let parsed = messages(&receipt.cargo_messages)?;
    if parsed.artifacts != receipt.compiler_artifacts || parsed.scripts != receipt.build_scripts {
        return Err(invalid("Cargo observations differ from the raw messages"));
    }
    for files in [
        &receipt.before.files,
        &receipt.after.files,
        &receipt.dependencies,
        &receipt.generated_inputs,
        &receipt.artifacts,
    ] {
        validate_files(files)?;
    }
    validate_files(std::slice::from_ref(&receipt.executable))?;
    absolute(&receipt.before.root)?;
    absolute(&receipt.context.workspace_root)?;
    absolute(&receipt.context.package_manifest)?;
    absolute(&receipt.context.dep_info_base)?;
    absolute(&receipt.context.target_directory)?;
    if receipt.before != receipt.after
        || receipt.before.root != receipt.context.workspace_root
        || !receipt
            .context
            .package_manifest
            .starts_with(&receipt.before.root)
        || receipt
            .before
            .files
            .iter()
            .any(|file| !file.canonical_path.starts_with(&receipt.before.root))
    {
        return Err(invalid("inconsistent source snapshots/root"));
    }
    let package_manifest = find_file(&receipt.before.files, &receipt.context.package_manifest);
    let selected_manifest = receipt.selected_artifact["manifest_path"]
        .as_str()
        .and_then(|path| find_file(&receipt.before.files, Path::new(path)));
    if package_manifest
        .zip(selected_manifest)
        .is_some_and(|(left, right)| !same_file(left, right))
    {
        return Err(invalid(
            "selected artifact belongs to a different package manifest",
        ));
    }
    if !receipt
        .executable
        .canonical_path
        .starts_with(&receipt.context.target_directory)
        || receipt.artifacts.iter().any(|file| {
            !file
                .canonical_path
                .starts_with(&receipt.context.target_directory)
        })
        || receipt.dep_infos.iter().any(|info| {
            !info
                .file
                .canonical_path
                .starts_with(&receipt.context.target_directory)
        })
    {
        return Err(invalid(
            "artifact/dep-info outside the recorded Cargo target directory",
        ));
    }
    if receipt.executable.sha256 != expected_executable_sha256 {
        return Err(invalid(
            "receipt executable hash differs from report/current program",
        ));
    }
    let selected = receipt
        .compiler_artifacts
        .iter()
        .filter(|artifact| {
            artifact["executable"].as_str().is_some_and(|path| {
                find_file(&receipt.artifacts, Path::new(path))
                    .is_some_and(|file| same_file(file, &receipt.executable))
            })
        })
        .collect::<Vec<_>>();
    if selected.len() != 1 || *selected[0] != receipt.selected_artifact {
        return Err(invalid(
            "selected executable is not one unique Cargo artifact",
        ));
    }
    let mut declared_artifacts = BTreeSet::new();
    for artifact in &receipt.compiler_artifacts {
        for path in artifact["filenames"].as_array().unwrap() {
            declared_artifacts.insert(PathBuf::from(
                path.as_str()
                    .ok_or_else(|| invalid("invalid artifact filename"))?,
            ));
        }
        if let Some(path) = artifact["executable"].as_str() {
            declared_artifacts.insert(PathBuf::from(path));
        }
    }
    if declared_artifacts
        != receipt
            .artifacts
            .iter()
            .map(|file| file.path.clone())
            .collect()
    {
        return Err(invalid("artifact file set differs from Cargo JSON"));
    }
    let mut dep_targets = BTreeSet::new();
    let mut dep_paths = BTreeSet::new();
    let mut dep_files = BTreeSet::new();
    for info in &receipt.dep_infos {
        validate_files(std::slice::from_ref(&info.file))?;
        if info.contents.len() as u64 > MAX_RECORD_BYTES
            || info.file.bytes != info.contents.len() as u64
            || info.file.sha256 != sha256(&info.contents)
            || !dep_files.insert(&info.file.path)
        {
            return Err(invalid("dep-info identity/contents mismatch"));
        }
        let (targets, paths) = dep_rules(&info.contents, &receipt.context.dep_info_base)?;
        dep_targets.extend(targets);
        dep_paths.extend(paths);
    }
    if dep_paths
        != receipt
            .dependencies
            .iter()
            .map(|file| file.path.clone())
            .collect()
    {
        return Err(invalid("dependency file set differs from public dep-info"));
    }
    let out_dirs = receipt
        .build_scripts
        .iter()
        .filter_map(|script| script["out_dir"].as_str())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let generated = receipt
        .dependencies
        .iter()
        .filter(|file| out_dirs.iter().any(|dir| file.path.starts_with(dir)))
        .cloned()
        .collect::<Vec<_>>();
    if generated != receipt.generated_inputs {
        return Err(invalid(
            "generated input set differs from Cargo build-script outputs",
        ));
    }
    let mut gaps = vec![];
    let mut known = true;
    if receipt.dep_infos.is_empty()
        || !dep_targets.iter().any(|path| {
            find_file(&receipt.artifacts, path)
                .is_some_and(|file| same_file(file, &receipt.executable))
        })
    {
        known = false;
        gaps.push("public dep-info for selected executable absent".into());
    }
    let source_by_canonical = receipt
        .before
        .files
        .iter()
        .map(|file| (&file.canonical_path, file))
        .collect::<BTreeMap<_, _>>();
    for dependency in &receipt.dependencies {
        if let Some(before) = source_by_canonical.get(&dependency.canonical_path) {
            if !same_file(before, dependency) {
                return Err(invalid(
                    "dependency bytes differ from pre/post source snapshot",
                ));
            }
        } else if !receipt.generated_inputs.contains(dependency) {
            known = false;
            gaps.push(format!(
                "pre-build input absent: {}",
                dependency.path.display()
            ));
        }
    }
    for path in [
        receipt.before.root.join("Cargo.toml"),
        receipt.before.root.join("Cargo.lock"),
        receipt.context.package_manifest.clone(),
    ] {
        if find_file(&receipt.before.files, &path).is_none() {
            known = false;
            gaps.push(format!(
                "pre-build Cargo manifest/lock absent: {}",
                path.display()
            ));
        }
    }
    for manifest in receipt
        .compiler_artifacts
        .iter()
        .filter_map(|artifact| artifact["manifest_path"].as_str())
        .map(PathBuf::from)
    {
        if find_file(&receipt.before.files, &manifest).is_none() {
            gaps.push(format!(
                "dependency manifest not measured before build: {}",
                manifest.display()
            ));
        }
    }
    if receipt.context.target.is_empty()
        || receipt.context.rustc_version.is_empty()
        || receipt.context.cargo_arguments.is_empty()
    {
        known = false;
        gaps.push("target/compiler/invocation facts absent".into());
    }
    if receipt
        .compiler_artifacts
        .iter()
        .any(|artifact| artifact["fresh"] == true)
    {
        gaps.push("cached artifacts lack an observation of the original compilation".into());
    }
    gaps.extend([
        "build-script input file/environment reads are not traced by stable Cargo JSON or dep-info".into(),
        "proc-macro and native compiler/linker input completeness is not proved".into(),
        "Cargo configuration, target/compiler/flags and invocation facts are not fully authenticated by artifact messages".into(),
        "pre/post hashes do not observe source bytes at the instant the compiler reads them".into(),
    ]);
    if !receipt.generated_inputs.is_empty() {
        gaps.push("generated inputs are measured after the build, without a during-compilation byte observation".into());
    }
    gaps.sort();
    gaps.dedup();
    Ok(ReceiptVerification {
        status: "pending".into(),
        known_inputs_verified: known,
        compiled_listed_inputs_verified: false,
        executable_sha256: Some(receipt.executable.sha256.clone()),
        gaps,
    })
}

/// Compare a portable receipt to the build binding recorded in a report. The caller
/// decides where that binding came from: arbitrary JSON does not become authoritative
/// merely by passing this consistency check. Use verify_current_receipt for the
/// actual running program's immutable compile-time binding.
pub fn verify_receipt_binding(
    receipt: Option<&BuildReceipt>,
    expected_executable_sha256: &str,
    build_binding: &Value,
) -> io::Result<ReceiptVerification> {
    let mut checked = verify_receipt(receipt, expected_executable_sha256)?;
    let Some(receipt) = receipt else {
        return Ok(checked);
    };
    let source = &build_binding["source_inputs"];
    if build_binding["listed_inputs_verified"] != true {
        checked
            .gaps
            .push("running/report build has no compile-time measured source list".into());
        return Ok(checked);
    }
    let binding_sha = build_binding["source_manifest_sha256"]
        .as_str()
        .ok_or_else(|| invalid("compiled source list digest absent"))?;
    if sha256(&serde_json::to_vec(source).map_err(|error| invalid(error.to_string()))?)
        != binding_sha
    {
        return Err(invalid("compiled source list digest mismatch"));
    }
    let root = Path::new(
        source["root"]
            .as_str()
            .ok_or_else(|| invalid("compiled source root absent"))?,
    );
    let package = Path::new(
        source["package_root"]
            .as_str()
            .ok_or_else(|| invalid("compiled package root absent"))?,
    );
    absolute(root)?;
    absolute(package)?;
    if root != receipt.before.root || !package.starts_with(root) {
        return Err(invalid(
            "receipt root differs from embedded compile-time source root",
        ));
    }
    let compiled_manifest = package.join("Cargo.toml");
    // A path dependency's compiled package can be inside a downstream workspace.
    // Its manifest still must be measured in the receipt, not silently omitted.
    if find_file(&receipt.before.files, &compiled_manifest).is_none() {
        checked.known_inputs_verified = false;
        checked.gaps.push(format!(
            "compiled package manifest absent from receipt: {}",
            compiled_manifest.display()
        ));
    }
    for (field, recorded) in [
        ("build_target", receipt.context.target.as_str()),
        ("toolchain", receipt.context.rustc_version.as_str()),
    ] {
        if build_binding[field]
            .as_str()
            .is_some_and(|compiled| compiled != recorded)
        {
            return Err(invalid(format!(
                "receipt {field} differs from embedded compile-time value"
            )));
        }
    }
    if let Some(compiled) = build_binding["opt_level"].as_str()
        && receipt.selected_artifact["profile"]["opt_level"] != compiled
    {
        return Err(invalid(
            "receipt optimization differs from embedded compile-time value",
        ));
    }
    let files = source["files"]
        .as_array()
        .ok_or_else(|| invalid("compiled input file list absent"))?;
    if files.is_empty() {
        return Err(invalid("compiled input file list empty"));
    }
    let mut names = BTreeSet::new();
    let mut complete_list = true;
    for row in files {
        let name = row["path"]
            .as_str()
            .ok_or_else(|| invalid("compiled input path absent"))?;
        let path = Path::new(name);
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || !names.insert(name)
        {
            return Err(invalid("invalid/duplicate compiled input path"));
        }
        let absolute_path = root.join(path);
        let Some(file) = find_file(&receipt.before.files, &absolute_path) else {
            complete_list = false;
            checked.known_inputs_verified = false;
            checked.gaps.push(format!(
                "compile-time listed input absent from receipt: {name}"
            ));
            continue;
        };
        if row["bytes"].as_u64() != Some(file.bytes)
            || row["sha256"].as_str() != Some(file.sha256.as_str())
        {
            return Err(invalid(format!(
                "receipt input differs from embedded compile-time bytes: {name}"
            )));
        }
    }
    checked.compiled_listed_inputs_verified =
        complete_list && find_file(&receipt.before.files, &compiled_manifest).is_some();
    checked.gaps.sort();
    checked.gaps.dedup();
    Ok(checked)
}

/// Bind to both the running executable bytes and the immutable file measurements
/// embedded by this crate's actual build script. Neither is taken from the receipt.
/// Unbound builds remain pending; contradictory roots or listed bytes are rejected.
pub fn verify_current_receipt(receipt: Option<&BuildReceipt>) -> io::Result<ReceiptVerification> {
    let current = super::executable()?;
    let hash = current["sha256"]
        .as_str()
        .ok_or_else(|| invalid("current executable hash absent"))?;
    verify_receipt_binding(receipt, hash, &super::build_binding())
}
