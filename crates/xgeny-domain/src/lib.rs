//! Canonical, I/O-free Rust representation of the `XGENy` v0.1 wire protocol.

mod capability;
mod common;
mod execution;
mod journal;
mod policy;
mod work_graph;

pub use capability::*;
pub use common::*;
pub use execution::*;
pub use journal::*;
pub use policy::*;
pub use work_graph::*;

use serde::{Deserialize, Serialize};

/// A strongly typed canonical document after JSON Schema validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ProtocolDocument {
    CapabilityDefinition(Box<CapabilityDefinitionBody>),
    CapabilityInstance(Box<CapabilityInstanceBody>),
    PermissionRequest(Box<PermissionRequestBody>),
    PolicyDecision(Box<PolicyDecisionBody>),
    InvocationPlan(Box<InvocationPlanBody>),
    WorkGraph(Box<WorkGraphBody>),
    RunJournalEvent(Box<RunJournalEventBody>),
    ExecutionReceipt(Box<ExecutionReceiptBody>),
}
