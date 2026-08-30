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
    MAX_ACCEPTED_PLAN_EDGES, MAX_ACCEPTED_PLAN_STEPS, PlannedExecutionProfile,
    PlannedInvocationMaterialRecord, PlannedInvocationSpec, PlanningContractError,
    ReconstructableMaterialReference, RunEvent, RunEventBody, RunState, StepStatus, WorkFrontier,
    dependency_release_block_reason, derive_frontier, receipt_releases_dependency,
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
}

/// Provider-neutral boundary for one bounded planning decision.
pub trait PlannerPort {
    /// Return one proposal without executing tools or changing the Run store.
    ///
    /// # Errors
    ///
    /// Returns only a fixed failure class. Raw provider response bodies must remain behind this
    /// boundary.
    fn plan(&mut self, context: &PlanningContext) -> Result<PlanProposal, PlannerPortFailure>;
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
    PlannedStepBudgetExhausted,
    ToolCallBudgetExhausted,
    ContextBudgetExceeded,
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
}

/// Single-orchestrator, model-provider-neutral coordinator for a durable Run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLoop {
    budget: AgentLoopBudget,
}

impl AgentLoop {
    #[must_use]
    pub const fn new(budget: AgentLoopBudget) -> Self {
        Self { budget }
    }

    #[must_use]
    pub const fn budget(&self) -> &AgentLoopBudget {
        &self.budget
    }

    /// Select at most one current frontier action, or make at most one planner decision.
    ///
    /// Existing recovery, reconciliation, verification, committed intent, and admission actions
    /// always precede planning. This method returns those actions to the caller and never invokes
    /// an Executor, verifier, policy UI, or admission path automatically.
    ///
    /// A plan result and all of its secret-free material-reference sidecars are committed in one
    /// store transaction. Planner rejection, timeout, and materialization failure leave the Run
    /// journal unchanged.
    ///
    /// `resolver` is a planning-time canonicalizer and must be deterministic and side-effect-free.
    /// Core recomputes its result against final Step IDs and rejects any mismatch before calling
    /// the materializer.
    ///
    /// # Errors
    ///
    /// Returns an error for missing/corrupt state, wrong lease, changed durable configuration,
    /// event creation, canonicalization, or store failure.
    #[allow(clippy::too_many_arguments)]
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
            Err(ContextBuildError::Canonicalization) => {
                return Err(AgentLoopError::Canonicalization);
            }
            Err(ContextBuildError::Catalog) => return Err(AgentLoopError::CapabilityCatalog),
        };
        let proposal = match planner.plan(&context) {
            Ok(proposal) => proposal,
            Err(failure) => {
                return Ok(AgentLoopTick::PlannerUnavailable {
                    failure,
                    head: AgentLoopHead::from_state(&state),
                });
            }
        };
        self.handle_proposal(
            store,
            events,
            capabilities,
            resolver,
            materializer,
            &state,
            &frontier,
            &context,
            proposal,
            tool_calls,
        )
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

    #[allow(clippy::too_many_arguments)]
    fn handle_proposal<S, F, R, M>(
        &self,
        store: &mut S,
        events: &mut F,
        capabilities: &CapabilityRegistry,
        resolver: &R,
        materializer: &mut M,
        state: &RunState,
        frontier: &WorkFrontier,
        context: &PlanningContext,
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
            PlanProposal::CompletionCandidate { summary } => {
                Self::record_completion_candidate(store, events, state, frontier, context, &summary)
            }
            PlanProposal::Plan { steps } => {
                if tool_calls >= self.budget.max_tool_calls {
                    return Ok(AgentLoopTick::ProposalRejected {
                        reason: ProposalRejection::ToolCallBudgetExhausted,
                        head: AgentLoopHead::from_state(state),
                    });
                }
                let prepared = match prepare_plan(
                    state,
                    frontier,
                    context,
                    capabilities,
                    resolver,
                    &self.budget,
                    steps,
                ) {
                    Ok(prepared) => prepared,
                    Err(reason) => {
                        return Ok(AgentLoopTick::ProposalRejected {
                            reason,
                            head: AgentLoopHead::from_state(state),
                        });
                    }
                };
                let (accepted_steps, inputs) = match materialize_plan(
                    materializer,
                    state,
                    &prepared.proposal_digest,
                    prepared.steps,
                ) {
                    Ok(values) => values,
                    Err(failure) => {
                        return Ok(AgentLoopTick::MaterializerUnavailable {
                            failure,
                            head: AgentLoopHead::from_state(state),
                        });
                    }
                };
                let decision = ExpectedPlanningTurn::new(
                    next_turn_index(state)?,
                    context.context_digest(),
                    &prepared.proposal_digest,
                )?;
                let step_ids = accepted_steps
                    .iter()
                    .map(|step| step.step_id.clone())
                    .collect();
                let event = create_event(
                    events,
                    state,
                    RunEventBody::PlanAccepted {
                        decision,
                        steps: accepted_steps,
                    },
                )?;
                let commit = store.append_with_plan_inputs(
                    ExpectedHead::from_state(state),
                    event,
                    inputs,
                )?;
                Ok(AgentLoopTick::PlanAccepted {
                    step_ids,
                    head: AgentLoopHead::from_state(&commit.state),
                })
            }
        }
    }

    fn record_completion_candidate<S, F>(
        store: &mut S,
        events: &mut F,
        state: &RunState,
        frontier: &WorkFrontier,
        context: &PlanningContext,
        summary: &str,
    ) -> Result<AgentLoopTick, AgentLoopError>
    where
        S: RunStore,
        F: EventFactory,
    {
        if !frontier.all_steps_receipt_completed() {
            return Ok(AgentLoopTick::ProposalRejected {
                reason: ProposalRejection::CompletionWithoutReceiptCompletedPlan,
                head: AgentLoopHead::from_state(state),
            });
        }
        if !valid_bounded_text(summary, MAX_COMPLETION_SUMMARY_BYTES) {
            return Ok(AgentLoopTick::ProposalRejected {
                reason: ProposalRejection::InvalidCompletionSummary,
                head: AgentLoopHead::from_state(state),
            });
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
            return Ok(AgentLoopTick::ProposalRejected {
                reason: ProposalRejection::ProposalTooLarge,
                head: AgentLoopHead::from_state(state),
            });
        }
        let summary_digest = digest_serializable(&CompletionSummaryDigestInput {
            domain: "xgeny.completion-summary/v1",
            summary,
        })?;
        let candidate_id = content_id(
            "completion",
            &digest_serializable(&CompletionCandidateIdInput {
                domain: "xgeny.completion-candidate-id/v1",
                run_id: &state.run_id,
                context_digest: context.context_digest(),
                proposal_digest: &proposal_digest,
            })?,
        );
        let decision = ExpectedPlanningTurn::new(
            next_turn_index(state)?,
            context.context_digest(),
            &proposal_digest,
        )?;
        let event = create_event(
            events,
            state,
            RunEventBody::CompletionCandidateRecorded {
                decision,
                candidate_id: candidate_id.clone(),
                summary_digest: summary_digest.clone(),
            },
        )?;
        let commit = store.append(ExpectedHead::from_state(state), event)?;
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

#[derive(Debug)]
enum ContextBuildError {
    BudgetExceeded,
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
    let mut summaries = Vec::new();
    for definition in registry.definitions() {
        let definition_digest =
            definition_contract_digest(definition).map_err(|_| ContextBuildError::Catalog)?;
        summaries.push(PlanningCapabilitySummary {
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
        });
    }
    summaries.sort_by(|left, right| {
        (
            left.capability.capability_id.as_str(),
            left.capability.contract_version.as_str(),
        )
            .cmp(&(
                right.capability.capability_id.as_str(),
                right.capability.contract_version.as_str(),
            ))
    });
    let catalog_entries: Vec<_> = summaries
        .iter()
        .map(|summary| CatalogDigestEntry {
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
    let step_summaries: Vec<_> = state
        .steps
        .values()
        .map(|step| PlanningStepSummary {
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
        })
        .collect();
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
    let maximum_size = usize::try_from(budget.max_context_bytes).unwrap_or(usize::MAX);
    if canonical_size(&payload).map_err(|_| ContextBuildError::Canonicalization)? > maximum_size {
        return Err(ContextBuildError::BudgetExceeded);
    }

    // Deterministic round-robin packing prevents either a large WorkGraph or a large catalog from
    // monopolizing the bounded context. Schemas and Step summaries are included as whole items;
    // an item that does not fit is omitted rather than truncated into an ambiguous contract.
    for index in 0..step_summaries.len().max(summaries.len()) {
        if let Some(step) = step_summaries.get(index) {
            payload.steps.push(step.clone());
            payload.omitted_steps = total_steps - payload.steps.len();
            let size = canonical_size(&payload).map_err(|_| ContextBuildError::Canonicalization)?;
            if size > maximum_size {
                payload.steps.pop();
                payload.omitted_steps = total_steps - payload.steps.len();
            }
        }
        if let Some(summary) = summaries.get(index) {
            payload.capabilities.push(summary.clone());
            payload.omitted_capabilities = total_capabilities - payload.capabilities.len();
            let size = canonical_size(&payload).map_err(|_| ContextBuildError::Canonicalization)?;
            if size > maximum_size {
                payload.capabilities.pop();
                payload.omitted_capabilities = total_capabilities - payload.capabilities.len();
            }
        }
    }
    let canonical_size_bytes =
        u64::try_from(canonical_size(&payload).map_err(|_| ContextBuildError::Canonicalization)?)
            .map_err(|_| ContextBuildError::BudgetExceeded)?;
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
    #[error("AgentLoop is not durably configured")]
    AgentLoopNotConfigured,
    #[error("AgentLoop budget counter overflowed")]
    BudgetCounterOverflow,
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

    struct NoAppendStore;

    impl RunStore for NoAppendStore {
        fn append(
            &mut self,
            _expected: ExpectedHead,
            _event: RunEvent,
        ) -> Result<xgeny_local_store::Commit, StoreError> {
            panic!("blocked proposal must not append")
        }

        fn load(&self) -> Result<Option<xgeny_local_store::RunSnapshot>, StoreError> {
            Ok(None)
        }
    }

    struct NoEvents;

    impl EventFactory for NoEvents {
        fn create_metadata(
            &mut self,
            _state: &RunState,
        ) -> Result<crate::EventMetadata, EventFactoryError> {
            panic!("blocked proposal must not create an event")
        }
    }

    #[derive(Default)]
    struct CountingMaterializer(Cell<usize>);

    impl PlanMaterializer for CountingMaterializer {
        fn materialize(
            &mut self,
            _request: PlanMaterializationRequest<'_>,
        ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
            self.0.set(self.0.get() + 1);
            Err(PlanMaterializerFailure::Rejected)
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
        let mut materializer = CountingMaterializer::default();
        let outcome = AgentLoop::new(test_budget())
            .handle_proposal(
                &mut NoAppendStore,
                &mut NoEvents,
                &CapabilityRegistry::new(),
                &resolver,
                &mut materializer,
                state,
                &frontier,
                &context,
                PlanProposal::plan(proposal(dependency)),
                0,
            )
            .expect("blocked proposal should be classified");
        assert!(matches!(
            outcome,
            AgentLoopTick::ProposalRejected {
                reason: ProposalRejection::BlockedExistingDependency,
                ..
            }
        ));
        assert_eq!(resolver.0.get(), 0);
        assert_eq!(materializer.0.get(), 0);
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
}
