use crate::{Message, SystemMessage, ToolDefinition};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CurrentSystem {
    pub prompt: String,
    pub tools: Vec<ToolDefinition>,
}

/// Replays declarations without changing the stored transcript.
pub fn current_system(messages: &[Message]) -> CurrentSystem {
    let mut content = Vec::new();
    let mut sections = BTreeMap::new();
    let mut tools = Vec::<ToolDefinition>::new();
    for message in messages {
        if let Message::System(system) = message {
            if !system.content.is_empty() {
                content.push(system.content.clone());
            }
            for (key, value) in &system.sections {
                if let Some(value) = value {
                    sections.insert(key.clone(), value.clone());
                } else {
                    sections.remove(key);
                }
            }
            tools.retain(|tool| !system.tools_removed.contains(&tool.name));
            for tool in &system.tools_added {
                tools.retain(|old| old.name != tool.name);
                tools.push(tool.clone());
            }
        }
    }
    content.extend(sections.into_values());
    CurrentSystem {
        prompt: content.join("\n\n"),
        tools,
    }
}

pub fn tool_changes(messages: &[Message], desired: &[ToolDefinition]) -> Option<Message> {
    let current = current_system(messages).tools;
    let removed = current
        .iter()
        .filter(|tool| !desired.contains(tool))
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let added = desired
        .iter()
        .filter(|tool| !current.contains(tool))
        .cloned()
        .collect::<Vec<_>>();
    if removed.is_empty() && added.is_empty() {
        return None;
    }
    Some(Message::System(SystemMessage {
        tools_added: added,
        tools_removed: removed,
        ..Default::default()
    }))
}
