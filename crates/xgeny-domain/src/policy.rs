use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    CapabilityRef, CriticalAction, EffectClass, Extensions, GrantLifetime, ResolvedResource,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRequestBody {
    pub api_version: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: Extensions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_extensions: Vec<String>,
    pub request_id: String,
    pub run_id: String,
    pub step_id: String,
    pub capability: CapabilityRef,
    pub effect_class: EffectClass,
    pub requested_scopes: Vec<String>,
    pub resolved_resources: Vec<ResolvedResource>,
    pub critical_actions: Vec<CriticalAction>,
    pub requested_lifetime: GrantLifetime,
    pub reason: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyDecisionBody {
    pub api_version: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: Extensions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_extensions: Vec<String>,
    pub decision_id: String,
    pub request_id: String,
    pub decision: Decision,
    pub policy_sources: Vec<PolicySource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_reasons: Vec<String>,
    pub decided_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicySource {
    pub kind: PolicySourceKind,
    pub id: String,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySourceKind {
    Host,
    UserProfile,
    RunGrant,
    ManagedLease,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grant {
    pub lifetime: GrantLifetime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub scopes: Vec<String>,
    pub resources: Vec<ResolvedResource>,
    pub critical_actions: Vec<CriticalAction>,
}
