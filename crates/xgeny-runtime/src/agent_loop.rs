use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};

use serde::{Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_domain::{CapabilityRef, CriticalAction, EffectClass, ExecutionStyle};
use xgeny_local_store::{ExpectedHead, RunStore, StoreError};
use xgeny_policy::ResourceResolver;
use xgeny_workgraph::{
    AcceptedPlanStep, AgentLoopBudget, CompletionCandidateState, ContinuationAction,
    DependencyBlockReason, ExpectedPlanningTurn, FrontierAction, MAX_ACCEPTED_OBJECTIVE_BYTES,
    MAX_ACCEPTED_PLAN_EDGES, MAX_ACCEPTED_PLAN_STEPS, ModelCallAbandonmentReason, ModelCallBudget,
    ModelCallRejectionReason, ModelCallReservation, ModelCallSettlement, ModelCallStatus,
    ModelCallUnknownReason, PlannedExecutionProfile, PlannedInvocationMaterialRecord,
    PlannedInvocationSpec, PlanningContractError, ReconstructableMaterialReference, RunEvent,
    RunEventBody, RunState, StepStatus, WorkFrontier, dependency_release_block_reason,
    derive_frontier, receipt_releases_dependency,
};

use crate::admission::{definition_contract_digest, prepare_invocation_facts};
use crate::{
    AdmissionError, CapabilityRegistry, EventFactory, EventFactoryError, EventMetadataError,
    RunLease,
};

const PLANNING_CONTEXT_PROFILE_V1: &str = "xgeny.planning-context/v1";
const MAX_PROPOSAL_BYTES: usize = 256 * 1024;
const MAX_PROPOSAL_KEY_BYTES: usize = 128;
const MAX_COMPLETION_SUMMARY_BYTES: usize = 5_000;
const MAX_CAPABILITY_SUMMARY_BYTES: usize = 512;
const MAX_PLANNING_CONTEXT_BYTES: u64 = 512 * 1024;
const MAX_PLANNING_CATALOG_DEFINITIONS: usize = 1_024;
const MAX_PLANNING_SOURCE_STEPS: usize = 4_096;
const MAX_PLANNING_DEFINITION_BYTES: usize = 256 * 1024;
const MAX_PLANNING_STEP_BYTES: usize = 64 * 1024;
const MAX_PLANNING_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PLANNING_VALUE_DEPTH: usize = 64;
const MAX_PLANNING_VALUE_NODES: usize = 32_768;
const MAX_PLANNING_VALUE_TEXT_BYTES: usize = 256 * 1024;
const MAX_PLANNING_DEFINITION_COLLECTION_ITEMS: usize = 4_096;
const MAX_PLANNING_HEADER_TEXT_BYTES: usize = 256 * 1024;

/// One immutable journal position exposed by a bounded loop tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLoopHead {
    pub sequence: u64,
    pub digest: String,
}

impl AgentLoopHead {
    fn from_state(state: &RunState) -> Self {
        Self {
            sequence: state.journal_sequence,
            digest: state.journal_head_digest.clone(),
        }
    }
}

/// Whole Capability contract entry selected for one bounded planning context.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanningCapabilitySummary {
    capability: CapabilityRef,
    definition_digest: String,
    summary: String,
    input_schema: Value,
    output_schema: Value,
    effect_class: EffectClass,
    resource_selectors: Vec<PlanningResourceSelector>,
    critical_actions: Vec<CriticalAction>,
    execution_styles: Vec<ExecutionStyle>,
    idempotency_key_supported: bool,
    default_timeout_ms: u64,
    max_timeout_ms: u64,
}

impl PlanningCapabilitySummary {
    #[must_use]
    pub const fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    #[must_use]
    pub fn definition_digest(&self) -> &str {
        &self.definition_digest
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    #[must_use]
    pub const fn output_schema(&self) -> &Value {
        &self.output_schema
    }

    #[must_use]
    pub const fn effect_class(&self) -> EffectClass {
        self.effect_class
    }

    #[must_use]
    pub fn resource_selectors(&self) -> &[PlanningResourceSelector] {
        &self.resource_selectors
    }

    #[must_use]
    pub fn critical_actions(&self) -> &[CriticalAction] {
        &self.critical_actions
    }

    #[must_use]
    pub fn execution_styles(&self) -> &[ExecutionStyle] {
        &self.execution_styles
    }

    #[must_use]
    pub const fn idempotency_key_supported(&self) -> bool {
        self.idempotency_key_supported
    }

    #[must_use]
    pub const fn default_timeout_ms(&self) -> u64 {
        self.default_timeout_ms
    }

    #[must_use]
    pub const fn max_timeout_ms(&self) -> u64 {
        self.max_timeout_ms
    }
}

impl fmt::Debug for PlanningCapabilitySummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlanningCapabilitySummary")
            .field("capability", &self.capability)
            .field("definition_digest", &self.definition_digest)
            .field("summary", &"<redacted>")
            .field("input_schema", &"<redacted>")
            .field("output_schema", &"<redacted>")
            .field("effect_class", &self.effect_class)
            .field("resource_selector_count", &self.resource_selectors.len())
            .field("critical_actions", &self.critical_actions)
            .field("execution_styles", &self.execution_styles)
            .field("idempotency_key_supported", &self.idempotency_key_supported)
            .field("default_timeout_ms", &self.default_timeout_ms)
            .field("max_timeout_ms", &self.max_timeout_ms)
            .finish()
    }
}

/// Non-secret resource argument mapping needed for provider-neutral tool use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanningResourceSelector {
    scope: String,
    argument_pointer: String,
}

impl PlanningResourceSelector {
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub fn argument_pointer(&self) -> &str {
        &self.argument_pointer
    }
}

/// One whole existing Step selected into the bounded continuation context.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanningStepSummary {
    step_id: String,
    objective: String,
    depends_on: Vec<String>,
    status: StepStatus,
    attempts: u32,
    capability: Option<CapabilityRef>,
    dependency_released: bool,
}

impl PlanningStepSummary {
    #[must_use]
    pub fn step_id(&self) -> &str {
        &self.step_id
    }

    #[must_use]
    pub fn objective(&self) -> &str {
        &self.objective
    }

    #[must_use]
    pub fn depends_on(&self) -> &[String] {
        &self.depends_on
    }

    #[must_use]
    pub const fn status(&self) -> StepStatus {
        self.status
    }

    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    #[must_use]
    pub const fn capability(&self) -> Option<&CapabilityRef> {
        self.capability.as_ref()
    }

    #[must_use]
    pub const fn dependency_released(&self) -> bool {
        self.dependency_released
    }
}

impl fmt::Debug for PlanningStepSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlanningStepSummary")
            .field("step_id", &self.step_id)
            .field("objective", &"<redacted>")
            .field("depends_on", &self.depends_on)
            .field("status", &self.status)
            .field("attempts", &self.attempts)
            .field("capability", &self.capability)
            .field("dependency_released", &self.dependency_released)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanningContextPayload {
    profile_version: &'static str,
    run_id: String,
    authority: String,
    authority_epoch: u64,
    journal_sequence: u64,
    journal_head_digest: String,
    goal: String,
    total_steps: usize,
    verified_completed_steps: usize,
    steps: Vec<PlanningStepSummary>,
    omitted_steps: usize,
    catalog_digest: String,
    capabilities: Vec<PlanningCapabilitySummary>,
    omitted_capabilities: usize,
}

/// Deterministic provider-neutral input for one planner call.
///
/// It contains no invocation arguments, credentials, policy decisions, selected Instances, or
/// effect authority. Goal, objective, summary, and schema content remains arbitrary provider-bound
/// text, so a composition root must still apply its egress policy. The canonical payload is
/// bounded by the Run's durable context budget.
///
/// `Serialize` emits only the canonical provider payload. [`Self::context_digest`] and
/// [`Self::canonical_size_bytes`] are host-owned companion facts and are intentionally not
/// self-included in that payload.
#[derive(Clone, PartialEq, Eq)]
pub struct PlanningContext {
    payload: PlanningContextPayload,
    context_digest: String,
    canonical_size_bytes: u64,
}

impl PlanningContext {
    #[must_use]
    pub const fn profile_version(&self) -> &str {
        self.payload.profile_version
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.payload.run_id
    }

    #[must_use]
    pub fn goal(&self) -> &str {
        &self.payload.goal
    }

    #[must_use]
    pub fn authority(&self) -> &str {
        &self.payload.authority
    }

    #[must_use]
    pub const fn authority_epoch(&self) -> u64 {
        self.payload.authority_epoch
    }

    #[must_use]
    pub const fn journal_sequence(&self) -> u64 {
        self.payload.journal_sequence
    }

    #[must_use]
    pub fn journal_head_digest(&self) -> &str {
        &self.payload.journal_head_digest
    }

    #[must_use]
    pub fn catalog_digest(&self) -> &str {
        &self.payload.catalog_digest
    }

    #[must_use]
    pub fn context_digest(&self) -> &str {
        &self.context_digest
    }

    #[must_use]
    pub const fn canonical_size_bytes(&self) -> u64 {
        self.canonical_size_bytes
    }

    #[must_use]
    pub fn capabilities(&self) -> &[PlanningCapabilitySummary] {
        &self.payload.capabilities
    }

    #[must_use]
    pub const fn omitted_capabilities(&self) -> usize {
        self.payload.omitted_capabilities
    }

    #[must_use]
    pub const fn total_steps(&self) -> usize {
        self.payload.total_steps
    }

    #[must_use]
    pub const fn verified_completed_steps(&self) -> usize {
        self.payload.verified_completed_steps
    }

    #[must_use]
    pub fn steps(&self) -> &[PlanningStepSummary] {
        &self.payload.steps
    }

    #[must_use]
    pub const fn omitted_steps(&self) -> usize {
        self.payload.omitted_steps
    }
}

impl Serialize for PlanningContext {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.payload.serialize(serializer)
    }
}

impl fmt::Debug for PlanningContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlanningContext")
            .field("profile_version", &self.payload.profile_version)
            .field("run_id", &self.payload.run_id)
            .field("authority", &self.payload.authority)
            .field("authority_epoch", &self.payload.authority_epoch)
            .field("journal_sequence", &self.payload.journal_sequence)
            .field("journal_head_digest", &self.payload.journal_head_digest)
            .field("goal", &"<redacted>")
            .field("total_steps", &self.payload.total_steps)
            .field(
                "verified_completed_steps",
                &self.payload.verified_completed_steps,
            )
            .field("step_count", &self.payload.steps.len())
            .field("omitted_steps", &self.payload.omitted_steps)
            .field("catalog_digest", &self.payload.catalog_digest)
            .field("capability_count", &self.payload.capabilities.len())
            .field("omitted_capabilities", &self.payload.omitted_capabilities)
            .field("context_digest", &self.context_digest)
            .field("canonical_size_bytes", &self.canonical_size_bytes)
            .finish()
    }
}

/// A dependency named by an untrusted planner proposal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PlanDependency {
    ExistingStep { step_id: String },
    ProposedStep { key: String },
}

impl PlanDependency {
    #[must_use]
    pub fn existing(step_id: impl Into<String>) -> Self {
        Self::ExistingStep {
            step_id: step_id.into(),
        }
    }

    #[must_use]
    pub fn proposed(key: impl Into<String>) -> Self {
        Self::ProposedStep { key: key.into() }
    }
}

/// One untrusted, transient Step proposed by a planner provider.
#[derive(Clone, PartialEq, Eq)]
pub struct ProposedPlanStep {
    key: String,
    objective: String,
    depends_on: Vec<PlanDependency>,
    capability: CapabilityRef,
    arguments: Value,
}

impl ProposedPlanStep {
    #[must_use]
    pub fn new(
        key: impl Into<String>,
        objective: impl Into<String>,
        depends_on: Vec<PlanDependency>,
        capability: CapabilityRef,
        arguments: Value,
    ) -> Self {
        Self {
            key: key.into(),
            objective: objective.into(),
            depends_on,
            capability,
            arguments,
        }
    }
}

impl fmt::Debug for ProposedPlanStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProposedPlanStep")
            .field("key", &self.key)
            .field("objective", &"<redacted>")
            .field("depends_on", &self.depends_on)
            .field("capability", &self.capability)
            .field("arguments", &"<redacted>")
            .finish()
    }
}

/// Provider-neutral planner result. All text and arguments remain transient until Core validation.
#[derive(Clone, PartialEq, Eq)]
pub enum PlanProposal {
    Plan { steps: Vec<ProposedPlanStep> },
    CompletionCandidate { summary: String },
}

impl PlanProposal {
    #[must_use]
    pub fn plan(steps: Vec<ProposedPlanStep>) -> Self {
        Self::Plan { steps }
    }

    #[must_use]
    pub fn completion_candidate(summary: impl Into<String>) -> Self {
        Self::CompletionCandidate {
            summary: summary.into(),
        }
    }
}

impl fmt::Debug for PlanProposal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan { steps } => formatter
                .debug_struct("PlanProposal::Plan")
                .field("step_count", &steps.len())
                .field("steps", &"<redacted>")
                .finish(),
            Self::CompletionCandidate { .. } => formatter
                .debug_struct("PlanProposal::CompletionCandidate")
                .field("summary", &"<redacted>")
                .finish(),
        }
    }
}

/// Closed, non-sensitive planner failure taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PlannerPortFailure {
    #[error("planner timed out")]
    Timeout,
    #[error("planner is unavailable")]
    Unavailable,
    #[error("planner response could not be decoded")]
    InvalidResponse,
    #[error("planner request exceeded provider limits")]
    ProviderLimit,
    #[error("planner provider rejected the request")]
    ProviderRejected,
}

/// Core-owned envelope for exactly one durably reserved planner call.
///
/// This value is intentionally not serializable. The context may contain arbitrary provider-bound
/// text and schemas; only the reservation identifiers and digests belong in the Run journal.
pub struct PlannerCallRequest<'a> {
    call_id: &'a str,
    request_digest: &'a str,
    context: &'a PlanningContext,
}

impl<'a> PlannerCallRequest<'a> {
    #[must_use]
    pub const fn call_id(&self) -> &'a str {
        self.call_id
    }

    #[must_use]
    pub const fn request_digest(&self) -> &'a str {
        self.request_digest
    }

    #[must_use]
    pub const fn context(&self) -> &'a PlanningContext {
        self.context
    }
}

impl fmt::Debug for PlannerCallRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlannerCallRequest")
            .field("call_id", &self.call_id)
            .field("request_digest", &self.request_digest)
            .field("context_digest", &self.context.context_digest())
            .field("context", &"<redacted>")
            .finish()
    }
}

/// Provider-neutral boundary for one bounded planning decision.
pub trait PlannerPort {
    /// Stable, non-secret registry identifier supplied by the trusted host.
    ///
    /// Syntactic validation does not detect secrets. Credentials and raw provider content must
    /// never be placed in this identifier.
    fn planner_id(&self) -> &str;

    /// Digest of an approved, non-secret, versioned request profile owned by the trusted host.
    ///
    /// SHA-256 shape validation does not establish provenance or confidentiality. Raw prompts,
    /// responses, errors, and credentials must never be embedded or ad hoc hashed into this value.
    fn request_profile_digest(&self) -> &str;

    /// Return one proposal without executing tools or changing the Run store.
    ///
    /// # Errors
    ///
    /// Returns only a fixed failure class. One invocation may issue at most one provider request;
    /// hidden SDK retries would bypass the durable call budget. Raw response bodies must remain
    /// behind this boundary.
    fn plan(
        &mut self,
        request: &PlannerCallRequest<'_>,
    ) -> Result<PlanProposal, PlannerPortFailure>;
}

/// Exact normalized material that a trusted host materializer must durably retain before commit.
pub struct PlanMaterializationRequest<'a> {
    run_id: &'a str,
    step_id: &'a str,
    proposal_digest: &'a str,
    capability: &'a CapabilityRef,
    normalized_arguments: &'a Value,
    material_digest: &'a str,
}

impl<'a> PlanMaterializationRequest<'a> {
    #[must_use]
    pub const fn run_id(&self) -> &'a str {
        self.run_id
    }

    #[must_use]
    pub const fn step_id(&self) -> &'a str {
        self.step_id
    }

    #[must_use]
    pub const fn proposal_digest(&self) -> &'a str {
        self.proposal_digest
    }

    #[must_use]
    pub const fn capability(&self) -> &'a CapabilityRef {
        self.capability
    }

    #[must_use]
    pub const fn normalized_arguments(&self) -> &'a Value {
        self.normalized_arguments
    }

    #[must_use]
    pub const fn material_digest(&self) -> &'a str {
        self.material_digest
    }
}

impl fmt::Debug for PlanMaterializationRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlanMaterializationRequest")
            .field("run_id", &self.run_id)
            .field("step_id", &self.step_id)
            .field("proposal_digest", &self.proposal_digest)
            .field("capability", &self.capability)
            .field("normalized_arguments", &"<redacted>")
            .field("material_digest", &self.material_digest)
            .finish()
    }
}

/// Closed, non-sensitive host materialization failure taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PlanMaterializerFailure {
    #[error("plan materializer is unavailable")]
    Unavailable,
    #[error("plan materializer rejected the invocation")]
    Rejected,
    #[error("plan materializer could not persist an immutable recipe")]
    PersistenceFailed,
}

/// Trusted host boundary that persists an immutable reconstruction recipe before plan commit.
pub trait PlanMaterializer {
    /// Return a bounded opaque reference after the normalized material is durable.
    ///
    /// # Errors
    ///
    /// Returns only a fixed failure class. Implementations may leave unreachable orphan recipes
    /// when a later Run-store compare-and-append loses a race, but must never expose raw material.
    fn materialize(
        &mut self,
        request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure>;
}

/// Why the loop deliberately yielded without selecting or committing more work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLoopQuiescence {
    FailedSteps,
    ManualIntervention,
    UnverifiedCompletion,
    BlockedDependencies,
    WaitingDependencies,
    ModelTurnBudgetExhausted,
    ModelCallBudgetExhausted,
    PlannedStepBudgetExhausted,
    ToolCallBudgetExhausted,
    ContextBudgetExceeded,
    ContextInputLimitExceeded,
    NoActionableWork,
}

/// Fixed validation result for an untrusted proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalRejection {
    EmptyPlan,
    TooManySteps,
    TooManyEdges,
    ProposalTooLarge,
    InvalidStepKey,
    DuplicateStepKey,
    InvalidObjective,
    InvalidDependency,
    DuplicateDependency,
    UnknownDependency,
    BlockedExistingDependency,
    SelfDependency,
    DependencyCycle,
    CapabilityUnavailable,
    CapabilityUnsupported,
    InvocationInvalid,
    DuplicateSemanticAction,
    PlannedStepBudgetExceeded,
    ToolCallBudgetExhausted,
    CompletionWithoutReceiptCompletedPlan,
    InvalidCompletionSummary,
}

/// Result of exactly one bounded orchestration tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLoopTick {
    Configured {
        head: AgentLoopHead,
    },
    ModelCallLifecycleConfigured {
        head: AgentLoopHead,
    },
    ActionRequired {
        action: FrontierAction,
        head: AgentLoopHead,
    },
    PlanAccepted {
        step_ids: Vec<String>,
        head: AgentLoopHead,
    },
    CompletionCandidate {
        candidate: CompletionCandidateState,
        newly_recorded: bool,
        head: AgentLoopHead,
    },
    Quiescent {
        reason: AgentLoopQuiescence,
        head: AgentLoopHead,
    },
    PlannerUnavailable {
        failure: PlannerPortFailure,
        head: AgentLoopHead,
    },
    ProposalRejected {
        reason: ProposalRejection,
        head: AgentLoopHead,
    },
    MaterializerUnavailable {
        failure: PlanMaterializerFailure,
        head: AgentLoopHead,
    },
    ModelCallRecoveryRequired {
        call_id: String,
        reason: ModelCallUnknownReason,
        newly_recorded: bool,
        head: AgentLoopHead,
    },
    ModelCallRejected {
        reason: ModelCallRejectionReason,
        head: AgentLoopHead,
    },
    ModelCallAbandoned {
        call_id: String,
        head: AgentLoopHead,
    },
}

/// Single-orchestrator, model-provider-neutral coordinator for a durable Run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLoop {
    budget: AgentLoopBudget,
    model_call_budget: ModelCallBudget,
}

impl AgentLoop {
    /// Build a loop whose conservative call budget equals its accepted-turn budget.
    ///
    /// # Panics
    ///
    /// Panics when `budget` was constructed directly with a zero `max_model_turns` instead of
    /// through [`AgentLoopBudget::new`].
    #[must_use]
    pub fn new(budget: AgentLoopBudget) -> Self {
        let model_call_budget = ModelCallBudget::new(budget.max_model_turns)
            .expect("a validated AgentLoop budget has a non-zero model-turn limit");
        Self {
            budget,
            model_call_budget,
        }
    }

    #[must_use]
    pub const fn with_model_call_budget(
        budget: AgentLoopBudget,
        model_call_budget: ModelCallBudget,
    ) -> Self {
        Self {
            budget,
            model_call_budget,
        }
    }

    #[must_use]
    pub const fn budget(&self) -> &AgentLoopBudget {
        &self.budget
    }

    #[must_use]
    pub const fn model_call_budget(&self) -> &ModelCallBudget {
        &self.model_call_budget
    }

    /// Select at most one current frontier action, or make at most one planner decision.
    ///
    /// Existing recovery, reconciliation, verification, committed intent, and admission actions
    /// always precede planning. This method returns those actions to the caller and never invokes
    /// an Executor, verifier, policy UI, or admission path automatically.
    ///
    /// A plan result and all of its secret-free material-reference sidecars are committed in one
    /// store transaction. Every possible provider call is first reserved durably. Typed provider,
    /// proposal, and materialization failures therefore leave the `WorkGraph` unchanged but append a
    /// closed lifecycle outcome to the Run journal.
    ///
    /// `resolver` is a planning-time canonicalizer and must be deterministic and side-effect-free.
    /// Core recomputes its result against final Step IDs and rejects any mismatch before calling
    /// the materializer.
    ///
    /// # Errors
    ///
    /// Returns an error for missing/corrupt state, wrong lease, changed durable configuration,
    /// event creation, canonicalization, or store failure.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn tick<S, F, L, R, P, M>(
        &self,
        store: &mut S,
        events: &mut F,
        lease: &L,
        capabilities: &CapabilityRegistry,
        resolver: &R,
        planner: &mut P,
        materializer: &mut M,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
        L: RunLease,
        R: ResourceResolver,
        P: PlannerPort,
        M: PlanMaterializer,
    {
        let state = store
            .load_current()?
            .ok_or(AgentLoopError::RunNotInitialized)?;
        verify_lease(lease, &state)?;
        if state.agent_loop.is_none() {
            return self.configure(store, events, &state);
        }
        let loop_state = state
            .agent_loop
            .as_ref()
            .ok_or(AgentLoopError::AgentLoopNotConfigured)?;
        if loop_state.budget != self.budget {
            return Err(AgentLoopError::BudgetMismatch);
        }
        if loop_state.model_calls.is_none() {
            return self.configure_model_call_lifecycle(store, events, &state);
        }
        let model_calls = loop_state
            .model_calls
            .as_ref()
            .ok_or(AgentLoopError::ModelCallLifecycleNotConfigured)?;
        if model_calls.budget != self.model_call_budget {
            return Err(AgentLoopError::ModelCallBudgetMismatch);
        }
        let frontier = derive_frontier(&state)?;
        let tool_calls = total_tool_calls(&state)?;
        if let Some(action) = frontier.next_action() {
            if action_would_issue_new_tool_call(&state, action)
                && tool_calls >= loop_state.budget.max_tool_calls
            {
                return Ok(AgentLoopTick::Quiescent {
                    reason: AgentLoopQuiescence::ToolCallBudgetExhausted,
                    head: AgentLoopHead::from_state(&state),
                });
            }
            return Ok(AgentLoopTick::ActionRequired {
                action: action.clone(),
                head: AgentLoopHead::from_state(&state),
            });
        }

        if let Some(active) = &model_calls.active_call {
            return match active.status {
                ModelCallStatus::Reserved => Self::mark_model_call_unknown(
                    store,
                    events,
                    &state,
                    active.reservation.call_id(),
                    ModelCallUnknownReason::Interrupted,
                ),
                ModelCallStatus::Unknown { reason } => {
                    Ok(AgentLoopTick::ModelCallRecoveryRequired {
                        call_id: active.reservation.call_id().to_owned(),
                        reason,
                        newly_recorded: false,
                        head: AgentLoopHead::from_state(&state),
                    })
                }
            };
        }

        if let Some(candidate) = &loop_state.completion_candidate {
            return Ok(AgentLoopTick::CompletionCandidate {
                candidate: candidate.clone(),
                newly_recorded: false,
                head: AgentLoopHead::from_state(&state),
            });
        }
        if let Some(reason) = terminal_quiescence(&frontier) {
            return Ok(AgentLoopTick::Quiescent {
                reason,
                head: AgentLoopHead::from_state(&state),
            });
        }
        if loop_state.accepted_model_turns >= loop_state.budget.max_model_turns {
            return Ok(AgentLoopTick::Quiescent {
                reason: AgentLoopQuiescence::ModelTurnBudgetExhausted,
                head: AgentLoopHead::from_state(&state),
            });
        }
        if model_calls.reserved_calls >= model_calls.budget.max_model_calls() {
            return Ok(AgentLoopTick::Quiescent {
                reason: AgentLoopQuiescence::ModelCallBudgetExhausted,
                head: AgentLoopHead::from_state(&state),
            });
        }
        if state.steps.len()
            >= usize::try_from(loop_state.budget.max_planned_steps).unwrap_or(usize::MAX)
            && !frontier.all_steps_receipt_completed()
        {
            return Ok(AgentLoopTick::Quiescent {
                reason: AgentLoopQuiescence::PlannedStepBudgetExhausted,
                head: AgentLoopHead::from_state(&state),
            });
        }

        let context = match build_context(&state, &frontier, capabilities, &loop_state.budget) {
            Ok(context) => context,
            Err(ContextBuildError::BudgetExceeded) => {
                return Ok(AgentLoopTick::Quiescent {
                    reason: AgentLoopQuiescence::ContextBudgetExceeded,
                    head: AgentLoopHead::from_state(&state),
                });
            }
            Err(ContextBuildError::InputLimitExceeded) => {
                return Ok(AgentLoopTick::Quiescent {
                    reason: AgentLoopQuiescence::ContextInputLimitExceeded,
                    head: AgentLoopHead::from_state(&state),
                });
            }
            Err(ContextBuildError::Canonicalization) => {
                return Err(AgentLoopError::Canonicalization);
            }
            Err(ContextBuildError::Catalog) => return Err(AgentLoopError::CapabilityCatalog),
        };
        let planner_id = planner.planner_id().to_owned();
        let request_profile_digest = planner.request_profile_digest().to_owned();
        if !valid_sha256_digest(&request_profile_digest) {
            return Err(AgentLoopError::InvalidPlannerRequestProfileDigest);
        }
        let request_digest = planner_request_digest(
            &planner_id,
            &request_profile_digest,
            context.context_digest(),
        )?;
        let call_index = model_calls
            .reserved_calls
            .checked_add(1)
            .ok_or(AgentLoopError::BudgetCounterOverflow)?;
        let turn_index = next_turn_index(&state)?;
        let reservation = ModelCallReservation::new(
            &state.run_id,
            state.authority_epoch,
            planner_id,
            call_index,
            turn_index,
            state.journal_sequence,
            &state.journal_head_digest,
            context.context_digest(),
            request_digest,
        )?;
        let reservation_event = create_event(
            events,
            &state,
            RunEventBody::ModelCallReserved {
                reservation: reservation.clone(),
            },
        )?;
        let reservation_commit =
            store.append(ExpectedHead::from_state(&state), reservation_event)?;
        let reserved_state = reservation_commit.state;
        let request = PlannerCallRequest {
            call_id: reservation.call_id(),
            request_digest: reservation.request_digest(),
            context: &context,
        };
        let proposal = match planner.plan(&request) {
            Ok(proposal) => proposal,
            Err(failure) => {
                return Self::record_planner_failure(
                    store,
                    events,
                    &reserved_state,
                    reservation.call_id(),
                    failure,
                );
            }
        };
        self.handle_proposal(
            store,
            events,
            capabilities,
            resolver,
            materializer,
            &state,
            &reserved_state,
            &frontier,
            &context,
            reservation.call_id(),
            proposal,
            tool_calls,
        )
    }

    /// Explicitly discard one unresolved model call. No call-budget slot is refunded.
    ///
    /// # Errors
    ///
    /// Returns an error for missing state, a wrong lease, changed durable configuration, or when
    /// the named call is not the active unresolved call.
    pub fn abandon_model_call<S, F, L>(
        &self,
        store: &mut S,
        events: &mut F,
        lease: &L,
        call_id: &str,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
        L: RunLease,
    {
        let state = store
            .load_current()?
            .ok_or(AgentLoopError::RunNotInitialized)?;
        verify_lease(lease, &state)?;
        let loop_state = state
            .agent_loop
            .as_ref()
            .ok_or(AgentLoopError::AgentLoopNotConfigured)?;
        if loop_state.budget != self.budget {
            return Err(AgentLoopError::BudgetMismatch);
        }
        let lifecycle = loop_state
            .model_calls
            .as_ref()
            .ok_or(AgentLoopError::ModelCallLifecycleNotConfigured)?;
        if lifecycle.budget != self.model_call_budget {
            return Err(AgentLoopError::ModelCallBudgetMismatch);
        }
        let active = lifecycle
            .active_call
            .as_ref()
            .ok_or(AgentLoopError::ModelCallNotActive)?;
        if active.reservation.call_id() != call_id {
            return Err(AgentLoopError::ModelCallIdMismatch);
        }
        let event = create_event(
            events,
            &state,
            RunEventBody::ModelCallSettled {
                call_id: call_id.to_owned(),
                settlement: ModelCallSettlement::Abandoned {
                    reason: ModelCallAbandonmentReason::RecoveryDiscarded,
                },
            },
        )?;
        let commit = store.append(ExpectedHead::from_state(&state), event)?;
        Ok(AgentLoopTick::ModelCallAbandoned {
            call_id: call_id.to_owned(),
            head: AgentLoopHead::from_state(&commit.state),
        })
    }

    fn configure<S, F>(
        &self,
        store: &mut S,
        events: &mut F,
        state: &RunState,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        let event = create_event(
            events,
            state,
            RunEventBody::AgentLoopConfigured {
                budget: self.budget.clone(),
            },
        )?;
        let commit = store.append(ExpectedHead::from_state(state), event)?;
        Ok(AgentLoopTick::Configured {
            head: AgentLoopHead::from_state(&commit.state),
        })
    }

    fn configure_model_call_lifecycle<S, F>(
        &self,
        store: &mut S,
        events: &mut F,
        state: &RunState,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        let event = create_event(
            events,
            state,
            RunEventBody::ModelCallLifecycleConfigured {
                budget: self.model_call_budget.clone(),
            },
        )?;
        let commit = store.append(ExpectedHead::from_state(state), event)?;
        Ok(AgentLoopTick::ModelCallLifecycleConfigured {
            head: AgentLoopHead::from_state(&commit.state),
        })
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn handle_proposal<S, F, R, M>(
        &self,
        store: &mut S,
        events: &mut F,
        capabilities: &CapabilityRegistry,
        resolver: &R,
        materializer: &mut M,
        base_state: &RunState,
        reserved_state: &RunState,
        frontier: &WorkFrontier,
        context: &PlanningContext,
        call_id: &str,
        proposal: PlanProposal,
        tool_calls: u32,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
        R: ResourceResolver,
        M: PlanMaterializer,
    {
        match proposal {
            PlanProposal::CompletionCandidate { summary } => Self::record_completion_candidate(
                store,
                events,
                base_state,
                reserved_state,
                frontier,
                context,
                call_id,
                &summary,
            ),
            PlanProposal::Plan { steps } => {
                if tool_calls >= self.budget.max_tool_calls {
                    return Self::record_proposal_rejection(
                        store,
                        events,
                        reserved_state,
                        call_id,
                        ProposalRejection::ToolCallBudgetExhausted,
                    );
                }
                let prepared = match prepare_plan(
                    base_state,
                    frontier,
                    context,
                    capabilities,
                    resolver,
                    &self.budget,
                    steps,
                ) {
                    Ok(prepared) => prepared,
                    Err(reason) => {
                        return Self::record_proposal_rejection(
                            store,
                            events,
                            reserved_state,
                            call_id,
                            reason,
                        );
                    }
                };
                let (accepted_steps, inputs) = match materialize_plan(
                    materializer,
                    base_state,
                    &prepared.proposal_digest,
                    prepared.steps,
                ) {
                    Ok(values) => values,
                    Err(failure) => {
                        let commit = match append_model_call_rejection(
                            store,
                            events,
                            reserved_state,
                            call_id,
                            ModelCallRejectionReason::MaterializationFailed,
                        ) {
                            Ok(commit) => commit,
                            Err(AgentLoopError::Store(StoreError::HeadConflict { .. })) => {
                                return Self::resolve_model_call_conflict(
                                    store,
                                    events,
                                    call_id,
                                    ModelCallConflictIntent::RejectStale,
                                );
                            }
                            Err(error) => return Err(error),
                        };
                        return Ok(AgentLoopTick::MaterializerUnavailable {
                            failure,
                            head: AgentLoopHead::from_state(&commit.state),
                        });
                    }
                };
                let decision = ExpectedPlanningTurn::for_model_call(
                    next_turn_index(base_state)?,
                    call_id,
                    context.context_digest(),
                    &prepared.proposal_digest,
                )?;
                let step_ids = accepted_steps
                    .iter()
                    .map(|step| step.step_id.clone())
                    .collect();
                let event = create_event(
                    events,
                    reserved_state,
                    RunEventBody::PlanAccepted {
                        decision,
                        steps: accepted_steps,
                    },
                )?;
                let commit = match store.append_with_plan_inputs(
                    ExpectedHead::from_state(reserved_state),
                    event,
                    inputs,
                ) {
                    Ok(commit) => commit,
                    Err(StoreError::HeadConflict { .. }) => {
                        return Self::record_stale_model_call(store, events, call_id);
                    }
                    Err(error) => return Err(error.into()),
                };
                Ok(AgentLoopTick::PlanAccepted {
                    step_ids,
                    head: AgentLoopHead::from_state(&commit.state),
                })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_completion_candidate<S, F>(
        store: &mut S,
        events: &mut F,
        base_state: &RunState,
        reserved_state: &RunState,
        frontier: &WorkFrontier,
        context: &PlanningContext,
        call_id: &str,
        summary: &str,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        if !frontier.all_steps_receipt_completed() {
            return Self::record_proposal_rejection(
                store,
                events,
                reserved_state,
                call_id,
                ProposalRejection::CompletionWithoutReceiptCompletedPlan,
            );
        }
        if !valid_bounded_text(summary, MAX_COMPLETION_SUMMARY_BYTES) {
            return Self::record_proposal_rejection(
                store,
                events,
                reserved_state,
                call_id,
                ProposalRejection::InvalidCompletionSummary,
            );
        }
        let proposal_digest = digest_serializable(&CompletionProposalDigestInput {
            domain: "xgeny.completion-proposal/v1",
            context_digest: context.context_digest(),
            summary,
        })?;
        if canonical_size(&CompletionProposalDigestInput {
            domain: "xgeny.completion-proposal/v1",
            context_digest: context.context_digest(),
            summary,
        })? > MAX_PROPOSAL_BYTES
        {
            return Self::record_proposal_rejection(
                store,
                events,
                reserved_state,
                call_id,
                ProposalRejection::ProposalTooLarge,
            );
        }
        let summary_digest = digest_serializable(&CompletionSummaryDigestInput {
            domain: "xgeny.completion-summary/v1",
            summary,
        })?;
        let candidate_id = content_id(
            "completion",
            &digest_serializable(&CompletionCandidateIdInput {
                domain: "xgeny.completion-candidate-id/v1",
                run_id: &base_state.run_id,
                context_digest: context.context_digest(),
                proposal_digest: &proposal_digest,
            })?,
        );
        let decision = ExpectedPlanningTurn::for_model_call(
            next_turn_index(base_state)?,
            call_id,
            context.context_digest(),
            &proposal_digest,
        )?;
        let event = create_event(
            events,
            reserved_state,
            RunEventBody::CompletionCandidateRecorded {
                decision,
                candidate_id: candidate_id.clone(),
                summary_digest: summary_digest.clone(),
            },
        )?;
        let commit = match store.append(ExpectedHead::from_state(reserved_state), event) {
            Ok(commit) => commit,
            Err(StoreError::HeadConflict { .. }) => {
                return Self::record_stale_model_call(store, events, call_id);
            }
            Err(error) => return Err(error.into()),
        };
        let candidate = commit
            .state
            .agent_loop
            .as_ref()
            .and_then(|loop_state| loop_state.completion_candidate.clone())
            .ok_or(AgentLoopError::CompletionCandidateNotProjected)?;
        Ok(AgentLoopTick::CompletionCandidate {
            candidate,
            newly_recorded: true,
            head: AgentLoopHead::from_state(&commit.state),
        })
    }

    fn record_proposal_rejection<S, F>(
        store: &mut S,
        events: &mut F,
        reserved_state: &RunState,
        call_id: &str,
        reason: ProposalRejection,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        let commit = match append_model_call_rejection(
            store,
            events,
            reserved_state,
            call_id,
            ModelCallRejectionReason::ProposalRejected,
        ) {
            Ok(commit) => commit,
            Err(AgentLoopError::Store(StoreError::HeadConflict { .. })) => {
                return Self::resolve_model_call_conflict(
                    store,
                    events,
                    call_id,
                    ModelCallConflictIntent::RejectStale,
                );
            }
            Err(error) => return Err(error),
        };
        Ok(AgentLoopTick::ProposalRejected {
            reason,
            head: AgentLoopHead::from_state(&commit.state),
        })
    }

    fn record_planner_failure<S, F>(
        store: &mut S,
        events: &mut F,
        reserved_state: &RunState,
        call_id: &str,
        failure: PlannerPortFailure,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        let (commit, conflict_intent) = match failure {
            PlannerPortFailure::Timeout => (
                append_model_call_unknown(
                    store,
                    events,
                    reserved_state,
                    call_id,
                    ModelCallUnknownReason::Timeout,
                ),
                ModelCallConflictIntent::MarkUnknown(ModelCallUnknownReason::Timeout),
            ),
            PlannerPortFailure::Unavailable => (
                append_model_call_unknown(
                    store,
                    events,
                    reserved_state,
                    call_id,
                    ModelCallUnknownReason::TransportUnavailable,
                ),
                ModelCallConflictIntent::MarkUnknown(ModelCallUnknownReason::TransportUnavailable),
            ),
            PlannerPortFailure::InvalidResponse => (
                append_model_call_rejection(
                    store,
                    events,
                    reserved_state,
                    call_id,
                    ModelCallRejectionReason::PlannerInvalidResponse,
                ),
                ModelCallConflictIntent::RejectStale,
            ),
            PlannerPortFailure::ProviderLimit => (
                append_model_call_rejection(
                    store,
                    events,
                    reserved_state,
                    call_id,
                    ModelCallRejectionReason::ProviderLimit,
                ),
                ModelCallConflictIntent::RejectStale,
            ),
            PlannerPortFailure::ProviderRejected => (
                append_model_call_rejection(
                    store,
                    events,
                    reserved_state,
                    call_id,
                    ModelCallRejectionReason::ProviderRejected,
                ),
                ModelCallConflictIntent::RejectStale,
            ),
        };
        let commit = match commit {
            Ok(commit) => commit,
            Err(AgentLoopError::Store(StoreError::HeadConflict { .. })) => {
                return Self::resolve_model_call_conflict(store, events, call_id, conflict_intent);
            }
            Err(error) => return Err(error),
        };
        Ok(AgentLoopTick::PlannerUnavailable {
            failure,
            head: AgentLoopHead::from_state(&commit.state),
        })
    }

    fn mark_model_call_unknown<S, F>(
        store: &mut S,
        events: &mut F,
        state: &RunState,
        call_id: &str,
        reason: ModelCallUnknownReason,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        let commit = match append_model_call_unknown(store, events, state, call_id, reason) {
            Ok(commit) => commit,
            Err(AgentLoopError::Store(StoreError::HeadConflict { .. })) => {
                return Self::resolve_model_call_conflict(
                    store,
                    events,
                    call_id,
                    ModelCallConflictIntent::MarkUnknown(reason),
                );
            }
            Err(error) => return Err(error),
        };
        Ok(AgentLoopTick::ModelCallRecoveryRequired {
            call_id: call_id.to_owned(),
            reason,
            newly_recorded: true,
            head: AgentLoopHead::from_state(&commit.state),
        })
    }

    fn record_stale_model_call<S, F>(
        store: &mut S,
        events: &mut F,
        call_id: &str,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        Self::resolve_model_call_conflict(
            store,
            events,
            call_id,
            ModelCallConflictIntent::RejectStale,
        )
    }

    fn resolve_model_call_conflict<S, F>(
        store: &mut S,
        events: &mut F,
        call_id: &str,
        intent: ModelCallConflictIntent,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        let current = store
            .load_current()?
            .ok_or(AgentLoopError::RunNotInitialized)?;
        let active = current
            .agent_loop
            .as_ref()
            .ok_or(AgentLoopError::AgentLoopNotConfigured)?
            .model_calls
            .as_ref()
            .ok_or(AgentLoopError::ModelCallLifecycleNotConfigured)?
            .active_call
            .as_ref()
            .ok_or(AgentLoopError::ModelCallNoLongerActive)?;
        if active.reservation.call_id() != call_id {
            return Err(AgentLoopError::ModelCallNoLongerActive);
        }
        match active.status {
            ModelCallStatus::Unknown { reason } => Ok(AgentLoopTick::ModelCallRecoveryRequired {
                call_id: call_id.to_owned(),
                reason,
                newly_recorded: false,
                head: AgentLoopHead::from_state(&current),
            }),
            ModelCallStatus::Reserved => match intent {
                ModelCallConflictIntent::MarkUnknown(reason) => {
                    let commit =
                        append_model_call_unknown(store, events, &current, call_id, reason)?;
                    Ok(AgentLoopTick::ModelCallRecoveryRequired {
                        call_id: call_id.to_owned(),
                        reason,
                        newly_recorded: true,
                        head: AgentLoopHead::from_state(&commit.state),
                    })
                }
                ModelCallConflictIntent::RejectStale => {
                    let commit = append_model_call_rejection(
                        store,
                        events,
                        &current,
                        call_id,
                        ModelCallRejectionReason::StaleHead,
                    )?;
                    Ok(AgentLoopTick::ModelCallRejected {
                        reason: ModelCallRejectionReason::StaleHead,
                        head: AgentLoopHead::from_state(&commit.state),
                    })
                }
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelCallConflictIntent {
    MarkUnknown(ModelCallUnknownReason),
    RejectStale,
}

fn append_model_call_unknown<S, F>(
    store: &mut S,
    events: &mut F,
    state: &RunState,
    call_id: &str,
    reason: ModelCallUnknownReason,
) -> Result<xgeny_local_store::Commit, AgentLoopError>
where
    S: RunStore,
    F: EventFactory,
{
    let event = create_event(
        events,
        state,
        RunEventBody::ModelCallBecameUnknown {
            call_id: call_id.to_owned(),
            reason,
        },
    )?;
    Ok(store.append(ExpectedHead::from_state(state), event)?)
}

fn append_model_call_rejection<S, F>(
    store: &mut S,
    events: &mut F,
    state: &RunState,
    call_id: &str,
    reason: ModelCallRejectionReason,
) -> Result<xgeny_local_store::Commit, AgentLoopError>
where
    S: RunStore,
    F: EventFactory,
{
    let event = create_event(
        events,
        state,
        RunEventBody::ModelCallSettled {
            call_id: call_id.to_owned(),
            settlement: ModelCallSettlement::Rejected { reason },
        },
    )?;
    Ok(store.append(ExpectedHead::from_state(state), event)?)
}

fn verify_lease<L: RunLease>(lease: &L, state: &RunState) -> Result<(), AgentLoopError> {
    if lease.run_id() != state.run_id {
        return Err(AgentLoopError::LeaseRunMismatch);
    }
    Ok(())
}

fn total_tool_calls(state: &RunState) -> Result<u32, AgentLoopError> {
    state.steps.values().try_fold(0_u32, |total, step| {
        total
            .checked_add(step.attempts)
            .ok_or(AgentLoopError::BudgetCounterOverflow)
    })
}

fn action_would_issue_new_tool_call(state: &RunState, action: &FrontierAction) -> bool {
    match action.action {
        ContinuationAction::Admit | ContinuationAction::Verify => false,
        ContinuationAction::DriveEffect => state
            .steps
            .get(&action.step_id)
            .is_some_and(|step| step.status == StepStatus::IntentCommitted),
    }
}

fn terminal_quiescence(frontier: &WorkFrontier) -> Option<AgentLoopQuiescence> {
    if !frontier.manual_required_step_ids.is_empty() {
        Some(AgentLoopQuiescence::ManualIntervention)
    } else if !frontier.failed_step_ids.is_empty() {
        Some(AgentLoopQuiescence::FailedSteps)
    } else if !frontier.unverified_completed_step_ids.is_empty() {
        Some(AgentLoopQuiescence::UnverifiedCompletion)
    } else if !frontier.blocked.is_empty() {
        Some(AgentLoopQuiescence::BlockedDependencies)
    } else if !frontier.waiting.is_empty() {
        Some(AgentLoopQuiescence::WaitingDependencies)
    } else if frontier.total_steps > 0 && !frontier.all_steps_receipt_completed() {
        Some(AgentLoopQuiescence::NoActionableWork)
    } else {
        None
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogDigestEntry<'a> {
    capability_id: &'a str,
    contract_version: &'a str,
    definition_digest: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogDigestInput<'a> {
    domain: &'static str,
    entries: &'a [CatalogDigestEntry<'a>],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextDigestInput<'a> {
    domain: &'static str,
    payload: &'a PlanningContextPayload,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannerRequestDigestInput<'a> {
    domain: &'static str,
    planner_id: &'a str,
    request_profile_digest: &'a str,
    context_digest: &'a str,
}

fn planner_request_digest(
    planner_id: &str,
    request_profile_digest: &str,
    context_digest: &str,
) -> Result<String, AgentLoopError> {
    digest_serializable(&PlannerRequestDigestInput {
        domain: "xgeny.model-call-request/v1",
        planner_id,
        request_profile_digest,
        context_digest,
    })
}

fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[derive(Debug)]
enum ContextBuildError {
    BudgetExceeded,
    InputLimitExceeded,
    Canonicalization,
    Catalog,
}

#[allow(clippy::too_many_lines)] // Keeps one canonical packing pipeline and exact byte checks.
fn build_context(
    state: &RunState,
    frontier: &WorkFrontier,
    registry: &CapabilityRegistry,
    budget: &AgentLoopBudget,
) -> Result<PlanningContext, ContextBuildError> {
    if state
        .run_id
        .len()
        .checked_add(state.authority.len())
        .and_then(|total| total.checked_add(state.journal_head_digest.len()))
        .and_then(|total| total.checked_add(state.goal.len()))
        .is_none_or(|total| total > MAX_PLANNING_HEADER_TEXT_BYTES)
    {
        return Err(ContextBuildError::InputLimitExceeded);
    }
    if registry.definition_count() > MAX_PLANNING_CATALOG_DEFINITIONS
        || state.steps.len() > MAX_PLANNING_SOURCE_STEPS
    {
        return Err(ContextBuildError::InputLimitExceeded);
    }
    let mut source_bytes = 0_usize;
    let mut summaries = Vec::new();
    for definition in registry.definitions() {
        validate_planning_definition_shape(definition)?;
        let definition_size =
            canonical_size(definition).map_err(|_| ContextBuildError::Canonicalization)?;
        if definition_size > MAX_PLANNING_DEFINITION_BYTES {
            return Err(ContextBuildError::InputLimitExceeded);
        }
        source_bytes = source_bytes
            .checked_add(definition_size)
            .filter(|size| *size <= MAX_PLANNING_SOURCE_BYTES)
            .ok_or(ContextBuildError::InputLimitExceeded)?;
        let definition_digest =
            definition_contract_digest(definition).map_err(|_| ContextBuildError::Catalog)?;
        let summary = PlanningCapabilitySummary {
            capability: CapabilityRef {
                capability_id: definition.metadata.id.clone(),
                contract_version: definition.metadata.contract_version.clone(),
            },
            definition_digest,
            summary: bounded_summary(&definition.spec.summary),
            input_schema: definition.spec.input_schema.clone(),
            output_schema: definition.spec.output_schema.clone(),
            effect_class: definition.spec.effect.class,
            resource_selectors: definition
                .spec
                .effect
                .resource_selectors
                .iter()
                .map(|selector| PlanningResourceSelector {
                    scope: selector.scope.clone(),
                    argument_pointer: selector.argument_pointer.clone(),
                })
                .collect(),
            critical_actions: definition.spec.effect.critical_actions.clone(),
            execution_styles: definition.spec.execution.styles.clone(),
            idempotency_key_supported: definition.spec.execution.idempotency_key_supported,
            default_timeout_ms: definition.spec.execution.default_timeout_ms,
            max_timeout_ms: definition.spec.execution.max_timeout_ms,
        };
        let summary_size =
            canonical_size(&summary).map_err(|_| ContextBuildError::Canonicalization)?;
        summaries.push((summary, summary_size));
    }
    summaries.sort_by(|left, right| {
        (
            left.0.capability.capability_id.as_str(),
            left.0.capability.contract_version.as_str(),
        )
            .cmp(&(
                right.0.capability.capability_id.as_str(),
                right.0.capability.contract_version.as_str(),
            ))
    });
    let catalog_entries: Vec<_> = summaries
        .iter()
        .map(|(summary, _)| CatalogDigestEntry {
            capability_id: &summary.capability.capability_id,
            contract_version: &summary.capability.contract_version,
            definition_digest: &summary.definition_digest,
        })
        .collect();
    let catalog_digest = digest_serializable(&CatalogDigestInput {
        domain: "xgeny.capability-catalog/v1",
        entries: &catalog_entries,
    })
    .map_err(|_| ContextBuildError::Canonicalization)?;
    let mut step_summaries = Vec::with_capacity(state.steps.len());
    for step in state.steps.values() {
        validate_planning_step_shape(step)?;
        let summary = PlanningStepSummary {
            step_id: step.step_id.clone(),
            objective: step.objective.clone(),
            depends_on: step.depends_on.clone(),
            status: step.status,
            attempts: step.attempts,
            capability: step
                .planned_invocation
                .as_ref()
                .map(|invocation| CapabilityRef {
                    capability_id: invocation.capability_id().to_owned(),
                    contract_version: invocation.contract_version().to_owned(),
                }),
            dependency_released: receipt_releases_dependency(step),
        };
        let summary_size =
            canonical_size(&summary).map_err(|_| ContextBuildError::Canonicalization)?;
        if summary_size > MAX_PLANNING_STEP_BYTES {
            return Err(ContextBuildError::InputLimitExceeded);
        }
        source_bytes = source_bytes
            .checked_add(summary_size)
            .filter(|size| *size <= MAX_PLANNING_SOURCE_BYTES)
            .ok_or(ContextBuildError::InputLimitExceeded)?;
        step_summaries.push((summary, summary_size));
    }
    let total_steps = step_summaries.len();
    let total_capabilities = summaries.len();
    let mut payload = PlanningContextPayload {
        profile_version: PLANNING_CONTEXT_PROFILE_V1,
        run_id: state.run_id.clone(),
        authority: state.authority.clone(),
        authority_epoch: state.authority_epoch,
        journal_sequence: state.journal_sequence,
        journal_head_digest: state.journal_head_digest.clone(),
        goal: state.goal.clone(),
        total_steps: frontier.total_steps,
        verified_completed_steps: frontier.verified_completed_step_ids.len(),
        steps: Vec::new(),
        omitted_steps: total_steps,
        catalog_digest,
        capabilities: Vec::new(),
        omitted_capabilities: total_capabilities,
    };
    let maximum_size = usize::try_from(budget.max_context_bytes)
        .unwrap_or(usize::MAX)
        .min(usize::try_from(MAX_PLANNING_CONTEXT_BYTES).unwrap_or(usize::MAX));
    let mut conservative_size =
        canonical_size(&payload).map_err(|_| ContextBuildError::Canonicalization)?;
    if conservative_size > maximum_size {
        return Err(ContextBuildError::BudgetExceeded);
    }

    // Deterministic round-robin packing prevents either a large WorkGraph or a large catalog from
    // monopolizing the bounded context. Schemas and Step summaries are included as whole items;
    // an item that does not fit is omitted rather than truncated into an ambiguous contract.
    for index in 0..step_summaries.len().max(summaries.len()) {
        if let Some((step, item_size)) = step_summaries.get(index) {
            let delta = item_size.saturating_add(usize::from(!payload.steps.is_empty()));
            if conservative_size.saturating_add(delta) <= maximum_size {
                payload.steps.push(step.clone());
                payload.omitted_steps = total_steps - payload.steps.len();
                conservative_size += delta;
            }
        }
        if let Some((summary, item_size)) = summaries.get(index) {
            let delta = item_size.saturating_add(usize::from(!payload.capabilities.is_empty()));
            if conservative_size.saturating_add(delta) <= maximum_size {
                payload.capabilities.push(summary.clone());
                payload.omitted_capabilities = total_capabilities - payload.capabilities.len();
                conservative_size += delta;
            }
        }
    }
    let exact_size = canonical_size(&payload).map_err(|_| ContextBuildError::Canonicalization)?;
    if exact_size > maximum_size {
        return Err(ContextBuildError::BudgetExceeded);
    }
    let canonical_size_bytes =
        u64::try_from(exact_size).map_err(|_| ContextBuildError::BudgetExceeded)?;
    let context_digest = digest_serializable(&ContextDigestInput {
        domain: "xgeny.planning-context.digest/v1",
        payload: &payload,
    })
    .map_err(|_| ContextBuildError::Canonicalization)?;
    Ok(PlanningContext {
        payload,
        context_digest,
        canonical_size_bytes,
    })
}

fn validate_planning_definition_shape(
    definition: &xgeny_domain::CapabilityDefinitionBody,
) -> Result<(), ContextBuildError> {
    let collection_items = definition
        .extensions
        .len()
        .saturating_add(definition.required_extensions.len())
        .saturating_add(definition.metadata.labels.len())
        .saturating_add(definition.spec.required_capabilities.len())
        .saturating_add(definition.spec.effect.resource_selectors.len())
        .saturating_add(definition.spec.effect.critical_actions.len())
        .saturating_add(definition.spec.execution.styles.len())
        .saturating_add(definition.spec.verification.len())
        .saturating_add(definition.spec.discovery.as_ref().map_or(0, |discovery| {
            discovery
                .keywords
                .len()
                .saturating_add(discovery.examples.len())
        }));
    if collection_items > MAX_PLANNING_DEFINITION_COLLECTION_ITEMS {
        return Err(ContextBuildError::InputLimitExceeded);
    }
    let mut stats = PlanningValueStats::default();
    add_planning_text(&mut stats, definition.api_version.len())?;
    for extension in &definition.required_extensions {
        add_planning_text(&mut stats, extension.len())?;
    }
    for key in definition.extensions.keys() {
        add_planning_text(&mut stats, key.len())?;
    }
    add_planning_text(&mut stats, definition.metadata.id.len())?;
    add_planning_text(&mut stats, definition.metadata.contract_version.len())?;
    add_planning_text(&mut stats, definition.metadata.display_name.len())?;
    for (key, value) in &definition.metadata.labels {
        add_planning_text(&mut stats, key.len())?;
        add_planning_text(&mut stats, value.len())?;
    }
    add_planning_text(&mut stats, definition.spec.summary.len())?;
    for required in &definition.spec.required_capabilities {
        add_planning_text(&mut stats, required.capability_id.len())?;
        add_planning_text(&mut stats, required.contract_version.len())?;
    }
    for selector in &definition.spec.effect.resource_selectors {
        add_planning_text(&mut stats, selector.scope.len())?;
        add_planning_text(&mut stats, selector.argument_pointer.len())?;
    }
    for rule in &definition.spec.verification {
        if let Some(description) = &rule.description {
            add_planning_text(&mut stats, description.len())?;
        }
    }
    if let Some(discovery) = &definition.spec.discovery {
        for keyword in &discovery.keywords {
            add_planning_text(&mut stats, keyword.len())?;
        }
        for example in &discovery.examples {
            add_planning_text(&mut stats, example.len())?;
        }
        if let Some(details_ref) = &discovery.details_ref {
            add_planning_text(&mut stats, details_ref.len())?;
        }
    }
    validate_planning_value(&definition.spec.input_schema, &mut stats)?;
    validate_planning_value(&definition.spec.output_schema, &mut stats)?;
    for value in definition.extensions.values() {
        validate_planning_value(value, &mut stats)?;
    }
    Ok(())
}

fn validate_planning_step_shape(
    step: &xgeny_workgraph::StepState,
) -> Result<(), ContextBuildError> {
    if step.depends_on.len() > MAX_PLANNING_DEFINITION_COLLECTION_ITEMS {
        return Err(ContextBuildError::InputLimitExceeded);
    }
    let mut text_bytes = step
        .step_id
        .len()
        .checked_add(step.objective.len())
        .ok_or(ContextBuildError::InputLimitExceeded)?;
    for dependency in &step.depends_on {
        text_bytes = text_bytes
            .checked_add(dependency.len())
            .filter(|total| *total <= MAX_PLANNING_STEP_BYTES)
            .ok_or(ContextBuildError::InputLimitExceeded)?;
    }
    if let Some(invocation) = &step.planned_invocation {
        text_bytes = text_bytes
            .checked_add(invocation.capability_id().len())
            .and_then(|total| total.checked_add(invocation.contract_version().len()))
            .ok_or(ContextBuildError::InputLimitExceeded)?;
    }
    if text_bytes > MAX_PLANNING_STEP_BYTES {
        return Err(ContextBuildError::InputLimitExceeded);
    }
    Ok(())
}

#[derive(Default)]
struct PlanningValueStats {
    nodes: usize,
    text_bytes: usize,
}

fn validate_planning_value(
    root: &Value,
    stats: &mut PlanningValueStats,
) -> Result<(), ContextBuildError> {
    let mut pending = vec![(root, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_PLANNING_VALUE_DEPTH {
            return Err(ContextBuildError::InputLimitExceeded);
        }
        stats.nodes = stats
            .nodes
            .checked_add(1)
            .filter(|nodes| *nodes <= MAX_PLANNING_VALUE_NODES)
            .ok_or(ContextBuildError::InputLimitExceeded)?;
        match value {
            Value::String(text) => add_planning_text(stats, text.len())?,
            Value::Array(values) => {
                for child in values {
                    pending.push((child, depth + 1));
                }
            }
            Value::Object(values) => {
                for (key, child) in values {
                    add_planning_text(stats, key.len())?;
                    pending.push((child, depth + 1));
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    Ok(())
}

fn add_planning_text(
    stats: &mut PlanningValueStats,
    bytes: usize,
) -> Result<(), ContextBuildError> {
    stats.text_bytes = stats
        .text_bytes
        .checked_add(bytes)
        .filter(|total| *total <= MAX_PLANNING_VALUE_TEXT_BYTES)
        .ok_or(ContextBuildError::InputLimitExceeded)?;
    Ok(())
}

fn bounded_summary(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars().filter(|character| !character.is_control()) {
        if result.len() + character.len_utf8() > MAX_CAPABILITY_SUMMARY_BYTES {
            break;
        }
        result.push(character);
    }
    result
}

struct PreparedPlan {
    proposal_digest: String,
    steps: Vec<PreparedPlanStep>,
}

struct PreparedPlanStep {
    step_id: String,
    objective: String,
    depends_on: Vec<String>,
    capability: CapabilityRef,
    normalized_arguments: Value,
    definition_digest: String,
    action_digest: String,
    material_digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RawPlanProposalSizeInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    context_digest: &'a str,
    steps: &'a [RawPlanProposalSizeStep<'a>],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RawPlanProposalSizeStep<'a> {
    key: &'a str,
    objective: &'a str,
    depends_on: &'a [PlanDependency],
    capability: &'a CapabilityRef,
    arguments: &'a Value,
}

struct ValidatedPlanStep {
    key: String,
    objective: String,
    depends_on: Vec<PlanDependency>,
    capability: CapabilityRef,
    normalized_arguments: Value,
    definition_digest: String,
    action_digest: String,
    material_digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptedPlanDigestStep<'a> {
    proposal_key: &'a str,
    objective: &'a str,
    depends_on: &'a [PlanDependency],
    capability: &'a CapabilityRef,
    definition_digest: &'a str,
    action_digest: &'a str,
    material_digest: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptedPlanDigestInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    context_digest: &'a str,
    steps: &'a [AcceptedPlanDigestStep<'a>],
}

#[allow(clippy::too_many_lines)] // Validation order is a security boundary before materialization.
fn prepare_plan<R: ResourceResolver>(
    state: &RunState,
    frontier: &WorkFrontier,
    context: &PlanningContext,
    registry: &CapabilityRegistry,
    resolver: &R,
    budget: &AgentLoopBudget,
    mut steps: Vec<ProposedPlanStep>,
) -> Result<PreparedPlan, ProposalRejection> {
    if steps.is_empty() {
        return Err(ProposalRejection::EmptyPlan);
    }
    if steps.len() > MAX_ACCEPTED_PLAN_STEPS {
        return Err(ProposalRejection::TooManySteps);
    }
    let resulting_steps = state
        .steps
        .len()
        .checked_add(steps.len())
        .ok_or(ProposalRejection::PlannedStepBudgetExceeded)?;
    if u32::try_from(resulting_steps)
        .ok()
        .is_none_or(|count| count > budget.max_planned_steps)
    {
        return Err(ProposalRejection::PlannedStepBudgetExceeded);
    }
    let raw_size_steps: Vec<_> = steps
        .iter()
        .map(|step| RawPlanProposalSizeStep {
            key: &step.key,
            objective: &step.objective,
            depends_on: &step.depends_on,
            capability: &step.capability,
            arguments: &step.arguments,
        })
        .collect();
    let raw_size_input = RawPlanProposalSizeInput {
        domain: "xgeny.plan-proposal.raw-size/v1",
        run_id: &state.run_id,
        context_digest: context.context_digest(),
        steps: &raw_size_steps,
    };
    let proposal_size =
        canonical_size(&raw_size_input).map_err(|_| ProposalRejection::InvocationInvalid)?;
    if proposal_size > MAX_PROPOSAL_BYTES {
        return Err(ProposalRejection::ProposalTooLarge);
    }
    validate_proposal_structure(state, frontier, context, &steps)?;
    steps.sort_by(|left, right| left.key.cmp(&right.key));
    for step in &mut steps {
        step.depends_on.sort();
    }

    let existing_action_digests: BTreeSet<_> = state
        .steps
        .values()
        .flat_map(|step| {
            step.planned_invocation
                .as_ref()
                .map(|invocation| invocation.action_digest().to_owned())
                .into_iter()
                .chain(
                    step.intent
                        .as_ref()
                        .map(|intent| intent.action_digest.clone()),
                )
        })
        .collect();
    let mut proposed_action_digests = BTreeSet::new();
    let mut validated = Vec::with_capacity(steps.len());
    for step in steps {
        if !context
            .capabilities()
            .iter()
            .any(|summary| summary.capability() == &step.capability)
        {
            return Err(ProposalRejection::CapabilityUnavailable);
        }
        let definition = registry
            .definition(&step.capability)
            .ok_or(ProposalRejection::CapabilityUnavailable)?;
        if !definition
            .spec
            .execution
            .styles
            .contains(&ExecutionStyle::Sync)
            || !definition.spec.effect.critical_actions.is_empty()
        {
            return Err(ProposalRejection::CapabilityUnsupported);
        }
        let preflight_step_id =
            preflight_step_id(&state.run_id, context.context_digest(), &step.key)
                .map_err(|_| ProposalRejection::InvocationInvalid)?;
        let facts = prepare_invocation_facts(
            &state.run_id,
            &preflight_step_id,
            &step.capability,
            &step.arguments,
            registry,
            resolver,
        )
        .map_err(|error| map_invocation_rejection(&error))?;
        if existing_action_digests.contains(&facts.action_digest)
            || !proposed_action_digests.insert(facts.action_digest.clone())
        {
            return Err(ProposalRejection::DuplicateSemanticAction);
        }
        validated.push(ValidatedPlanStep {
            key: step.key,
            objective: step.objective,
            depends_on: step.depends_on,
            capability: step.capability,
            normalized_arguments: facts.normalized_arguments,
            definition_digest: facts.definition_digest,
            action_digest: facts.action_digest,
            material_digest: facts.material_digest,
        });
    }
    let digest_steps: Vec<_> = validated
        .iter()
        .map(|step| AcceptedPlanDigestStep {
            proposal_key: &step.key,
            objective: &step.objective,
            depends_on: &step.depends_on,
            capability: &step.capability,
            definition_digest: &step.definition_digest,
            action_digest: &step.action_digest,
            material_digest: &step.material_digest,
        })
        .collect();
    let proposal_digest = digest_serializable(&AcceptedPlanDigestInput {
        domain: "xgeny.plan-proposal.accepted/v2",
        run_id: &state.run_id,
        context_digest: context.context_digest(),
        steps: &digest_steps,
    })
    .map_err(|_| ProposalRejection::InvocationInvalid)?;
    let step_ids = derive_step_ids(&state.run_id, &proposal_digest, &validated)
        .map_err(|_| ProposalRejection::InvocationInvalid)?;

    let mut prepared = Vec::with_capacity(validated.len());
    for step in validated {
        let step_id = step_ids[&step.key].clone();
        let final_facts = prepare_invocation_facts(
            &state.run_id,
            &step_id,
            &step.capability,
            &step.normalized_arguments,
            registry,
            resolver,
        )
        .map_err(|error| map_invocation_rejection(&error))?;
        if final_facts.normalized_arguments != step.normalized_arguments
            || final_facts.definition_digest != step.definition_digest
            || final_facts.action_digest != step.action_digest
            || final_facts.material_digest != step.material_digest
        {
            return Err(ProposalRejection::InvocationInvalid);
        }
        let mut depends_on = step
            .depends_on
            .into_iter()
            .map(|dependency| match dependency {
                PlanDependency::ExistingStep { step_id } => step_id,
                PlanDependency::ProposedStep { key } => step_ids[&key].clone(),
            })
            .collect::<Vec<_>>();
        depends_on.sort();
        prepared.push(PreparedPlanStep {
            step_id,
            objective: step.objective,
            depends_on,
            capability: step.capability,
            normalized_arguments: final_facts.normalized_arguments,
            definition_digest: final_facts.definition_digest,
            action_digest: final_facts.action_digest,
            material_digest: final_facts.material_digest,
        });
    }
    Ok(PreparedPlan {
        proposal_digest,
        steps: prepared,
    })
}

#[allow(clippy::too_many_lines)] // One pass shares bounded edge and DAG accounting.
fn validate_proposal_structure(
    state: &RunState,
    frontier: &WorkFrontier,
    context: &PlanningContext,
    steps: &[ProposedPlanStep],
) -> Result<(), ProposalRejection> {
    let mut keys = BTreeSet::new();
    for step in steps {
        if !valid_proposal_identifier(&step.key) {
            return Err(ProposalRejection::InvalidStepKey);
        }
        if !keys.insert(step.key.as_str()) {
            return Err(ProposalRejection::DuplicateStepKey);
        }
        if !valid_bounded_text(&step.objective, MAX_ACCEPTED_OBJECTIVE_BYTES) {
            return Err(ProposalRejection::InvalidObjective);
        }
    }
    let mut edge_count = 0_usize;
    let mut indegree: BTreeMap<&str, usize> = keys.iter().copied().map(|key| (key, 0)).collect();
    let mut children: BTreeMap<&str, Vec<&str>> =
        keys.iter().copied().map(|key| (key, Vec::new())).collect();
    for step in steps {
        let mut unique = BTreeSet::new();
        for dependency in &step.depends_on {
            edge_count = edge_count
                .checked_add(1)
                .ok_or(ProposalRejection::TooManyEdges)?;
            if edge_count > MAX_ACCEPTED_PLAN_EDGES {
                return Err(ProposalRejection::TooManyEdges);
            }
            if !unique.insert(dependency) {
                return Err(ProposalRejection::DuplicateDependency);
            }
            match dependency {
                PlanDependency::ExistingStep { step_id } => {
                    if !valid_existing_step_id(step_id) {
                        return Err(ProposalRejection::InvalidDependency);
                    }
                    if !context
                        .steps()
                        .iter()
                        .any(|summary| summary.step_id() == step_id)
                    {
                        return Err(ProposalRejection::UnknownDependency);
                    }
                    let existing = state
                        .steps
                        .get(step_id)
                        .ok_or(ProposalRejection::UnknownDependency)?;
                    if matches!(
                        dependency_release_block_reason(existing),
                        Some(
                            DependencyBlockReason::Failed
                                | DependencyBlockReason::ManualRequired
                                | DependencyBlockReason::ReceiptMissing
                                | DependencyBlockReason::DependencyBlocked
                        )
                    ) {
                        return Err(ProposalRejection::BlockedExistingDependency);
                    }
                    if frontier
                        .blocked
                        .iter()
                        .any(|blocked| blocked.step_id == *step_id)
                    {
                        return Err(ProposalRejection::BlockedExistingDependency);
                    }
                }
                PlanDependency::ProposedStep { key } => {
                    if !valid_proposal_identifier(key) {
                        return Err(ProposalRejection::InvalidDependency);
                    }
                    if key == &step.key {
                        return Err(ProposalRejection::SelfDependency);
                    }
                    if !keys.contains(key.as_str()) {
                        return Err(ProposalRejection::UnknownDependency);
                    }
                    let degree = indegree
                        .get_mut(step.key.as_str())
                        .expect("proposal keys were indexed");
                    *degree = degree
                        .checked_add(1)
                        .ok_or(ProposalRejection::TooManyEdges)?;
                    children
                        .get_mut(key.as_str())
                        .expect("proposal dependency keys were indexed")
                        .push(&step.key);
                }
            }
        }
    }
    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter_map(|(key, degree)| (*degree == 0).then_some(*key))
        .collect();
    let mut visited = 0_usize;
    while let Some(key) = ready.pop_first() {
        visited = visited.saturating_add(1);
        for child in &children[key] {
            let degree = indegree.get_mut(child).expect("proposal child was indexed");
            *degree = degree
                .checked_sub(1)
                .expect("a recorded dependency cannot underflow");
            if *degree == 0 {
                ready.insert(child);
            }
        }
    }
    if visited != steps.len() {
        return Err(ProposalRejection::DependencyCycle);
    }
    Ok(())
}

fn map_invocation_rejection(error: &AdmissionError) -> ProposalRejection {
    match error {
        AdmissionError::DefinitionNotFound { .. } => ProposalRejection::CapabilityUnavailable,
        AdmissionError::UnsupportedEffectClass { .. }
        | AdmissionError::DefinitionDoesNotSupportIdempotencyKey
        | AdmissionError::UnsupportedExecutionStyle => ProposalRejection::CapabilityUnsupported,
        _ => ProposalRejection::InvocationInvalid,
    }
}

fn derive_step_ids(
    run_id: &str,
    proposal_digest: &str,
    steps: &[ValidatedPlanStep],
) -> Result<BTreeMap<String, String>, AgentLoopError> {
    steps
        .iter()
        .map(|step| {
            let digest = digest_serializable(&StepIdDigestInput {
                domain: "xgeny.plan-step-id/v1",
                run_id,
                proposal_digest,
                proposal_key: &step.key,
            })?;
            Ok((step.key.clone(), content_id("step", &digest)))
        })
        .collect()
}

fn preflight_step_id(
    run_id: &str,
    context_digest: &str,
    proposal_key: &str,
) -> Result<String, AgentLoopError> {
    let digest = digest_serializable(&PreflightStepIdDigestInput {
        domain: "xgeny.plan-step-preflight-id/v1",
        run_id,
        context_digest,
        proposal_key,
    })?;
    Ok(content_id("preflight", &digest))
}

fn materialize_plan<M: PlanMaterializer>(
    materializer: &mut M,
    state: &RunState,
    proposal_digest: &str,
    steps: Vec<PreparedPlanStep>,
) -> Result<(Vec<AcceptedPlanStep>, Vec<PlannedInvocationMaterialRecord>), PlanMaterializerFailure>
{
    let mut accepted = Vec::with_capacity(steps.len());
    let mut inputs = Vec::with_capacity(steps.len());
    for step in steps {
        let spec = PlannedInvocationSpec::new(
            &step.capability.capability_id,
            &step.capability.contract_version,
            step.definition_digest.as_str(),
            step.action_digest.as_str(),
            step.material_digest.as_str(),
            PlannedExecutionProfile::LocalSyncOnceV1,
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
        .map_err(|_| PlanMaterializerFailure::Rejected)?;
        let reference = materializer.materialize(PlanMaterializationRequest {
            run_id: &state.run_id,
            step_id: &step.step_id,
            proposal_digest,
            capability: &step.capability,
            normalized_arguments: &step.normalized_arguments,
            material_digest: &step.material_digest,
        })?;
        let (binding, input) = PlannedInvocationMaterialRecord::bind(
            &state.run_id,
            &step.step_id,
            proposal_digest,
            spec,
            reference,
        )
        .map_err(|_| PlanMaterializerFailure::Rejected)?;
        accepted.push(AcceptedPlanStep {
            step_id: step.step_id,
            objective: step.objective,
            depends_on: step.depends_on,
            invocation: binding,
        });
        inputs.push(input);
    }
    Ok((accepted, inputs))
}

fn valid_proposal_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PROPOSAL_KEY_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_existing_step_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn valid_bounded_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn next_turn_index(state: &RunState) -> Result<u32, AgentLoopError> {
    state
        .agent_loop
        .as_ref()
        .ok_or(AgentLoopError::AgentLoopNotConfigured)?
        .accepted_model_turns
        .checked_add(1)
        .ok_or(AgentLoopError::BudgetCounterOverflow)
}

fn create_event<F: EventFactory>(
    events: &mut F,
    state: &RunState,
    body: RunEventBody,
) -> Result<RunEvent, AgentLoopError> {
    let metadata = events.create_metadata(state)?;
    metadata.validate()?;
    Ok(RunEvent {
        event_id: metadata.event_id,
        run_id: state.run_id.clone(),
        authority: state.authority.clone(),
        authority_epoch: state.authority_epoch,
        recorded_at: metadata.recorded_at,
        body,
    })
}

fn canonical_size<T: Serialize + ?Sized>(value: &T) -> Result<usize, AgentLoopError> {
    serde_jcs::to_vec(value)
        .map(|canonical| canonical.len())
        .map_err(|_| AgentLoopError::Canonicalization)
}

fn digest_serializable<T: Serialize + ?Sized>(value: &T) -> Result<String, AgentLoopError> {
    let canonical = serde_jcs::to_vec(value).map_err(|_| AgentLoopError::Canonicalization)?;
    let digest = Sha256::digest(canonical);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(format!("sha256:{encoded}"))
}

fn content_id(prefix: &str, digest: &str) -> String {
    format!(
        "{prefix}-{}",
        digest.strip_prefix("sha256:").unwrap_or(digest)
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StepIdDigestInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    proposal_digest: &'a str,
    proposal_key: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreflightStepIdDigestInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    context_digest: &'a str,
    proposal_key: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionProposalDigestInput<'a> {
    domain: &'static str,
    context_digest: &'a str,
    summary: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionSummaryDigestInput<'a> {
    domain: &'static str,
    summary: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionCandidateIdInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    context_digest: &'a str,
    proposal_digest: &'a str,
}

#[derive(Debug, Error)]
pub enum AgentLoopError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    EventFactory(#[from] EventFactoryError),
    #[error(transparent)]
    EventMetadata(#[from] EventMetadataError),
    #[error(transparent)]
    Frontier(#[from] xgeny_workgraph::FrontierError),
    #[error(transparent)]
    PlanningContract(#[from] PlanningContractError),
    #[error("durable Run is not initialized")]
    RunNotInitialized,
    #[error("Run lease does not match the durable Run")]
    LeaseRunMismatch,
    #[error("AgentLoop durable budget differs from this runtime configuration")]
    BudgetMismatch,
    #[error("durable model-call budget differs from this runtime configuration")]
    ModelCallBudgetMismatch,
    #[error("AgentLoop is not durably configured")]
    AgentLoopNotConfigured,
    #[error("durable model-call lifecycle is not configured")]
    ModelCallLifecycleNotConfigured,
    #[error("no unresolved model call is active")]
    ModelCallNotActive,
    #[error("the requested model call is not the active call")]
    ModelCallIdMismatch,
    #[error("the model call was settled or superseded during concurrent outcome recording")]
    ModelCallNoLongerActive,
    #[error("AgentLoop budget counter overflowed")]
    BudgetCounterOverflow,
    #[error("planner request profile digest is not canonical lowercase SHA-256")]
    InvalidPlannerRequestProfileDigest,
    #[error("planning canonicalization failed")]
    Canonicalization,
    #[error("Capability catalog could not be committed to planning context")]
    CapabilityCatalog,
    #[error("completion candidate event did not project a candidate")]
    CompletionCandidateNotProjected,
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use xgeny_policy::ResourceResolutionFailure;
    use xgeny_workgraph::AgentLoopState;

    use super::*;

    #[derive(Default)]
    struct CountingResolver(Cell<usize>);

    impl ResourceResolver for CountingResolver {
        fn resolve(
            &self,
            _scope: &str,
            resource: &str,
        ) -> Result<String, ResourceResolutionFailure> {
            self.0.set(self.0.get() + 1);
            Ok(resource.to_owned())
        }
    }

    fn test_budget() -> AgentLoopBudget {
        AgentLoopBudget::new(4, 16, 16, 262_144).expect("budget should validate")
    }

    fn step(
        step_id: &str,
        status: StepStatus,
        depends_on: Vec<String>,
    ) -> xgeny_workgraph::StepState {
        let released = status == StepStatus::Completed && step_id == "released";
        xgeny_workgraph::StepState {
            step_id: step_id.to_owned(),
            objective: format!("objective {step_id}"),
            depends_on,
            planned_invocation: None,
            status,
            attempts: 0,
            intent: None,
            effect_evidence_digest: None,
            execution_receipt_id: released.then(|| "receipt-released".to_owned()),
            execution_receipt_digest: released.then(|| format!("sha256:{}", "b".repeat(64))),
            uncertainty_reason: None,
            reconciliation_evidence_digest: None,
        }
    }

    fn state(steps: Vec<xgeny_workgraph::StepState>) -> RunState {
        RunState {
            run_id: "run-terminal-dependency".to_owned(),
            authority: "local:test".to_owned(),
            authority_epoch: 1,
            goal: "continue safely".to_owned(),
            revision: 3,
            journal_sequence: 3,
            journal_head_digest: format!("sha256:{}", "a".repeat(64)),
            steps: steps
                .into_iter()
                .map(|step| (step.step_id.clone(), step))
                .collect(),
            authorization_consumption: BTreeMap::new(),
            agent_loop: Some(AgentLoopState {
                budget: test_budget(),
                accepted_model_turns: 0,
                model_calls: None,
                completion_candidate: None,
            }),
        }
    }

    fn proposal(dependency: &str) -> Vec<ProposedPlanStep> {
        vec![ProposedPlanStep::new(
            "child",
            "child objective",
            vec![PlanDependency::existing(dependency)],
            CapabilityRef {
                capability_id: "xgeny.test/not-needed".to_owned(),
                contract_version: "1.0.0".to_owned(),
            },
            serde_json::json!({}),
        )]
    }

    fn assert_blocked_before_materializer(state: &RunState, dependency: &str) {
        let frontier = derive_frontier(state).expect("frontier should derive");
        let context = build_context(state, &frontier, &CapabilityRegistry::new(), &test_budget())
            .expect("context should fit");
        let resolver = CountingResolver::default();
        let outcome = prepare_plan(
            state,
            &frontier,
            &context,
            &CapabilityRegistry::new(),
            &resolver,
            &test_budget(),
            proposal(dependency),
        );
        assert!(matches!(
            outcome,
            Err(ProposalRejection::BlockedExistingDependency)
        ));
        assert_eq!(resolver.0.get(), 0);
    }

    #[test]
    fn terminal_and_transitively_blocked_existing_dependencies_reject_before_resolution() {
        for status in [
            StepStatus::Failed,
            StepStatus::ManualRequired,
            StepStatus::Completed,
        ] {
            let state = state(vec![step("terminal", status, Vec::new())]);
            assert_blocked_before_materializer(&state, "terminal");
        }

        let state = state(vec![
            step("failed-root", StepStatus::Failed, Vec::new()),
            step(
                "blocked-middle",
                StepStatus::Planned,
                vec!["failed-root".to_owned()],
            ),
        ]);
        assert_blocked_before_materializer(&state, "blocked-middle");
    }

    #[test]
    fn receipt_released_existing_dependency_passes_structure_validation() {
        let state = state(vec![step("released", StepStatus::Completed, Vec::new())]);
        let frontier = derive_frontier(&state).expect("frontier should derive");
        let context = build_context(
            &state,
            &frontier,
            &CapabilityRegistry::new(),
            &test_budget(),
        )
        .expect("context should fit");
        validate_proposal_structure(&state, &frontier, &context, &proposal("released"))
            .expect("receipt-released dependency should remain usable");
    }

    #[test]
    fn context_hard_limit_caps_a_larger_forward_compatible_budget() {
        let state = state(Vec::new());
        let frontier = derive_frontier(&state).expect("frontier should derive");
        let oversized = AgentLoopBudget::new(1, 1, 1, MAX_PLANNING_CONTEXT_BYTES + 1)
            .expect("durable budget itself remains forward-compatible");
        let context = build_context(&state, &frontier, &CapabilityRegistry::new(), &oversized)
            .expect("hard cap should not invalidate an otherwise fitting legacy budget");
        assert!(context.canonical_size_bytes() <= MAX_PLANNING_CONTEXT_BYTES);
    }

    #[test]
    fn deeply_nested_capability_schema_is_rejected_before_clone_and_digest() {
        let document: xgeny_domain::ProtocolDocument = serde_json::from_str(include_str!(
            "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
        ))
        .expect("definition fixture should deserialize");
        let xgeny_domain::ProtocolDocument::CapabilityDefinition(mut definition) = document else {
            panic!("expected CapabilityDefinition fixture")
        };
        let mut schema = serde_json::json!({"type": "string"});
        for _ in 0..=MAX_PLANNING_VALUE_DEPTH {
            schema = serde_json::json!({"nested": schema});
        }
        definition.spec.input_schema = schema;
        let mut registry = CapabilityRegistry::new();
        registry
            .register_schema_validated_definition(*definition)
            .expect("registry trusts the protocol ingress boundary");
        let state = state(Vec::new());
        let frontier = derive_frontier(&state).expect("frontier should derive");
        assert!(matches!(
            build_context(&state, &frontier, &registry, &test_budget()),
            Err(ContextBuildError::InputLimitExceeded)
        ));
    }

    #[test]
    fn oversized_capability_text_is_rejected_before_canonicalization() {
        let document: xgeny_domain::ProtocolDocument = serde_json::from_str(include_str!(
            "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
        ))
        .expect("definition fixture should deserialize");
        let xgeny_domain::ProtocolDocument::CapabilityDefinition(mut definition) = document else {
            panic!("expected CapabilityDefinition fixture")
        };
        definition.spec.summary = "x".repeat(MAX_PLANNING_VALUE_TEXT_BYTES + 1);
        assert!(matches!(
            validate_planning_definition_shape(&definition),
            Err(ContextBuildError::InputLimitExceeded)
        ));
    }

    #[test]
    fn oversized_step_text_is_rejected_before_clone_and_canonicalization() {
        let mut oversized = step("oversized", StepStatus::Planned, Vec::new());
        oversized.objective = "x".repeat(MAX_PLANNING_STEP_BYTES + 1);
        assert!(matches!(
            validate_planning_step_shape(&oversized),
            Err(ContextBuildError::InputLimitExceeded)
        ));
    }

    #[test]
    fn oversized_context_header_text_is_rejected_before_goal_clone() {
        let mut oversized = state(Vec::new());
        oversized.goal = "x".repeat(MAX_PLANNING_HEADER_TEXT_BYTES + 1);
        let frontier = derive_frontier(&oversized).expect("frontier should derive");
        assert!(matches!(
            build_context(
                &oversized,
                &frontier,
                &CapabilityRegistry::new(),
                &test_budget(),
            ),
            Err(ContextBuildError::InputLimitExceeded)
        ));
    }
}
