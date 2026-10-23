use cayw_codemode::{ErrorKind, Limits};
use serde_json::{Value, json};

pub const FIRST: &str = "cayw.emit({phase = 'first'}); return {answer = 42}";
pub const PIPELINE_CONTENT: &str = "offline CAYW\n第二行\n";
pub const REPAIR_CONTENT: &str = "repaired offline fixture\n";
pub const NUMBERS: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
pub const BEFORE_LIMIT: &str = "completed before limit\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    Cold,
    Warm,
    Files,
    SyntaxFailure,
    SyntaxRepair,
    LoopBudget,
    Cancel,
    GateDenial,
    Sandbox,
    UnknownCapability,
    Arguments,
    Calculation,
    Signatures,
    OutputBudget,
    MemoryBudget,
    CallBudget,
    ActiveTimeout,
    HostTimeout,
}
pub const CASES: [Case; 18] = [
    Case::Cold,
    Case::Warm,
    Case::Files,
    Case::SyntaxFailure,
    Case::SyntaxRepair,
    Case::LoopBudget,
    Case::Cancel,
    Case::GateDenial,
    Case::Sandbox,
    Case::UnknownCapability,
    Case::Arguments,
    Case::Calculation,
    Case::Signatures,
    Case::OutputBudget,
    Case::MemoryBudget,
    Case::CallBudget,
    Case::ActiveTimeout,
    Case::HostTimeout,
];

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cold => "cold_process_first_success",
            Self::Warm => "warm_process_first_success",
            Self::Files => "file_pipeline",
            Self::SyntaxFailure => "syntax_failure",
            Self::SyntaxRepair => "syntax_repair_success",
            Self::LoopBudget => "loop_budget",
            Self::Cancel => "cancel_pending_host",
            Self::GateDenial => "host_permission_denied",
            Self::Sandbox => "sandbox_globals",
            Self::UnknownCapability => "unknown_capability",
            Self::Arguments => "invalid_arguments",
            Self::Calculation => "file_calculation",
            Self::Signatures => "capability_signatures",
            Self::OutputBudget => "output_budget",
            Self::MemoryBudget => "memory_budget",
            Self::CallBudget => "host_call_budget",
            Self::ActiveTimeout => "active_timeout",
            Self::HostTimeout => "timeout_pending_host",
        }
    }
    pub fn workspace_name(self) -> &'static str {
        match self {
            Self::SyntaxFailure | Self::SyntaxRepair => "syntax-repair",
            _ => self.name(),
        }
    }
    pub fn script(self) -> String {
        match self {
            Self::Cold | Self::Warm => FIRST.into(),
            Self::Files => format!(r#"
local written = cayw.call("fs.write", {{path = "pipeline.txt", content = {content}}})
local read = cayw.call("fs.read", {{path = "pipeline.txt"}})
local listing = cayw.call("fs.list", {{path = "."}})
local stat = cayw.call("fs.stat", {{path = "pipeline.txt"}})
local found = false
for _, entry in listing.entries do
    if entry.name == "pipeline.txt" and entry.kind == "file" then found = true end
end
cayw.emit({{content = read.content, sha256 = read.sha256}})
return {{content = read.content, sha256 = read.sha256, bytes = read.bytes,
    same_hash = written.sha256 == read.sha256, listed = found, kind = stat.kind}}
"#, content = serde_json::to_string(PIPELINE_CONTENT).expect("serialize fixed string")),
            Self::SyntaxFailure => format!(r#"
local written = cayw.call("fs.write", {{path = "repair.txt", content = {content}}})
local read =
return read
"#, content = serde_json::to_string(REPAIR_CONTENT).expect("serialize fixed string")),
            Self::SyntaxRepair => format!(r#"
local written = cayw.call("fs.write", {{path = "repair.txt", content = {content}}})
local read = cayw.call("fs.read", {{path = "repair.txt"}})
cayw.emit(read)
return {{content = read.content, sha256 = read.sha256, same_hash = written.sha256 == read.sha256}}
"#, content = serde_json::to_string(REPAIR_CONTENT).expect("serialize fixed string")),
            Self::LoopBudget => "while true do end".into(),
            Self::Cancel | Self::HostTimeout => format!(
                "return cayw.call('search.query', {{query = {:?}}})",
                crate::fixtures::BLOCK_QUERY
            ),
            Self::GateDenial => "return cayw.call('fs.write', {path = 'denied.txt', content = 'must not be written'})".into(),
            Self::Sandbox => r#"return {
    os_absent = os == nil, io_absent = io == nil, process_absent = process == nil,
    require_absent = require == nil, loadfile_absent = loadfile == nil,
    dofile_absent = dofile == nil, debug_absent = debug == nil,
    capabilities = cayw.discover()
}"#.into(),
            Self::UnknownCapability => "return cayw.call('shell.exec', {command = 'fixture never runs a shell'})".into(),
            Self::Arguments => "return cayw.call('fs.read', {path = 42})".into(),
            Self::Calculation => format!(r#"
local written = cayw.call("fs.write", {{path = "numbers.txt", content = {content}}})
local read = cayw.call("fs.read", {{path = "numbers.txt"}})
local sum = 0
local count = 0
for line in string.gmatch(read.content, "%d+") do
    local value = tonumber(line)
    if value % 2 == 0 then sum += value; count += 1 end
end
return {{even_sum = sum, even_count = count, input_bytes = #read.content}}
"#, content = serde_json::to_string(NUMBERS).expect("serialize fixed numbers")),
            Self::Signatures => r#"
local signatures = cayw.array()
for _, name in {"fs.list", "fs.read", "fs.stat", "fs.write"} do
    table.insert(signatures, cayw.describe(name))
end
return signatures
"#.into(),
            Self::OutputBudget => format!(r#"
cayw.call("fs.write", {{path = "before.txt", content = {content}}})
cayw.emit({{phase = "before_limit"}})
cayw.emit(string.rep("x", 8192))
cayw.call("fs.write", {{path = "after.txt", content = "must not be written"}})
"#, content = serde_json::to_string(BEFORE_LIMIT).expect("serialize fixed content")),
            Self::MemoryBudget => format!(r#"
cayw.call("fs.write", {{path = "before.txt", content = {content}}})
local allocated = string.rep("x", 8 * 1024 * 1024)
cayw.call("fs.write", {{path = "after.txt", content = "must not be written"}})
return #allocated
"#, content = serde_json::to_string(BEFORE_LIMIT).expect("serialize fixed content")),
            Self::CallBudget => format!(r#"
cayw.call("fs.write", {{path = "before.txt", content = {content}}})
cayw.call("fs.read", {{path = "before.txt"}})
cayw.call("fs.stat", {{path = "before.txt"}})
cayw.call("fs.write", {{path = "after.txt", content = "must not be written"}})
"#, content = serde_json::to_string(BEFORE_LIMIT).expect("serialize fixed content")),
            Self::ActiveTimeout => "while true do end".into(),
        }
    }
    pub fn grants(self) -> Vec<(String, Value)> {
        let write = |path, content| ("fs.write".into(), json!({"path":path,"content":content}));
        let read = |path| ("fs.read".into(), json!({"path":path}));
        match self {
            Self::Files => vec![
                write("pipeline.txt", PIPELINE_CONTENT),
                read("pipeline.txt"),
                ("fs.list".into(), json!({"path":"."})),
                ("fs.stat".into(), json!({"path":"pipeline.txt"})),
            ],
            Self::SyntaxFailure | Self::SyntaxRepair => {
                vec![write("repair.txt", REPAIR_CONTENT), read("repair.txt")]
            }
            Self::Cancel | Self::HostTimeout => vec![(
                "search.query".into(),
                json!({"query":crate::fixtures::BLOCK_QUERY}),
            )],
            Self::Calculation => vec![write("numbers.txt", NUMBERS), read("numbers.txt")],
            Self::OutputBudget | Self::MemoryBudget | Self::CallBudget => {
                let mut grants = vec![write("before.txt", BEFORE_LIMIT)];
                if self == Self::CallBudget {
                    grants.push(read("before.txt"));
                    grants.push(("fs.stat".into(), json!({"path":"before.txt"})));
                }
                grants.push(write("after.txt", "must not be written"));
                grants
            }
            _ => vec![],
        }
    }
    /// Calls that must actually reach the Host, independent of the available grants.
    /// A syntax error has grants prepared but dispatches nothing; a denied call
    /// reaches the gate without receiving a grant or starting an audit record.
    pub fn expected_operations(self) -> Vec<(String, Value)> {
        match self {
            Self::Files
            | Self::SyntaxRepair
            | Self::Cancel
            | Self::HostTimeout
            | Self::Calculation => self.grants(),
            Self::OutputBudget | Self::MemoryBudget => self.grants().into_iter().take(1).collect(),
            Self::CallBudget => self.grants().into_iter().take(2).collect(),
            Self::GateDenial => vec![(
                "fs.write".into(),
                json!({"path":"denied.txt","content":"must not be written"}),
            )],
            _ => vec![],
        }
    }
    pub fn expected_error(self) -> Option<ErrorKind> {
        match self {
            Self::SyntaxFailure => Some(ErrorKind::Script),
            Self::LoopBudget => Some(ErrorKind::InstructionLimit),
            Self::Cancel => Some(ErrorKind::Cancelled),
            Self::GateDenial => Some(ErrorKind::Capability),
            Self::UnknownCapability => Some(ErrorKind::UnknownCapability),
            Self::Arguments => Some(ErrorKind::Arguments),
            Self::OutputBudget => Some(ErrorKind::OutputLimit),
            Self::MemoryBudget => Some(ErrorKind::MemoryLimit),
            Self::CallBudget => Some(ErrorKind::HostCallLimit),
            Self::ActiveTimeout | Self::HostTimeout => Some(ErrorKind::Timeout),
            _ => None,
        }
    }
    pub fn limits(self) -> Limits {
        let mut limits = Limits::default();
        match self {
            Self::LoopBudget => limits.instruction_budget = 2_000,
            Self::OutputBudget => limits.output_bytes = 4096,
            Self::MemoryBudget => limits.memory_bytes = 1024 * 1024,
            Self::CallBudget => limits.host_calls = 2,
            Self::ActiveTimeout => {
                limits.instruction_budget = u64::MAX;
                limits.timeout = std::time::Duration::from_secs(1);
            }
            Self::HostTimeout => {
                limits.instruction_budget = u64::MAX;
                limits.host_wait = std::time::Duration::from_secs(1);
            }
            _ => {}
        }
        limits
    }
    pub fn expected_calls(self) -> usize {
        self.expected_operations().len()
    }
    pub fn signature_names(self) -> &'static [&'static str] {
        match self {
            Self::Files | Self::Signatures => &["fs.list", "fs.read", "fs.stat", "fs.write"],
            Self::SyntaxFailure | Self::SyntaxRepair | Self::Calculation => {
                &["fs.read", "fs.write"]
            }
            Self::OutputBudget | Self::MemoryBudget | Self::GateDenial => &["fs.write"],
            Self::CallBudget => &["fs.read", "fs.stat", "fs.write"],
            Self::Arguments => &["fs.read"],
            Self::Cancel | Self::HostTimeout => &["search.query"],
            _ => &[],
        }
    }
    pub fn tool_signatures(self) -> Value {
        let path = json!({"type":"string","minLength":1,"maxLength":4096});
        Value::Array(self.signature_names().iter().map(|name| {
            let (description, properties, required) = match *name {
                "fs.read" => ("Read one bounded UTF-8 regular file and its SHA-256.", json!({"path":path}), vec!["path"]),
                "fs.list" => ("List bounded direct directory entries without following symbolic links.", json!({"path":path}), vec!["path"]),
                "fs.stat" => ("Inspect an entry without following symbolic links.", json!({"path":path}), vec!["path"]),
                "fs.write" => ("Atomically create a UTF-8 file. Replacing an existing file requires overwrite=true and expected_sha256.", json!({
                    "path":path,"content":{"type":"string"},"overwrite":{"type":"boolean"},
                    "expected_sha256":{"type":"string","pattern":"^[0-9a-fA-F]{64}$"}
                }), vec!["path","content"]),
                "search.query" => ("Query the application's existing bounded search service.", json!({
                    "query":{"type":"string","minLength":1,"maxLength":8192},"path":path,
                    "limit":{"type":"integer","minimum":1,"maximum":1000}
                }), vec!["query"]),
                _ => unreachable!("fixed signature name"),
            };
            json!({"name":name,"description":description,"parameters":{
                "type":"object","properties":properties,"required":required,"additionalProperties":false
            }})
        }).collect())
    }
    pub fn expected_output(self) -> Value {
        let (path, content) = match self {
            Self::Files => ("pipeline.txt", PIPELINE_CONTENT),
            Self::SyntaxRepair => ("repair.txt", REPAIR_CONTENT),
            Self::Calculation => return json!({"even_sum":30,"even_count":5,"input_bytes":21}),
            Self::Signatures => return self.tool_signatures(),
            _ => return Value::Null,
        };
        json!({"path":path,"content":content,"sha256":crate::measurement::sha256(content.as_bytes())})
    }
    pub fn expected_workspace(self) -> Value {
        let (path, content) = match self {
            Self::Files => ("pipeline.txt", PIPELINE_CONTENT),
            Self::SyntaxRepair => ("repair.txt", REPAIR_CONTENT),
            Self::Calculation => ("numbers.txt", NUMBERS),
            Self::OutputBudget | Self::MemoryBudget | Self::CallBudget => {
                ("before.txt", BEFORE_LIMIT)
            }
            _ => return json!([]),
        };
        json!([{"path":path,"kind":"file","content":content,"bytes":content.len(),
            "sha256":crate::measurement::sha256(content.as_bytes())}])
    }
}
