//! Offline JEv adapter. No model connection or paid call is made here.
//! Scores stay provisional and cannot satisfy StrongFlow evidence.

use crate::*;
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
