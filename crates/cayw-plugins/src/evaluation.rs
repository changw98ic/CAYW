//! Offline host adapters. No connections, model selection or paid calls are made
//! here. JEv scores remain provisional; Fusion collects blind independent answers
//! without voting, fact claims or invented conflict resolution.

use crate::*;
use cayw_ai::{ArgumentValidator, Usage};
use futures::{StreamExt, stream::FuturesUnordered};
use sha2::{Digest, Sha256};
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
        let values = [self.entailment, self.contradiction, self.neutral];
        if values
            .iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            || (values.iter().sum::<f64>() - 1.0).abs() > 1e-6
        {
            return Err(PluginError::Protocol(
                "JEv scores must be finite probabilities summing to one".into(),
            ));
        }
        Ok(())
    }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionInput {
    pub question: String,
    pub canonical_context: Value,
    pub constraints: Vec<String>,
    /// Object schema, validated strictly and locally by cayw-ai.
    pub answer_schema: Value,
}

impl FusionInput {
    pub fn digest(&self) -> Result<String, PluginError> {
        let value = serde_json::to_value(self)
            .map_err(|error| PluginError::InvalidArguments(error.to_string()))?;
        let mut bytes = Vec::new();
        canonical_bytes(&value, &mut bytes)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

/// Only the identical input is supplied. No other candidate's answer or identity
/// is passed to a source. The host is responsible for provider independence.
#[async_trait]
pub trait FusionCandidateSource: Send + Sync {
    fn provider_id(&self) -> &str;
    async fn answer(&self, input: Arc<FusionInput>, cancel: CancellationToken)
    -> CandidateResponse;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CandidateResponse {
    Answer {
        value: Value,
        usage: Option<Usage>,
    },
    Failed {
        message: String,
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CandidateOutcome {
    Answer {
        value: Value,
        usage: Option<Usage>,
    },
    InvalidAnswer {
        value: Value,
        error: String,
        usage: Option<Usage>,
    },
    Failed {
        message: String,
        usage: Option<Usage>,
    },
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FusionCandidate {
    pub provider_id: String,
    pub input_digest: String,
    pub outcome: CandidateOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FusionReport {
    pub input: FusionInput,
    pub input_digest: String,
    /// Retained in host registration order, including every failure and usage.
    pub candidates: Vec<FusionCandidate>,
    /// Distinct valid values need investigation. No majority is called true.
    pub disagreements: Vec<(String, String)>,
}

pub async fn collect_fusion(
    input: FusionInput,
    sources: Vec<Arc<dyn FusionCandidateSource>>,
    per_candidate_timeout: Duration,
    cancel: CancellationToken,
) -> Result<FusionReport, PluginError> {
    if !(3..=16).contains(&sources.len())
        || per_candidate_timeout.is_zero()
        || input.question.trim().is_empty()
        || input
            .constraints
            .iter()
            .any(|constraint| constraint.trim().is_empty())
    {
        return Err(PluginError::InvalidArguments(
            "Fusion requires a question, 3–16 distinct providers and a nonzero timeout".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    for source in &sources {
        if !valid_name(source.provider_id())
            || !ids.insert(source.provider_id().to_ascii_lowercase())
        {
            return Err(PluginError::InvalidArguments(
                "Fusion provider identities must be distinct".into(),
            ));
        }
    }
    let validator = Arc::new(
        ArgumentValidator::new(&input.answer_schema)
            .map_err(|error| PluginError::InvalidArguments(error.to_string()))?,
    );
    let digest = input.digest()?;
    let input = Arc::new(input);
    let all_cancel = cancel.child_token();
    let _guard = CancelOnDrop(all_cancel.clone());
    let mut pending = FuturesUnordered::new();
    for (index, source) in sources.into_iter().enumerate() {
        let input = input.clone();
        let token = all_cancel.child_token();
        let validator = validator.clone();
        let digest = digest.clone();
        pending.push(async move {
            let provider_id = source.provider_id().to_owned();
            let _guard = CancelOnDrop(token.clone());
            let outcome = tokio::select! {
                biased;
                _ = token.cancelled() => CandidateOutcome::Cancelled,
                result = tokio::time::timeout(per_candidate_timeout, source.answer(input, token.clone())) => match result {
                    Err(_) => CandidateOutcome::TimedOut,
                    Ok(CandidateResponse::Failed { message, usage }) => CandidateOutcome::Failed { message, usage },
                    Ok(CandidateResponse::Answer { value, usage }) => match validator.validate(&value) {
                        Ok(()) => CandidateOutcome::Answer { value, usage },
                        Err(error) => CandidateOutcome::InvalidAnswer { value, error: error.to_string(), usage },
                    },
                },
            };
            (index, FusionCandidate { provider_id, input_digest: digest, outcome })
        });
    }
    let mut candidates = Vec::new();
    while let Some(candidate) = pending.next().await {
        candidates.push(candidate);
    }
    candidates.sort_by_key(|(index, _)| *index);
    let candidates: Vec<FusionCandidate> = candidates
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect();
    let mut disagreements = Vec::new();
    for (index, first) in candidates.iter().enumerate() {
        if let CandidateOutcome::Answer {
            value: first_value, ..
        } = &first.outcome
        {
            for second in candidates.iter().skip(index + 1) {
                if let CandidateOutcome::Answer {
                    value: second_value,
                    ..
                } = &second.outcome
                    && first_value != second_value
                {
                    disagreements.push((first.provider_id.clone(), second.provider_id.clone()));
                }
            }
        }
    }
    Ok(FusionReport {
        input: (*input).clone(),
        input_digest: digest,
        candidates,
        disagreements,
    })
}

fn canonical_bytes(value: &Value, output: &mut Vec<u8>) -> Result<(), PluginError> {
    match value {
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                canonical_bytes(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(object) => {
            output.push(b'{');
            let mut fields: Vec<_> = object.iter().collect();
            fields.sort_by_key(|(key, _)| *key);
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)
                    .map_err(|error| PluginError::InvalidArguments(error.to_string()))?;
                output.push(b':');
                canonical_bytes(value, output)?;
            }
            output.push(b'}');
        }
        primitive => serde_json::to_writer(output, primitive)
            .map_err(|error| PluginError::InvalidArguments(error.to_string()))?,
    }
    Ok(())
}
