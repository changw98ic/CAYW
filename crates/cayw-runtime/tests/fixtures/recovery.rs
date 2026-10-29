//! Offline child for the real-process recovery matrix. No network model is used.
use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_codemode::CapabilityError;
use cayw_host::{ExternalHost, ProcessIo, Profile, ProfileKind};
use cayw_ipc::{Client, ClientConfig, RpcError, RpcHandler, ServerConfig};
use cayw_runtime::{RequestPolicy, Runtime, RuntimeConfig, ScopedPermission};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const WATCHDOG: Duration = Duration::from_secs(120);
pub const SCRIPT: &str = "return cayw.call('search.query', {query='fixture'})";
/// The process profile is configured but the leaf process is only launched
/// after the Core start boundary. Killing at that boundary therefore proves the
/// registered-but-never-launched process scope, not an observed OS process.
pub const PROCESS_SCRIPT: &str = "return cayw.call('test.run', {profile='leaf'})";
const RUN_BUDGET: Duration = Duration::from_secs(600);
const TOKEN: &str = "offline-crash-matrix-authentication-token";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub session_id: String,
    pub task_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hook {
    pub sequence: u64,
    pub point: String,
    pub data: Value,
}
#[derive(Default)]
struct Captured {
    sequence: u64,
    planned: Option<Value>,
    started: Option<Value>,
}
#[derive(Default)]
pub struct Context {
    captured: Mutex<Captured>,
}

pub fn arguments() -> Value {
    json!({"query":"fixture"})
}
pub fn process_arguments() -> Value {
    json!({"profile":"leaf"})
}
pub fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn read_lines(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}
pub fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    sync_directory(path.parent().ok_or("JSON path has no parent")?)?;
    Ok(())
}
fn append(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    sync_directory(path.parent().ok_or("log path has no parent")?)?;
    Ok(())
}

fn install(root: &Path, checkpoint: &str, context: Arc<Context>) -> Result<()> {
    let root = root.to_path_buf();
    let checkpoint = checkpoint.to_owned();
    let gated = AtomicBool::new(false);
    cayw_ai::faults::install(Arc::new(move |point, data| {
        // The only identity accepted by the external service is a trusted
        // Runtime hook. The script arguments contain only the search query.
        let hook = {
            let mut captured = context.captured.lock().expect("origin lock poisoned");
            if point == "core.plan.committed" {
                captured.planned = Some(data.clone());
            }
            if point == "core.start.committed" {
                captured.started = Some(data.clone());
            }
            captured.sequence += 1;
            let hook = Hook {
                sequence: captured.sequence,
                point: point.into(),
                data,
            };
            append(&root.join("hooks.jsonl"), &hook).expect("sync hook original");
            hook
        };
        if point == checkpoint && !gated.swap(true, Ordering::SeqCst) {
            let ready = json!({
                "pid":std::process::id(), "checkpoint":checkpoint,
                "identity":read_json(&root.join("identity.json")).expect("original SDK identity"),
                "hook":hook,
            });
            let pending = root.join("ready.pending");
            write_json(&pending, &ready).expect("sync ready file");
            fs::rename(&pending, root.join("ready.json")).expect("publish ready file");
            sync_directory(&root).expect("sync ready directory");
            // The parent owns the watchdog and sends a real SIGKILL. No
            // in-process panic, cancellation or fake journal row simulates it.
            loop {
                std::thread::park();
            }
        }
    }))
    .map_err(io::Error::other)?;
    Ok(())
}

struct FixedProvider {
    root: PathBuf,
    role: String,
    script: &'static str,
    calls: AtomicU64,
}
impl FixedProvider {
    fn new(root: &Path, role: &str, script: &'static str) -> Self {
        Self {
            root: root.to_owned(),
            role: role.into(),
            script,
            calls: AtomicU64::new(0),
        }
    }
}
impl Provider for FixedProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.role != "original" {
            append(
                &self.root.join(format!("provider-{}.jsonl", self.role)),
                &json!({
                    "index":index, "model":request.model, "messages":request.messages,
                }),
            )
            .expect("record forbidden provider invocation");
            panic!("recovery or observation invoked the model");
        }
        let message = cayw_runtime::fixture::script_response(
            &request,
            self.script,
            &format!("crash-fixture-{index}"),
        );
        append(
            &self.root.join("provider-original.jsonl"),
            &json!({
                "index":index, "model":request.model,
                "messages":request.messages, "response":message,
            }),
        )
        .expect("sync actual offline Provider input and output");
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

struct EffectService {
    root: PathBuf,
    context: Arc<Context>,
}
#[async_trait]
impl ExternalHost for EffectService {
    async fn call(
        &self,
        capability: &str,
        args: Value,
        _: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        if capability != "search.query" || args != arguments() {
            return Err(CapabilityError::new("unexpected fixture service operation"));
        }
        let (planned, started) = {
            let captured = self.context.captured.lock().expect("origin lock poisoned");
            (
                captured
                    .planned
                    .clone()
                    .expect("Host entered without committed Core plan"),
                captured
                    .started
                    .clone()
                    .expect("Host entered without committed Core start"),
            )
        };
        assert_eq!(planned["key"], started["key"]);
        assert_eq!(planned["lease"], started["lease"]);
        assert_eq!(started["name"], capability);
        assert_eq!(started["arguments"], args);
        let prior = read_lines(&self.root.join("external-entries.jsonl"))
            .expect("read real service entries")
            .len();
        let entry_count = prior + 1;
        let origin = json!({"plan":planned,"start":started});
        append(
            &self.root.join("external-entries.jsonl"),
            &json!({
                "entry_count":entry_count, "capability":capability,
                "arguments":args, "trusted_origin":origin,
            }),
        )
        .expect("sync real external entry");
        assert_eq!(entry_count, 1, "the external service was re-entered");
        let effect_path = self.root.join("effect.json");
        let receipt_path = self.root.join("effect-receipt.json");
        let result = json!({"effect_id":uuid::Uuid::new_v4().to_string(),"value":"fixture"});
        let receipt = json!({
            "schema":"cayw.runtime.crash-external-receipt.v1",
            "trusted_origin":origin, "entry_count":entry_count,
            "capability":capability, "arguments":args, "result":result,
            "effect_path":effect_path, "receipt_path":receipt_path,
        });
        let hook = json!({
            "trusted_origin":origin, "entry_count":entry_count,
            "effect_path":effect_path, "receipt_path":receipt_path,
        });
        cayw_ai::faults::hit("host.effect.before", hook.clone());
        let mut effect = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&effect_path)
            .expect("create the unique external effect");
        serde_json::to_writer(
            &mut effect,
            &json!({"result":result,"trusted_origin":origin}),
        )
        .expect("write actual external effect");
        effect.write_all(b"\n").expect("write effect newline");
        let mut evidence = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&receipt_path)
            .expect("create original-identity receipt");
        serde_json::to_writer(&mut evidence, &receipt).expect("write original-identity receipt");
        evidence.write_all(b"\n").expect("write receipt newline");
        cayw_ai::faults::hit("host.effect.visible", hook.clone());
        effect.sync_all().expect("sync external effect");
        evidence.sync_all().expect("sync original-identity receipt");
        sync_directory(&self.root).expect("sync both external directory entries");
        cayw_ai::faults::hit("host.effect.durable", hook);
        Ok(result)
    }
}

fn base_config() -> RuntimeConfig {
    let mut config = RuntimeConfig {
        max_run_time: RUN_BUDGET,
        max_queue_time: RUN_BUDGET,
        ..RuntimeConfig::default()
    };
    config.cells.max_yield_time = Duration::from_secs(300);
    config.codemode.timeout = RUN_BUDGET;
    config.host.call_timeout = RUN_BUDGET;
    config
}

pub fn open(root: &Path, role: &str, context: Arc<Context>) -> Result<Runtime> {
    let mut config = base_config();
    config.permissions = vec![ScopedPermission::exact("search.query", arguments())];
    // Bind a real Requests authority without protecting the test service.
    // The declared protected operation is never executed by this script.
    config.request_policy = Some(RequestPolicy {
        permission_policy_id: "crash-matrix-v1".into(),
        operations: vec![cayw_requests::OperationScope::exact(
            "read-fixture",
            "fs.read",
            json!({"path":"unused-crash-matrix.txt"}),
            "workspace-policy",
        )],
    });
    config.host.external = Some(Arc::new(EffectService {
        root: root.to_owned(),
        context,
    }));
    Ok(Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        Arc::new(FixedProvider::new(root, role, SCRIPT)),
        Model::new("offline-fixture", "recovery-matrix"),
        config,
    )?)
}

/// The exact config a reopened connection and the trusted host must agree on.
/// The leaf profile is a real executable; only `ProcessIo::Pipes` is used.
pub fn process_config(root: &Path, program: &Path) -> RuntimeConfig {
    let mut config = base_config();
    config.permissions = vec![ScopedPermission::exact("test.run", process_arguments())];
    config.host.profiles.insert(
        "leaf".into(),
        Profile {
            kind: ProfileKind::Test,
            program: program.to_owned(),
            args: vec!["leaf".into(), root.to_string_lossy().into_owned()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 4096,
            allow_filter: false,
            io: ProcessIo::Pipes,
        },
    );
    config
}

pub fn open_process(root: &Path, role: &str, program: &Path) -> Result<Runtime> {
    Ok(Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        Arc::new(FixedProvider::new(root, role, PROCESS_SCRIPT)),
        Model::new("offline-fixture", "process-recovery-matrix"),
        process_config(root, program),
    )?)
}

struct ReplyGate(Runtime);
#[async_trait]
impl RpcHandler for ReplyGate {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, RpcError> {
        let original = params.clone();
        let result = self.0.call(method, params, cancel).await;
        if method == "task.resume" {
            let receipt = match &result {
                Ok(value) => json!({"ok":value}),
                Err(error) => json!({"error":error}),
            };
            cayw_ai::faults::hit(
                "rpc.reply.before",
                json!({
                    "method":method,"params":original,"result":receipt,
                }),
            );
        }
        result
    }
}

async fn run(mode: &str, root: &Path, checkpoint: &str) -> Result<()> {
    let identity: Identity = serde_json::from_value(read_json(&root.join("identity.json"))?)?;
    let context = Arc::new(Context::default());
    install(root, checkpoint, context.clone())?;
    match mode {
        "sdk" => {
            let runtime = open(root, "original", context)?;
            let result = runtime
                .resume(&identity.task_id, CancellationToken::new())
                .await?;
            cayw_ai::faults::hit(
                "sdk.reply.before",
                json!({"identity":identity,"result":result}),
            );
            write_json(&root.join("unexpected-completion.json"), &result)?;
        }
        "process" => {
            // The parent supplies the fixture executable because a Cargo bin
            // target cannot read another target's CARGO_BIN_EXE variable.
            let program = std::env::var_os("CAYW_PROCESS_FIXTURE")
                .ok_or("process mode requires CAYW_PROCESS_FIXTURE")?;
            let runtime = open_process(root, "original", Path::new(&program))?;
            let result = runtime
                .resume(&identity.task_id, CancellationToken::new())
                .await?;
            write_json(&root.join("unexpected-completion.json"), &result)?;
        }
        "ipc" => {
            let runtime = open(root, "original", context)?;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            write_json(
                &root.join("ipc-server.json"),
                &json!({"address":address,"authenticated":true}),
            )?;
            let stop = CancellationToken::new();
            let mut config = ServerConfig::new(TOKEN);
            config.request_timeout = RUN_BUDGET;
            config.frame_timeout = RUN_BUDGET;
            let server = tokio::spawn(cayw_ipc::serve(
                listener,
                Arc::new(ReplyGate(runtime)),
                config,
                stop.clone(),
            ));
            let client = Client::connect_with_config(
                address,
                TOKEN,
                ClientConfig {
                    request_timeout: RUN_BUDGET,
                    ..ClientConfig::default()
                },
            )
            .await?;
            // This result can only arrive after the real authenticated hello.
            let health = client.request("runtime.health", json!({})).await?;
            write_json(&root.join("ipc-authenticated-health.json"), &health)?;
            let result = client
                .request("task.resume", json!({"task_id":identity.task_id}))
                .await?;
            write_json(&root.join("unexpected-completion.json"), &result)?;
            client.close().await;
            stop.cancel();
            server.await??;
        }
        _ => return Err("expected sdk, ipc or process mode".into()),
    }
    Err(format!("checkpoint {checkpoint} was not reached").into())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 || args[0] != "run" {
        return Err(
            "usage: cayw-runtime-recovery-fixture run sdk|ipc|process root checkpoint".into(),
        );
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(run(&args[1], Path::new(&args[2]), &args[3]))
}
