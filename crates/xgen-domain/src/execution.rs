use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ArtifactRef, CapabilityRef, EffectClass, ExecutionMode, Extensions, Placement,
    ResolvedResource, VerificationEvidence, VerificationRule,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationPlanBody {
    pub api_version: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: Extensions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_extensions: Vec<String>,
    pub plan_id: String,
    pub run_id: String,
    pub step_id: String,
    pub capability: CapabilityRef,
    pub selected_instance_id: String,
    pub arguments: Value,
    pub arguments_size_bytes: u64,
    pub input_digest: String,
    pub resolved_resources: Vec<ResolvedResource>,
    pub effect_class: EffectClass,
    pub execution_mode: ExecutionMode,
    pub policy_decision_id: String,
    pub idempotency_key: Option<String>,
    pub verification_plan: Vec<VerificationRule>,
    pub candidates: Vec<Candidate>,
    pub selection_reasons: Vec<String>,
    pub fallback: Fallback,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Candidate {
    pub instance_id: String,
    pub eligible: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackCondition {
    ReadOnly,
    EffectNotStarted,
    ConfirmedNotExecuted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Fallback {
    pub allowed_conditions: Vec<FallbackCondition>,
    pub instance_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionReceiptBody {
    pub api_version: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: Extensions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_extensions: Vec<String>,
    pub receipt_id: String,
    pub run_id: String,
    pub step_id: String,
    pub invocation_id: String,
    pub plan_id: String,
    pub capability: CapabilityRef,
    pub instance_id: String,
    pub input_digest: String,
    pub input_summary: String,
    pub policy: ReceiptPolicy,
    pub executor: Executor,
    pub effect: ReceiptEffect,
    pub status: ReceiptStatus,
    pub started_at: String,
    pub ended_at: String,
    pub output_digest: String,
    pub artifacts: Vec<ArtifactRef>,
    pub verification: Vec<VerificationEvidence>,
    pub redactions_applied: Vec<String>,
    pub previous_receipt_digest: Option<String>,
    pub receipt_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptPolicy {
    pub decision_id: String,
    pub decision_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Executor {
    pub id: String,
    pub placement: Placement,
    pub platform: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptEffect {
    pub class: EffectClass,
    pub started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
    Unknown,
}
