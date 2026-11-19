use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use serde_json::json;
use std::sync::Arc;

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: cayw_runtime::fixture::script_response(&request, &self.0, "memory-policy"),
        }]))
    }
}

#[tokio::test]
async fn main_sdk_without_router_uses_volume_aware_host_protection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("memory")).unwrap();
    let insensitive = root.join("Memory").exists();
    if !insensitive {
        std::fs::create_dir(root.join("Memory")).unwrap();
    }
    let arguments = json!({"path":"Memory/new.txt", "content":"model"});
    let control = json!({"path":"control.txt", "content":"finished"});
    let script = format!(
        r#"
local allowed = pcall(function() cayw.call('fs.write', {{path='Memory/new.txt',content='model'}}) end)
assert(allowed == {expected}, 'volume-aware protection returned wrong outcome')
cayw.call('fs.write', {{path='control.txt',content='finished'}})
return {{checked=true}}
"#,
        expected = if insensitive { "false" } else { "true" }
    );
    let runtime = Runtime::open(
        temp.path().join("session.db"),
        &root,
        Arc::new(Script(script)),
        Model::new("fixture", "memory-policy"),
        RuntimeConfig {
            main_session: true,
            permissions: vec![
                ScopedPermission::exact("fs.write", arguments),
                ScopedPermission::exact("fs.write", control),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let session = runtime.create(None).unwrap();
    runtime
        .prompt(
            &session.id,
            "verify memory protection",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("control.txt")).unwrap(),
        "finished"
    );
    assert!(!root.join("memory/new.txt").exists());
    if !insensitive {
        assert_eq!(
            std::fs::read_to_string(root.join("Memory/new.txt")).unwrap(),
            "model"
        );
    }
    runtime.shutdown_background();
}
