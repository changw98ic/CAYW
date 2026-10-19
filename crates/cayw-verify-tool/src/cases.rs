use cayw_codemode::{ErrorKind, Limits};
use serde_json::{Value, json};

pub const FIRST: &str = "cayw.emit({phase = 'first'}); return {answer = 42}";
pub const PIPELINE_CONTENT: &str = "offline CAYW\n第二行\n";
pub const REPAIR_CONTENT: &str = "repaired offline fixture\n";

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
}
pub const CASES: [Case; 11] = [
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
            Self::Cancel => format!(
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
            Self::Cancel => vec![(
                "search.query".into(),
                json!({"query":crate::fixtures::BLOCK_QUERY}),
            )],
            _ => vec![],
        }
    }
    /// Calls that must actually reach the Host, independent of the available grants.
    /// A syntax error has grants prepared but dispatches nothing; a denied call
    /// reaches the gate without receiving a grant or starting an audit record.
    pub fn expected_operations(self) -> Vec<(String, Value)> {
        match self {
            Self::Files | Self::SyntaxRepair | Self::Cancel => self.grants(),
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
            _ => None,
        }
    }
    pub fn limits(self) -> Limits {
        Limits {
            instruction_budget: if self == Self::LoopBudget {
                2_000
            } else {
                100_000
            },
            ..Limits::default()
        }
    }
    pub fn expected_calls(self) -> usize {
        self.expected_operations().len()
    }
}
