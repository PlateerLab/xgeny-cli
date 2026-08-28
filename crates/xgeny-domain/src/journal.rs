use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Extensions;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunJournalEventBody {
    pub api_version: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: Extensions,
    pub schema_version: String,
    pub event_id: String,
    pub run_id: String,
    pub sequence: u64,
    pub timestamp: String,
    pub actor: Actor,
    pub event_type: String,
    pub idempotency_key: Option<String>,
    pub causation_id: Option<String>,
    pub correlation_id: String,
    pub required_extensions: Vec<String>,
    pub previous_event_digest: Option<String>,
    pub payload: serde_json::Map<String, Value>,
    pub event_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Model,
    Runtime,
    Tool,
    Xgen,
    Connector,
}
