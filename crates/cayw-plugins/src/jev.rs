//! Offline JEv adapter. This module opens no model connection and makes no paid call.
//! The external process may ask local Laya; every accepted score stays provisional
//! and cannot satisfy StrongFlow evidence.

use crate::*;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JevInput {
    pub premise: String,
    pub hypothesis: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JevScores {
    pub entailment: f64,
    pub contradiction: f64,
    pub neutral: f64,
}

impl JevScores {
    fn validate(&self) -> Result<(), PluginError> {
        if !self.is_distribution() {
            return Err(PluginError::Protocol(
                "JEv scores must be finite probabilities summing to one".into(),
            ));
        }
        Ok(())
    }

    fn is_distribution(&self) -> bool {
        let values = [self.entailment, self.contradiction, self.neutral];
        values
            .iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
            && (values.iter().sum::<f64>() - 1.0).abs() <= 1e-6
    }
}

/// Turn three independent P(true) answers into one distribution.
/// The leftover after the first two shares goes to neutral, so the sum is 1.
/// A zero sum, or any value outside 0..=1, is refused. Nothing is invented.
pub fn normalize_noul(raw: [f64; 3]) -> Result<JevScores, PluginError> {
    if raw
        .iter()
        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
    {
        return Err(PluginError::Protocol(
            "JEv scores must be finite probabilities summing to one".into(),
        ));
    }
    let sum = raw.iter().sum::<f64>();
    if sum == 0.0 || !sum.is_finite() {
        return Err(PluginError::Protocol(
            "JEv scores must be finite probabilities summing to one".into(),
        ));
    }
    let scores = JevScores {
        entailment: raw[0] / sum,
        contradiction: raw[1] / sum,
        neutral: (1.0 - raw[0] / sum - raw[1] / sum).clamp(0.0, 1.0),
    };
    scores.validate()?;
    Ok(scores)
}

/// One slice of a session. The text is data. The place is where it belongs,
/// not whether it should be kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPart {
    pub index: u64,
    pub role: String,
    pub text: String,
}

/// Five places. `none` means the part belongs in none of the four memories.
pub const MEMORY_PLACES: [&str; 5] = ["long_term", "short_term", "project", "preference", "none"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPlacement {
    pub index: u64,
    pub role: String,
    pub text: String,
    pub place: String,
    pub probabilities: BTreeMap<String, f64>,
}

/// Split session text into non-empty parts.
/// A blank line, or a new `user:` / `assistant:` / `tool:` line, starts a part.
/// A part longer than 1200 characters is cut at that limit.
pub fn split_session(session: &str) -> Vec<SessionPart> {
    let mut parts = Vec::new();
    let mut role = "session".to_string();
    let mut body = String::new();
    let push = |parts: &mut Vec<SessionPart>, role: &str, body: &str| {
        let text = body.trim();
        if text.is_empty() {
            return;
        }
        for chunk in split_chunks(text, 1200) {
            parts.push(SessionPart {
                index: parts.len() as u64,
                role: role.to_string(),
                text: chunk,
            });
        }
    };
    for line in session.lines() {
        if line.trim().is_empty() {
            push(&mut parts, &role, &body);
            body.clear();
            continue;
        }
        if let Some((next_role, rest)) = role_line(line) {
            push(&mut parts, &role, &body);
            body.clear();
            role = next_role;
            if !rest.is_empty() {
                body.push_str(rest);
            }
            continue;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    push(&mut parts, &role, &body);
    parts
}

fn role_line(line: &str) -> Option<(String, &str)> {
    let (name, rest) = line.split_once(':')?;
    let role = match name.trim().to_ascii_lowercase().as_str() {
        "user" => "user",
        "assistant" => "assistant",
        "tool" => "tool",
        _ => return None,
    };
    Some((role.to_string(), rest.trim()))
}

fn split_chunks(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.len() > limit {
        let mut cut = limit;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        chunks.push(rest[..cut].trim().to_string());
        rest = rest[cut..].trim();
    }
    if !rest.is_empty() {
        chunks.push(rest.to_string());
    }
    chunks
}

/// Group part indexes into batches. Each batch stays within the character budget.
pub fn batch_part_indexes(parts: &[SessionPart], max_chars: usize) -> Vec<Vec<u64>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut used = 0usize;
    for part in parts {
        let size = part.text.chars().count().max(1);
        if !current.is_empty() && used.saturating_add(size) > max_chars {
            batches.push(current);
            current = Vec::new();
            used = 0;
        }
        current.push(part.index);
        used = used.saturating_add(size);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Accept one choice and its five probabilities.
/// The five numbers must be finite, inside 0..=1, and sum to 1.
/// The choice must be one of the five places and must match the largest probability.
pub fn normalize_placement(
    part: &SessionPart,
    choice: &str,
    raw: BTreeMap<String, f64>,
) -> Result<MemoryPlacement, PluginError> {
    if MEMORY_PLACES.len() != raw.len()
        || MEMORY_PLACES.iter().any(|place| !raw.contains_key(*place))
    {
        return Err(PluginError::Protocol(
            "JEv memory placement must cover the five places".into(),
        ));
    }
    let values = MEMORY_PLACES.map(|place| raw[place]);
    if values
        .iter()
        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        || (values.iter().sum::<f64>() - 1.0).abs() > 1e-6
        || !MEMORY_PLACES.contains(&choice)
    {
        return Err(PluginError::Protocol(
            "JEv memory placement must be one finite distribution".into(),
        ));
    }
    let top = MEMORY_PLACES
        .iter()
        .copied()
        .max_by(|left, right| raw[*left].total_cmp(&raw[*right]))
        .unwrap_or("none");
    if top != choice {
        return Err(PluginError::Protocol(
            "JEv memory choice must match its largest probability".into(),
        ));
    }
    Ok(MemoryPlacement {
        index: part.index,
        role: part.role.clone(),
        text: part.text.clone(),
        place: choice.to_string(),
        probabilities: raw,
    })
}

#[async_trait]
pub trait JevEvaluator: Send + Sync {
    fn evaluator_id(&self) -> &str;
    async fn evaluate(
        &self,
        input: JevInput,
        cancel: CancellationToken,
    ) -> Result<JevScores, PluginError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevReport {
    pub evaluator_id: String,
    pub input: JevInput,
    pub scores: JevScores,
    /// This can never satisfy a StrongFlow execution evidence requirement.
    pub provisional: bool,
}

pub struct Jev {
    evaluator: Option<Arc<dyn JevEvaluator>>,
    timeout: Duration,
}

impl Jev {
    pub fn new(evaluator: Option<Arc<dyn JevEvaluator>>, timeout: Duration) -> Self {
        Self { evaluator, timeout }
    }

    pub async fn evaluate(
        &self,
        input: JevInput,
        cancel: CancellationToken,
    ) -> Result<JevReport, PluginError> {
        if input.premise.trim().is_empty() || input.hypothesis.trim().is_empty() {
            return Err(PluginError::InvalidArguments(
                "premise and hypothesis are required".into(),
            ));
        }
        let evaluator = self
            .evaluator
            .as_ref()
            .ok_or_else(|| PluginError::Unavailable("no JEv evaluator registered".into()))?;
        if !valid_name(evaluator.evaluator_id()) {
            return Err(PluginError::InvalidManifest("invalid evaluator id".into()));
        }
        let call_cancel = cancel.child_token();
        let _guard = CancelOnDrop(call_cancel.clone());
        let scores = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(PluginError::Cancelled),
            result = tokio::time::timeout(self.timeout, evaluator.evaluate(input.clone(), call_cancel)) => result.unwrap_or(Err(PluginError::Timeout)),
        }?;
        scores.validate()?;
        Ok(JevReport {
            evaluator_id: evaluator.evaluator_id().into(),
            input,
            scores,
            provisional: true,
        })
    }
}

/// Adapts an explicitly loaded Native or External capability. The registry still
/// checks the host's permissions and validates its arguments on every request.
pub struct PluginJevEvaluator {
    pub registry: Arc<PluginRegistry>,
    pub plugin_name: String,
    pub method: String,
    pub timeout: Duration,
}

#[async_trait]
impl JevEvaluator for PluginJevEvaluator {
    fn evaluator_id(&self) -> &str {
        &self.plugin_name
    }
    async fn evaluate(
        &self,
        input: JevInput,
        cancel: CancellationToken,
    ) -> Result<JevScores, PluginError> {
        let value = self
            .registry
            .call(
                &self.plugin_name,
                &self.method,
                serde_json::to_value(input)
                    .map_err(|error| PluginError::InvalidArguments(error.to_string()))?,
                cancel,
                self.timeout,
            )
            .await?;
        serde_json::from_value(value).map_err(|error| PluginError::Protocol(error.to_string()))
    }
}
