#![doc = "I/O-free state transitions and hash-chained events for a durable `XGENy` run."]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunEvent {
    pub event_id: String,
    pub run_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub recorded_at: String,
    pub body: RunEventBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RunEventBody {
    RunCreated {
        goal: String,
    },
    AgentLoopConfigured {
        budget: AgentLoopBudget,
    },
    ModelCallLifecycleConfigured {
        budget: ModelCallBudget,
    },
    ModelCallReserved {
        reservation: ModelCallReservation,
    },
    ModelCallBecameUnknown {
        call_id: String,
        reason: ModelCallUnknownReason,
    },
    ModelCallSettled {
        call_id: String,
        settlement: ModelCallSettlement,
    },
    PlanAccepted {
        decision: ExpectedPlanningTurn,
        steps: Vec<AcceptedPlanStep>,
    },
    CompletionCandidateRecorded {
        decision: ExpectedPlanningTurn,
        candidate_id: String,
        summary_digest: String,
    },
    StepPlanned {
        step_id: String,
        objective: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        depends_on: Vec<String>,
    },
    EffectIntentCommitted {
        step_id: String,
        intent: Box<EffectIntent>,
    },
    InvocationMaterialUnavailable {
        step_id: String,
        effect_id: String,
        reason: InvocationMaterialUnavailableReason,
    },
    EffectExecutionStarted {
        step_id: String,
        effect_id: String,
    },
    EffectSucceeded {
        step_id: String,
        effect_id: String,
        #[serde(rename = "receiptDigest")]
        evidence_digest: String,
    },
    EffectFailed {
        step_id: String,
        effect_id: String,
        #[serde(rename = "receiptDigest")]
        evidence_digest: String,
    },
    EffectBecameUnknown {
        step_id: String,
        effect_id: String,
        reason: String,
    },
    ReconciliationStarted {
        step_id: String,
        effect_id: String,
    },
    ReconciliationResolved {
        step_id: String,
        effect_id: String,
        resolution: ReconciliationResolution,
        evidence_digest: String,
    },
    ManualInterventionRequired {
        step_id: String,
        effect_id: String,
        reason: String,
    },
    VerificationPassed {
        step_id: String,
    },
    VerificationFailed {
        step_id: String,
        reason: String,
    },
    VerificationRecorded {
        step_id: String,
        effect_id: String,
        disposition: VerificationDisposition,
        receipt_id: String,
        receipt_digest: String,
    },
}

impl RunEventBody {
    fn kind(&self) -> &'static str {
        match self {
            Self::RunCreated { .. } => "run_created",
            Self::AgentLoopConfigured { .. } => "agent_loop_configured",
            Self::ModelCallLifecycleConfigured { .. } => "model_call_lifecycle_configured",
            Self::ModelCallReserved { .. } => "model_call_reserved",
            Self::ModelCallBecameUnknown { .. } => "model_call_became_unknown",
            Self::ModelCallSettled { .. } => "model_call_settled",
            Self::PlanAccepted { .. } => "plan_accepted",
            Self::CompletionCandidateRecorded { .. } => "completion_candidate_recorded",
            Self::StepPlanned { .. } => "step_planned",
            Self::EffectIntentCommitted { .. } => "effect_intent_committed",
            Self::InvocationMaterialUnavailable { .. } => "invocation_material_unavailable",
            Self::EffectExecutionStarted { .. } => "effect_execution_started",
            Self::EffectSucceeded { .. } => "effect_succeeded",
            Self::EffectFailed { .. } => "effect_failed",
            Self::EffectBecameUnknown { .. } => "effect_became_unknown",
            Self::ReconciliationStarted { .. } => "reconciliation_started",
            Self::ReconciliationResolved { .. } => "reconciliation_resolved",
            Self::ManualInterventionRequired { .. } => "manual_intervention_required",
            Self::VerificationPassed { .. } => "verification_passed",
            Self::VerificationFailed { .. } => "verification_failed",
            Self::VerificationRecorded { .. } => "verification_recorded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectIntent {
    pub effect_id: String,
    pub action_digest: String,
    pub invocation: InvocationBinding,
    pub effect_class: EffectClass,
    pub idempotency_key: Option<String>,
    pub sink_guarantee: SinkGuarantee,
    pub authorization: AuthorizationUse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_provenance: Option<ReceiptProvenance>,
}

/// Core-issued, secret-free facts needed to construct a protocol `ExecutionReceipt` after a
/// process restart. The adapter cannot create or modify this binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptProvenance {
    pub profile_version: String,
    pub invocation_id: String,
    pub plan_id: String,
    pub policy_decision_id: String,
    pub policy_decision_digest: String,
    pub executor_id: String,
    pub executor_placement: ReceiptPlacement,
    pub executor_platform: String,
    pub input_summary: String,
    pub verification_plan: Vec<ReceiptVerificationRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptPlacement {
    Local,
    Device,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptVerificationStrategy {
    OutputSchema,
    Postcondition,
    ArtifactDigest,
    Receipt,
    Human,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptVerificationRule {
    pub strategy: ReceiptVerificationStrategy,
    pub required: bool,
}

/// Final core verification result bound to one persisted `ExecutionReceipt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDisposition {
    Passed,
    Failed,
    Inconclusive,
}

/// Calculate the canonical digest covered by a new authorization binding.
///
/// # Errors
///
/// Returns an error when RFC 8785 canonicalization fails.
pub fn receipt_provenance_digest(
    provenance: &ReceiptProvenance,
) -> Result<String, AuthorizationDigestError> {
    let canonical = serde_jcs::to_vec(provenance)
        .map_err(|error| AuthorizationDigestError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

/// Immutable executable binding retained with a durable effect intent.
///
/// Dynamic health, authentication state, and cost hints are deliberately excluded. The binding
/// digest identifies the adapter endpoint/operation selected by the trusted admission path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationBinding {
    pub capability_id: String,
    pub contract_version: String,
    pub definition_digest: String,
    pub instance_id: String,
    pub instance_binding_digest: String,
}

pub const INVOCATION_MATERIAL_FORMAT_VERSION: u32 = 1;
const MAX_MATERIAL_REFERENCE_COMPONENT_BYTES: usize = 128;

/// Secret-free, version-pinned recipe reference used to reconstruct invocation arguments.
///
/// A reference is an identifier, never a path, URL, bearer token, raw argument, or credential.
/// Its provider owns the durable recipe and must return canonical invocation arguments when asked
/// for this exact `(reference_id, revision)` pair.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReconstructableMaterialReference {
    provider_id: String,
    reference_id: String,
    revision: String,
}

impl ReconstructableMaterialReference {
    /// Build a bounded opaque reference. Components intentionally reject path and URI syntax.
    ///
    /// # Errors
    ///
    /// Returns an error when a component is empty, oversized, or contains characters outside the
    /// identifier alphabet `[A-Za-z0-9._-]`.
    pub fn new(
        provider_id: impl Into<String>,
        reference_id: impl Into<String>,
        revision: impl Into<String>,
    ) -> Result<Self, InvocationMaterialError> {
        let reference = Self {
            provider_id: provider_id.into(),
            reference_id: reference_id.into(),
            revision: revision.into(),
        };
        validate_reference_component("provider_id", &reference.provider_id)?;
        validate_reference_component("reference_id", &reference.reference_id)?;
        validate_reference_component("revision", &reference.revision)?;
        Ok(reference)
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    #[must_use]
    pub fn reference_id(&self) -> &str {
        &self.reference_id
    }

    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    fn validate(&self) -> Result<(), InvocationMaterialError> {
        validate_reference_component("provider_id", &self.provider_id)?;
        validate_reference_component("reference_id", &self.reference_id)?;
        validate_reference_component("revision", &self.revision)
    }
}

impl std::fmt::Debug for ReconstructableMaterialReference {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReconstructableMaterialReference")
            .field("provider_id", &self.provider_id)
            .field("reference_id", &"<redacted>")
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    content = "reference",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InvocationMaterialRetention {
    Ephemeral,
    ReconstructableReference(ReconstructableMaterialReference),
}

impl std::fmt::Debug for InvocationMaterialRetention {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ephemeral => formatter.write_str("Ephemeral"),
            Self::ReconstructableReference(reference) => formatter
                .debug_tuple("ReconstructableReference")
                .field(reference)
                .finish(),
        }
    }
}

pub const PLANNED_INVOCATION_FORMAT_VERSION: u32 = 1;
pub const MAX_ACCEPTED_PLAN_STEPS: usize = 32;
pub const MAX_ACCEPTED_PLAN_EDGES: usize = 128;
pub const MAX_ACCEPTED_OBJECTIVE_BYTES: usize = 5_000;
pub const MODEL_CALL_FORMAT_VERSION: u32 = 1;

/// Durable upper bound for conservative model-call reservations.
///
/// A reservation consumes one slot even if the process exits before the provider observes the
/// request. This deliberately avoids undercounting possible sends after an ambiguous crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCallBudget {
    max_model_calls: u32,
}

impl ModelCallBudget {
    /// Build a non-zero model-call reservation budget.
    ///
    /// # Errors
    ///
    /// Returns an error when `max_model_calls` is zero.
    pub fn new(max_model_calls: u32) -> Result<Self, PlanningContractError> {
        let budget = Self { max_model_calls };
        budget.validate()?;
        Ok(budget)
    }

    #[must_use]
    pub const fn max_model_calls(&self) -> u32 {
        self.max_model_calls
    }

    fn validate(&self) -> Result<(), PlanningContractError> {
        if self.max_model_calls == 0 {
            return Err(PlanningContractError::ZeroBudget("max_model_calls"));
        }
        Ok(())
    }
}

/// Immutable Core-owned binding for one possible outbound planner request.
///
/// The record shape contains only bounded identifiers and commitments. A trusted host must supply
/// non-secret registry identifiers and commitments; the syntactic validators are not secret
/// detectors. Prompt text, response content, credentials, and provider error bodies must never be
/// placed in these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCallReservation {
    format_version: u32,
    call_id: String,
    planner_id: String,
    call_index: u32,
    turn_index: u32,
    base_sequence: u64,
    base_head_digest: String,
    context_digest: String,
    request_digest: String,
}

impl ModelCallReservation {
    /// Bind a planner request to one exact Run head and accepted-turn position.
    ///
    /// # Errors
    ///
    /// Returns an error for a syntactically invalid planner ID, zero indexes, malformed digests, or
    /// canonical encoding failure. Planner-ID validation checks only length and allowed ASCII; the
    /// caller remains responsible for supplying a non-secret registry identifier.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: impl Into<String>,
        authority_epoch: u64,
        planner_id: impl Into<String>,
        call_index: u32,
        turn_index: u32,
        base_sequence: u64,
        base_head_digest: impl Into<String>,
        context_digest: impl Into<String>,
        request_digest: impl Into<String>,
    ) -> Result<Self, PlanningContractError> {
        let run_id = run_id.into();
        let planner_id = planner_id.into();
        let base_head_digest = base_head_digest.into();
        let context_digest = context_digest.into();
        let request_digest = request_digest.into();
        require_planning_identifier("run_id", &run_id)?;
        require_model_call_identifier("planner_id", &planner_id)?;
        if call_index == 0 {
            return Err(PlanningContractError::ModelCallIndexZero);
        }
        if turn_index == 0 {
            return Err(PlanningContractError::TurnIndexZero);
        }
        require_planning_digest("base_head_digest", &base_head_digest)?;
        require_planning_digest("context_digest", &context_digest)?;
        require_planning_digest("request_digest", &request_digest)?;
        let call_id = model_call_id(
            &run_id,
            authority_epoch,
            &planner_id,
            call_index,
            turn_index,
            base_sequence,
            &base_head_digest,
            &context_digest,
            &request_digest,
        )?;
        Ok(Self {
            format_version: MODEL_CALL_FORMAT_VERSION,
            call_id,
            planner_id,
            call_index,
            turn_index,
            base_sequence,
            base_head_digest,
            context_digest,
            request_digest,
        })
    }

    #[must_use]
    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    #[must_use]
    pub fn planner_id(&self) -> &str {
        &self.planner_id
    }

    #[must_use]
    pub const fn call_index(&self) -> u32 {
        self.call_index
    }

    #[must_use]
    pub const fn turn_index(&self) -> u32 {
        self.turn_index
    }

    #[must_use]
    pub const fn base_sequence(&self) -> u64 {
        self.base_sequence
    }

    #[must_use]
    pub fn base_head_digest(&self) -> &str {
        &self.base_head_digest
    }

    #[must_use]
    pub fn context_digest(&self) -> &str {
        &self.context_digest
    }

    #[must_use]
    pub fn request_digest(&self) -> &str {
        &self.request_digest
    }

    fn validate_for(
        &self,
        run_id: &str,
        authority_epoch: u64,
    ) -> Result<(), PlanningContractError> {
        if self.format_version != MODEL_CALL_FORMAT_VERSION {
            return Err(PlanningContractError::UnsupportedModelCallFormatVersion(
                self.format_version,
            ));
        }
        require_planning_identifier("run_id", run_id)?;
        require_model_call_id("call_id", &self.call_id)?;
        require_model_call_identifier("planner_id", &self.planner_id)?;
        if self.call_index == 0 {
            return Err(PlanningContractError::ModelCallIndexZero);
        }
        if self.turn_index == 0 {
            return Err(PlanningContractError::TurnIndexZero);
        }
        require_planning_digest("base_head_digest", &self.base_head_digest)?;
        require_planning_digest("context_digest", &self.context_digest)?;
        require_planning_digest("request_digest", &self.request_digest)?;
        let expected = model_call_id(
            run_id,
            authority_epoch,
            &self.planner_id,
            self.call_index,
            self.turn_index,
            self.base_sequence,
            &self.base_head_digest,
            &self.context_digest,
            &self.request_digest,
        )?;
        if self.call_id != expected {
            return Err(PlanningContractError::ModelCallIdMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallUnknownReason {
    Timeout,
    TransportUnavailable,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallRejectionReason {
    PlannerInvalidResponse,
    ProviderLimit,
    ProposalRejected,
    MaterializationFailed,
    StaleHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallAbandonmentReason {
    RecoveryDiscarded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ModelCallStatus {
    Reserved,
    Unknown { reason: ModelCallUnknownReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "disposition",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ModelCallSettlement {
    Rejected { reason: ModelCallRejectionReason },
    Abandoned { reason: ModelCallAbandonmentReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCallState {
    pub reservation: ModelCallReservation,
    pub status: ModelCallStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCallLifecycleState {
    pub budget: ModelCallBudget,
    pub reserved_calls: u32,
    pub settled_calls: u32,
    pub unknown_calls: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_call: Option<ModelCallState>,
}

/// Durable limits for the model-owned portion of one local Runtime-mode Run.
///
/// `max_tool_calls` counts external effect starts (`StepState::attempts`), not conservative
/// reconciliation probes or Core verification. Those safety actions must remain available after
/// the model budget is exhausted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentLoopBudget {
    pub max_model_turns: u32,
    pub max_planned_steps: u32,
    pub max_tool_calls: u32,
    pub max_context_bytes: u64,
}

impl AgentLoopBudget {
    /// Build a non-zero bounded loop budget.
    ///
    /// # Errors
    ///
    /// Returns an error when any limit is zero.
    pub fn new(
        max_model_turns: u32,
        max_planned_steps: u32,
        max_tool_calls: u32,
        max_context_bytes: u64,
    ) -> Result<Self, PlanningContractError> {
        let budget = Self {
            max_model_turns,
            max_planned_steps,
            max_tool_calls,
            max_context_bytes,
        };
        budget.validate()?;
        Ok(budget)
    }

    fn validate(&self) -> Result<(), PlanningContractError> {
        for (field, value) in [
            ("max_model_turns", u64::from(self.max_model_turns)),
            ("max_planned_steps", u64::from(self.max_planned_steps)),
            ("max_tool_calls", u64::from(self.max_tool_calls)),
            ("max_context_bytes", self.max_context_bytes),
        ] {
            if value == 0 {
                return Err(PlanningContractError::ZeroBudget(field));
            }
        }
        Ok(())
    }
}

/// Host-owned binding for one accepted planner decision.
///
/// The model does not echo or choose these values. The Core binds the decision to the exact
/// context and accepted proposal digests before appending it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedPlanningTurn {
    turn_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_call_id: Option<String>,
    context_digest: String,
    proposal_digest: String,
}

impl ExpectedPlanningTurn {
    /// Build a self-validating accepted-turn binding.
    ///
    /// # Errors
    ///
    /// Returns an error for turn zero or malformed digests.
    pub fn new(
        turn_index: u32,
        context_digest: impl Into<String>,
        proposal_digest: impl Into<String>,
    ) -> Result<Self, PlanningContractError> {
        let turn = Self {
            turn_index,
            model_call_id: None,
            context_digest: context_digest.into(),
            proposal_digest: proposal_digest.into(),
        };
        turn.validate()?;
        Ok(turn)
    }

    /// Build an accepted decision bound to one durable model-call reservation.
    ///
    /// # Errors
    ///
    /// Returns an error for turn zero, a malformed call ID, or malformed digests.
    pub fn for_model_call(
        turn_index: u32,
        call_id: impl Into<String>,
        context_digest: impl Into<String>,
        proposal_digest: impl Into<String>,
    ) -> Result<Self, PlanningContractError> {
        let turn = Self {
            turn_index,
            model_call_id: Some(call_id.into()),
            context_digest: context_digest.into(),
            proposal_digest: proposal_digest.into(),
        };
        turn.validate()?;
        Ok(turn)
    }

    #[must_use]
    pub const fn turn_index(&self) -> u32 {
        self.turn_index
    }

    #[must_use]
    pub fn model_call_id(&self) -> Option<&str> {
        self.model_call_id.as_deref()
    }

    #[must_use]
    pub fn context_digest(&self) -> &str {
        &self.context_digest
    }

    #[must_use]
    pub fn proposal_digest(&self) -> &str {
        &self.proposal_digest
    }

    fn validate(&self) -> Result<(), PlanningContractError> {
        if self.turn_index == 0 {
            return Err(PlanningContractError::TurnIndexZero);
        }
        if let Some(call_id) = &self.model_call_id {
            require_model_call_id("model_call_id", call_id)?;
        }
        require_planning_digest("context_digest", &self.context_digest)?;
        require_planning_digest("proposal_digest", &self.proposal_digest)
    }
}

/// Host-selected execution semantics for a planned invocation.
///
/// The initial profile deliberately excludes Instance, trust, data-boundary, policy and approval
/// choices. Those remain trusted routing/admission decisions after planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedExecutionProfile {
    LocalSyncOnceV1,
}

/// Secret-free semantic facts calculated from normalized transient arguments before plan commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedInvocationSpec {
    capability_id: String,
    contract_version: String,
    definition_digest: String,
    action_digest: String,
    plan_input_digest: String,
    execution_profile: PlannedExecutionProfile,
    target_os: String,
    target_arch: String,
}

impl PlannedInvocationSpec {
    /// Build the immutable invocation facts that an accepted Step must later admit.
    ///
    /// # Errors
    ///
    /// Returns an error for empty identifiers, unsupported target names, or malformed digests.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        capability_id: impl Into<String>,
        contract_version: impl Into<String>,
        definition_digest: impl Into<String>,
        action_digest: impl Into<String>,
        plan_input_digest: impl Into<String>,
        execution_profile: PlannedExecutionProfile,
        target_os: impl Into<String>,
        target_arch: impl Into<String>,
    ) -> Result<Self, PlanningContractError> {
        let spec = Self {
            capability_id: capability_id.into(),
            contract_version: contract_version.into(),
            definition_digest: definition_digest.into(),
            action_digest: action_digest.into(),
            plan_input_digest: plan_input_digest.into(),
            execution_profile,
            target_os: target_os.into(),
            target_arch: target_arch.into(),
        };
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<(), PlanningContractError> {
        require_planning_identifier("capability_id", &self.capability_id)?;
        require_planning_identifier("contract_version", &self.contract_version)?;
        require_planning_identifier("target_os", &self.target_os)?;
        require_planning_identifier("target_arch", &self.target_arch)?;
        if !matches!(self.target_os.as_str(), "linux" | "macos" | "windows") {
            return Err(PlanningContractError::UnsupportedTarget("target_os"));
        }
        if !matches!(self.target_arch.as_str(), "x86_64" | "aarch64") {
            return Err(PlanningContractError::UnsupportedTarget("target_arch"));
        }
        require_planning_digest("definition_digest", &self.definition_digest)?;
        require_planning_digest("action_digest", &self.action_digest)?;
        require_planning_digest("plan_input_digest", &self.plan_input_digest)
    }
}

/// Journal-safe binding between one accepted Step and its exact future admission input.
///
/// Raw arguments and the local provider reference are deliberately absent. The latter lives only
/// in the atomically committed [`PlannedInvocationMaterialRecord`] sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedInvocationBinding {
    format_version: u32,
    plan_id: String,
    proposal_digest: String,
    capability_id: String,
    contract_version: String,
    definition_digest: String,
    action_digest: String,
    plan_input_digest: String,
    execution_profile: PlannedExecutionProfile,
    target_os: String,
    target_arch: String,
    spec_digest: String,
    plan_input_record_digest: String,
}

impl PlannedInvocationBinding {
    #[must_use]
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    #[must_use]
    pub fn proposal_digest(&self) -> &str {
        &self.proposal_digest
    }

    #[must_use]
    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }

    #[must_use]
    pub fn contract_version(&self) -> &str {
        &self.contract_version
    }

    #[must_use]
    pub fn definition_digest(&self) -> &str {
        &self.definition_digest
    }

    #[must_use]
    pub fn action_digest(&self) -> &str {
        &self.action_digest
    }

    #[must_use]
    pub fn plan_input_digest(&self) -> &str {
        &self.plan_input_digest
    }

    #[must_use]
    pub const fn execution_profile(&self) -> PlannedExecutionProfile {
        self.execution_profile
    }

    #[must_use]
    pub fn target_os(&self) -> &str {
        &self.target_os
    }

    #[must_use]
    pub fn target_arch(&self) -> &str {
        &self.target_arch
    }

    #[must_use]
    pub fn plan_input_record_digest(&self) -> &str {
        &self.plan_input_record_digest
    }

    fn as_spec(&self) -> PlannedInvocationSpec {
        PlannedInvocationSpec {
            capability_id: self.capability_id.clone(),
            contract_version: self.contract_version.clone(),
            definition_digest: self.definition_digest.clone(),
            action_digest: self.action_digest.clone(),
            plan_input_digest: self.plan_input_digest.clone(),
            execution_profile: self.execution_profile,
            target_os: self.target_os.clone(),
            target_arch: self.target_arch.clone(),
        }
    }

    fn validate(&self) -> Result<(), PlanningContractError> {
        if self.format_version != PLANNED_INVOCATION_FORMAT_VERSION {
            return Err(PlanningContractError::UnsupportedFormatVersion(
                self.format_version,
            ));
        }
        require_planning_identifier("plan_id", &self.plan_id)?;
        require_planning_digest("proposal_digest", &self.proposal_digest)?;
        require_planning_digest("spec_digest", &self.spec_digest)?;
        require_planning_digest("plan_input_record_digest", &self.plan_input_record_digest)?;
        self.as_spec().validate()?;
        let expected = planned_invocation_spec_digest(&self.as_spec())?;
        if expected != self.spec_digest {
            return Err(PlanningContractError::SpecDigestMismatch);
        }
        Ok(())
    }
}

/// Local-store sidecar containing the opaque immutable recipe for one planned invocation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedInvocationMaterialRecord {
    format_version: u32,
    run_id: String,
    step_id: String,
    plan_id: String,
    proposal_digest: String,
    spec_digest: String,
    reference: ReconstructableMaterialReference,
    record_digest: String,
}

impl PlannedInvocationMaterialRecord {
    /// Bind an immutable provider recipe to one journal-safe planned invocation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identities, references, or canonical digest failures.
    pub fn bind(
        run_id: impl Into<String>,
        step_id: impl Into<String>,
        proposal_digest: impl Into<String>,
        spec: PlannedInvocationSpec,
        reference: ReconstructableMaterialReference,
    ) -> Result<(PlannedInvocationBinding, Self), PlanningContractError> {
        let run_id = run_id.into();
        let step_id = step_id.into();
        let proposal_digest = proposal_digest.into();
        require_planning_identifier("run_id", &run_id)?;
        require_planning_identifier("step_id", &step_id)?;
        require_planning_digest("proposal_digest", &proposal_digest)?;
        spec.validate()?;
        reference.validate()?;
        let spec_digest = planned_invocation_spec_digest(&spec)?;
        let plan_id = planned_invocation_id(&run_id, &step_id, &proposal_digest, &spec_digest)?;
        let mut record = Self {
            format_version: PLANNED_INVOCATION_FORMAT_VERSION,
            run_id,
            step_id,
            plan_id: plan_id.clone(),
            proposal_digest: proposal_digest.clone(),
            spec_digest: spec_digest.clone(),
            reference,
            record_digest: String::new(),
        };
        record.record_digest = planned_invocation_material_record_digest(&record)?;
        let binding = PlannedInvocationBinding {
            format_version: PLANNED_INVOCATION_FORMAT_VERSION,
            plan_id,
            proposal_digest,
            capability_id: spec.capability_id,
            contract_version: spec.contract_version,
            definition_digest: spec.definition_digest,
            action_digest: spec.action_digest,
            plan_input_digest: spec.plan_input_digest,
            execution_profile: spec.execution_profile,
            target_os: spec.target_os,
            target_arch: spec.target_arch,
            spec_digest,
            plan_input_record_digest: record.record_digest.clone(),
        };
        record.verify_for(&record.run_id, &record.step_id, &binding)?;
        Ok((binding, record))
    }

    /// Verify the sidecar's exact Run/Step/spec binding.
    ///
    /// # Errors
    ///
    /// Returns an error for tampering, unsupported versions, or cross-Step reuse.
    pub fn verify_for(
        &self,
        run_id: &str,
        step_id: &str,
        binding: &PlannedInvocationBinding,
    ) -> Result<(), PlanningContractError> {
        if self.format_version != PLANNED_INVOCATION_FORMAT_VERSION {
            return Err(PlanningContractError::UnsupportedFormatVersion(
                self.format_version,
            ));
        }
        binding.validate()?;
        self.reference.validate()?;
        if self.run_id != run_id {
            return Err(PlanningContractError::BindingMismatch("run_id"));
        }
        if self.step_id != step_id {
            return Err(PlanningContractError::BindingMismatch("step_id"));
        }
        if self.plan_id != binding.plan_id {
            return Err(PlanningContractError::BindingMismatch("plan_id"));
        }
        if self.proposal_digest != binding.proposal_digest {
            return Err(PlanningContractError::BindingMismatch("proposal_digest"));
        }
        if self.spec_digest != binding.spec_digest {
            return Err(PlanningContractError::BindingMismatch("spec_digest"));
        }
        let expected_plan_id =
            planned_invocation_id(run_id, step_id, &self.proposal_digest, &self.spec_digest)?;
        if expected_plan_id != self.plan_id {
            return Err(PlanningContractError::PlanIdMismatch);
        }
        let expected_record_digest = planned_invocation_material_record_digest(self)?;
        if expected_record_digest != self.record_digest
            || self.record_digest != binding.plan_input_record_digest
        {
            return Err(PlanningContractError::RecordDigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[must_use]
    pub fn step_id(&self) -> &str {
        &self.step_id
    }

    #[must_use]
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    #[must_use]
    pub const fn reference(&self) -> &ReconstructableMaterialReference {
        &self.reference
    }

    #[must_use]
    pub fn record_digest(&self) -> &str {
        &self.record_digest
    }
}

impl std::fmt::Debug for PlannedInvocationMaterialRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlannedInvocationMaterialRecord")
            .field("format_version", &self.format_version)
            .field("run_id", &self.run_id)
            .field("step_id", &self.step_id)
            .field("plan_id", &self.plan_id)
            .field("proposal_digest", &self.proposal_digest)
            .field("spec_digest", &self.spec_digest)
            .field("reference", &self.reference)
            .field("record_digest", &self.record_digest)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcceptedPlanStep {
    pub step_id: String,
    pub objective: String,
    pub depends_on: Vec<String>,
    pub invocation: PlannedInvocationBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionCandidateState {
    pub candidate_id: String,
    pub context_digest: String,
    pub proposal_digest: String,
    pub summary_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentLoopState {
    pub budget: AgentLoopBudget,
    pub accepted_model_turns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_calls: Option<ModelCallLifecycleState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_candidate: Option<CompletionCandidateState>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelCallIdDigestInput<'a> {
    domain: &'static str,
    format_version: u32,
    run_id: &'a str,
    authority_epoch: u64,
    planner_id: &'a str,
    call_index: u32,
    turn_index: u32,
    base_sequence: u64,
    base_head_digest: &'a str,
    context_digest: &'a str,
    request_digest: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedInvocationSpecDigestInput<'a> {
    domain: &'static str,
    capability_id: &'a str,
    contract_version: &'a str,
    definition_digest: &'a str,
    action_digest: &'a str,
    plan_input_digest: &'a str,
    execution_profile: PlannedExecutionProfile,
    target_os: &'a str,
    target_arch: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedInvocationIdInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    step_id: &'a str,
    proposal_digest: &'a str,
    spec_digest: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedInvocationMaterialRecordDigestInput<'a> {
    domain: &'static str,
    format_version: u32,
    run_id: &'a str,
    step_id: &'a str,
    plan_id: &'a str,
    proposal_digest: &'a str,
    spec_digest: &'a str,
    reference: &'a ReconstructableMaterialReference,
}

fn planned_invocation_spec_digest(
    spec: &PlannedInvocationSpec,
) -> Result<String, PlanningContractError> {
    let canonical = serde_jcs::to_vec(&PlannedInvocationSpecDigestInput {
        domain: "xgeny.planned-invocation.spec/v1",
        capability_id: &spec.capability_id,
        contract_version: &spec.contract_version,
        definition_digest: &spec.definition_digest,
        action_digest: &spec.action_digest,
        plan_input_digest: &spec.plan_input_digest,
        execution_profile: spec.execution_profile,
        target_os: &spec.target_os,
        target_arch: &spec.target_arch,
    })
    .map_err(|error| PlanningContractError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn planned_invocation_id(
    run_id: &str,
    step_id: &str,
    proposal_digest: &str,
    spec_digest: &str,
) -> Result<String, PlanningContractError> {
    let canonical = serde_jcs::to_vec(&PlannedInvocationIdInput {
        domain: "xgeny.planned-invocation.id/v1",
        run_id,
        step_id,
        proposal_digest,
        spec_digest,
    })
    .map_err(|error| PlanningContractError::Canonicalization(error.to_string()))?;
    let digest = sha256_digest(&canonical);
    let encoded = digest.strip_prefix("sha256:").unwrap_or(&digest);
    Ok(format!("plan-{encoded}"))
}

#[allow(clippy::too_many_arguments)]
fn model_call_id(
    run_id: &str,
    authority_epoch: u64,
    planner_id: &str,
    call_index: u32,
    turn_index: u32,
    base_sequence: u64,
    base_head_digest: &str,
    context_digest: &str,
    request_digest: &str,
) -> Result<String, PlanningContractError> {
    let canonical = serde_jcs::to_vec(&ModelCallIdDigestInput {
        domain: "xgeny.model-call.id/v1",
        format_version: MODEL_CALL_FORMAT_VERSION,
        run_id,
        authority_epoch,
        planner_id,
        call_index,
        turn_index,
        base_sequence,
        base_head_digest,
        context_digest,
        request_digest,
    })
    .map_err(|error| PlanningContractError::Canonicalization(error.to_string()))?;
    let digest = sha256_digest(&canonical);
    let encoded = digest.strip_prefix("sha256:").unwrap_or(&digest);
    Ok(format!("model-call-{encoded}"))
}

fn planned_invocation_material_record_digest(
    record: &PlannedInvocationMaterialRecord,
) -> Result<String, PlanningContractError> {
    let canonical = serde_jcs::to_vec(&PlannedInvocationMaterialRecordDigestInput {
        domain: "xgeny.planned-invocation.material-record/v1",
        format_version: record.format_version,
        run_id: &record.run_id,
        step_id: &record.step_id,
        plan_id: &record.plan_id,
        proposal_digest: &record.proposal_digest,
        spec_digest: &record.spec_digest,
        reference: &record.reference,
    })
    .map_err(|error| PlanningContractError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn require_planning_identifier(
    field: &'static str,
    value: &str,
) -> Result<(), PlanningContractError> {
    const MAX_IDENTIFIER_BYTES: usize = 256;
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control)
    {
        return Err(PlanningContractError::InvalidIdentifier(field));
    }
    Ok(())
}

/// Validate bounded identifier wire syntax only.
///
/// This is intentionally not a credential, token, or sensitive-content detector. Callers must
/// source non-secret identifiers from a trusted registry instead of treating this check as DLP.
fn require_model_call_identifier(
    field: &'static str,
    value: &str,
) -> Result<(), PlanningContractError> {
    const MAX_IDENTIFIER_BYTES: usize = 256;
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(PlanningContractError::InvalidIdentifier(field));
    }
    Ok(())
}

fn require_model_call_id(field: &'static str, value: &str) -> Result<(), PlanningContractError> {
    require_model_call_identifier(field, value)?;
    let encoded = value
        .strip_prefix("model-call-")
        .ok_or(PlanningContractError::InvalidIdentifier(field))?;
    if encoded.len() != 64
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PlanningContractError::InvalidIdentifier(field));
    }
    Ok(())
}

fn require_planning_digest(field: &'static str, value: &str) -> Result<(), PlanningContractError> {
    if !is_sha256_digest(value) {
        return Err(PlanningContractError::InvalidDigest(field));
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PlanningContractError {
    #[error("planning budget `{0}` must be greater than zero")]
    ZeroBudget(&'static str),
    #[error("planning turn index must be greater than zero")]
    TurnIndexZero,
    #[error("model-call index must be greater than zero")]
    ModelCallIndexZero,
    #[error("planning identifier `{0}` is invalid")]
    InvalidIdentifier(&'static str),
    #[error("planned invocation target `{0}` is unsupported")]
    UnsupportedTarget(&'static str),
    #[error("planning digest `{0}` must be a lowercase SHA-256 digest")]
    InvalidDigest(&'static str),
    #[error("planned invocation format version {0} is unsupported")]
    UnsupportedFormatVersion(u32),
    #[error("model-call format version {0} is unsupported")]
    UnsupportedModelCallFormatVersion(u32),
    #[error("model-call ID differs from its durable bindings")]
    ModelCallIdMismatch,
    #[error("planned invocation spec digest differs from its fields")]
    SpecDigestMismatch,
    #[error("planned invocation material binding differs at `{0}`")]
    BindingMismatch(&'static str),
    #[error("planned invocation plan ID differs from its binding")]
    PlanIdMismatch,
    #[error("planned invocation material record digest differs from its binding")]
    RecordDigestMismatch,
    #[error("planning canonicalization failed: {0}")]
    Canonicalization(String),
    #[error(transparent)]
    InvocationMaterial(#[from] InvocationMaterialError),
}

/// Durable, secret-free sidecar binding for one exact effect intent.
///
/// The record deliberately excludes invocation arguments and credentials. It binds a recovery
/// mode and canonical material digest to the Run, Step, effect, semantic action, and selected
/// executable Instance. It is committed atomically with the intent by a supporting `RunStore`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationMaterialRecord {
    format_version: u32,
    material_id: String,
    run_id: String,
    step_id: String,
    effect_id: String,
    action_digest: String,
    invocation: InvocationBinding,
    material_digest: String,
    retention: InvocationMaterialRetention,
    record_digest: String,
}

impl InvocationMaterialRecord {
    /// Create a self-verifying material binding for a committed effect intent.
    ///
    /// # Errors
    ///
    /// Returns an error for missing identity fields, invalid references, or canonicalization
    /// failures.
    pub fn new(
        run_id: impl Into<String>,
        step_id: impl Into<String>,
        intent: &EffectIntent,
        material_digest: impl Into<String>,
        retention: InvocationMaterialRetention,
    ) -> Result<Self, InvocationMaterialError> {
        let mut record = Self {
            format_version: INVOCATION_MATERIAL_FORMAT_VERSION,
            material_id: String::new(),
            run_id: run_id.into(),
            step_id: step_id.into(),
            effect_id: intent.effect_id.clone(),
            action_digest: intent.action_digest.clone(),
            invocation: intent.invocation.clone(),
            material_digest: material_digest.into(),
            retention,
            record_digest: String::new(),
        };
        record.validate_shape()?;
        record.material_id =
            invocation_material_id(&record.run_id, &record.effect_id, &record.material_digest)?;
        record.record_digest = invocation_material_record_digest(&record)?;
        record.verify_for(&record.run_id, &record.step_id, intent)?;
        Ok(record)
    }

    /// Verify content integrity and exact binding to a durable intent.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed fields, tampering, or cross-intent
    /// record reuse.
    pub fn verify_for(
        &self,
        run_id: &str,
        step_id: &str,
        intent: &EffectIntent,
    ) -> Result<(), InvocationMaterialError> {
        if self.format_version != INVOCATION_MATERIAL_FORMAT_VERSION {
            return Err(InvocationMaterialError::UnsupportedFormatVersion(
                self.format_version,
            ));
        }
        self.validate_shape()?;
        if self.run_id != run_id {
            return Err(InvocationMaterialError::BindingMismatch("run_id"));
        }
        if self.step_id != step_id {
            return Err(InvocationMaterialError::BindingMismatch("step_id"));
        }
        if self.effect_id != intent.effect_id {
            return Err(InvocationMaterialError::BindingMismatch("effect_id"));
        }
        if self.action_digest != intent.action_digest {
            return Err(InvocationMaterialError::BindingMismatch("action_digest"));
        }
        if self.invocation != intent.invocation {
            return Err(InvocationMaterialError::BindingMismatch("invocation"));
        }
        if intent.authorization.binding.material_digest != self.material_digest {
            return Err(InvocationMaterialError::BindingMismatch("material_digest"));
        }
        if intent.authorization.binding.material_retention_digest
            != invocation_material_retention_digest(&self.retention)?
        {
            return Err(InvocationMaterialError::BindingMismatch(
                "material_retention_digest",
            ));
        }
        let expected_id =
            invocation_material_id(&self.run_id, &self.effect_id, &self.material_digest)?;
        if self.material_id != expected_id {
            return Err(InvocationMaterialError::MaterialIdMismatch);
        }
        let expected_digest = invocation_material_record_digest(self)?;
        if self.record_digest != expected_digest {
            return Err(InvocationMaterialError::RecordDigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    #[must_use]
    pub fn material_id(&self) -> &str {
        &self.material_id
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[must_use]
    pub fn step_id(&self) -> &str {
        &self.step_id
    }

    #[must_use]
    pub fn effect_id(&self) -> &str {
        &self.effect_id
    }

    #[must_use]
    pub fn action_digest(&self) -> &str {
        &self.action_digest
    }

    #[must_use]
    pub const fn invocation(&self) -> &InvocationBinding {
        &self.invocation
    }

    #[must_use]
    pub fn material_digest(&self) -> &str {
        &self.material_digest
    }

    #[must_use]
    pub const fn retention(&self) -> &InvocationMaterialRetention {
        &self.retention
    }

    #[must_use]
    pub fn record_digest(&self) -> &str {
        &self.record_digest
    }

    fn validate_shape(&self) -> Result<(), InvocationMaterialError> {
        for (field, value) in [
            ("run_id", self.run_id.as_str()),
            ("step_id", self.step_id.as_str()),
            ("effect_id", self.effect_id.as_str()),
            ("action_digest", self.action_digest.as_str()),
            ("material_digest", self.material_digest.as_str()),
        ] {
            if value.is_empty() {
                return Err(InvocationMaterialError::EmptyField(field));
            }
        }
        if let InvocationMaterialRetention::ReconstructableReference(reference) = &self.retention {
            reference.validate()?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for InvocationMaterialRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InvocationMaterialRecord")
            .field("format_version", &self.format_version)
            .field("material_id", &self.material_id)
            .field("run_id", &self.run_id)
            .field("step_id", &self.step_id)
            .field("effect_id", &self.effect_id)
            .field("action_digest", &self.action_digest)
            .field("invocation", &self.invocation)
            .field("material_digest", &self.material_digest)
            .field("retention", &self.retention)
            .field("record_digest", &self.record_digest)
            .finish()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InvocationMaterialDigestInput<'a, T: Serialize + ?Sized> {
    domain: &'static str,
    material: &'a T,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InvocationMaterialIdInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    effect_id: &'a str,
    material_digest: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InvocationMaterialRecordDigestInput<'a> {
    domain: &'static str,
    format_version: u32,
    material_id: &'a str,
    run_id: &'a str,
    step_id: &'a str,
    effect_id: &'a str,
    action_digest: &'a str,
    invocation: &'a InvocationBinding,
    material_digest: &'a str,
    retention: &'a InvocationMaterialRetention,
}

/// Commit to canonical invocation material without storing it.
///
/// # Errors
///
/// Returns an error when RFC 8785 canonicalization fails.
pub fn invocation_material_digest<T: Serialize + ?Sized>(
    material: &T,
) -> Result<String, InvocationMaterialError> {
    let canonical = serde_jcs::to_vec(&InvocationMaterialDigestInput {
        domain: "xgeny.invocation-material.payload/v1",
        material,
    })
    .map_err(|error| InvocationMaterialError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

/// Commit an authorization to the host-selected material retention recipe.
///
/// # Errors
///
/// Returns an error when RFC 8785 canonicalization fails.
pub fn invocation_material_retention_digest(
    retention: &InvocationMaterialRetention,
) -> Result<String, InvocationMaterialError> {
    let canonical = serde_jcs::to_vec(&InvocationMaterialDigestInput {
        domain: "xgeny.invocation-material.retention/v1",
        material: retention,
    })
    .map_err(|error| InvocationMaterialError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn invocation_material_id(
    run_id: &str,
    effect_id: &str,
    material_digest: &str,
) -> Result<String, InvocationMaterialError> {
    let canonical = serde_jcs::to_vec(&InvocationMaterialIdInput {
        domain: "xgeny.invocation-material.id/v1",
        run_id,
        effect_id,
        material_digest,
    })
    .map_err(|error| InvocationMaterialError::Canonicalization(error.to_string()))?;
    let digest = sha256_digest(&canonical);
    let encoded = digest.strip_prefix("sha256:").unwrap_or(&digest);
    Ok(format!("material-{encoded}"))
}

fn invocation_material_record_digest(
    record: &InvocationMaterialRecord,
) -> Result<String, InvocationMaterialError> {
    let canonical = serde_jcs::to_vec(&InvocationMaterialRecordDigestInput {
        domain: "xgeny.invocation-material.record/v1",
        format_version: record.format_version,
        material_id: &record.material_id,
        run_id: &record.run_id,
        step_id: &record.step_id,
        effect_id: &record.effect_id,
        action_digest: &record.action_digest,
        invocation: &record.invocation,
        material_digest: &record.material_digest,
        retention: &record.retention,
    })
    .map_err(|error| InvocationMaterialError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn validate_reference_component(
    field: &'static str,
    value: &str,
) -> Result<(), InvocationMaterialError> {
    if value.is_empty() {
        return Err(InvocationMaterialError::InvalidReferenceComponent(field));
    }
    if matches!(value, "." | "..")
        || value.len() > MAX_MATERIAL_REFERENCE_COMPONENT_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(InvocationMaterialError::InvalidReferenceComponent(field));
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum InvocationMaterialError {
    #[error("unsupported invocation material format version {0}")]
    UnsupportedFormatVersion(u32),
    #[error("invocation material field `{0}` must not be empty")]
    EmptyField(&'static str),
    #[error("invocation material reference component `{0}` is invalid")]
    InvalidReferenceComponent(&'static str),
    #[error("invocation material binding differs at `{0}`")]
    BindingMismatch(&'static str),
    #[error("invocation material identifier does not match its binding")]
    MaterialIdMismatch,
    #[error("invocation material record digest does not match its content")]
    RecordDigestMismatch,
    #[error("invocation material canonicalization failed: {0}")]
    Canonicalization(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationMaterialUnavailableReason {
    EphemeralMaterialLost,
    ReferenceUnavailable,
    ReferenceChanged,
    AdapterBindingUnavailable,
    CredentialBindingChanged,
    UnsupportedMaterialVersion,
}

impl InvocationMaterialUnavailableReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::EphemeralMaterialLost => "ephemeral_material_lost",
            Self::ReferenceUnavailable => "reference_unavailable",
            Self::ReferenceChanged => "reference_changed",
            Self::AdapterBindingUnavailable => "adapter_binding_unavailable",
            Self::CredentialBindingChanged => "credential_binding_changed",
            Self::UnsupportedMaterialVersion => "unsupported_material_version",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Reversible,
    Idempotent,
    NonIdempotent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkGuarantee {
    None,
    DeduplicateByKey,
    QueryByKey,
    DeduplicateAndQuery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationUse {
    pub grant_id: String,
    pub grant_digest: String,
    pub max_uses: u32,
    pub binding: AuthorizationBinding,
}

/// Run-local facts covered by one issued authorization digest.
///
/// The journal head is the state against which policy and routing were evaluated. Persisting the
/// binding lets replay reject copying an intent to another Run, Step, authority epoch, action, or
/// executable Instance even though the low-level journal types remain serializable primitives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationBinding {
    pub run_id: String,
    pub step_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub issued_at_sequence: u64,
    pub issued_at_head_digest: String,
    pub capability_id: String,
    pub contract_version: String,
    pub definition_digest: String,
    pub instance_id: String,
    pub instance_binding_digest: String,
    pub action_digest: String,
    pub material_digest: String,
    pub material_retention_digest: String,
    pub policy_evidence_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_provenance_digest: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizationDigestInput<'a> {
    domain: &'static str,
    binding: &'a AuthorizationBinding,
    max_uses: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OnceAuthorizationIdInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    action_digest: &'a str,
}

/// Derive the stable budget identity for one semantic action within a Run.
///
/// Keeping this derivation in the reducer's crate prevents a caller from changing only the grant
/// ID to mint a fresh one-shot budget for the same Run/action pair.
///
/// # Errors
///
/// Returns an error if RFC 8785 canonical JSON encoding fails.
pub fn once_authorization_id(
    run_id: &str,
    action_digest: &str,
) -> Result<String, AuthorizationDigestError> {
    let canonical = serde_jcs::to_vec(&OnceAuthorizationIdInput {
        domain: "xgeny.authorization-budget.once/v1",
        run_id,
        action_digest,
    })
    .map_err(|error| AuthorizationDigestError::Canonicalization(error.to_string()))?;
    let digest = sha256_digest(&canonical);
    let encoded = digest.strip_prefix("sha256:").unwrap_or(&digest);
    Ok(format!("authorization-{encoded}"))
}

/// Calculate the content digest the reducer expects for a durable authorization binding.
///
/// # Errors
///
/// Returns an error if RFC 8785 canonical JSON encoding fails.
pub fn authorization_digest(
    binding: &AuthorizationBinding,
    max_uses: u32,
) -> Result<String, AuthorizationDigestError> {
    let canonical = serde_jcs::to_vec(&AuthorizationDigestInput {
        domain: "xgeny.authorization.once/v2",
        binding,
        max_uses,
    })
    .map_err(|error| AuthorizationDigestError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationResolution {
    ProvedApplied,
    ProvedNotApplied,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventRecord {
    pub sequence: u64,
    pub previous_digest: Option<String>,
    pub event: RunEvent,
    pub digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DigestInput<'a> {
    sequence: u64,
    previous_digest: Option<&'a str>,
    event: &'a RunEvent,
}

impl EventRecord {
    /// Build the next immutable record in a hash chain.
    ///
    /// # Errors
    ///
    /// Returns an error if RFC 8785 canonical JSON encoding fails.
    pub fn next(previous: Option<&Self>, event: RunEvent) -> Result<Self, RecordError> {
        let sequence = previous.map_or(Ok(1), |record| {
            record
                .sequence
                .checked_add(1)
                .ok_or(RecordError::SequenceOverflow)
        })?;
        let previous_digest = previous.map(|record| record.digest.clone());
        let digest = record_digest(sequence, previous_digest.as_deref(), &event)?;
        Ok(Self {
            sequence,
            previous_digest,
            event,
            digest,
        })
    }

    /// Verify this record's derived digest.
    ///
    /// # Errors
    ///
    /// Returns an error if canonicalization fails or the stored digest differs.
    pub fn verify_digest(&self) -> Result<(), RecordError> {
        let actual = record_digest(self.sequence, self.previous_digest.as_deref(), &self.event)?;
        if actual != self.digest {
            return Err(RecordError::DigestMismatch {
                sequence: self.sequence,
                expected: self.digest.clone(),
                actual,
            });
        }
        Ok(())
    }
}

fn record_digest(
    sequence: u64,
    previous_digest: Option<&str>,
    event: &RunEvent,
) -> Result<String, RecordError> {
    let canonical = serde_jcs::to_vec(&DigestInput {
        sequence,
        previous_digest,
        event,
    })
    .map_err(|error| RecordError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn sha256_digest(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunState {
    pub run_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub goal: String,
    pub revision: u64,
    pub journal_sequence: u64,
    pub journal_head_digest: String,
    pub steps: BTreeMap<String, StepState>,
    pub authorization_consumption: BTreeMap<String, AuthorizationConsumption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_loop: Option<AgentLoopState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepState {
    pub step_id: String,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_invocation: Option<PlannedInvocationBinding>,
    pub status: StepStatus,
    pub attempts: u32,
    pub intent: Option<EffectIntent>,
    #[serde(rename = "receiptDigest")]
    pub effect_evidence_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_receipt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_receipt_digest: Option<String>,
    pub uncertainty_reason: Option<String>,
    pub reconciliation_evidence_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Planned,
    IntentCommitted,
    Executing,
    EffectUnknown,
    Reconciling,
    Validating,
    Completed,
    Failed,
    ManualRequired,
}

/// Existing Core operation that can advance one member of the derived frontier.
///
/// This is deliberately smaller than the internal Step lifecycle. [`DriveEffect`](Self::DriveEffect)
/// delegates `IntentCommitted`, `Executing`, `EffectUnknown`, and `Reconciling` to the existing
/// effect runtime, which retains the conservative recovery rules for each state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationAction {
    DriveEffect,
    Verify,
    Admit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierAction {
    pub step_id: String,
    pub action: ContinuationAction,
}

/// Why one dependency cannot release a downstream Step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DependencyBlockReason {
    NotCompleted,
    Failed,
    ManualRequired,
    ReceiptMissing,
    DependencyBlocked,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DependencyBlocker {
    pub step_id: String,
    pub reason: DependencyBlockReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingStep {
    pub step_id: String,
    pub pending_dependencies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedStep {
    pub step_id: String,
    pub blockers: Vec<DependencyBlocker>,
}

/// Deterministic, non-durable coordination view derived from one verified [`RunState`].
///
/// `actionable` is ordered conservatively: uncertain effects, reconciliation, verification,
/// unstarted committed intents, then newly admissible Steps. Equal lifecycle states are ordered
/// by byte-exact Step ID. A caller should commit at most one action and derive the frontier again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkFrontier {
    pub run_id: String,
    pub revision: u64,
    pub journal_sequence: u64,
    pub journal_head_digest: String,
    pub total_steps: usize,
    pub actionable: Vec<FrontierAction>,
    pub waiting: Vec<WaitingStep>,
    pub blocked: Vec<BlockedStep>,
    pub verified_completed_step_ids: Vec<String>,
    pub unverified_completed_step_ids: Vec<String>,
    pub failed_step_ids: Vec<String>,
    pub manual_required_step_ids: Vec<String>,
}

impl WorkFrontier {
    /// Return the first conservative single-orchestrator action, if any.
    #[must_use]
    pub fn next_action(&self) -> Option<&FrontierAction> {
        self.actionable.first()
    }

    /// Report whether every currently planned Step has a Receipt-bound completion.
    ///
    /// This does not assert that the user's goal or Run is complete. Run-level completion needs
    /// an explicit lifecycle contract and is intentionally outside this derived view.
    #[must_use]
    pub fn all_steps_receipt_completed(&self) -> bool {
        self.total_steps > 0 && self.verified_completed_step_ids.len() == self.total_steps
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FrontierError {
    #[error("step `{step_id}` depends on itself")]
    SelfDependency { step_id: String },
    #[error("step `{step_id}` repeats dependency `{dependency_id}`")]
    DuplicateDependency {
        step_id: String,
        dependency_id: String,
    },
    #[error("step `{step_id}` refers to unknown dependency `{dependency_id}`")]
    UnknownDependency {
        step_id: String,
        dependency_id: String,
    },
    #[error("WorkGraph dependency cycle detected")]
    DependencyCycle,
    #[error(
        "active step `{step_id}` has dependency `{dependency_id}` that is not Receipt-released"
    )]
    ActiveDependencyNotReleased {
        step_id: String,
        dependency_id: String,
        reason: DependencyBlockReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyOutcome {
    Satisfied,
    Pending,
    Blocked,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FrontierMetrics {
    #[cfg(test)]
    validation_steps: u64,
    #[cfg(test)]
    validation_dependencies: u64,
    #[cfg(test)]
    classification_steps: u64,
    #[cfg(test)]
    classification_dependencies: u64,
}

impl FrontierMetrics {
    fn record_validation_step(&mut self) {
        #[cfg(test)]
        {
            self.validation_steps = self.validation_steps.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_validation_dependency(&mut self) {
        #[cfg(test)]
        {
            self.validation_dependencies = self.validation_dependencies.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_classification_step(&mut self) {
        #[cfg(test)]
        {
            self.classification_steps = self.classification_steps.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_classification_dependency(&mut self) {
        #[cfg(test)]
        {
            self.classification_dependencies = self.classification_dependencies.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }
}

struct ValidatedFrontierGraph<'a> {
    remaining_dependencies: BTreeMap<&'a str, usize>,
    dependents: BTreeMap<&'a str, Vec<&'a str>>,
    zero_indegree: BTreeSet<&'a str>,
}

#[derive(Default)]
struct FrontierAssembly<'a> {
    outcomes: BTreeMap<&'a str, DependencyOutcome>,
    ranked_actions: Vec<(u8, &'a str, ContinuationAction)>,
    waiting: Vec<WaitingStep>,
    blocked: Vec<BlockedStep>,
    verified_completed_step_ids: Vec<String>,
    unverified_completed_step_ids: Vec<String>,
    failed_step_ids: Vec<String>,
    manual_required_step_ids: Vec<String>,
    processed: usize,
}

impl<'a> FrontierAssembly<'a> {
    fn record_step(
        &mut self,
        state: &'a RunState,
        step_id: &'a str,
        metrics: &mut FrontierMetrics,
    ) -> Result<(), FrontierError> {
        self.processed = self.processed.saturating_add(1);
        metrics.record_classification_step();
        let step = &state.steps[step_id];
        let (pending_dependencies, blockers) = self.partition_dependencies(state, step, metrics);
        let dependencies_released = blockers.is_empty() && pending_dependencies.is_empty();
        if step.status != StepStatus::Planned && !dependencies_released {
            let (dependency_id, reason) = blockers.first().map_or_else(
                || {
                    (
                        pending_dependencies
                            .first()
                            .expect("unreleased dependency must be classified")
                            .clone(),
                        DependencyBlockReason::NotCompleted,
                    )
                },
                |blocker| (blocker.step_id.clone(), blocker.reason),
            );
            return Err(FrontierError::ActiveDependencyNotReleased {
                step_id: step_id.to_owned(),
                dependency_id,
                reason,
            });
        }

        let outcome = self.record_status(step_id, step, pending_dependencies, blockers);
        self.outcomes.insert(step_id, outcome);
        Ok(())
    }

    fn partition_dependencies(
        &self,
        state: &RunState,
        step: &StepState,
        metrics: &mut FrontierMetrics,
    ) -> (Vec<String>, Vec<DependencyBlocker>) {
        let mut pending = Vec::new();
        let mut blockers = Vec::new();
        for dependency_id in &step.depends_on {
            metrics.record_classification_dependency();
            match self
                .outcomes
                .get(dependency_id.as_str())
                .copied()
                .expect("topological traversal processes dependencies first")
            {
                DependencyOutcome::Satisfied => {}
                DependencyOutcome::Pending => pending.push(dependency_id.clone()),
                DependencyOutcome::Blocked => blockers.push(DependencyBlocker {
                    step_id: dependency_id.clone(),
                    reason: blocker_reason(&state.steps[dependency_id]),
                }),
            }
        }
        pending.sort();
        blockers.sort();
        blockers.dedup();
        (pending, blockers)
    }

    fn record_status(
        &mut self,
        step_id: &'a str,
        step: &StepState,
        pending_dependencies: Vec<String>,
        blockers: Vec<DependencyBlocker>,
    ) -> DependencyOutcome {
        match step.status {
            StepStatus::Planned if !blockers.is_empty() => {
                self.blocked.push(BlockedStep {
                    step_id: step_id.to_owned(),
                    blockers,
                });
                DependencyOutcome::Blocked
            }
            StepStatus::Planned if !pending_dependencies.is_empty() => {
                self.waiting.push(WaitingStep {
                    step_id: step_id.to_owned(),
                    pending_dependencies,
                });
                DependencyOutcome::Pending
            }
            StepStatus::Planned => {
                self.ranked_actions
                    .push((5, step_id, ContinuationAction::Admit));
                DependencyOutcome::Pending
            }
            StepStatus::IntentCommitted => {
                self.ranked_actions
                    .push((4, step_id, ContinuationAction::DriveEffect));
                DependencyOutcome::Pending
            }
            StepStatus::Executing => {
                self.ranked_actions
                    .push((0, step_id, ContinuationAction::DriveEffect));
                DependencyOutcome::Pending
            }
            StepStatus::EffectUnknown => {
                self.ranked_actions
                    .push((1, step_id, ContinuationAction::DriveEffect));
                DependencyOutcome::Pending
            }
            StepStatus::Reconciling => {
                self.ranked_actions
                    .push((2, step_id, ContinuationAction::DriveEffect));
                DependencyOutcome::Pending
            }
            StepStatus::Validating => {
                self.ranked_actions
                    .push((3, step_id, ContinuationAction::Verify));
                DependencyOutcome::Pending
            }
            StepStatus::Completed if receipt_releases_dependency(step) => {
                self.verified_completed_step_ids.push(step_id.to_owned());
                DependencyOutcome::Satisfied
            }
            StepStatus::Completed => {
                self.unverified_completed_step_ids.push(step_id.to_owned());
                DependencyOutcome::Blocked
            }
            StepStatus::Failed => {
                self.failed_step_ids.push(step_id.to_owned());
                DependencyOutcome::Blocked
            }
            StepStatus::ManualRequired => {
                self.manual_required_step_ids.push(step_id.to_owned());
                DependencyOutcome::Blocked
            }
        }
    }

    fn finish(mut self, state: &RunState) -> WorkFrontier {
        self.ranked_actions
            .sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
        let actionable = self
            .ranked_actions
            .into_iter()
            .map(|(_, step_id, action)| FrontierAction {
                step_id: step_id.to_owned(),
                action,
            })
            .collect();
        self.waiting
            .sort_by(|left, right| left.step_id.cmp(&right.step_id));
        self.blocked
            .sort_by(|left, right| left.step_id.cmp(&right.step_id));
        self.verified_completed_step_ids.sort();
        self.unverified_completed_step_ids.sort();
        self.failed_step_ids.sort();
        self.manual_required_step_ids.sort();

        WorkFrontier {
            run_id: state.run_id.clone(),
            revision: state.revision,
            journal_sequence: state.journal_sequence,
            journal_head_digest: state.journal_head_digest.clone(),
            total_steps: state.steps.len(),
            actionable,
            waiting: self.waiting,
            blocked: self.blocked,
            verified_completed_step_ids: self.verified_completed_step_ids,
            unverified_completed_step_ids: self.unverified_completed_step_ids,
            failed_step_ids: self.failed_step_ids,
            manual_required_step_ids: self.manual_required_step_ids,
        }
    }
}

fn validate_frontier_graph<'a>(
    state: &'a RunState,
    metrics: &mut FrontierMetrics,
) -> Result<ValidatedFrontierGraph<'a>, FrontierError> {
    let mut remaining_dependencies = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = state
        .steps
        .keys()
        .map(|step_id| (step_id.as_str(), Vec::new()))
        .collect();
    for (step_id, step) in &state.steps {
        metrics.record_validation_step();
        let mut unique = BTreeSet::new();
        for dependency_id in &step.depends_on {
            metrics.record_validation_dependency();
            if dependency_id == step_id {
                return Err(FrontierError::SelfDependency {
                    step_id: step_id.clone(),
                });
            }
            if !unique.insert(dependency_id.as_str()) {
                return Err(FrontierError::DuplicateDependency {
                    step_id: step_id.clone(),
                    dependency_id: dependency_id.clone(),
                });
            }
            let Some(children) = dependents.get_mut(dependency_id.as_str()) else {
                return Err(FrontierError::UnknownDependency {
                    step_id: step_id.clone(),
                    dependency_id: dependency_id.clone(),
                });
            };
            children.push(step_id);
        }
        remaining_dependencies.insert(step_id.as_str(), step.depends_on.len());
    }
    for children in dependents.values_mut() {
        children.sort_unstable();
    }
    let zero_indegree = remaining_dependencies
        .iter()
        .filter_map(|(step_id, count)| (*count == 0).then_some(*step_id))
        .collect();
    Ok(ValidatedFrontierGraph {
        remaining_dependencies,
        dependents,
        zero_indegree,
    })
}

/// Derive the current actionable, waiting, and transitively blocked `WorkGraph` frontier.
///
/// The calculation is iterative and performs one validation and one classification visit per
/// Step and dependency edge. It never executes tools or changes durable state. This pure function
/// checks projected Receipt identity shape, not a Receipt sidecar or chain; execution-authoritative
/// callers must supply a `RunState` from a store that verified those bindings.
///
/// # Errors
///
/// Returns an error for an unknown, duplicate, self, or cyclic dependency, or when an already
/// active/terminal Step could only have been reached by bypassing an unreleased dependency.
pub fn derive_frontier(state: &RunState) -> Result<WorkFrontier, FrontierError> {
    derive_frontier_with_metrics(state, &mut FrontierMetrics::default())
}

fn derive_frontier_with_metrics(
    state: &RunState,
    metrics: &mut FrontierMetrics,
) -> Result<WorkFrontier, FrontierError> {
    let ValidatedFrontierGraph {
        mut remaining_dependencies,
        dependents,
        mut zero_indegree,
    } = validate_frontier_graph(state, metrics)?;
    let mut assembly = FrontierAssembly::default();
    while let Some(step_id) = zero_indegree.pop_first() {
        assembly.record_step(state, step_id, metrics)?;
        for child in &dependents[step_id] {
            let remaining = remaining_dependencies
                .get_mut(child)
                .expect("dependent was indexed during validation");
            *remaining = remaining
                .checked_sub(1)
                .expect("dependency count cannot underflow");
            if *remaining == 0 {
                zero_indegree.insert(child);
            }
        }
    }
    if assembly.processed != state.steps.len() {
        return Err(FrontierError::DependencyCycle);
    }
    Ok(assembly.finish(state))
}

/// Return whether this Step carries a non-empty terminal Core Receipt ID and a well-formed
/// lowercase SHA-256 Receipt digest required to release a downstream dependency.
///
/// This is a projection-shape check. Receipt document authenticity and journal binding are the
/// responsibility of the verified store that supplied the [`RunState`].
#[must_use]
pub fn receipt_releases_dependency(step: &StepState) -> bool {
    step.status == StepStatus::Completed
        && step
            .execution_receipt_id
            .as_deref()
            .is_some_and(|receipt_id| !receipt_id.is_empty())
        && step
            .execution_receipt_digest
            .as_deref()
            .is_some_and(is_sha256_digest)
}

fn is_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Return the fail-closed reason a Step cannot release a dependency, if any.
#[must_use]
pub fn dependency_release_block_reason(step: &StepState) -> Option<DependencyBlockReason> {
    if receipt_releases_dependency(step) {
        return None;
    }
    Some(match step.status {
        StepStatus::Failed => DependencyBlockReason::Failed,
        StepStatus::ManualRequired => DependencyBlockReason::ManualRequired,
        StepStatus::Completed => DependencyBlockReason::ReceiptMissing,
        _ => DependencyBlockReason::NotCompleted,
    })
}

fn blocker_reason(step: &StepState) -> DependencyBlockReason {
    match step.status {
        StepStatus::Failed => DependencyBlockReason::Failed,
        StepStatus::ManualRequired => DependencyBlockReason::ManualRequired,
        StepStatus::Completed if !receipt_releases_dependency(step) => {
            DependencyBlockReason::ReceiptMissing
        }
        _ => DependencyBlockReason::DependencyBlocked,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationConsumption {
    pub grant_digest: String,
    pub max_uses: u32,
    pub uses: u32,
    pub effect_ids: BTreeSet<String>,
}

/// Apply one committed event without performing I/O or invoking an effect.
///
/// # Errors
///
/// Returns an error when chain metadata, authority, or lifecycle invariants fail.
pub fn apply_record(
    current: Option<&RunState>,
    record: &EventRecord,
) -> Result<RunState, TransitionError> {
    verify_record_against_state(current, record)?;
    let mut state = match (current, &record.event.body) {
        (None, RunEventBody::RunCreated { goal }) => RunState {
            run_id: record.event.run_id.clone(),
            authority: record.event.authority.clone(),
            authority_epoch: record.event.authority_epoch,
            goal: goal.clone(),
            revision: 0,
            journal_sequence: 0,
            journal_head_digest: String::new(),
            steps: BTreeMap::new(),
            authorization_consumption: BTreeMap::new(),
            agent_loop: None,
        },
        (None, _) => return Err(TransitionError::FirstEventMustCreateRun),
        (Some(_), RunEventBody::RunCreated { .. }) => {
            return Err(TransitionError::RunAlreadyCreated);
        }
        (Some(state), _) => state.clone(),
    };

    if current.is_some() {
        apply_body(&mut state, &record.event.body)?;
    }
    state.revision = record.sequence;
    state.journal_sequence = record.sequence;
    state.journal_head_digest.clone_from(&record.digest);
    Ok(state)
}

fn verify_record_against_state(
    current: Option<&RunState>,
    record: &EventRecord,
) -> Result<(), TransitionError> {
    let expected_sequence = current.map_or(Ok(1), |state| {
        state
            .journal_sequence
            .checked_add(1)
            .ok_or(RecordError::SequenceOverflow)
    })?;
    if record.sequence != expected_sequence {
        return Err(TransitionError::UnexpectedSequence {
            expected: expected_sequence,
            actual: record.sequence,
        });
    }
    let expected_previous = current.map(|state| state.journal_head_digest.clone());
    if record.previous_digest != expected_previous {
        return Err(TransitionError::PreviousDigestMismatch {
            sequence: record.sequence,
            expected: expected_previous,
            actual: record.previous_digest.clone(),
        });
    }
    record.verify_digest()?;
    if let Some(state) = current {
        if record.event.run_id != state.run_id {
            return Err(TransitionError::RunIdMismatch);
        }
        if record.event.authority != state.authority {
            return Err(TransitionError::AuthorityMismatch);
        }
        if record.event.authority_epoch != state.authority_epoch {
            return Err(TransitionError::AuthorityEpochMismatch);
        }
    }
    Ok(())
}

fn apply_body(state: &mut RunState, body: &RunEventBody) -> Result<(), TransitionError> {
    match body {
        RunEventBody::RunCreated { .. } => unreachable!("handled by apply_record"),
        RunEventBody::AgentLoopConfigured { budget } => configure_agent_loop(state, budget)?,
        RunEventBody::ModelCallLifecycleConfigured { budget } => {
            configure_model_call_lifecycle(state, budget)?;
        }
        RunEventBody::ModelCallReserved { reservation } => {
            reserve_model_call(state, reservation)?;
        }
        RunEventBody::ModelCallBecameUnknown { call_id, reason } => {
            mark_model_call_unknown(state, call_id, *reason)?;
        }
        RunEventBody::ModelCallSettled {
            call_id,
            settlement,
        } => settle_model_call(state, call_id, *settlement)?,
        RunEventBody::PlanAccepted { decision, steps } => {
            accept_plan(state, decision, steps)?;
        }
        RunEventBody::CompletionCandidateRecorded {
            decision,
            candidate_id,
            summary_digest,
        } => record_completion_candidate(state, decision, candidate_id, summary_digest)?,
        RunEventBody::StepPlanned {
            step_id,
            objective,
            depends_on,
        } => plan_step(state, step_id, objective, depends_on, None)?,
        RunEventBody::EffectIntentCommitted { step_id, intent } => {
            commit_effect_intent(state, step_id, intent)?;
        }
        _ => apply_effect_lifecycle(state, body)?,
    }
    Ok(())
}

fn configure_agent_loop(
    state: &mut RunState,
    budget: &AgentLoopBudget,
) -> Result<(), TransitionError> {
    budget.validate()?;
    if state.agent_loop.is_some() {
        return Err(TransitionError::AgentLoopAlreadyConfigured);
    }
    let planned_steps =
        u32::try_from(state.steps.len()).map_err(|_| TransitionError::PlannedStepBudgetExceeded)?;
    if planned_steps > budget.max_planned_steps {
        return Err(TransitionError::PlannedStepBudgetExceeded);
    }
    let tool_calls = state.steps.values().try_fold(0_u32, |total, step| {
        total
            .checked_add(step.attempts)
            .ok_or(TransitionError::ToolCallBudgetExceeded)
    })?;
    if tool_calls > budget.max_tool_calls {
        return Err(TransitionError::ToolCallBudgetExceeded);
    }
    state.agent_loop = Some(AgentLoopState {
        budget: budget.clone(),
        accepted_model_turns: 0,
        model_calls: None,
        completion_candidate: None,
    });
    Ok(())
}

fn configure_model_call_lifecycle(
    state: &mut RunState,
    budget: &ModelCallBudget,
) -> Result<(), TransitionError> {
    budget.validate()?;
    let loop_state = state
        .agent_loop
        .as_mut()
        .ok_or(TransitionError::AgentLoopNotConfigured)?;
    if loop_state.model_calls.is_some() {
        return Err(TransitionError::ModelCallLifecycleAlreadyConfigured);
    }
    if budget.max_model_calls < loop_state.accepted_model_turns {
        return Err(
            TransitionError::ModelCallBudgetBelowHistoricalAcceptedTurns {
                max_model_calls: budget.max_model_calls,
                accepted_model_turns: loop_state.accepted_model_turns,
            },
        );
    }
    loop_state.model_calls = Some(ModelCallLifecycleState {
        budget: budget.clone(),
        reserved_calls: loop_state.accepted_model_turns,
        settled_calls: loop_state.accepted_model_turns,
        unknown_calls: 0,
        active_call: None,
    });
    validate_model_call_counters(loop_state)
}

fn reserve_model_call(
    state: &mut RunState,
    reservation: &ModelCallReservation,
) -> Result<(), TransitionError> {
    reservation.validate_for(&state.run_id, state.authority_epoch)?;
    let loop_state = state
        .agent_loop
        .as_mut()
        .ok_or(TransitionError::AgentLoopNotConfigured)?;
    if loop_state.completion_candidate.is_some() {
        return Err(TransitionError::CompletionCandidateAlreadyRecorded);
    }
    if loop_state.accepted_model_turns >= loop_state.budget.max_model_turns {
        return Err(TransitionError::ModelTurnBudgetExceeded);
    }
    let calls = loop_state
        .model_calls
        .as_mut()
        .ok_or(TransitionError::ModelCallLifecycleNotConfigured)?;
    if calls.active_call.is_some() {
        return Err(TransitionError::ModelCallActive);
    }
    if calls.reserved_calls >= calls.budget.max_model_calls {
        return Err(TransitionError::ModelCallBudgetExceeded);
    }
    let expected_call_index = calls
        .reserved_calls
        .checked_add(1)
        .ok_or(TransitionError::ModelCallBudgetExceeded)?;
    if reservation.call_index != expected_call_index {
        return Err(TransitionError::UnexpectedModelCallIndex {
            expected: expected_call_index,
            actual: reservation.call_index,
        });
    }
    let expected_turn_index = loop_state
        .accepted_model_turns
        .checked_add(1)
        .ok_or(TransitionError::ModelTurnBudgetExceeded)?;
    if reservation.turn_index != expected_turn_index {
        return Err(TransitionError::UnexpectedModelCallTurn {
            expected: expected_turn_index,
            actual: reservation.turn_index,
        });
    }
    if reservation.base_sequence != state.journal_sequence
        || reservation.base_head_digest != state.journal_head_digest
    {
        return Err(TransitionError::ModelCallBaseHeadMismatch);
    }
    calls.reserved_calls = expected_call_index;
    calls.active_call = Some(ModelCallState {
        reservation: reservation.clone(),
        status: ModelCallStatus::Reserved,
    });
    validate_model_call_counters(loop_state)
}

fn mark_model_call_unknown(
    state: &mut RunState,
    call_id: &str,
    reason: ModelCallUnknownReason,
) -> Result<(), TransitionError> {
    require_model_call_id("call_id", call_id)?;
    let loop_state = state
        .agent_loop
        .as_mut()
        .ok_or(TransitionError::AgentLoopNotConfigured)?;
    let calls = loop_state
        .model_calls
        .as_mut()
        .ok_or(TransitionError::ModelCallLifecycleNotConfigured)?;
    let active = calls
        .active_call
        .as_mut()
        .ok_or(TransitionError::ModelCallNotReserved)?;
    if active.reservation.call_id != call_id {
        return Err(TransitionError::ModelCallIdMismatch);
    }
    if active.status != ModelCallStatus::Reserved {
        return Err(TransitionError::ModelCallNotReserved);
    }
    calls.unknown_calls = calls
        .unknown_calls
        .checked_add(1)
        .ok_or(TransitionError::ModelCallCounterOverflow)?;
    active.status = ModelCallStatus::Unknown { reason };
    validate_model_call_counters(loop_state)
}

fn settle_model_call(
    state: &mut RunState,
    call_id: &str,
    settlement: ModelCallSettlement,
) -> Result<(), TransitionError> {
    require_model_call_id("call_id", call_id)?;
    let loop_state = state
        .agent_loop
        .as_mut()
        .ok_or(TransitionError::AgentLoopNotConfigured)?;
    let calls = loop_state
        .model_calls
        .as_mut()
        .ok_or(TransitionError::ModelCallLifecycleNotConfigured)?;
    let active = calls
        .active_call
        .as_ref()
        .ok_or(TransitionError::ModelCallNotReserved)?;
    if active.reservation.call_id != call_id {
        return Err(TransitionError::ModelCallIdMismatch);
    }
    match settlement {
        ModelCallSettlement::Rejected { reason } => {
            if active.status != ModelCallStatus::Reserved {
                return Err(TransitionError::ModelCallNotReserved);
            }
            let immediate_sequence = active
                .reservation
                .base_sequence
                .checked_add(1)
                .ok_or(TransitionError::ModelCallCounterOverflow)?;
            let is_immediate = state.journal_sequence == immediate_sequence;
            if is_immediate != (reason != ModelCallRejectionReason::StaleHead) {
                return Err(TransitionError::ModelCallSettlementHeadMismatch);
            }
        }
        ModelCallSettlement::Abandoned { .. } => {}
    }
    calls.settled_calls = calls
        .settled_calls
        .checked_add(1)
        .ok_or(TransitionError::ModelCallCounterOverflow)?;
    calls.active_call = None;
    validate_model_call_counters(loop_state)
}

fn validate_model_call_counters(loop_state: &AgentLoopState) -> Result<(), TransitionError> {
    let Some(calls) = &loop_state.model_calls else {
        return Ok(());
    };
    if loop_state.accepted_model_turns > calls.settled_calls
        || calls.settled_calls > calls.reserved_calls
        || calls.unknown_calls > calls.reserved_calls
    {
        return Err(TransitionError::ModelCallCounterInvariant);
    }
    Ok(())
}

fn accept_plan(
    state: &mut RunState,
    decision: &ExpectedPlanningTurn,
    steps: &[AcceptedPlanStep],
) -> Result<(), TransitionError> {
    decision.validate()?;
    validate_model_call_success(state, decision)?;
    let loop_state = state
        .agent_loop
        .as_ref()
        .ok_or(TransitionError::AgentLoopNotConfigured)?;
    if loop_state.completion_candidate.is_some() {
        return Err(TransitionError::CompletionCandidateAlreadyRecorded);
    }
    let expected_turn = loop_state
        .accepted_model_turns
        .checked_add(1)
        .ok_or(TransitionError::ModelTurnBudgetExceeded)?;
    if decision.turn_index != expected_turn {
        return Err(TransitionError::UnexpectedPlanningTurn {
            expected: expected_turn,
            actual: decision.turn_index,
        });
    }
    if expected_turn > loop_state.budget.max_model_turns {
        return Err(TransitionError::ModelTurnBudgetExceeded);
    }
    if steps.is_empty() || steps.len() > MAX_ACCEPTED_PLAN_STEPS {
        return Err(TransitionError::AcceptedPlanStepCountInvalid {
            actual: steps.len(),
        });
    }
    let total_steps = state
        .steps
        .len()
        .checked_add(steps.len())
        .ok_or(TransitionError::PlannedStepBudgetExceeded)?;
    if u32::try_from(total_steps).map_or(true, |total| total > loop_state.budget.max_planned_steps)
    {
        return Err(TransitionError::PlannedStepBudgetExceeded);
    }

    let proposed_ids = validate_accepted_plan_headers(state, decision, steps)?;
    let mut candidate = build_accepted_plan_candidate(state, steps, &proposed_ids)?;
    derive_frontier(&candidate).map_err(map_plan_frontier_error)?;
    let candidate_loop = candidate
        .agent_loop
        .as_mut()
        .expect("candidate retains configured loop");
    candidate_loop.accepted_model_turns = expected_turn;
    settle_successful_model_call(candidate_loop)?;
    *state = candidate;
    Ok(())
}

fn validate_model_call_success(
    state: &RunState,
    decision: &ExpectedPlanningTurn,
) -> Result<(), TransitionError> {
    let loop_state = state
        .agent_loop
        .as_ref()
        .ok_or(TransitionError::AgentLoopNotConfigured)?;
    match (&loop_state.model_calls, decision.model_call_id()) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(TransitionError::ModelCallLifecycleNotConfigured),
        (Some(_), None) => Err(TransitionError::ModelCallDecisionRequired),
        (Some(calls), Some(call_id)) => {
            let active = calls
                .active_call
                .as_ref()
                .ok_or(TransitionError::ModelCallNotReserved)?;
            if active.reservation.call_id != call_id {
                return Err(TransitionError::ModelCallIdMismatch);
            }
            if active.status != ModelCallStatus::Reserved {
                return Err(TransitionError::ModelCallNotReserved);
            }
            if active.reservation.turn_index != decision.turn_index {
                return Err(TransitionError::UnexpectedModelCallTurn {
                    expected: active.reservation.turn_index,
                    actual: decision.turn_index,
                });
            }
            if active.reservation.context_digest != decision.context_digest {
                return Err(TransitionError::ModelCallContextMismatch);
            }
            let immediate_sequence = active
                .reservation
                .base_sequence
                .checked_add(1)
                .ok_or(TransitionError::ModelCallCounterOverflow)?;
            if state.journal_sequence != immediate_sequence {
                return Err(TransitionError::StaleModelCallResponse);
            }
            Ok(())
        }
    }
}

fn settle_successful_model_call(loop_state: &mut AgentLoopState) -> Result<(), TransitionError> {
    let Some(calls) = loop_state.model_calls.as_mut() else {
        return Ok(());
    };
    let active = calls
        .active_call
        .as_ref()
        .ok_or(TransitionError::ModelCallNotReserved)?;
    if active.status != ModelCallStatus::Reserved {
        return Err(TransitionError::ModelCallNotReserved);
    }
    calls.settled_calls = calls
        .settled_calls
        .checked_add(1)
        .ok_or(TransitionError::ModelCallCounterOverflow)?;
    calls.active_call = None;
    validate_model_call_counters(loop_state)
}

fn validate_accepted_plan_headers(
    state: &RunState,
    decision: &ExpectedPlanningTurn,
    steps: &[AcceptedPlanStep],
) -> Result<BTreeSet<String>, TransitionError> {
    let mut proposed_ids = BTreeSet::new();
    let mut action_owners = existing_action_owners(state)?;
    for step in steps {
        require_planning_identifier("step_id", &step.step_id)?;
        validate_accepted_objective(&step.objective)?;
        step.invocation.validate()?;
        if let Some(existing_step_id) = action_owners
            .insert(
                step.invocation.action_digest().to_owned(),
                step.step_id.clone(),
            )
            .filter(|existing_step_id| existing_step_id != &step.step_id)
        {
            return Err(TransitionError::DuplicatePlannedAction {
                step_id: step.step_id.clone(),
                existing_step_id,
            });
        }
        if step.invocation.proposal_digest != decision.proposal_digest {
            return Err(TransitionError::PlanProposalDigestMismatch {
                step_id: step.step_id.clone(),
            });
        }
        if state.steps.contains_key(&step.step_id) || !proposed_ids.insert(step.step_id.clone()) {
            return Err(TransitionError::DuplicateStep(step.step_id.clone()));
        }
    }
    Ok(proposed_ids)
}

fn existing_action_owners(state: &RunState) -> Result<BTreeMap<String, String>, TransitionError> {
    let mut owners = BTreeMap::new();
    for step in state.steps.values() {
        for action_digest in step
            .planned_invocation
            .as_ref()
            .map(PlannedInvocationBinding::action_digest)
            .into_iter()
            .chain(
                step.intent
                    .as_ref()
                    .map(|intent| intent.action_digest.as_str()),
            )
        {
            if let Some(existing_step_id) = owners
                .insert(action_digest.to_owned(), step.step_id.clone())
                .filter(|existing_step_id| existing_step_id != &step.step_id)
            {
                return Err(TransitionError::DuplicatePlannedAction {
                    step_id: step.step_id.clone(),
                    existing_step_id,
                });
            }
        }
    }
    Ok(owners)
}

fn build_accepted_plan_candidate(
    state: &RunState,
    steps: &[AcceptedPlanStep],
    proposed_ids: &BTreeSet<String>,
) -> Result<RunState, TransitionError> {
    let blocked_existing: BTreeSet<_> = derive_frontier(state)
        .map_err(map_plan_frontier_error)?
        .blocked
        .into_iter()
        .map(|blocked| blocked.step_id)
        .collect();
    let mut candidate = state.clone();
    let mut edge_count = 0_usize;
    for step in steps {
        let mut unique = BTreeSet::new();
        for dependency_id in &step.depends_on {
            edge_count = edge_count
                .checked_add(1)
                .ok_or(TransitionError::AcceptedPlanEdgeBudgetExceeded)?;
            if edge_count > MAX_ACCEPTED_PLAN_EDGES {
                return Err(TransitionError::AcceptedPlanEdgeBudgetExceeded);
            }
            if dependency_id == &step.step_id {
                return Err(TransitionError::SelfDependency {
                    step_id: step.step_id.clone(),
                });
            }
            if !unique.insert(dependency_id.as_str()) {
                return Err(TransitionError::DuplicateDependency {
                    step_id: step.step_id.clone(),
                    dependency_id: dependency_id.clone(),
                });
            }
            if !state.steps.contains_key(dependency_id) && !proposed_ids.contains(dependency_id) {
                return Err(TransitionError::UnknownDependency {
                    step_id: step.step_id.clone(),
                    dependency_id: dependency_id.clone(),
                });
            }
            if let Some(existing) = state.steps.get(dependency_id) {
                let reason = if blocked_existing.contains(dependency_id) {
                    Some(DependencyBlockReason::DependencyBlocked)
                } else {
                    dependency_release_block_reason(existing)
                };
                if matches!(
                    reason,
                    Some(
                        DependencyBlockReason::Failed
                            | DependencyBlockReason::ManualRequired
                            | DependencyBlockReason::ReceiptMissing
                            | DependencyBlockReason::DependencyBlocked
                    )
                ) {
                    return Err(TransitionError::PlanDependencyBlocked {
                        step_id: step.step_id.clone(),
                        dependency_id: dependency_id.clone(),
                        reason: reason.expect("matched reason must exist"),
                    });
                }
            }
        }
        candidate.steps.insert(
            step.step_id.clone(),
            StepState {
                step_id: step.step_id.clone(),
                objective: step.objective.clone(),
                depends_on: step.depends_on.clone(),
                planned_invocation: Some(step.invocation.clone()),
                status: StepStatus::Planned,
                attempts: 0,
                intent: None,
                effect_evidence_digest: None,
                execution_receipt_id: None,
                execution_receipt_digest: None,
                uncertainty_reason: None,
                reconciliation_evidence_digest: None,
            },
        );
    }
    Ok(candidate)
}

fn map_plan_frontier_error(error: FrontierError) -> TransitionError {
    match error {
        FrontierError::SelfDependency { step_id } => TransitionError::SelfDependency { step_id },
        FrontierError::DuplicateDependency {
            step_id,
            dependency_id,
        } => TransitionError::DuplicateDependency {
            step_id,
            dependency_id,
        },
        FrontierError::UnknownDependency {
            step_id,
            dependency_id,
        } => TransitionError::UnknownDependency {
            step_id,
            dependency_id,
        },
        FrontierError::DependencyCycle => TransitionError::DependencyCycle,
        FrontierError::ActiveDependencyNotReleased {
            step_id,
            dependency_id,
            reason,
        } => TransitionError::DependencyNotReleased {
            step_id,
            dependency_id,
            reason,
        },
    }
}

fn validate_accepted_objective(objective: &str) -> Result<(), TransitionError> {
    if objective.is_empty()
        || objective.len() > MAX_ACCEPTED_OBJECTIVE_BYTES
        || objective.chars().any(char::is_control)
    {
        return Err(TransitionError::InvalidAcceptedObjective);
    }
    Ok(())
}

fn record_completion_candidate(
    state: &mut RunState,
    decision: &ExpectedPlanningTurn,
    candidate_id: &str,
    summary_digest: &str,
) -> Result<(), TransitionError> {
    decision.validate()?;
    validate_model_call_success(state, decision)?;
    require_planning_identifier("candidate_id", candidate_id)?;
    require_planning_digest("summary_digest", summary_digest)?;
    let frontier = derive_frontier(state).map_err(map_plan_frontier_error)?;
    if !frontier.all_steps_receipt_completed() {
        return Err(TransitionError::CompletionCandidateRequiresReceiptCompletedPlan);
    }
    let loop_state = state
        .agent_loop
        .as_mut()
        .ok_or(TransitionError::AgentLoopNotConfigured)?;
    if loop_state.completion_candidate.is_some() {
        return Err(TransitionError::CompletionCandidateAlreadyRecorded);
    }
    let expected_turn = loop_state
        .accepted_model_turns
        .checked_add(1)
        .ok_or(TransitionError::ModelTurnBudgetExceeded)?;
    if decision.turn_index != expected_turn {
        return Err(TransitionError::UnexpectedPlanningTurn {
            expected: expected_turn,
            actual: decision.turn_index,
        });
    }
    if expected_turn > loop_state.budget.max_model_turns {
        return Err(TransitionError::ModelTurnBudgetExceeded);
    }
    loop_state.accepted_model_turns = expected_turn;
    loop_state.completion_candidate = Some(CompletionCandidateState {
        candidate_id: candidate_id.to_owned(),
        context_digest: decision.context_digest.clone(),
        proposal_digest: decision.proposal_digest.clone(),
        summary_digest: summary_digest.to_owned(),
    });
    settle_successful_model_call(loop_state)
}

fn plan_step(
    state: &mut RunState,
    step_id: &str,
    objective: &str,
    depends_on: &[String],
    planned_invocation: Option<PlannedInvocationBinding>,
) -> Result<(), TransitionError> {
    if let Some(loop_state) = &state.agent_loop {
        if loop_state.completion_candidate.is_some() {
            return Err(TransitionError::CompletionCandidateAlreadyRecorded);
        }
        let total_steps = state
            .steps
            .len()
            .checked_add(1)
            .ok_or(TransitionError::PlannedStepBudgetExceeded)?;
        if u32::try_from(total_steps)
            .map_or(true, |total| total > loop_state.budget.max_planned_steps)
        {
            return Err(TransitionError::PlannedStepBudgetExceeded);
        }
    }
    if state.steps.contains_key(step_id) {
        return Err(TransitionError::DuplicateStep(step_id.to_owned()));
    }
    let mut unique = BTreeSet::new();
    for dependency_id in depends_on {
        if dependency_id == step_id {
            return Err(TransitionError::SelfDependency {
                step_id: step_id.to_owned(),
            });
        }
        if !unique.insert(dependency_id.as_str()) {
            return Err(TransitionError::DuplicateDependency {
                step_id: step_id.to_owned(),
                dependency_id: dependency_id.clone(),
            });
        }
        if !state.steps.contains_key(dependency_id) {
            return Err(TransitionError::UnknownDependency {
                step_id: step_id.to_owned(),
                dependency_id: dependency_id.clone(),
            });
        }
    }
    state.steps.insert(
        step_id.to_owned(),
        StepState {
            step_id: step_id.to_owned(),
            objective: objective.to_owned(),
            depends_on: depends_on.to_vec(),
            planned_invocation,
            status: StepStatus::Planned,
            attempts: 0,
            intent: None,
            effect_evidence_digest: None,
            execution_receipt_id: None,
            execution_receipt_digest: None,
            uncertainty_reason: None,
            reconciliation_evidence_digest: None,
        },
    );
    Ok(())
}

fn apply_effect_lifecycle(
    state: &mut RunState,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    match body {
        RunEventBody::RunCreated { .. }
        | RunEventBody::AgentLoopConfigured { .. }
        | RunEventBody::ModelCallLifecycleConfigured { .. }
        | RunEventBody::ModelCallReserved { .. }
        | RunEventBody::ModelCallBecameUnknown { .. }
        | RunEventBody::ModelCallSettled { .. }
        | RunEventBody::PlanAccepted { .. }
        | RunEventBody::CompletionCandidateRecorded { .. }
        | RunEventBody::StepPlanned { .. }
        | RunEventBody::EffectIntentCommitted { .. } => {
            unreachable!("handled by apply_body")
        }
        RunEventBody::InvocationMaterialUnavailable {
            step_id,
            effect_id,
            reason,
        } => mark_material_unavailable(state, step_id, effect_id, *reason, body)?,
        RunEventBody::EffectExecutionStarted { step_id, effect_id } => {
            record_execution_started(state, step_id, effect_id, body)?;
        }
        RunEventBody::EffectSucceeded {
            step_id,
            effect_id,
            evidence_digest,
        } => record_effect_observation(
            state,
            step_id,
            effect_id,
            evidence_digest,
            StepStatus::Validating,
            body,
        )?,
        RunEventBody::EffectFailed {
            step_id,
            effect_id,
            evidence_digest,
        } => record_effect_observation(
            state,
            step_id,
            effect_id,
            evidence_digest,
            StepStatus::Failed,
            body,
        )?,
        RunEventBody::EffectBecameUnknown {
            step_id,
            effect_id,
            reason,
        } => record_effect_unknown(state, step_id, effect_id, reason, body)?,
        RunEventBody::ReconciliationStarted { step_id, effect_id } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            require_status(step, StepStatus::EffectUnknown, body)?;
            step.status = StepStatus::Reconciling;
        }
        RunEventBody::ReconciliationResolved {
            step_id,
            effect_id,
            resolution,
            evidence_digest,
        } => record_reconciliation(
            state,
            step_id,
            effect_id,
            *resolution,
            evidence_digest,
            body,
        )?,
        RunEventBody::ManualInterventionRequired {
            step_id,
            effect_id,
            reason,
        } => record_manual_required(state, step_id, effect_id, reason, body)?,
        RunEventBody::VerificationPassed { step_id } => {
            let step = step_mut(state, step_id)?;
            require_status(step, StepStatus::Validating, body)?;
            step.status = StepStatus::Completed;
        }
        RunEventBody::VerificationFailed { step_id, .. } => {
            let step = step_mut(state, step_id)?;
            require_status(step, StepStatus::Validating, body)?;
            step.status = StepStatus::Failed;
        }
        RunEventBody::VerificationRecorded {
            step_id,
            effect_id,
            disposition,
            receipt_id,
            receipt_digest,
        } => record_verification(
            state,
            step_id,
            effect_id,
            *disposition,
            receipt_id,
            receipt_digest,
            body,
        )?,
    }
    Ok(())
}

fn record_execution_started(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    if let Some(loop_state) = &state.agent_loop {
        let starts = state.steps.values().try_fold(0_u32, |total, step| {
            total
                .checked_add(step.attempts)
                .ok_or(TransitionError::ToolCallBudgetExceeded)
        })?;
        if starts >= loop_state.budget.max_tool_calls {
            return Err(TransitionError::ToolCallBudgetExceeded);
        }
    }
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::IntentCommitted, body)?;
    step.status = StepStatus::Executing;
    step.attempts =
        step.attempts
            .checked_add(1)
            .ok_or_else(|| TransitionError::AttemptOverflow {
                step_id: step_id.to_owned(),
            })?;
    Ok(())
}

fn record_effect_unknown(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    reason: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::Executing, body)?;
    step.status = StepStatus::EffectUnknown;
    step.uncertainty_reason = Some(reason.to_owned());
    Ok(())
}

fn record_reconciliation(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    resolution: ReconciliationResolution,
    evidence_digest: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::Reconciling, body)?;
    step.reconciliation_evidence_digest = Some(evidence_digest.to_owned());
    step.uncertainty_reason = None;
    step.status = match resolution {
        ReconciliationResolution::ProvedApplied => StepStatus::Validating,
        ReconciliationResolution::ProvedNotApplied => StepStatus::IntentCommitted,
        ReconciliationResolution::Failed => StepStatus::Failed,
    };
    Ok(())
}

fn record_manual_required(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    reason: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    if !matches!(
        step.status,
        StepStatus::EffectUnknown | StepStatus::Reconciling
    ) {
        return invalid_transition(step, body);
    }
    step.status = StepStatus::ManualRequired;
    step.uncertainty_reason = Some(reason.to_owned());
    Ok(())
}

fn record_effect_observation(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    evidence_digest: &str,
    next_status: StepStatus,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::Executing, body)?;
    step.status = next_status;
    step.effect_evidence_digest = Some(evidence_digest.to_owned());
    Ok(())
}

fn record_verification(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    disposition: VerificationDisposition,
    receipt_id: &str,
    receipt_digest: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::Validating, body)?;
    step.execution_receipt_id = Some(receipt_id.to_owned());
    step.execution_receipt_digest = Some(receipt_digest.to_owned());
    step.status = match disposition {
        VerificationDisposition::Passed => {
            step.uncertainty_reason = None;
            StepStatus::Completed
        }
        VerificationDisposition::Failed => {
            step.uncertainty_reason = None;
            StepStatus::Failed
        }
        VerificationDisposition::Inconclusive => {
            step.uncertainty_reason = Some("verification_inconclusive".to_owned());
            StepStatus::ManualRequired
        }
    };
    Ok(())
}

fn mark_material_unavailable(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    reason: InvocationMaterialUnavailableReason,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::IntentCommitted, body)?;
    step.status = StepStatus::ManualRequired;
    step.uncertainty_reason = Some(reason.code().to_owned());
    Ok(())
}

fn commit_effect_intent(
    state: &mut RunState,
    step_id: &str,
    intent: &EffectIntent,
) -> Result<(), TransitionError> {
    match (
        &intent.receipt_provenance,
        &intent.authorization.binding.receipt_provenance_digest,
    ) {
        (Some(provenance), Some(expected)) => {
            let actual = receipt_provenance_digest(provenance)?;
            if &actual != expected {
                return Err(TransitionError::ReceiptProvenanceDigestMismatch {
                    effect_id: intent.effect_id.clone(),
                });
            }
        }
        (None, None) => {}
        _ => {
            return Err(TransitionError::ReceiptProvenanceBindingMismatch {
                effect_id: intent.effect_id.clone(),
            });
        }
    }
    if intent.authorization.max_uses != 1 {
        return Err(TransitionError::InvalidAuthorizationBudget {
            grant_id: intent.authorization.grant_id.clone(),
        });
    }
    if intent.sink_guarantee != SinkGuarantee::None
        && intent.idempotency_key.as_ref().is_none_or(String::is_empty)
    {
        return Err(TransitionError::SinkGuaranteeRequiresIdempotencyKey {
            effect_id: intent.effect_id.clone(),
        });
    }
    if state.steps.values().any(|step| {
        step.intent
            .as_ref()
            .is_some_and(|existing| existing.effect_id == intent.effect_id)
    }) {
        return Err(TransitionError::DuplicateEffect(intent.effect_id.clone()));
    }

    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| TransitionError::UnknownStep(step_id.to_owned()))?;
    require_status(
        step,
        StepStatus::Planned,
        &RunEventBody::EffectIntentCommitted {
            step_id: step_id.to_owned(),
            intent: Box::new(intent.clone()),
        },
    )?;
    if let Some(blocker) = first_unreleased_dependency(state, step)? {
        return Err(TransitionError::DependencyNotReleased {
            step_id: step_id.to_owned(),
            dependency_id: blocker.step_id,
            reason: blocker.reason,
        });
    }

    validate_planned_invocation_binding(step, intent)?;
    validate_authorization_binding(state, step_id, intent)?;

    let consumption = state
        .authorization_consumption
        .entry(intent.authorization.grant_id.clone())
        .or_insert_with(|| AuthorizationConsumption {
            grant_digest: intent.authorization.grant_digest.clone(),
            max_uses: intent.authorization.max_uses,
            uses: 0,
            effect_ids: BTreeSet::new(),
        });
    if consumption.grant_digest != intent.authorization.grant_digest
        || consumption.max_uses != intent.authorization.max_uses
    {
        return Err(TransitionError::AuthorizationGrantChanged {
            grant_id: intent.authorization.grant_id.clone(),
        });
    }
    if consumption.uses >= consumption.max_uses {
        return Err(TransitionError::AuthorizationBudgetExceeded {
            grant_id: intent.authorization.grant_id.clone(),
            max_uses: consumption.max_uses,
        });
    }
    consumption.uses += 1;
    consumption.effect_ids.insert(intent.effect_id.clone());

    let step = state
        .steps
        .get_mut(step_id)
        .expect("step was checked before authorization mutation");
    step.intent = Some(intent.clone());
    step.status = StepStatus::IntentCommitted;
    Ok(())
}

fn validate_planned_invocation_binding(
    step: &StepState,
    intent: &EffectIntent,
) -> Result<(), TransitionError> {
    let Some(planned) = &step.planned_invocation else {
        return Ok(());
    };
    planned.validate()?;
    for (field, matches) in [
        (
            "capability_id",
            planned.capability_id == intent.invocation.capability_id,
        ),
        (
            "contract_version",
            planned.contract_version == intent.invocation.contract_version,
        ),
        (
            "definition_digest",
            planned.definition_digest == intent.invocation.definition_digest,
        ),
        (
            "action_digest",
            planned.action_digest == intent.action_digest,
        ),
        (
            "plan_input_digest",
            planned.plan_input_digest == intent.authorization.binding.material_digest,
        ),
    ] {
        if !matches {
            return Err(TransitionError::PlannedInvocationMismatch {
                step_id: step.step_id.clone(),
                field,
            });
        }
    }
    let Some(provenance) = intent.receipt_provenance.as_ref() else {
        return Err(TransitionError::PlannedInvocationMismatch {
            step_id: step.step_id.clone(),
            field: "receipt_provenance.plan_id",
        });
    };
    if provenance.plan_id != planned.plan_id {
        return Err(TransitionError::PlannedInvocationMismatch {
            step_id: step.step_id.clone(),
            field: "receipt_provenance.plan_id",
        });
    }
    match planned.execution_profile {
        PlannedExecutionProfile::LocalSyncOnceV1 => {
            if intent.idempotency_key.as_ref().is_none_or(String::is_empty) {
                return Err(TransitionError::PlannedInvocationMismatch {
                    step_id: step.step_id.clone(),
                    field: "idempotency_key",
                });
            }
            if provenance.executor_placement != ReceiptPlacement::Local {
                return Err(TransitionError::PlannedInvocationMismatch {
                    step_id: step.step_id.clone(),
                    field: "receipt_provenance.executor_placement",
                });
            }
            let expected_platform = format!("{}-{}", planned.target_os, planned.target_arch);
            if provenance.executor_platform != expected_platform {
                return Err(TransitionError::PlannedInvocationMismatch {
                    step_id: step.step_id.clone(),
                    field: "receipt_provenance.executor_platform",
                });
            }
        }
    }
    Ok(())
}

fn first_unreleased_dependency(
    state: &RunState,
    step: &StepState,
) -> Result<Option<DependencyBlocker>, TransitionError> {
    let mut first = None;
    for dependency_id in &step.depends_on {
        let dependency =
            state
                .steps
                .get(dependency_id)
                .ok_or_else(|| TransitionError::UnknownDependency {
                    step_id: step.step_id.clone(),
                    dependency_id: dependency_id.clone(),
                })?;
        if let Some(reason) = dependency_release_block_reason(dependency) {
            let candidate = DependencyBlocker {
                step_id: dependency_id.clone(),
                reason,
            };
            if first.as_ref().is_none_or(|current| candidate < *current) {
                first = Some(candidate);
            }
        }
    }
    Ok(first)
}

fn validate_authorization_binding(
    state: &RunState,
    step_id: &str,
    intent: &EffectIntent,
) -> Result<(), TransitionError> {
    let authorization = &intent.authorization;
    let binding = &authorization.binding;
    let invocation = &intent.invocation;

    if binding.run_id != state.run_id {
        return Err(TransitionError::AuthorizationRunMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.step_id != step_id {
        return Err(TransitionError::AuthorizationStepMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.authority != state.authority {
        return Err(TransitionError::AuthorizationAuthorityMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.authority_epoch != state.authority_epoch {
        return Err(TransitionError::AuthorizationEpochMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.issued_at_sequence != state.journal_sequence
        || binding.issued_at_head_digest != state.journal_head_digest
    {
        return Err(TransitionError::AuthorizationHeadMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.action_digest != intent.action_digest {
        return Err(TransitionError::AuthorizationActionMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.capability_id != invocation.capability_id
        || binding.contract_version != invocation.contract_version
        || binding.definition_digest != invocation.definition_digest
        || binding.instance_id != invocation.instance_id
        || binding.instance_binding_digest != invocation.instance_binding_digest
    {
        return Err(TransitionError::AuthorizationInvocationMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    let expected_id = once_authorization_id(&binding.run_id, &binding.action_digest)?;
    if authorization.grant_id != expected_id {
        return Err(TransitionError::AuthorizationIdMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    let expected = authorization_digest(binding, authorization.max_uses)?;
    if authorization.grant_digest != expected {
        return Err(TransitionError::AuthorizationDigestMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    Ok(())
}

fn step_mut<'a>(
    state: &'a mut RunState,
    step_id: &str,
) -> Result<&'a mut StepState, TransitionError> {
    state
        .steps
        .get_mut(step_id)
        .ok_or_else(|| TransitionError::UnknownStep(step_id.to_owned()))
}

fn matching_step_mut<'a>(
    state: &'a mut RunState,
    step_id: &str,
    effect_id: &str,
) -> Result<&'a mut StepState, TransitionError> {
    let step = step_mut(state, step_id)?;
    let actual = step.intent.as_ref().map(|intent| intent.effect_id.as_str());
    if actual != Some(effect_id) {
        return Err(TransitionError::EffectMismatch {
            step_id: step_id.to_owned(),
            expected: actual.map(str::to_owned),
            actual: effect_id.to_owned(),
        });
    }
    Ok(step)
}

fn require_status(
    step: &StepState,
    required: StepStatus,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    if step.status != required {
        return invalid_transition(step, body);
    }
    Ok(())
}

fn invalid_transition<T>(step: &StepState, body: &RunEventBody) -> Result<T, TransitionError> {
    Err(TransitionError::InvalidStepTransition {
        step_id: step.step_id.clone(),
        from: step.status,
        event: body.kind(),
    })
}

/// Rebuild a projection from committed records only.
///
/// This function intentionally has no executor or tool port, so replay cannot emit effects.
///
/// # Errors
///
/// Returns an error when the chain or any state transition is invalid.
pub fn replay(records: &[EventRecord]) -> Result<RunState, ReplayError> {
    let mut state = None;
    for record in records {
        state = Some(apply_record(state.as_ref(), record).map_err(replay_transition_error)?);
    }
    state.ok_or(ReplayError::EmptyJournal)
}

fn replay_transition_error(error: TransitionError) -> ReplayError {
    match error {
        TransitionError::UnexpectedSequence { expected, actual } => {
            ReplayError::UnexpectedSequence { expected, actual }
        }
        TransitionError::PreviousDigestMismatch {
            sequence,
            expected,
            actual,
        } => ReplayError::PreviousDigestMismatch {
            sequence,
            expected,
            actual,
        },
        TransitionError::Record(error) => ReplayError::Record(error),
        error => ReplayError::Transition(error),
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RecordError {
    #[error("event canonicalization failed: {0}")]
    Canonicalization(String),
    #[error("event record {sequence} digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        sequence: u64,
        expected: String,
        actual: String,
    },
    #[error("event sequence overflowed u64")]
    SequenceOverflow,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthorizationDigestError {
    #[error("authorization canonicalization failed: {0}")]
    Canonicalization(String),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransitionError {
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error("first event must create the run")]
    FirstEventMustCreateRun,
    #[error("run is already created")]
    RunAlreadyCreated,
    #[error(transparent)]
    PlanningContract(#[from] PlanningContractError),
    #[error("agent loop is already configured")]
    AgentLoopAlreadyConfigured,
    #[error("agent loop is not configured")]
    AgentLoopNotConfigured,
    #[error("model-call lifecycle is already configured")]
    ModelCallLifecycleAlreadyConfigured,
    #[error("model-call lifecycle is not configured")]
    ModelCallLifecycleNotConfigured,
    #[error(
        "model-call budget {max_model_calls} is below {accepted_model_turns} historical accepted turns"
    )]
    ModelCallBudgetBelowHistoricalAcceptedTurns {
        max_model_calls: u32,
        accepted_model_turns: u32,
    },
    #[error("model-call reservation budget is exhausted")]
    ModelCallBudgetExceeded,
    #[error("a model call is already reserved or unresolved")]
    ModelCallActive,
    #[error("model-call index mismatch: expected {expected}, got {actual}")]
    UnexpectedModelCallIndex { expected: u32, actual: u32 },
    #[error("model-call turn mismatch: expected {expected}, got {actual}")]
    UnexpectedModelCallTurn { expected: u32, actual: u32 },
    #[error("model-call reservation is bound to another Run head")]
    ModelCallBaseHeadMismatch,
    #[error("model-call ID differs from the active reservation")]
    ModelCallIdMismatch,
    #[error("model-call context differs from the active reservation")]
    ModelCallContextMismatch,
    #[error("a lifecycle-enabled planning decision requires an active model-call ID")]
    ModelCallDecisionRequired,
    #[error("the model call is absent or no longer reserved")]
    ModelCallNotReserved,
    #[error("a successful model response was produced against a stale Run head")]
    StaleModelCallResponse,
    #[error("model-call settlement reason does not match reservation head freshness")]
    ModelCallSettlementHeadMismatch,
    #[error("model-call counter overflowed")]
    ModelCallCounterOverflow,
    #[error("model-call projection counters violate their durable ordering")]
    ModelCallCounterInvariant,
    #[error("planning turn mismatch: expected {expected}, got {actual}")]
    UnexpectedPlanningTurn { expected: u32, actual: u32 },
    #[error("accepted model-turn budget is exhausted")]
    ModelTurnBudgetExceeded,
    #[error("planned Step budget is exhausted")]
    PlannedStepBudgetExceeded,
    #[error("external tool-call budget is exhausted")]
    ToolCallBudgetExceeded,
    #[error("accepted plan must contain 1..=32 Steps, got {actual}")]
    AcceptedPlanStepCountInvalid { actual: usize },
    #[error("accepted plan exceeds the 128-edge limit")]
    AcceptedPlanEdgeBudgetExceeded,
    #[error("accepted plan objective is empty, oversized, or contains control characters")]
    InvalidAcceptedObjective,
    #[error("accepted Step `{step_id}` has a proposal digest that differs from its turn")]
    PlanProposalDigestMismatch { step_id: String },
    #[error(
        "accepted Step `{step_id}` depends on terminally blocked Step `{dependency_id}` ({reason:?})"
    )]
    PlanDependencyBlocked {
        step_id: String,
        dependency_id: String,
        reason: DependencyBlockReason,
    },
    #[error("accepted plan contains a dependency cycle")]
    DependencyCycle,
    #[error("a completion candidate is already recorded")]
    CompletionCandidateAlreadyRecorded,
    #[error("a completion candidate requires a non-empty Receipt-completed current plan")]
    CompletionCandidateRequiresReceiptCompletedPlan,
    #[error("event sequence mismatch: expected {expected}, got {actual}")]
    UnexpectedSequence { expected: u64, actual: u64 },
    #[error("event {sequence} previous digest mismatch")]
    PreviousDigestMismatch {
        sequence: u64,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error("event run id differs from the run projection")]
    RunIdMismatch,
    #[error("event authority differs from the run projection")]
    AuthorityMismatch,
    #[error("event authority epoch differs from the run projection")]
    AuthorityEpochMismatch,
    #[error("step `{0}` already exists")]
    DuplicateStep(String),
    #[error("step `{step_id}` depends on itself")]
    SelfDependency { step_id: String },
    #[error("step `{step_id}` repeats dependency `{dependency_id}`")]
    DuplicateDependency {
        step_id: String,
        dependency_id: String,
    },
    #[error("step `{step_id}` refers to unknown dependency `{dependency_id}`")]
    UnknownDependency {
        step_id: String,
        dependency_id: String,
    },
    #[error("unknown step `{0}`")]
    UnknownStep(String),
    #[error("step `{step_id}` dependency `{dependency_id}` is not released ({reason:?})")]
    DependencyNotReleased {
        step_id: String,
        dependency_id: String,
        reason: DependencyBlockReason,
    },
    #[error("effect `{0}` already has a committed intent")]
    DuplicateEffect(String),
    #[error("planned invocation for Step `{step_id}` differs at `{field}`")]
    PlannedInvocationMismatch {
        step_id: String,
        field: &'static str,
    },
    #[error(
        "planned Step `{step_id}` repeats the semantic action owned by Step `{existing_step_id}`"
    )]
    DuplicatePlannedAction {
        step_id: String,
        existing_step_id: String,
    },
    #[error("step `{step_id}` expected effect {expected:?}, got `{actual}`")]
    EffectMismatch {
        step_id: String,
        expected: Option<String>,
        actual: String,
    },
    #[error("step `{step_id}` cannot apply `{event}` from {from:?}")]
    InvalidStepTransition {
        step_id: String,
        from: StepStatus,
        event: &'static str,
    },
    #[error("authorization `{grant_id}` must allow exactly one use")]
    InvalidAuthorizationBudget { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another Run")]
    AuthorizationRunMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another Step")]
    AuthorizationStepMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another authority")]
    AuthorizationAuthorityMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another authority epoch")]
    AuthorizationEpochMismatch { grant_id: String },
    #[error("authorization `{grant_id}` was issued against another journal head")]
    AuthorizationHeadMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another semantic action")]
    AuthorizationActionMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another executable invocation")]
    AuthorizationInvocationMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is not the stable ID for its Run/action budget")]
    AuthorizationIdMismatch { grant_id: String },
    #[error("authorization `{grant_id}` digest does not cover its durable binding")]
    AuthorizationDigestMismatch { grant_id: String },
    #[error(transparent)]
    AuthorizationDigest(#[from] AuthorizationDigestError),
    #[error("authorization `{grant_id}` changed after first consumption")]
    AuthorizationGrantChanged { grant_id: String },
    #[error("authorization `{grant_id}` exceeded its {max_uses}-use budget")]
    AuthorizationBudgetExceeded { grant_id: String, max_uses: u32 },
    #[error("effect `{effect_id}` claims a keyed sink guarantee without an idempotency key")]
    SinkGuaranteeRequiresIdempotencyKey { effect_id: String },
    #[error("effect `{effect_id}` receipt provenance and authorization binding differ")]
    ReceiptProvenanceBindingMismatch { effect_id: String },
    #[error("effect `{effect_id}` receipt provenance digest is invalid")]
    ReceiptProvenanceDigestMismatch { effect_id: String },
    #[error("step `{step_id}` attempt counter overflowed")]
    AttemptOverflow { step_id: String },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReplayError {
    #[error("journal has no run-created event")]
    EmptyJournal,
    #[error("event sequence mismatch: expected {expected}, got {actual}")]
    UnexpectedSequence { expected: u64, actual: u64 },
    #[error("event {sequence} previous digest mismatch")]
    PreviousDigestMismatch {
        sequence: u64,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const RUN_ID: &str = "run-1";
    const AUTHORITY: &str = "local:test";
    const AUTHORITY_EPOCH: u64 = 7;

    type IntentMutation = Box<dyn Fn(&mut EffectIntent)>;

    fn event(event_id: &str, body: RunEventBody) -> RunEvent {
        RunEvent {
            event_id: event_id.to_owned(),
            run_id: RUN_ID.to_owned(),
            authority: AUTHORITY.to_owned(),
            authority_epoch: AUTHORITY_EPOCH,
            recorded_at: "2026-08-28T00:00:00Z".to_owned(),
            body,
        }
    }

    fn append(
        records: &mut Vec<EventRecord>,
        state: Option<&RunState>,
        event: RunEvent,
    ) -> RunState {
        let previous = records.last();
        let record = EventRecord::next(previous, event).expect("record should be canonicalizable");
        let state = apply_record(state, &record).expect("transition should be valid");
        records.push(record);
        state
    }

    fn intent(state: &RunState, step_id: &str, effect_id: &str, max_uses: u32) -> RunEventBody {
        let action_digest = format!("sha256:action-{effect_id}");
        let material_digest = invocation_material_digest(&serde_json::json!({
            "effectId": effect_id
        }))
        .expect("material should canonicalize");
        let material_retention_digest =
            invocation_material_retention_digest(&InvocationMaterialRetention::Ephemeral)
                .expect("retention should canonicalize");
        let invocation = InvocationBinding {
            capability_id: "test.effect".to_owned(),
            contract_version: "1.0.0".to_owned(),
            definition_digest: "sha256:definition-1".to_owned(),
            instance_id: "test.instance".to_owned(),
            instance_binding_digest: "sha256:instance-1".to_owned(),
        };
        let binding = AuthorizationBinding {
            run_id: state.run_id.clone(),
            step_id: step_id.to_owned(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            issued_at_sequence: state.journal_sequence,
            issued_at_head_digest: state.journal_head_digest.clone(),
            capability_id: invocation.capability_id.clone(),
            contract_version: invocation.contract_version.clone(),
            definition_digest: invocation.definition_digest.clone(),
            instance_id: invocation.instance_id.clone(),
            instance_binding_digest: invocation.instance_binding_digest.clone(),
            action_digest: action_digest.clone(),
            material_digest,
            material_retention_digest,
            policy_evidence_digest: "sha256:policy-1".to_owned(),
            receipt_provenance_digest: None,
        };
        let grant_digest =
            authorization_digest(&binding, max_uses).expect("authorization should canonicalize");
        let grant_id = once_authorization_id(&binding.run_id, &binding.action_digest)
            .expect("authorization ID should canonicalize");
        RunEventBody::EffectIntentCommitted {
            step_id: step_id.to_owned(),
            intent: Box::new(EffectIntent {
                effect_id: effect_id.to_owned(),
                action_digest,
                invocation,
                effect_class: EffectClass::NonIdempotent,
                idempotency_key: None,
                sink_guarantee: SinkGuarantee::None,
                authorization: AuthorizationUse {
                    grant_id,
                    grant_digest,
                    max_uses,
                    binding,
                },
                receipt_provenance: None,
            }),
        }
    }

    fn planned_state(records: &mut Vec<EventRecord>) -> RunState {
        let created = append(
            records,
            None,
            event(
                "material-event-1",
                RunEventBody::RunCreated {
                    goal: "recover one invocation".to_owned(),
                },
            ),
        );
        append(
            records,
            Some(&created),
            event(
                "material-event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-material".to_owned(),
                    objective: "prepare effect".to_owned(),
                    depends_on: Vec::new(),
                },
            ),
        )
    }

    #[test]
    fn invocation_material_record_detects_cross_effect_and_content_tampering() {
        let mut records = Vec::new();
        let state = planned_state(&mut records);
        let RunEventBody::EffectIntentCommitted { mut intent, .. } =
            intent(&state, "step-material", "effect-material", 1)
        else {
            panic!("helper must create an intent")
        };
        let reference = ReconstructableMaterialReference::new("run-recipe", "recipe-1", "rev-1")
            .expect("reference should validate");
        let retention = InvocationMaterialRetention::ReconstructableReference(reference);
        intent.authorization.binding.material_retention_digest =
            invocation_material_retention_digest(&retention)
                .expect("retention should canonicalize");
        intent.authorization.grant_digest =
            authorization_digest(&intent.authorization.binding, intent.authorization.max_uses)
                .expect("authorization should canonicalize");
        let digest = intent.authorization.binding.material_digest.clone();
        let record =
            InvocationMaterialRecord::new(RUN_ID, "step-material", &intent, digest, retention)
                .expect("record should bind");
        record
            .verify_for(RUN_ID, "step-material", &intent)
            .expect("original record should verify");
        assert!(matches!(
            record.verify_for("run-other", "step-material", &intent),
            Err(InvocationMaterialError::BindingMismatch("run_id"))
        ));
        assert!(matches!(
            record.verify_for(RUN_ID, "step-other", &intent),
            Err(InvocationMaterialError::BindingMismatch("step_id"))
        ));

        let mut swapped_intent = (*intent).clone();
        swapped_intent.effect_id = "effect-other".to_owned();
        assert!(matches!(
            record.verify_for(RUN_ID, "step-material", &swapped_intent),
            Err(InvocationMaterialError::BindingMismatch("effect_id"))
        ));

        let mut tampered_json = serde_json::to_value(&record).expect("record should serialize");
        tampered_json["materialDigest"] = serde_json::json!("sha256:changed");
        let tampered: InvocationMaterialRecord =
            serde_json::from_value(tampered_json).expect("shape should deserialize");
        assert!(matches!(
            tampered.verify_for(RUN_ID, "step-material", &intent),
            Err(InvocationMaterialError::BindingMismatch("material_digest")
                | InvocationMaterialError::MaterialIdMismatch
                | InvocationMaterialError::RecordDigestMismatch)
        ));
        assert!(!format!("{record:?}").contains("recipe-1"));
    }

    #[test]
    fn material_reference_rejects_paths_uris_and_oversized_components() {
        for invalid in ["", ".", "..", "../recipe", "https://recipe", "recipe/value"] {
            assert!(matches!(
                ReconstructableMaterialReference::new("provider", invalid, "rev-1"),
                Err(InvocationMaterialError::InvalidReferenceComponent(
                    "reference_id"
                ))
            ));
        }
        assert!(
            ReconstructableMaterialReference::new(
                "provider",
                "x".repeat(MAX_MATERIAL_REFERENCE_COMPONENT_BYTES + 1),
                "rev-1"
            )
            .is_err()
        );
    }

    #[test]
    fn material_unavailable_moves_only_an_unstarted_intent_to_manual() {
        let mut records = Vec::new();
        let planned = planned_state(&mut records);
        let committed = append(
            &mut records,
            Some(&planned),
            event(
                "material-event-3",
                intent(&planned, "step-material", "effect-material", 1),
            ),
        );
        let manual = append(
            &mut records,
            Some(&committed),
            event(
                "material-event-4",
                RunEventBody::InvocationMaterialUnavailable {
                    step_id: "step-material".to_owned(),
                    effect_id: "effect-material".to_owned(),
                    reason: InvocationMaterialUnavailableReason::EphemeralMaterialLost,
                },
            ),
        );
        assert_eq!(
            manual.steps["step-material"].status,
            StepStatus::ManualRequired
        );
        assert_eq!(
            manual.steps["step-material"].uncertainty_reason.as_deref(),
            Some("ephemeral_material_lost")
        );

        let record = EventRecord::next(
            records.last(),
            event(
                "material-event-5",
                RunEventBody::InvocationMaterialUnavailable {
                    step_id: "step-material".to_owned(),
                    effect_id: "effect-material".to_owned(),
                    reason: InvocationMaterialUnavailableReason::ReferenceUnavailable,
                },
            ),
        )
        .expect("record should build");
        assert!(matches!(
            apply_record(Some(&manual), &record),
            Err(TransitionError::InvalidStepTransition {
                from: StepStatus::ManualRequired,
                ..
            })
        ));
    }

    #[test]
    fn durable_effect_happy_path_is_replayable() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event(
                "event-1",
                RunEventBody::RunCreated {
                    goal: "change one file safely".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".to_owned(),
                    objective: "write file".to_owned(),
                    depends_on: Vec::new(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-4",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".to_owned(),
                    effect_id: "effect-1".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-5",
                RunEventBody::EffectSucceeded {
                    step_id: "step-1".to_owned(),
                    effect_id: "effect-1".to_owned(),
                    evidence_digest: "sha256:receipt-1".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-6",
                RunEventBody::VerificationPassed {
                    step_id: "step-1".to_owned(),
                },
            ),
        );

        assert_eq!(state.steps["step-1"].status, StepStatus::Completed);
        assert_eq!(state.authorization_consumption.len(), 1);
        assert_eq!(
            state
                .authorization_consumption
                .values()
                .next()
                .expect("one authorization should exist")
                .uses,
            1
        );
        assert_eq!(replay(&records).expect("replay should pass"), state);
        assert_eq!(replay(&records).expect("second replay should pass"), state);
    }

    #[test]
    fn unknown_non_idempotent_effect_cannot_be_blindly_retried() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                    depends_on: Vec::new(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        for (event_id, body) in [
            (
                "event-4",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                },
            ),
            (
                "event-5",
                RunEventBody::EffectBecameUnknown {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                    reason: "ack lost".into(),
                },
            ),
        ] {
            state = append(&mut records, Some(&state), event(event_id, body));
        }

        let retry = EventRecord::next(
            records.last(),
            event(
                "event-6",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &retry),
            Err(TransitionError::InvalidStepTransition { .. })
        ));
    }

    #[test]
    fn proved_not_applied_allows_same_intent_to_resume() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                    depends_on: Vec::new(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        let bodies = [
            RunEventBody::EffectExecutionStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
            RunEventBody::EffectBecameUnknown {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
                reason: "timeout".into(),
            },
            RunEventBody::ReconciliationStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
            RunEventBody::ReconciliationResolved {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
                resolution: ReconciliationResolution::ProvedNotApplied,
                evidence_digest: "sha256:evidence-1".into(),
            },
            RunEventBody::EffectExecutionStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
        ];
        for (index, body) in bodies.into_iter().enumerate() {
            state = append(
                &mut records,
                Some(&state),
                event(&format!("event-{}", index + 4), body),
            );
        }

        assert_eq!(state.steps["step-1"].status, StepStatus::Executing);
        assert_eq!(state.steps["step-1"].attempts, 2);
        assert_eq!(state.authorization_consumption.len(), 1);
        assert_eq!(
            state
                .authorization_consumption
                .values()
                .next()
                .expect("one authorization should exist")
                .uses,
            1
        );
    }

    #[test]
    fn one_shot_authorization_cannot_be_rebound_to_another_step() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        for (event_id, step_id) in [("event-2", "step-1"), ("event-3", "step-2")] {
            state = append(
                &mut records,
                Some(&state),
                event(
                    event_id,
                    RunEventBody::StepPlanned {
                        step_id: step_id.into(),
                        objective: step_id.into(),
                        depends_on: Vec::new(),
                    },
                ),
            );
        }
        state = append(
            &mut records,
            Some(&state),
            event("event-4", intent(&state, "step-1", "effect-1", 1)),
        );
        let RunEventBody::EffectIntentCommitted {
            step_id,
            mut intent,
        } = intent(&state, "step-2", "effect-2", 1)
        else {
            unreachable!("intent helper always creates an effect intent")
        };
        intent.action_digest = "sha256:action-effect-1".to_owned();
        intent.authorization.binding.action_digest = intent.action_digest.clone();
        intent.authorization.grant_id = once_authorization_id(
            &intent.authorization.binding.run_id,
            &intent.authorization.binding.action_digest,
        )
        .expect("authorization ID should canonicalize");
        intent.authorization.grant_digest =
            authorization_digest(&intent.authorization.binding, intent.authorization.max_uses)
                .expect("authorization should canonicalize");
        let rebound = EventRecord::next(
            records.last(),
            event(
                "event-5",
                RunEventBody::EffectIntentCommitted { step_id, intent },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &rebound),
            Err(TransitionError::AuthorizationGrantChanged { .. })
        ));
    }

    #[test]
    fn durable_authorization_binding_mismatches_fail_without_mutating_state() {
        let mut records = Vec::new();
        let created = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let state = append(
            &mut records,
            Some(&created),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                    depends_on: Vec::new(),
                },
            ),
        );
        let original = state.clone();

        let mutations: Vec<IntentMutation> = vec![
            Box::new(|intent| intent.authorization.binding.run_id = "other-run".into()),
            Box::new(|intent| intent.authorization.binding.step_id = "other-step".into()),
            Box::new(|intent| intent.authorization.binding.authority = "other:authority".into()),
            Box::new(|intent| intent.authorization.binding.authority_epoch += 1),
            Box::new(|intent| intent.authorization.binding.issued_at_sequence += 1),
            Box::new(|intent| {
                intent.authorization.binding.issued_at_head_digest = "sha256:other-head".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.action_digest = "sha256:other-action".into();
            }),
            Box::new(|intent| intent.invocation.capability_id = "other.capability".into()),
            Box::new(|intent| intent.invocation.contract_version = "2.0.0".into()),
            Box::new(|intent| {
                intent.invocation.definition_digest = "sha256:other-definition".into();
            }),
            Box::new(|intent| intent.invocation.instance_id = "other.instance".into()),
            Box::new(|intent| {
                intent.invocation.instance_binding_digest = "sha256:other-binding".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.policy_evidence_digest = "sha256:other-policy".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.material_digest = "sha256:other-material".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.material_retention_digest =
                    "sha256:other-retention".into();
            }),
            Box::new(|intent| intent.authorization.grant_id = "authorization-forged".into()),
            Box::new(|intent| {
                intent.authorization.max_uses = 2;
                intent.authorization.grant_digest = authorization_digest(
                    &intent.authorization.binding,
                    intent.authorization.max_uses,
                )
                .expect("authorization should canonicalize");
            }),
            Box::new(|intent| intent.authorization.grant_digest = "sha256:forged".into()),
        ];

        for (index, mutate) in mutations.into_iter().enumerate() {
            let RunEventBody::EffectIntentCommitted {
                step_id,
                mut intent,
            } = intent(&state, "step-1", "effect-1", 1)
            else {
                unreachable!("intent helper always creates an effect intent")
            };
            mutate(&mut intent);
            let record = EventRecord::next(
                records.last(),
                event(
                    &format!("invalid-event-{index}"),
                    RunEventBody::EffectIntentCommitted { step_id, intent },
                ),
            )
            .expect("record should build");

            assert!(apply_record(Some(&state), &record).is_err());
            assert_eq!(state, original);
        }
    }

    #[test]
    fn replay_rejects_a_broken_hash_chain() {
        let mut records = Vec::new();
        let state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let _state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                    depends_on: Vec::new(),
                },
            ),
        );
        records[1].previous_digest = Some("sha256:tampered".into());

        assert!(matches!(
            replay(&records),
            Err(ReplayError::PreviousDigestMismatch { sequence: 2, .. })
        ));
    }

    #[test]
    fn keyed_sink_guarantee_requires_an_actual_key() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                    depends_on: Vec::new(),
                },
            ),
        );
        let RunEventBody::EffectIntentCommitted {
            step_id,
            mut intent,
        } = intent(&state, "step-1", "effect-1", 1)
        else {
            unreachable!("intent helper always returns an intent event");
        };
        intent.sink_guarantee = SinkGuarantee::QueryByKey;
        let invalid = EventRecord::next(
            records.last(),
            event(
                "event-3",
                RunEventBody::EffectIntentCommitted { step_id, intent },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &invalid),
            Err(TransitionError::SinkGuaranteeRequiresIdempotencyKey { .. })
        ));
    }

    #[test]
    fn authority_epoch_change_is_fenced() {
        let mut records = Vec::new();
        let current_projection = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let mut stale_event = event(
            "event-2",
            RunEventBody::StepPlanned {
                step_id: "step-1".into(),
                objective: "o".into(),
                depends_on: Vec::new(),
            },
        );
        stale_event.authority_epoch -= 1;
        let stale_record =
            EventRecord::next(records.last(), stale_event).expect("record should build");

        assert_eq!(
            apply_record(Some(&current_projection), &stale_record),
            Err(TransitionError::AuthorityEpochMismatch)
        );
    }

    #[test]
    fn event_sequence_overflow_is_rejected() {
        let previous = EventRecord {
            sequence: u64::MAX,
            previous_digest: None,
            event: event("event-max", RunEventBody::RunCreated { goal: "g".into() }),
            digest: "sha256:max".into(),
        };

        assert_eq!(
            EventRecord::next(
                Some(&previous),
                event(
                    "event-overflow",
                    RunEventBody::StepPlanned {
                        step_id: "step-1".into(),
                        objective: "o".into(),
                        depends_on: Vec::new(),
                    },
                ),
            ),
            Err(RecordError::SequenceOverflow)
        );
    }

    #[test]
    fn legacy_receipt_digest_wire_spelling_is_preserved_for_effect_evidence() {
        let body = RunEventBody::EffectSucceeded {
            step_id: "step-1".to_owned(),
            effect_id: "effect-1".to_owned(),
            evidence_digest: format!("sha256:{}", "a".repeat(64)),
        };
        let value = serde_json::to_value(&body).expect("event body should serialize");
        assert_eq!(
            value.pointer("/receiptDigest"),
            Some(&serde_json::json!(format!("sha256:{}", "a".repeat(64))))
        );
        assert!(value.pointer("/evidenceDigest").is_none());

        let round_trip: RunEventBody =
            serde_json::from_value(value).expect("legacy event should deserialize");
        assert_eq!(round_trip, body);
    }

    #[test]
    fn empty_dependencies_preserve_legacy_event_and_projection_wire_shape() {
        let body = RunEventBody::StepPlanned {
            step_id: "step-legacy".to_owned(),
            objective: "legacy plan".to_owned(),
            depends_on: Vec::new(),
        };
        let body_value = serde_json::to_value(&body).expect("event body should serialize");
        assert!(body_value.pointer("/dependsOn").is_none());
        assert_eq!(
            serde_json::from_value::<RunEventBody>(body_value).expect("legacy body should load"),
            body
        );

        let step = StepState {
            step_id: "step-legacy".to_owned(),
            objective: "legacy plan".to_owned(),
            depends_on: Vec::new(),
            planned_invocation: None,
            status: StepStatus::Planned,
            attempts: 0,
            intent: None,
            effect_evidence_digest: None,
            execution_receipt_id: None,
            execution_receipt_digest: None,
            uncertainty_reason: None,
            reconciliation_evidence_digest: None,
        };
        let step_value = serde_json::to_value(&step).expect("Step projection should serialize");
        assert!(step_value.pointer("/dependsOn").is_none());
        assert_eq!(
            serde_json::from_value::<StepState>(step_value).expect("legacy Step should load"),
            step
        );
    }

    #[test]
    fn frontier_visits_each_step_and_dependency_a_constant_number_of_times() {
        let step_count = 1_000_u32;
        let mut steps = BTreeMap::new();
        for index in 0..step_count {
            let step_id = format!("step-{index:04}");
            let depends_on = if index > 0 {
                vec![format!("step-{:04}", index - 1)]
            } else {
                Vec::new()
            };
            let mut step = StepState {
                step_id: step_id.clone(),
                objective: step_id.clone(),
                depends_on,
                planned_invocation: None,
                status: StepStatus::Completed,
                attempts: 1,
                intent: None,
                effect_evidence_digest: None,
                execution_receipt_id: Some(format!("receipt-{index:04}")),
                execution_receipt_digest: Some(format!("sha256:{}", "a".repeat(64))),
                uncertainty_reason: None,
                reconciliation_evidence_digest: None,
            };
            if index == step_count - 1 {
                step.status = StepStatus::Planned;
                step.execution_receipt_id = None;
                step.execution_receipt_digest = None;
            }
            steps.insert(step_id, step);
        }
        let state = RunState {
            run_id: RUN_ID.to_owned(),
            authority: AUTHORITY.to_owned(),
            authority_epoch: AUTHORITY_EPOCH,
            goal: "measure frontier visits".to_owned(),
            revision: u64::from(step_count),
            journal_sequence: u64::from(step_count),
            journal_head_digest: "sha256:head".to_owned(),
            steps,
            authorization_consumption: BTreeMap::new(),
            agent_loop: None,
        };
        let mut metrics = FrontierMetrics::default();

        let frontier =
            derive_frontier_with_metrics(&state, &mut metrics).expect("linear chain should derive");

        assert_eq!(frontier.actionable.len(), 1);
        assert_eq!(metrics.validation_steps, u64::from(step_count));
        assert_eq!(metrics.validation_dependencies, u64::from(step_count - 1));
        assert_eq!(metrics.classification_steps, u64::from(step_count));
        assert_eq!(
            metrics.classification_dependencies,
            u64::from(step_count - 1)
        );
    }

    fn planned_journal(step_count: u8) -> (Vec<EventRecord>, RunState) {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        for index in 0..step_count {
            state = append(
                &mut records,
                Some(&state),
                event(
                    &format!("event-{}", u16::from(index) + 2),
                    RunEventBody::StepPlanned {
                        step_id: format!("step-{index}"),
                        objective: format!("objective-{index}"),
                        depends_on: Vec::new(),
                    },
                ),
            );
        }
        (records, state)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn replay_matches_incremental_reduction_for_unique_plans(step_count in 0_u8..32) {
            let (records, incremental) = planned_journal(step_count);
            prop_assert_eq!(replay(&records), Ok(incremental));
        }

        #[test]
        fn mutation_of_any_committed_event_is_detected(
            step_count in 0_u8..32,
            selected in any::<usize>(),
        ) {
            let (mut records, _) = planned_journal(step_count);
            let index = selected % records.len();
            records[index].event.event_id.push_str("-tampered");

            let detected = matches!(
                replay(&records),
                Err(ReplayError::Record(RecordError::DigestMismatch { .. }))
            );
            prop_assert!(detected);
        }
    }
}
