//! Bounded graph planning and local replay.
//!
//! `script_for_plan` produces the complete Luau program used by a real Runtime
//! execution. `evaluate` runs that same program once, from the revision's entry,
//! so shared input, state and context references are rebuilt by re-executing
//! every preceding node. Every replay step is recomputed. Host calls are answered
//! only from the original captured records, matched to the node the runner has
//! just started, by exact name, arguments and call order.

use crate::types::{
    Evaluation, Gap, GraphPlan, Node, NodeResult, NodeTrace, RecordedCall, Status, Step,
    TRACE_SCHEMA, TraceEnvelope, now_ms,
};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{
    CapabilityError, CapabilityHost, CodeMode, CodeModeError, ErrorKind, Limits, VmOperation,
    VmServices,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_NODES: usize = 64;
pub const MAX_SOURCE_BYTES: usize = 16 * 1024;
pub const MAX_PLAN_BYTES: usize = 256 * 1024;
pub const MAX_JSON_DEPTH: usize = 32;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn invalid(message: impl Into<String>) -> crate::Error {
    crate::Error::Invalid(message.into())
}

struct NoHost;

#[async_trait]
impl CapabilityHost for NoHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::new(
            "source validation never calls the Host",
        ))
    }
}

fn validate_node_source(source: &str) -> Result<(), crate::Error> {
    let mode = CodeMode::new(Arc::new(NoHost), Limits::default())
        .map_err(|error| invalid(error.to_string()))?;
    mode.validate_source(source)
        .map_err(|error| invalid(error.to_string()))
}

fn node_api_script() -> String {
    format!(
        r#"local __node_api = (function()
  local runtime_api = cayw
  local api = {{call=runtime_api.call, null=runtime_api.null, array=runtime_api.array}}
  api.emit = function(value)
    if type(value) == "table" and value.schema == "{TRACE_SCHEMA}" then
      error("node output uses the reserved trace schema")
    end
    return runtime_api.emit(value)
  end
  return table.freeze(api)
end)()
local __node_globals = table.freeze({{cayw=__node_api}})
"#
    )
}

/// Structural checks that must pass before a plan is rendered or replayed.
pub fn validate_plan(plan: &GraphPlan) -> Result<(), crate::Error> {
    if plan.id.is_empty() || plan.id.len() > MAX_IDENTIFIER_BYTES {
        return Err(invalid("plan id must contain 1..=256 bytes"));
    }
    if plan.nodes.is_empty() {
        return Err(invalid("plan must contain at least one node"));
    }
    if plan.nodes.len() > MAX_NODES {
        return Err(invalid(format!(
            "plan contains more than {MAX_NODES} nodes"
        )));
    }
    let encoded = serde_json::to_vec(plan)
        .map_err(|error| invalid(format!("plan cannot be encoded as JSON: {error}")))?;
    if encoded.len() > MAX_PLAN_BYTES {
        return Err(invalid(format!(
            "encoded plan exceeds {MAX_PLAN_BYTES} bytes"
        )));
    }

    check_json(&plan.input, "plan input")?;
    check_json(&plan.initial_state, "plan initial_state")?;
    check_json(&plan.context, "plan context")?;

    let mut by_id: BTreeMap<&str, &Node> = BTreeMap::new();
    for node in &plan.nodes {
        if node.id.is_empty() || node.id.len() > MAX_IDENTIFIER_BYTES {
            return Err(invalid("node id must contain 1..=256 bytes"));
        }
        if node.id.chars().any(|c| c.is_control()) {
            return Err(invalid(format!(
                "node id {:?} contains control characters",
                node.id
            )));
        }
        if by_id.insert(node.id.as_str(), node).is_some() {
            return Err(invalid(format!("duplicate node id {:?}", node.id)));
        }
        if node.source.len() > MAX_SOURCE_BYTES {
            return Err(invalid(format!(
                "node {:?} source exceeds {MAX_SOURCE_BYTES} bytes",
                node.id
            )));
        }
        check_json(&node.params, &format!("params of node {:?}", node.id))?;
        validate_pointer(&node.input_pointer)
            .map_err(|error| invalid(format!("node {:?}: {error}", node.id)))?;
    }
    if !by_id.contains_key(plan.entry.as_str()) {
        return Err(invalid(format!(
            "entry {:?} does not name a node",
            plan.entry
        )));
    }

    let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut seen_edges: BTreeSet<(String, Option<String>)> = BTreeSet::new();
    let mut default_edges: BTreeSet<String> = BTreeSet::new();
    for edge in &plan.edges {
        if !by_id.contains_key(edge.from.as_str()) {
            return Err(invalid(format!(
                "edge source {:?} does not name a node",
                edge.from
            )));
        }
        if !by_id.contains_key(edge.to.as_str()) {
            return Err(invalid(format!(
                "edge target {:?} does not name a node",
                edge.to
            )));
        }
        if edge.branch.as_deref() == Some("") {
            return Err(invalid(format!(
                "edge {:?} -> {:?} has an empty branch",
                edge.from, edge.to
            )));
        }
        let key = (edge.from.clone(), edge.branch.clone());
        if !seen_edges.insert(key) {
            return Err(invalid(format!(
                "node {:?} has duplicate edges for branch {:?}",
                edge.from, edge.branch
            )));
        }
        if edge.branch.is_none() && !default_edges.insert(edge.from.clone()) {
            return Err(invalid(format!(
                "node {:?} has more than one default edge",
                edge.from
            )));
        }
        adjacency
            .entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }

    // Every node must be reachable from the entry and no edge may close a cycle.
    let mut reachable: BTreeSet<&str> = BTreeSet::new();
    let mut queue = VecDeque::from([plan.entry.as_str()]);
    while let Some(current) = queue.pop_front() {
        if !reachable.insert(current) {
            continue;
        }
        if let Some(next) = adjacency.get(current) {
            for target in next {
                queue.push_back(target.as_str());
            }
        }
    }
    if let Some(missing) = by_id.keys().find(|id| !reachable.contains(*id)) {
        return Err(invalid(format!(
            "node {missing:?} is unreachable from the entry"
        )));
    }
    let mut colors: BTreeMap<&str, u8> = BTreeMap::new();
    if let Some(node) = find_cycle(plan.entry.as_str(), &adjacency, &mut colors) {
        return Err(invalid(format!(
            "plan contains a cycle through node {node:?}"
        )));
    }
    Ok(())
}

fn find_cycle<'a>(
    node: &'a str,
    adjacency: &'a BTreeMap<String, Vec<String>>,
    colors: &mut BTreeMap<&'a str, u8>,
) -> Option<String> {
    match colors.get(node).copied().unwrap_or(0) {
        1 => return Some(node.to_owned()),
        2 => return None,
        _ => {}
    }
    colors.insert(node, 1);
    if let Some(targets) = adjacency.get(node) {
        for target in targets {
            if let Some(cycle) = find_cycle(target, adjacency, colors) {
                return Some(cycle);
            }
        }
    }
    colors.insert(node, 2);
    None
}

fn validate_pointer(pointer: &str) -> Result<(), String> {
    if pointer.is_empty() {
        return Ok(());
    }
    if !pointer.starts_with('/') {
        return Err("input_pointer must be empty or start with '/'".into());
    }
    for token in pointer[1..].split('/') {
        let bytes = token.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'~' {
                if index + 1 >= bytes.len() || !matches!(bytes[index + 1], b'0' | b'1') {
                    return Err("input_pointer contains an invalid '~' escape".into());
                }
                index += 2;
            } else {
                index += 1;
            }
        }
    }
    Ok(())
}

fn check_number(number: &serde_json::Number, label: &str) -> Result<(), crate::Error> {
    if let Some(value) = number.as_i64() {
        if value.unsigned_abs() > MAX_SAFE_INTEGER {
            return Err(invalid(format!(
                "{label} contains an integer that Luau cannot represent exactly"
            )));
        }
    } else if let Some(value) = number.as_u64() {
        if value > MAX_SAFE_INTEGER {
            return Err(invalid(format!(
                "{label} contains an integer that Luau cannot represent exactly"
            )));
        }
    } else if number.as_f64().is_none_or(|value| !value.is_finite()) {
        return Err(invalid(format!("{label} contains a non-finite number")));
    }
    Ok(())
}

fn check_json(value: &Value, label: &str) -> Result<(), crate::Error> {
    let mut stack = vec![(value, 1usize)];
    while let Some((current, depth)) = stack.pop() {
        if depth > MAX_JSON_DEPTH {
            return Err(invalid(format!(
                "{label} is nested deeper than {MAX_JSON_DEPTH} levels"
            )));
        }
        match current {
            Value::Number(number) => check_number(number, label)?,
            Value::Array(items) => {
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            Value::Object(fields) => {
                for item in fields.values() {
                    stack.push((item, depth + 1));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn push_luau_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            character if (character as u32) < 0x20 || character as u32 == 0x7f => {
                out.push_str(&format!("\\{:03}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

/// Renders a checked JSON value as a Luau expression. Strings are always quoted
/// literals; nothing is interpolated into surrounding source text.
fn luau_literal(value: &Value) -> Result<String, crate::Error> {
    check_json(value, "injected value")?;
    let mut out = String::new();
    write_luau(&mut out, value);
    Ok(out)
}

fn write_luau(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("cayw.null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                out.push_str(&value.to_string());
            } else if let Some(value) = number.as_u64() {
                out.push_str(&value.to_string());
            } else if let Some(value) = number.as_f64() {
                let rendered = format!("{value:?}");
                out.push_str(&rendered);
                if !rendered.contains(['.', 'e', 'E']) {
                    out.push_str(".0");
                }
            } else {
                out.push('0');
            }
        }
        Value::String(text) => push_luau_string(out, text),
        Value::Array(items) => {
            out.push_str("cayw.array({");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_luau(out, item);
            }
            out.push_str("})");
        }
        Value::Object(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push('[');
                push_luau_string(out, key);
                out.push_str("]=");
                write_luau(out, item);
            }
            out.push('}');
        }
    }
}

/// The complete Luau program for the real Runtime execution. Each `started`
/// trace records the incoming value *before* `input_pointer` mapping so a later
/// replay can re-map it after the pointer changes.
pub fn script_for_plan(plan: &GraphPlan) -> Result<String, crate::Error> {
    validate_plan(plan)?;
    for node in &plan.nodes {
        validate_node_source(&node.source)?;
    }
    let manifest = serde_json::to_value(TraceEnvelope {
        schema: TRACE_SCHEMA.into(),
        trace: NodeTrace::Manifest { plan: plan.clone() },
    })
    .map_err(|error| invalid(format!("plan manifest cannot be encoded: {error}")))?;

    let mut script = String::new();
    script.push_str(
        "-- cayw-trace-debug: complete graph execution\n\
         -- The sandbox exposes no clock, so completed.elapsed_ms is 0 here;\n\
         -- the capture layer derives node duration from event timestamps.\n",
    );
    // Compile node functions before declaring any runner locals. A node cannot
    // name or replace the runner's emitter, graph, validator or program counter.
    script.push_str("local __build_nodes = function(cayw, _G)\nreturn {\n");
    for node in &plan.nodes {
        script.push('[');
        push_luau_string(&mut script, &node.id);
        script.push_str("] = function(input, state, params, context)\n");
        script.push_str(&node.source);
        script.push_str("\nend,\n");
    }
    script.push_str("}\nend\n");
    script.push_str("cayw.emit(");
    script.push_str(&luau_literal(&manifest)?);
    script.push_str(")\n");

    script.push_str(
        "local function __check_depth(value, depth)\n\
         if depth > 32 then error(\"node result is nested deeper than 32 levels\") end\n\
         if type(value) == \"table\" then\n\
           for _, item in pairs(value) do __check_depth(item, depth + 1) end\n\
         end\n\
         end\n\
         local function __resolve(root, pointer)\n\
         if pointer == \"\" then return root end\n\
         local current = root\n\
         local position = 2\n\
         local length = string.len(pointer)\n\
         while position <= length + 1 do\n\
           local slash = string.find(pointer, \"/\", position, true)\n\
           local segment\n\
           if slash == nil then\n\
             segment = string.sub(pointer, position)\n\
             position = length + 2\n\
           else\n\
             segment = string.sub(pointer, position, slash - 1)\n\
             position = slash + 1\n\
           end\n\
           segment = string.gsub(string.gsub(segment, \"~1\", \"/\"), \"~0\", \"~\")\n\
           if type(current) ~= \"table\" then error(\"input_pointer does not resolve\") end\n\
           local direct = current[segment]\n\
           if direct == nil and string.match(segment, \"^%d+$\") then\n\
             direct = current[tonumber(segment) + 1]\n\
           end\n\
           if direct == nil then error(\"input_pointer does not resolve\") end\n\
           current = direct\n\
         end\n\
         return current\n\
         end\n",
    );

    script.push_str(&node_api_script());
    script.push_str(
        "local __nodes = __build_nodes(__node_api, __node_globals)\n__build_nodes = nil\n",
    );

    script.push_str("local __params = {\n");
    for node in &plan.nodes {
        script.push('[');
        push_luau_string(&mut script, &node.id);
        script.push_str("]=");
        script.push_str(&luau_literal(&node.params)?);
        script.push_str(",\n");
    }
    script.push_str("}\n");

    script.push_str("local __pointers = {\n");
    for node in &plan.nodes {
        if !node.input_pointer.is_empty() {
            script.push('[');
            push_luau_string(&mut script, &node.id);
            script.push_str("]=");
            push_luau_string(&mut script, &node.input_pointer);
            script.push_str(",\n");
        }
    }
    script.push_str("}\n");

    script.push_str("local __branches = {}\n");
    script.push_str("local __defaults = {}\n");
    script.push_str("local __outgoing = {}\n");
    for edge in &plan.edges {
        script.push_str("__outgoing[");
        push_luau_string(&mut script, &edge.from);
        script.push_str("]=true\n");
        match &edge.branch {
            Some(branch) => {
                script.push_str("__branches[");
                push_luau_string(&mut script, &edge.from);
                script.push_str("]=__branches[");
                push_luau_string(&mut script, &edge.from);
                script.push_str("] or {}\n__branches[");
                push_luau_string(&mut script, &edge.from);
                script.push_str("][");
                push_luau_string(&mut script, branch);
                script.push_str("]=");
                push_luau_string(&mut script, &edge.to);
                script.push('\n');
            }
            None => {
                script.push_str("__defaults[");
                push_luau_string(&mut script, &edge.from);
                script.push_str("]=");
                push_luau_string(&mut script, &edge.to);
                script.push('\n');
            }
        }
    }

    script.push_str("local __incoming = ");
    script.push_str(&luau_literal(&plan.input)?);
    script.push_str("\nlocal __state = ");
    script.push_str(&luau_literal(&plan.initial_state)?);
    script.push_str("\nlocal __context = ");
    script.push_str(&luau_literal(&plan.context)?);
    script.push_str("\nlocal __current = ");
    push_luau_string(&mut script, &plan.entry);
    script.push('\n');

    script.push_str(
        "local __visited = {}\n\
         local __count = 0\n\
         local __last = nil\n\
         while __current ~= nil do\n\
           if __visited[__current] then error(\"repeated node: \" .. tostring(__current)) end\n\
           __visited[__current] = true\n\
           __count = __count + 1\n\
         ",
    );
    script.push_str(&format!(
        "  if __count > {MAX_NODES} then error(\"node budget exceeded\") end\n"
    ));
    script.push_str(
        "  local __fn = __nodes[__current]\n\
           if __fn == nil then error(\"missing node: \" .. tostring(__current)) end\n\
           cayw.emit({schema = \"",
    );
    script.push_str(TRACE_SCHEMA);
    script.push_str(
        "\", trace = {kind = \"started\", node_id = __current, input = __incoming, state_before = __state}})\n\
           local __pointer = __pointers[__current]\n\
           local __mapped = __incoming\n\
           if __pointer ~= nil then __mapped = __resolve(__incoming, __pointer) end\n\
           local __result = __fn(__mapped, __state, __params[__current], __context)\n\
           if type(__result) ~= \"table\" then error(\"node \" .. __current .. \" must return a table\") end\n\
           if __result.output == nil then error(\"node \" .. __current .. \" must return output\") end\n\
           if __result.state == nil then error(\"node \" .. __current .. \" must return state\") end\n\
           __check_depth(__result.output, 1)\n\
           __check_depth(__result.state, 1)\n\
           local __branch = __result.branch\n\
           if __branch == cayw.null then __branch = nil end\n\
           if __branch ~= nil and type(__branch) ~= \"string\" then error(\"node \" .. __current .. \" branch must be a string\") end\n\
           if __branch == \"\" then error(\"node \" .. __current .. \" branch must not be empty\") end\n\
           for __field in pairs(__result) do\n\
             if __field ~= \"output\" and __field ~= \"state\" and __field ~= \"branch\" then error(\"node \" .. __current .. \" returned unexpected field \" .. tostring(__field)) end\n\
           end\n\
           local __next = nil\n\
           if __branch == nil then\n\
             __next = __defaults[__current]\n\
             if __next == nil and __outgoing[__current] then error(\"no default edge from \" .. __current) end\n\
           else\n\
             local __edges = __branches[__current]\n\
             if __edges ~= nil then __next = __edges[__branch] end\n\
             if __next == nil then error(\"no edge for branch \" .. __branch .. \" from \" .. __current) end\n\
           end\n\
           cayw.emit({schema = \"",
    );
    script.push_str(TRACE_SCHEMA);
    script.push_str(
        "\", trace = {kind = \"completed\", node_id = __current, result = __result, elapsed_ms = 0}})\n\
           __state = __result.state\n\
           __incoming = __result.output\n\
           __last = __result\n\
           __current = __next\n\
         end\n\
         return __last\n",
    );
    Ok(script)
}

struct ReplayHost {
    by_node: Mutex<BTreeMap<String, VecDeque<RecordedCall>>>,
    names: Vec<String>,
    current: Mutex<Option<String>>,
    last_evidence: Mutex<Option<u64>>,
    /// Set when replay lacks a captured answer. This is a replay gap, not an
    /// unknown business effect: no simulated call was dispatched.
    blocked: Mutex<Option<String>>,
}

impl ReplayHost {
    fn new(calls: &[RecordedCall]) -> Self {
        let mut by_node: BTreeMap<String, VecDeque<RecordedCall>> = BTreeMap::new();
        for call in calls {
            if let Some(node_id) = &call.node_id {
                by_node
                    .entry(node_id.clone())
                    .or_default()
                    .push_back(call.clone());
            }
        }
        let names: Vec<String> = calls
            .iter()
            .map(|call| call.name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self {
            by_node: Mutex::new(by_node),
            names,
            current: Mutex::new(None),
            last_evidence: Mutex::new(None),
            blocked: Mutex::new(None),
        }
    }

    fn enter(&self, node_id: &str) {
        *self.current.lock().expect("replay host poisoned") = Some(node_id.to_owned());
        *self.last_evidence.lock().expect("replay host poisoned") = None;
    }

    fn leftover(&self, node_id: &str) -> usize {
        self.by_node
            .lock()
            .expect("replay host poisoned")
            .get(node_id)
            .map_or(0, VecDeque::len)
    }

    fn evidence(&self) -> Option<u64> {
        *self.last_evidence.lock().expect("replay host poisoned")
    }

    fn blocked(&self) -> Option<String> {
        self.blocked.lock().expect("replay host poisoned").clone()
    }

    fn block(&self, message: String) -> CapabilityError {
        self.blocked
            .lock()
            .expect("replay host poisoned")
            .get_or_insert_with(|| message.clone());
        CapabilityError::new(message)
    }
}

#[async_trait]
impl CapabilityHost for ReplayHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.names
            .iter()
            .map(|name| ToolDefinition {
                name: name.clone(),
                description: "Captured response from the original execution".into(),
                parameters: json!({}),
            })
            .collect()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let Some(node_id) = self.current.lock().expect("replay host poisoned").clone() else {
            return Err(self.block(format!(
                "replay has no started node for the next call to {name}"
            )));
        };
        let Some(call) = self
            .by_node
            .lock()
            .expect("replay host poisoned")
            .get_mut(&node_id)
            .and_then(VecDeque::pop_front)
        else {
            return Err(self.block(format!(
                "replay has no captured response for the next call to {name}"
            )));
        };
        *self.last_evidence.lock().expect("replay host poisoned") = Some(call.evidence_cursor);
        if call.name != name || call.arguments != arguments {
            return Err(self.block(format!(
                "replayed call to {name} does not match the captured name or arguments"
            )));
        }
        match call.outcome {
            Some(cayw_core::CapabilityOutcome::Completed { result }) => Ok(result),
            Some(cayw_core::CapabilityOutcome::Failed { error }) => {
                Err(CapabilityError::new(error))
            }
            Some(cayw_core::CapabilityOutcome::NotExecuted { reason }) => {
                Err(CapabilityError::new(reason))
            }
            Some(cayw_core::CapabilityOutcome::OutcomeUnknown { error }) => {
                Err(CapabilityError::unknown(error))
            }
            None => Err(CapabilityError::unknown(format!(
                "captured call {name} has no settled outcome"
            ))),
        }
    }
}

#[derive(Clone)]
struct ObservedTrace {
    trace: NodeTrace,
    observed_at: Instant,
    observed_ms: u64,
    evidence: Option<u64>,
}

/// Receives the same NodeTrace events the runner emits. `invoke` is not part of
/// the node protocol; a graph script never calls a trusted VM service.
struct TraceObserver {
    host: Arc<ReplayHost>,
    events: Mutex<Vec<ObservedTrace>>,
}

#[async_trait]
impl VmServices for TraceObserver {
    fn emit(&self, value: Value) -> Result<(), CapabilityError> {
        if value.get("schema").and_then(Value::as_str) != Some(TRACE_SCHEMA) {
            return Ok(());
        }
        let envelope: TraceEnvelope = serde_json::from_value(value).map_err(|error| {
            CapabilityError::new(format!("replay trace is not an envelope: {error}"))
        })?;
        if let NodeTrace::Started { node_id, .. } = &envelope.trace {
            self.host.enter(node_id);
        }
        self.events
            .lock()
            .expect("replay trace poisoned")
            .push(ObservedTrace {
                trace: envelope.trace,
                observed_at: Instant::now(),
                observed_ms: now_ms(),
                evidence: self.host.evidence(),
            });
        Ok(())
    }

    async fn invoke(
        &self,
        _operation: VmOperation,
        _arguments: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::new(
            "replay does not expose trusted VM services",
        ))
    }
}

struct RunFailure {
    status: Status,
    message: String,
    evidence: Option<u64>,
}

fn replay_limits(calls: usize) -> Limits {
    Limits {
        memory_bytes: 16 * 1024 * 1024,
        instruction_budget: 100_000,
        timeout: Duration::from_secs(5),
        host_wait: Duration::from_secs(5),
        shutdown_grace: Duration::from_secs(2),
        host_calls: calls.saturating_add(1).max(1),
        output_bytes: 1024 * 1024,
    }
}

fn code_mode_failure(error: CodeModeError, evidence: Option<u64>) -> RunFailure {
    let status = match error.kind {
        // Cancellation left nothing running in the replay VM.
        ErrorKind::Cancelled => Status::Blocked,
        // A capability outside the captured set is a replay gap, not an
        // unknown business effect: replay never dispatches it.
        ErrorKind::UnknownCapability => Status::Blocked,
        // Only an outcome the original execution itself left unknown stays
        // unknown in replay.
        ErrorKind::OutcomeUnknown => Status::OutcomeUnknown,
        _ => Status::Failed,
    };
    RunFailure {
        status,
        message: format!("{:?}: {}", error.kind, error.message),
        evidence,
    }
}

/// The edited node must already appear on the captured path. A current base that
/// never reached it is a gap; an older branch is not borrowed automatically.
fn edit_point_gap(plan: &GraphPlan, base: Option<&Evaluation>, changed_node: &str) -> Option<Gap> {
    if !plan.nodes.iter().any(|node| node.id == changed_node) {
        return Some(Gap {
            node_id: Some(changed_node.into()),
            message: "the edited node does not exist in this plan".into(),
        });
    }
    let Some(base) = base else {
        if changed_node == plan.entry {
            return None;
        }
        return Some(Gap {
            node_id: Some(changed_node.into()),
            message: "no captured input or state exists for the edited node".into(),
        });
    };
    if base.steps.iter().any(|step| step.node_id == changed_node) {
        return None;
    }
    if changed_node == plan.entry {
        return None;
    }
    Some(Gap {
        node_id: Some(changed_node.into()),
        message: "the captured evaluation never reached the edited node".into(),
    })
}

fn blocked_evaluation(
    plan: &GraphPlan,
    version: u64,
    started: Instant,
    gaps: Vec<Gap>,
) -> Evaluation {
    Evaluation {
        id: format!("replay:{}:{}", plan.id, version),
        version,
        mode: "replay".into(),
        status: Status::Blocked,
        steps: Vec::new(),
        output: None,
        gaps,
        elapsed_ms: elapsed_ms(started),
        saved_at_ms: now_ms(),
    }
}

fn step_from_trace(
    started_trace: &NodeTrace,
    completed: Option<&NodeResult>,
    status: Status,
    error: Option<String>,
    started_at_ms: u64,
    elapsed: f64,
    evidence: Option<u64>,
) -> Result<Step, String> {
    let NodeTrace::Started {
        node_id,
        input,
        state_before,
    } = started_trace
    else {
        return Err("replay completed a node that has no started trace".into());
    };
    Ok(Step {
        node_id: node_id.clone(),
        status,
        input: input.clone(),
        state_before: state_before.clone(),
        output: completed.map(|result| result.output.clone()),
        state_after: completed.map(|result| result.state.clone()),
        branch: completed.and_then(|result| result.branch.clone()),
        error,
        started_at_ms,
        elapsed_ms: elapsed,
        evidence_cursor: evidence,
        reused: false,
    })
}

/// Replays `plan` by running `script_for_plan` once from the entry. Every node
/// on the path is recomputed, including nodes before the edit, so shared table
/// references in input, state and context are rebuilt. Host calls are answered
/// only by the captured response for the node the runner has started, with the
/// exact name, arguments and call order. Missing captures, unknown outcomes and
/// an uncaptured branch produce `blocked` with explicit gaps. An error output
/// is not reported as a completed round.
pub async fn evaluate(
    plan: &GraphPlan,
    base: Option<&Evaluation>,
    changed_node: Option<&str>,
    calls: &[RecordedCall],
    version: u64,
    cancel: CancellationToken,
) -> Result<Evaluation, crate::Error> {
    validate_plan(plan)?;
    let started = Instant::now();
    if let Some(changed) = changed_node
        && let Some(gap) = edit_point_gap(plan, base, changed)
    {
        return Ok(blocked_evaluation(plan, version, started, vec![gap]));
    }
    if cancel.is_cancelled() {
        return Ok(blocked_evaluation(
            plan,
            version,
            started,
            vec![Gap {
                node_id: Some(plan.entry.clone()),
                message: "replay cancelled before this node started".into(),
            }],
        ));
    }

    let script = script_for_plan(plan)?;
    let host = Arc::new(ReplayHost::new(calls));
    let observer = Arc::new(TraceObserver {
        host: host.clone(),
        events: Mutex::new(Vec::new()),
    });
    let mode = CodeMode::new(host.clone(), replay_limits(calls.len()))
        .map_err(|error| invalid(format!("replay could not start its bounded VM: {error}")))?;
    let execution = mode
        .execute_with_services(&script, cancel, observer.clone())
        .await;
    let traces = observer
        .events
        .lock()
        .expect("replay trace poisoned")
        .clone();

    let mut steps = Vec::new();
    let mut gaps = Vec::new();
    let mut pending: Option<(NodeTrace, Instant, u64)> = None;
    let mut status = Status::Completed;
    let mut output = None;
    let mut stopped = false;

    for ObservedTrace {
        trace,
        observed_at,
        observed_ms,
        evidence,
    } in &traces
    {
        match trace {
            NodeTrace::Manifest { .. } => {}
            NodeTrace::Started { .. } => {
                if pending.is_some() {
                    gaps.push(Gap {
                        node_id: None,
                        message: "replay started a node before the previous node completed".into(),
                    });
                    status = Status::Failed;
                    stopped = true;
                    break;
                }
                pending = Some((trace.clone(), *observed_at, *observed_ms));
            }
            NodeTrace::Completed {
                node_id, result, ..
            } => {
                let Some((started_trace, node_started, started_at_ms)) = pending.take() else {
                    gaps.push(Gap {
                        node_id: Some(node_id.clone()),
                        message: "replay completed a node that has not started".into(),
                    });
                    status = Status::Failed;
                    stopped = true;
                    break;
                };
                let started_id = match &started_trace {
                    NodeTrace::Started { node_id, .. } => node_id.clone(),
                    _ => String::new(),
                };
                if started_id != *node_id {
                    gaps.push(Gap {
                        node_id: Some(node_id.clone()),
                        message: format!(
                            "replay completed {node_id:?} while {started_id:?} was still running"
                        ),
                    });
                    status = Status::Failed;
                    stopped = true;
                    break;
                }
                let elapsed = observed_at.duration_since(node_started).as_secs_f64() * 1000.0;
                let leftover = host.leftover(node_id);
                steps.push(
                    step_from_trace(
                        &started_trace,
                        Some(result),
                        Status::Completed,
                        None,
                        started_at_ms,
                        elapsed,
                        *evidence,
                    )
                    .map_err(invalid)?,
                );
                if leftover > 0 {
                    gaps.push(Gap {
                        node_id: Some(node_id.clone()),
                        message: format!(
                            "replay made fewer Host calls than captured; {leftover} captured response(s) were not used"
                        ),
                    });
                }
                output = Some(result.output.clone());
            }
        }
    }

    if !stopped {
        match execution {
            Ok(_) => {
                if let Some((started_trace, _, _)) = pending {
                    let node_id = match &started_trace {
                        NodeTrace::Started { node_id, .. } => Some(node_id.clone()),
                        _ => None,
                    };
                    gaps.push(Gap {
                        node_id,
                        message: "replay finished without a completed trace for the started node"
                            .into(),
                    });
                    status = Status::Failed;
                }
            }
            Err(error) => {
                let evidence = host.evidence();
                let failure = if let Some(reason) = host.blocked() {
                    // A missing or mismatched capture is a replay gap. No call was
                    // dispatched, so it must not be reported as an unknown effect.
                    RunFailure {
                        status: Status::Blocked,
                        message: reason,
                        evidence,
                    }
                } else {
                    code_mode_failure(error, evidence)
                };
                if let Some((started_trace, node_started, started_at_ms)) = pending.take() {
                    let elapsed = elapsed_ms(node_started);
                    let node_id = match &started_trace {
                        NodeTrace::Started { node_id, .. } => node_id.clone(),
                        _ => plan.entry.clone(),
                    };
                    let message = format!("node {node_id:?}: {}", failure.message);
                    steps.push(
                        step_from_trace(
                            &started_trace,
                            None,
                            failure.status.clone(),
                            Some(message.clone()),
                            started_at_ms,
                            elapsed,
                            failure.evidence,
                        )
                        .map_err(invalid)?,
                    );
                    gaps.push(Gap {
                        node_id: Some(node_id),
                        message,
                    });
                } else {
                    gaps.push(Gap {
                        node_id: changed_node.map(str::to_owned),
                        message: failure.message.clone(),
                    });
                }
                status = match failure.status {
                    Status::OutcomeUnknown | Status::Blocked => Status::Blocked,
                    _ => Status::Failed,
                };
            }
        }
    }

    let final_output = (status == Status::Completed).then_some(output).flatten();
    Ok(Evaluation {
        id: format!("replay:{}:{}", plan.id, version),
        version,
        mode: "replay".into(),
        status,
        steps,
        output: final_output,
        gaps,
        elapsed_ms: elapsed_ms(started),
        saved_at_ms: now_ms(),
    })
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}
