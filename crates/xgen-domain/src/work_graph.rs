use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ArtifactRef, CapabilityRef, ExecutionMode, Extensions, VerificationState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphBody {
    pub api_version: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: Extensions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_extensions: Vec<String>,
    pub run_id: String,
    pub revision: u64,
    pub authority: String,
    pub goal: String,
    pub execution_mode: ExecutionMode,
    pub status: WorkGraphStatus,
    pub steps: Vec<WorkStep>,
    pub journal_sequence: u64,
    pub journal_head_digest: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkGraphStatus {
    Pending,
    Running,
    WaitingInput,
    Completed,
    Failed,
    Blocked,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkStep {
    pub step_id: String,
    pub objective: String,
    pub depends_on: Vec<String>,
    pub capability: CapabilityRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation_plan_id: Option<String>,
    pub status: WorkStepStatus,
    pub attempts: u64,
    pub verification_status: VerificationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStepStatus {
    Pending,
    Ready,
    Running,
    WaitingInput,
    Validating,
    Completed,
    Failed,
    Blocked,
    Cancelled,
}
