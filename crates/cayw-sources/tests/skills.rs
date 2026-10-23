use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{
    AccessRequest, DispatchDropState, DispatchJournal, DispatchOutcome, DispatchRecord,
    InvocationContext, Limits, Operation, Provenance, Registry, RegistryPolicy, RequestKey,
    SourceHandle, SourceIdentity, SourceOperation, SourceRequest, Visibility, content_sha256,
};
use cayw_codemode::CapabilityError;
use cayw_sources::skills::{FileSkill, FileSkillStats, FileSkills};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

struct Policy {
    names: bool,
    contents: bool,
}

#[async_trait]
impl RegistryPolicy for Policy {
    fn visible(&self, kind: Visibility, _: &Provenance, _: &str) -> bool {
        match kind {
            Visibility::SkillName => self.names,
            Visibility::SkillContent => self.contents,
            _ => false,
        }
    }

    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

struct Journal;

#[async_trait]
impl DispatchJournal for Journal {
    async fn started(&self, _: &DispatchRecord) -> Result<(), CapabilityError> {
        Ok(())
    }

    async fn settled(
        &self,
        _: &DispatchRecord,
        _: &DispatchOutcome,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }

    fn dropped(&self, _: &DispatchRecord, _: DispatchDropState) -> Result<(), CapabilityError> {
        Ok(())
    }
}

fn entry(name: &str, description: &str, path: impl AsRef<Path>) -> FileSkill {
    FileSkill {
        name: name.into(),
        description: description.into(),
        path: path.as_ref().to_path_buf(),
    }
}

fn load_ok(entries: Vec<FileSkill>, limits: &Limits) -> FileSkills {
    match FileSkills::load(entries, limits) {
        Ok(skills) => skills,
        Err(error) => panic!("load should succeed: {}", error.message),
    }
}

fn load_error(entries: Vec<FileSkill>, limits: &Limits, label: &str) -> CapabilityError {
    match FileSkills::load(entries, limits) {
        Ok(_) => panic!("{label}: load should fail"),
        Err(error) => {
            assert!(
                !error.outcome_unknown,
                "{label}: load rejection must be a known failure"
            );
            error
        }
    }
}

fn snapshot(stats: &FileSkillStats) -> Value {
    serde_json::to_value(stats.snapshot()).expect("stats snapshot must serialize")
}

fn counter(stats: &FileSkillStats, key: &str) -> u64 {
    snapshot(stats)
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("snapshot must contain numeric {key}"))
}

fn instance_id(stats: &FileSkillStats) -> String {
    snapshot(stats)
        .get("instance_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("snapshot must contain string instance_id"))
}

fn split(skills: FileSkills) -> (cayw_bridge::Manifest, Box<dyn SourceHandle>) {
    let manifest = skills.manifest().clone();
    (manifest, skills.into_handle())
}

fn identity(id: &str, version: &str) -> SourceIdentity {
    SourceIdentity {
        id: id.into(),
        version: version.into(),
        origin: "host:file-skills".into(),
    }
}

fn invocation(sequence: u64) -> InvocationContext {
    InvocationContext {
        session_id: "session".into(),
        task_id: "task".into(),
        attempt_id: "attempt".into(),
        execution_id: "execution".into(),
        sequence,
    }
}

fn read_request(name: &str, id: u64, max_result_bytes: usize) -> SourceRequest {
    SourceRequest {
        key: RequestKey {
            id,
            source: Provenance {
                registry_incarnation: "test-registry".into(),
                source: SourceIdentity {
                    id: "files".into(),
                    version: "test".into(),
                    origin: "host:test".into(),
                },
                generation: 1,
                manifest_sha256: "0".repeat(64),
            },
            operation: Operation::ReadSkill,
            target: name.into(),
        },
        operation: SourceOperation::ReadSkill { name: name.into() },
        max_result_bytes,
    }
}

async fn direct_read_with(
    handle: &dyn SourceHandle,
    name: &str,
    max_result_bytes: usize,
    cancel: CancellationToken,
) -> Result<String, CapabilityError> {
    let request = read_request(name, 7, max_result_bytes);
    let key = request.key.clone();
    let reply = handle.request(request, cancel).await?;
    assert_eq!(reply.key, key, "source must echo the full request key");
    reply
        .value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| CapabilityError::new("skill reply must be a UTF-8 string"))
}

async fn direct_read(
    handle: &dyn SourceHandle,
    name: &str,
    max_result_bytes: usize,
) -> Result<String, CapabilityError> {
    direct_read_with(handle, name, max_result_bytes, CancellationToken::new()).await
}

fn expect_ok(result: Result<String, CapabilityError>) -> String {
    match result {
        Ok(content) => content,
        Err(error) => panic!("read should succeed: {}", error.message),
    }
}

fn expect_known_error(result: Result<String, CapabilityError>) -> CapabilityError {
    match result {
        Ok(content) => panic!("read should fail, got: {content:?}"),
        Err(error) => {
            assert!(
                !error.outcome_unknown,
                "read rejection must be a known failure: {}",
                error.message
            );
            error
        }
    }
}

async fn call(
    registry: &Registry,
    sequence: u64,
    capability: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    registry
        .consumer()
        .scoped(capability, invocation(sequence), Arc::new(Journal))
        .expect("scoped bridge")
        .call(capability, arguments, CancellationToken::new())
        .await
}

#[tokio::test]
async fn loads_real_markdown_and_lists_without_reading_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let alpha = "# Alpha\n\nbody alpha marker\n";
    let faq = "# FAQ\n\nbody faq marker\n";
    std::fs::write(dir.path().join("alpha.md"), alpha).unwrap();
    std::fs::write(dir.path().join("faq.md"), faq).unwrap();

    let limits = Limits::default();
    let skills = load_ok(
        vec![
            entry("alpha", "Alpha guide", dir.path().join("alpha.md")),
            entry("faq", "FAQ guide", dir.path().join("faq.md")),
        ],
        &limits,
    );

    let manifest = skills.manifest();
    assert!(manifest.tools.is_empty());
    assert!(manifest.resources.is_empty());
    assert!(!manifest.search);
    assert_eq!(manifest.skills.len(), 2);
    let alpha_spec = manifest
        .skills
        .iter()
        .find(|skill| skill.name == "alpha")
        .expect("alpha skill");
    assert_eq!(alpha_spec.description, "Alpha guide");
    assert_eq!(alpha_spec.content_sha256, content_sha256(alpha));

    let stats = skills.stats();
    let cloned = stats.clone();
    assert_eq!(snapshot(&stats), snapshot(&cloned));
    assert_eq!(counter(&stats, "load_scans"), 2);
    assert_eq!(counter(&stats, "on_demand_reads"), 0);
    assert!(!snapshot(&stats)["closed"].as_bool().unwrap());
    assert!(!instance_id(&stats).is_empty());

    let (manifest, handle) = split(skills);
    let registry = Registry::new(
        limits,
        Arc::new(Policy {
            names: true,
            contents: true,
        }),
    )
    .unwrap();
    registry
        .register(identity("files", "v1"), manifest, handle)
        .unwrap();

    let listed = call(&registry, 1, "skills.list", json!({})).await.unwrap();
    let listed_text = listed.to_string();
    assert!(
        !listed_text.contains("body alpha marker"),
        "listing must not read the skill body"
    );
    assert!(
        !listed_text.contains("body faq marker"),
        "listing must not read the skill body"
    );
    assert_eq!(listed["skills"].as_array().unwrap().len(), 2);
    assert_eq!(counter(&stats, "on_demand_reads"), 0);

    let content = call(&registry, 2, "skills.read", json!({"name": "files/alpha"}))
        .await
        .unwrap();
    assert_eq!(content["value"].as_str().unwrap(), alpha);
    assert!(counter(&stats, "on_demand_reads") >= 1);

    registry.disconnect("files").unwrap();
    assert!(snapshot(&stats)["closed"].as_bool().unwrap());
}

#[test]
fn rejects_unsafe_or_unbounded_inputs_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits::default();

    let text = dir.path().join("text.md");
    std::fs::write(&text, "valid").unwrap();

    let non_utf8 = dir.path().join("binary.md");
    std::fs::write(&non_utf8, [0xff_u8, 0xfe, 0xfd]).unwrap();
    load_error(
        vec![entry("binary", "d", &non_utf8)],
        &limits,
        "non-UTF-8 Markdown",
    );

    let txt = dir.path().join("skill.txt");
    std::fs::write(&txt, "text").unwrap();
    load_error(
        vec![entry("text", "d", &txt)],
        &limits,
        "non-Markdown extension",
    );

    let directory = dir.path().join("directory.md");
    std::fs::create_dir(&directory).unwrap();
    load_error(
        vec![entry("directory", "d", &directory)],
        &limits,
        "directory path",
    );

    let leaf_link = dir.path().join("leaf-link.md");
    std::os::unix::fs::symlink(&text, &leaf_link).unwrap();
    load_error(
        vec![entry("leaf", "d", &leaf_link)],
        &limits,
        "leaf symlink",
    );

    let real_parent = dir.path().join("real-parent");
    std::fs::create_dir(&real_parent).unwrap();
    std::fs::write(real_parent.join("inner.md"), "inner").unwrap();
    let linked_parent = dir.path().join("linked-parent");
    std::os::unix::fs::symlink(&real_parent, &linked_parent).unwrap();
    load_error(
        vec![entry("inner", "d", linked_parent.join("inner.md"))],
        &limits,
        "symlinked parent component",
    );

    load_error(
        vec![
            entry("same", "first", &text),
            entry("same", "second", &text),
        ],
        &limits,
        "duplicate logical names",
    );

    for invalid in ["", "bad/name", "a b", "a\nb"] {
        load_error(
            vec![entry(invalid, "d", &text)],
            &limits,
            "invalid logical name",
        );
    }
    load_error(
        vec![entry(&"x".repeat(129), "d", &text)],
        &limits,
        "oversized logical name",
    );
    load_error(
        vec![entry("../real", "d", &text)],
        &limits,
        "path-like logical name",
    );

    let mut small = limits;
    small.max_skill_bytes = 4;
    let oversized = dir.path().join("oversized.md");
    std::fs::write(&oversized, "12345").unwrap();
    load_error(
        vec![entry("oversized", "d", &oversized)],
        &small,
        "file above max_skill_bytes",
    );

    let exact = dir.path().join("exact.md");
    std::fs::write(&exact, "1234").unwrap();
    let skills = load_ok(vec![entry("exact", "d", &exact)], &small);
    assert_eq!(
        skills.manifest().skills[0].content_sha256,
        content_sha256("1234")
    );
}

#[tokio::test]
async fn rejects_same_size_edits_and_same_content_inode_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits::default();

    let same_size = dir.path().join("same-size.md");
    std::fs::write(&same_size, "AAAA").unwrap();
    let skills = load_ok(vec![entry("change", "d", &same_size)], &limits);
    let stats = skills.stats();
    let handle = skills.into_handle();

    std::fs::write(&same_size, "BBBB").unwrap();
    expect_known_error(direct_read(&*handle, "change", 1024).await);
    assert!(
        counter(&stats, "rejections") >= 1,
        "same-size change must count as a rejection"
    );

    let replacement = dir.path().join("replacement.md");
    std::fs::write(&replacement, "ORIGINAL").unwrap();
    let skills = load_ok(vec![entry("inode", "d", &replacement)], &limits);
    let handle = skills.into_handle();

    let staged = dir.path().join("staged.md");
    std::fs::write(&staged, "ORIGINAL").unwrap();
    std::fs::rename(&staged, &replacement).unwrap();
    expect_known_error(direct_read(&*handle, "inode", 1024).await);
}

#[tokio::test]
async fn rejects_leaf_symlink_and_parent_directory_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits::default();

    let leaf = dir.path().join("leaf.md");
    std::fs::write(&leaf, "ORIGINAL-LEAF").unwrap();
    let skills = load_ok(vec![entry("leaf", "d", &leaf)], &limits);
    let handle = skills.into_handle();
    let moved_leaf = dir.path().join("leaf-original.md");
    std::fs::rename(&leaf, &moved_leaf).unwrap();
    let attacker = dir.path().join("attacker.md");
    std::fs::write(&attacker, "ATTACKER-LEAF").unwrap();
    std::os::unix::fs::symlink(&attacker, &leaf).unwrap();
    expect_known_error(direct_read(&*handle, "leaf", 1024).await);

    let parent = dir.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join("skill.md"), "ORIGINAL-PARENT").unwrap();
    let skills = load_ok(vec![entry("nested", "d", parent.join("skill.md"))], &limits);
    let handle = skills.into_handle();
    let moved_parent = dir.path().join("parent-old");
    std::fs::rename(&parent, &moved_parent).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join("skill.md"), "ATTACKER-PARENT").unwrap();
    expect_known_error(direct_read(&*handle, "nested", 1024).await);

    let linked_parent = dir.path().join("linked-parent");
    std::fs::create_dir(&linked_parent).unwrap();
    std::fs::write(linked_parent.join("skill.md"), "ORIGINAL-LINK").unwrap();
    let skills = load_ok(
        vec![entry("linked", "d", linked_parent.join("skill.md"))],
        &limits,
    );
    let handle = skills.into_handle();
    let moved_link = dir.path().join("linked-parent-old");
    std::fs::rename(&linked_parent, &moved_link).unwrap();
    let evil = dir.path().join("evil");
    std::fs::create_dir(&evil).unwrap();
    std::fs::write(evil.join("skill.md"), "ATTACKER-LINK").unwrap();
    std::os::unix::fs::symlink(&evil, &linked_parent).unwrap();
    expect_known_error(direct_read(&*handle, "linked", 1024).await);
}

#[tokio::test]
async fn bounds_result_size_and_honors_cancel_and_close_without_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits::default();

    let bounded = dir.path().join("bounded.md");
    std::fs::write(&bounded, "12345678").unwrap();
    let skills = load_ok(vec![entry("bounded", "d", &bounded)], &limits);
    let stats = skills.stats();
    let handle = skills.into_handle();

    expect_known_error(direct_read(&*handle, "bounded", 4).await);
    assert!(
        counter(&stats, "rejections") >= 1,
        "a result above max_result_bytes must count as a rejection"
    );

    let cancel = CancellationToken::new();
    cancel.cancel();
    let before = counter(&stats, "on_demand_reads");
    expect_known_error(direct_read_with(&*handle, "bounded", 1024, cancel).await);
    assert_eq!(
        counter(&stats, "on_demand_reads"),
        before,
        "a pre-cancelled read must not scan the file"
    );

    handle.disconnect();
    assert!(snapshot(&stats)["closed"].as_bool().unwrap());
    expect_known_error(direct_read(&*handle, "bounded", 1024).await);

    let big = "a".repeat(256 * 1024);
    let big_path = dir.path().join("big.md");
    std::fs::write(&big_path, &big).unwrap();
    let large_limits = Limits {
        max_skill_bytes: 512 * 1024,
        ..Default::default()
    };
    let skills = load_ok(vec![entry("big", "d", &big_path)], &large_limits);
    let handle = skills.into_handle();
    let cancel = CancellationToken::new();
    let pending = direct_read_with(&*handle, "big", 512 * 1024, cancel.clone());
    tokio::pin!(pending);
    let mut interrupted = false;
    tokio::select! {
        result = &mut pending => {
            match result {
                Ok(content) => assert_eq!(content, big),
                Err(error) => assert!(
                    !error.outcome_unknown,
                    "a finished read must not be unknown: {}",
                    error.message
                ),
            }
        }
        _ = async {
            tokio::task::yield_now().await;
            cancel.cancel();
        } => {
            interrupted = true;
        }
    }
    if interrupted {
        match pending.await {
            Ok(content) => assert_eq!(content, big),
            Err(error) => assert!(
                !error.outcome_unknown,
                "cancellation after start must stay a known failure: {}",
                error.message
            ),
        }
    }
}

#[tokio::test]
async fn serialized_skill_result_must_fit_the_original_byte_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("escaped.md");
    let body = "\"".repeat(8);
    std::fs::write(&path, &body).unwrap();
    let skills = load_ok(
        vec![entry("escaped", "Escaped text", &path)],
        &Limits::default(),
    );
    let stats = skills.stats();
    let handle = skills.into_handle();
    assert_eq!(body.len(), 8);
    assert_eq!(serde_json::to_vec(&body).unwrap().len(), 18);
    let error = expect_known_error(direct_read(&*handle, "escaped", 10).await);
    assert!(error.message.contains("result byte budget"));
    assert_eq!(counter(&stats, "rejections"), 1);
    assert_eq!(expect_ok(direct_read(&*handle, "escaped", 18).await), body);
}

#[tokio::test]
async fn reconnect_replaces_generation_and_fences_old_consumer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.md");
    std::fs::write(&path, "version one").unwrap();
    let limits = Limits::default();

    let v1 = load_ok(vec![entry("alpha", "Alpha", &path)], &limits);
    let stats1 = v1.stats();
    let (manifest1, handle1) = split(v1);
    let registry = Registry::new(
        limits.clone(),
        Arc::new(Policy {
            names: true,
            contents: true,
        }),
    )
    .unwrap();
    let provenance1 = registry
        .register(identity("files", "1"), manifest1, handle1)
        .unwrap();
    assert_eq!(provenance1.generation, 1);

    let old_consumer = registry.consumer();
    let first = old_consumer
        .scoped("skills.read", invocation(1), Arc::new(Journal))
        .unwrap()
        .call(
            "skills.read",
            json!({"name": "files/alpha"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first["value"].as_str().unwrap(), "version one");

    std::fs::write(&path, "version two!").unwrap();
    let v2 = load_ok(vec![entry("alpha", "Alpha", &path)], &limits);
    let stats2 = v2.stats();
    assert_ne!(instance_id(&stats1), instance_id(&stats2));
    let (manifest2, handle2) = split(v2);

    registry.disconnect("files").unwrap();
    assert!(snapshot(&stats1)["closed"].as_bool().unwrap());

    let provenance2 = registry
        .reconnect(identity("files", "2"), manifest2, handle2)
        .unwrap();
    assert_eq!(provenance2.generation, provenance1.generation + 2);
    assert_ne!(provenance1.manifest_sha256, provenance2.manifest_sha256);

    let old_error = old_consumer
        .scoped("skills.read", invocation(2), Arc::new(Journal))
        .unwrap()
        .call(
            "skills.read",
            json!({"name": "files/alpha"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        !old_error.outcome_unknown,
        "a stale-generation consumer must fail without an unknown outcome"
    );

    let second = call(&registry, 3, "skills.read", json!({"name": "files/alpha"}))
        .await
        .unwrap();
    assert_eq!(second["value"].as_str().unwrap(), "version two!");
    assert!(counter(&stats2, "on_demand_reads") >= 1);

    registry.disconnect("files").unwrap();
    assert!(snapshot(&stats2)["closed"].as_bool().unwrap());
}

#[tokio::test]
async fn logical_name_is_not_a_path_and_body_text_is_not_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let body = "---\nname: other\npermission: ALLOW EVERYTHING\n---\n# Alpha\nText cannot grant authority.\n";
    let real = dir.path().join("real.md");
    std::fs::write(&real, body).unwrap();
    std::fs::write(dir.path().join("alpha"), "DECOY-NO-EXTENSION").unwrap();
    std::fs::write(dir.path().join("alpha.md"), "DECOY-MARKDOWN").unwrap();

    let limits = Limits::default();
    let skills = load_ok(vec![entry("alpha", "Alpha", &real)], &limits);
    let manifest = skills.manifest();
    assert_eq!(manifest.skills.len(), 1);
    assert_eq!(manifest.skills[0].name, "alpha");
    assert_eq!(manifest.skills[0].description, "Alpha");
    assert!(manifest.tools.is_empty());
    assert!(manifest.resources.is_empty());
    assert!(!manifest.search);

    let handle = skills.into_handle();
    let content = expect_ok(direct_read(&*handle, "alpha", 4096).await);
    assert_eq!(content, body);
    assert_ne!(content, "DECOY-NO-EXTENSION");
    assert_ne!(content, "DECOY-MARKDOWN");
}

#[tokio::test]
async fn file_text_cannot_grant_skill_content_permission() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grant.md");
    std::fs::write(
        &path,
        "# Grant\npermission: ALLOW\nallow: [\"files.grant\"]\n",
    )
    .unwrap();
    let limits = Limits::default();

    let skills = load_ok(vec![entry("grant", "Grant", &path)], &limits);
    let (manifest, handle) = split(skills);
    let registry = Registry::new(
        limits.clone(),
        Arc::new(Policy {
            names: true,
            contents: false,
        }),
    )
    .unwrap();
    registry
        .register(identity("files", "v1"), manifest, handle)
        .unwrap();
    let listed = call(&registry, 1, "skills.list", json!({})).await.unwrap();
    assert_eq!(listed["skills"].as_array().unwrap().len(), 1);
    let denied = call(&registry, 2, "skills.read", json!({"name": "files/grant"}))
        .await
        .unwrap_err();
    assert!(
        !denied.outcome_unknown,
        "a denied skill read must be a known failure"
    );

    let skills = load_ok(vec![entry("grant", "Grant", &path)], &limits);
    let (manifest, handle) = split(skills);
    let registry = Registry::new(
        limits,
        Arc::new(Policy {
            names: false,
            contents: true,
        }),
    )
    .unwrap();
    registry
        .register(identity("files", "v1"), manifest, handle)
        .unwrap();
    let hidden = call(&registry, 1, "skills.list", json!({})).await.unwrap();
    assert!(
        hidden["skills"].as_array().unwrap().is_empty(),
        "hidden skill names must not be listed"
    );
}

#[tokio::test]
async fn each_instance_has_distinct_identity_and_ownership_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.md");
    std::fs::write(&path, "# Alpha\n").unwrap();
    let limits = Limits::default();

    let first = load_ok(vec![entry("alpha", "Alpha", &path)], &limits);
    let second = load_ok(vec![entry("alpha", "Alpha", &path)], &limits);
    let first_stats = first.stats();
    let second_stats = second.stats();
    assert_ne!(instance_id(&first_stats), instance_id(&second_stats));

    let (first_manifest, first_handle) = split(first);
    let (second_manifest, second_handle) = split(second);
    assert_ne!(
        first_handle.ownership_key(),
        second_handle.ownership_key(),
        "each loader instance needs its own opaque owner key"
    );

    let registry = Registry::new(
        limits,
        Arc::new(Policy {
            names: true,
            contents: true,
        }),
    )
    .unwrap();
    registry
        .register(identity("files-a", "v1"), first_manifest, first_handle)
        .unwrap();
    registry
        .register(identity("files-b", "v1"), second_manifest, second_handle)
        .unwrap();
}
