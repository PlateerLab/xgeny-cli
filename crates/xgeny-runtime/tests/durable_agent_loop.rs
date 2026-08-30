use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use serde_json::{Value, json};
use tempfile::tempdir;
use xgeny_domain::{
    CapabilityDefinitionBody, CapabilityRef, CriticalAction, EffectClass, ExecutionStyle,
    ProtocolDocument,
};
use xgeny_local_store::{
    Commit, ExpectedHead, MemoryRunStore, RunSnapshot, RunStore, SqliteRunStore, StoreError,
};
use xgeny_policy::{ResourceResolutionFailure, ResourceResolver};
use xgeny_runtime::{
    AgentLoop, AgentLoopError, AgentLoopQuiescence, AgentLoopTick, EventFactory, EventFactoryError,
    EventMetadata, PlanDependency, PlanMaterializationRequest, PlanMaterializer,
    PlanMaterializerFailure, PlanProposal, PlannerCallRequest, PlannerPort, PlannerPortFailure,
    ProposalRejection, ProposedPlanStep, RunLease,
};
use xgeny_workgraph::{
    AgentLoopBudget, AgentLoopState, ContinuationAction, EventRecord, ModelCallBudget,
    ModelCallLifecycleState, ModelCallRejectionReason, ModelCallSettlement, ModelCallUnknownReason,
    PlannedExecutionProfile, ReconstructableMaterialReference, RunEvent, RunEventBody, RunState,
    StepState, StepStatus, apply_record,
};

const AUTHORITY: &str = "local:test";
const AUTHORITY_EPOCH: u64 = 7;
const RAW_ALIAS: &str = "/workspace/area/../output.txt";
const CANONICAL_PATH: &str = "/workspace/output.txt";
const RAW_SENTINEL: &str = "RAW-PLANNER-ARGUMENT-MUST-NOT-BE-JOURNALED";
const PLANNER_ID: &str = "xgeny.test.scripted-planner";
const REQUEST_PROFILE_DIGEST: &str =
    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const RAW_ACK_LOSS: &str = "RAW-STORE-ACK-LOSS-MUST-NOT-BE-JOURNALED";

#[derive(Debug)]
struct FixedLease(String);

impl RunLease for FixedLease {
    fn run_id(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Default)]
struct DeterministicEvents;

impl EventFactory for DeterministicEvents {
    fn create_metadata(&mut self, state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        let sequence = state
            .journal_sequence
            .checked_add(1)
            .ok_or_else(|| EventFactoryError::new("sequence overflow"))?;
        Ok(EventMetadata {
            event_id: format!("agent-loop-event-{sequence}"),
            recorded_at: "2026-08-30T00:00:00Z".to_owned(),
        })
    }
}

#[derive(Default)]
struct ScriptedPlanner {
    planner_id_override: Option<String>,
    profile_digest_override: Option<String>,
    responses: VecDeque<Result<PlanProposal, PlannerPortFailure>>,
    contexts: Vec<xgeny_runtime::PlanningContext>,
    call_ids: Vec<String>,
    request_digests: Vec<String>,
    request_debugs: Vec<String>,
    reservation_observed: Option<Rc<Cell<bool>>>,
    calls: usize,
}

impl ScriptedPlanner {
    fn returning(proposal: PlanProposal) -> Self {
        Self {
            responses: VecDeque::from([Ok(proposal)]),
            ..Self::default()
        }
    }

    fn failing(failure: PlannerPortFailure) -> Self {
        Self {
            responses: VecDeque::from([Err(failure)]),
            ..Self::default()
        }
    }
}

impl PlannerPort for ScriptedPlanner {
    fn planner_id(&self) -> &str {
        self.planner_id_override.as_deref().unwrap_or(PLANNER_ID)
    }

    fn request_profile_digest(&self) -> &str {
        self.profile_digest_override
            .as_deref()
            .unwrap_or(REQUEST_PROFILE_DIGEST)
    }

    fn plan(
        &mut self,
        request: &PlannerCallRequest<'_>,
    ) -> Result<PlanProposal, PlannerPortFailure> {
        self.calls += 1;
        if let Some(observed) = &self.reservation_observed {
            assert!(
                observed.get(),
                "the durable reservation must commit before PlannerPort is invoked"
            );
        }
        self.contexts.push(request.context().clone());
        self.call_ids.push(request.call_id().to_owned());
        self.request_digests
            .push(request.request_digest().to_owned());
        self.request_debugs.push(format!("{request:?}"));
        self.responses
            .pop_front()
            .expect("planner call must be explicitly scripted")
    }
}

struct CapturedMaterial {
    step_id: String,
    normalized_arguments: Value,
    material_digest: String,
}

#[derive(Default)]
struct RecordingMaterializer {
    calls: usize,
    captured: Vec<CapturedMaterial>,
    failure: Option<PlanMaterializerFailure>,
}

impl PlanMaterializer for RecordingMaterializer {
    fn materialize(
        &mut self,
        request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        self.calls += 1;
        self.captured.push(CapturedMaterial {
            step_id: request.step_id().to_owned(),
            normalized_arguments: request.normalized_arguments().clone(),
            material_digest: request.material_digest().to_owned(),
        });
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        ReconstructableMaterialReference::new(
            "test-recipes",
            format!("recipe-{}", self.calls),
            "rev-1",
        )
        .map_err(|_| PlanMaterializerFailure::Rejected)
    }
}

#[derive(Debug, Default)]
struct CanonicalResolver {
    calls: Cell<usize>,
}

impl ResourceResolver for CanonicalResolver {
    fn resolve(&self, _scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        self.calls.set(self.calls.get() + 1);
        match resource {
            RAW_ALIAS | CANONICAL_PATH => Ok(CANONICAL_PATH.to_owned()),
            other => Ok(other.to_owned()),
        }
    }
}

#[derive(Debug, Default)]
struct AlternatingResolver {
    calls: Cell<usize>,
}

impl ResourceResolver for AlternatingResolver {
    fn resolve(&self, _scope: &str, _resource: &str) -> Result<String, ResourceResolutionFailure> {
        let next = self.calls.get() + 1;
        self.calls.set(next);
        Ok(if next % 2 == 1 {
            "/workspace/first.txt"
        } else {
            "/workspace/second.txt"
        }
        .to_owned())
    }
}

struct ProjectionStore {
    state: RunState,
    last_body: Option<RunEventBody>,
}

impl ProjectionStore {
    fn new(state: RunState) -> Self {
        Self {
            state,
            last_body: None,
        }
    }
}

impl RunStore for ProjectionStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        let actual = ExpectedHead::from_state(&self.state);
        if expected != actual {
            return Err(StoreError::HeadConflict { expected, actual });
        }
        let previous = EventRecord {
            sequence: self.state.journal_sequence,
            previous_digest: None,
            event: seed_event(
                "projection-placeholder",
                &self.state.run_id,
                RunEventBody::RunCreated {
                    goal: self.state.goal.clone(),
                },
            ),
            digest: self.state.journal_head_digest.clone(),
        };
        let body = event.body.clone();
        let record = EventRecord::next(Some(&previous), event)?;
        let state = apply_record(Some(&self.state), &record)?;
        self.state = state.clone();
        self.last_body = Some(body);
        Ok(Commit { record, state })
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        Ok(Some(RunSnapshot {
            records: Vec::new(),
            state: self.state.clone(),
        }))
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        Ok(Some(self.state.clone()))
    }
}

struct ReservationObservingStore {
    inner: MemoryRunStore,
    reservation_committed: Rc<Cell<bool>>,
}

impl ReservationObservingStore {
    fn new(reservation_committed: Rc<Cell<bool>>) -> Self {
        Self {
            inner: MemoryRunStore::new(),
            reservation_committed,
        }
    }
}

impl RunStore for ReservationObservingStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        let is_reservation = matches!(&event.body, RunEventBody::ModelCallReserved { .. });
        let commit = self.inner.append(expected, event)?;
        if is_reservation {
            self.reservation_committed.set(true);
        }
        Ok(commit)
    }

    fn append_with_plan_inputs(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: Vec<xgeny_workgraph::PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        self.inner.append_with_plan_inputs(expected, event, inputs)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        self.inner.load()
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        self.inner.load_current()
    }

    fn load_planned_invocation(
        &self,
        step_id: &str,
    ) -> Result<Option<xgeny_workgraph::PlannedInvocationMaterialRecord>, StoreError> {
        self.inner.load_planned_invocation(step_id)
    }
}

struct StalePlanStore {
    inner: MemoryRunStore,
    injected: bool,
}

impl StalePlanStore {
    fn new() -> Self {
        Self {
            inner: MemoryRunStore::new(),
            injected: false,
        }
    }
}

impl RunStore for StalePlanStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        self.inner.append(expected, event)
    }

    fn append_with_plan_inputs(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: Vec<xgeny_workgraph::PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        if !self.injected {
            let current = self
                .inner
                .load_current()?
                .expect("reservation should initialize the Run");
            self.inner.append(
                ExpectedHead::from_state(&current),
                seed_event(
                    "stale-head-race",
                    &current.run_id,
                    RunEventBody::StepPlanned {
                        step_id: "step-concurrent".to_owned(),
                        objective: "concurrent trusted state change".to_owned(),
                        depends_on: Vec::new(),
                    },
                ),
            )?;
            self.injected = true;
        }
        self.inner.append_with_plan_inputs(expected, event, inputs)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        self.inner.load()
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        self.inner.load_current()
    }

    fn load_planned_invocation(
        &self,
        step_id: &str,
    ) -> Result<Option<xgeny_workgraph::PlannedInvocationMaterialRecord>, StoreError> {
        self.inner.load_planned_invocation(step_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeConflictTarget {
    Unknown,
    Settled,
}

struct OutcomeConflictStore {
    inner: MemoryRunStore,
    target: OutcomeConflictTarget,
    injected: bool,
}

impl OutcomeConflictStore {
    fn new(target: OutcomeConflictTarget) -> Self {
        Self {
            inner: MemoryRunStore::new(),
            target,
            injected: false,
        }
    }
}

impl RunStore for OutcomeConflictStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        let targets_outcome = matches!(
            (self.target, &event.body),
            (
                OutcomeConflictTarget::Unknown,
                RunEventBody::ModelCallBecameUnknown { .. }
            ) | (
                OutcomeConflictTarget::Settled,
                RunEventBody::ModelCallSettled { .. }
            )
        );
        if targets_outcome && !self.injected {
            let current = self
                .inner
                .load_current()?
                .expect("reservation should initialize the Run");
            self.inner.append(
                ExpectedHead::from_state(&current),
                seed_event(
                    "outcome-head-race",
                    &current.run_id,
                    RunEventBody::StepPlanned {
                        step_id: "step-concurrent-outcome".to_owned(),
                        objective: "concurrent trusted state change".to_owned(),
                        depends_on: Vec::new(),
                    },
                ),
            )?;
            self.injected = true;
        }
        self.inner.append(expected, event)
    }

    fn append_with_plan_inputs(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: Vec<xgeny_workgraph::PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        self.inner.append_with_plan_inputs(expected, event, inputs)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        self.inner.load()
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        self.inner.load_current()
    }

    fn load_planned_invocation(
        &self,
        step_id: &str,
    ) -> Result<Option<xgeny_workgraph::PlannedInvocationMaterialRecord>, StoreError> {
        self.inner.load_planned_invocation(step_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConcurrentCallOutcome {
    Unknown,
    Settled,
}

struct StalePlanOutcomeStore {
    inner: MemoryRunStore,
    outcome: ConcurrentCallOutcome,
    injected: bool,
}

impl StalePlanOutcomeStore {
    fn new(outcome: ConcurrentCallOutcome) -> Self {
        Self {
            inner: MemoryRunStore::new(),
            outcome,
            injected: false,
        }
    }
}

impl RunStore for StalePlanOutcomeStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        self.inner.append(expected, event)
    }

    fn append_with_plan_inputs(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: Vec<xgeny_workgraph::PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        let call_id = match &event.body {
            RunEventBody::PlanAccepted { decision, .. } => {
                decision.model_call_id().map(str::to_owned)
            }
            _ => None,
        };
        if !self.injected
            && let Some(call_id) = call_id
        {
            let current = self
                .inner
                .load_current()?
                .expect("reservation should initialize the Run");
            let body = match self.outcome {
                ConcurrentCallOutcome::Unknown => RunEventBody::ModelCallBecameUnknown {
                    call_id,
                    reason: ModelCallUnknownReason::Interrupted,
                },
                ConcurrentCallOutcome::Settled => RunEventBody::ModelCallSettled {
                    call_id,
                    settlement: ModelCallSettlement::Rejected {
                        reason: ModelCallRejectionReason::PlannerInvalidResponse,
                    },
                },
            };
            self.inner.append(
                ExpectedHead::from_state(&current),
                seed_event("concurrent-model-call-outcome", &current.run_id, body),
            )?;
            self.injected = true;
        }
        self.inner.append_with_plan_inputs(expected, event, inputs)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        self.inner.load()
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        self.inner.load_current()
    }

    fn load_planned_invocation(
        &self,
        step_id: &str,
    ) -> Result<Option<xgeny_workgraph::PlannedInvocationMaterialRecord>, StoreError> {
        self.inner.load_planned_invocation(step_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckLossStage {
    Reservation,
    AcceptedPlan,
}

struct CommitAckLossStore<S> {
    inner: S,
    stage: AckLossStage,
    injected: bool,
}

impl<S> CommitAckLossStore<S> {
    fn new(inner: S, stage: AckLossStage) -> Self {
        Self {
            inner,
            stage,
            injected: false,
        }
    }
}

impl<S: RunStore> RunStore for CommitAckLossStore<S> {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        let lose_ack = self.stage == AckLossStage::Reservation
            && !self.injected
            && matches!(&event.body, RunEventBody::ModelCallReserved { .. });
        let commit = self.inner.append(expected, event)?;
        if lose_ack {
            self.injected = true;
            return Err(StoreError::Corrupt(RAW_ACK_LOSS.to_owned()));
        }
        Ok(commit)
    }

    fn append_with_plan_inputs(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: Vec<xgeny_workgraph::PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        let lose_ack = self.stage == AckLossStage::AcceptedPlan
            && !self.injected
            && matches!(&event.body, RunEventBody::PlanAccepted { .. });
        let commit = self
            .inner
            .append_with_plan_inputs(expected, event, inputs)?;
        if lose_ack {
            self.injected = true;
            return Err(StoreError::Corrupt(RAW_ACK_LOSS.to_owned()));
        }
        Ok(commit)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        self.inner.load()
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        self.inner.load_current()
    }

    fn load_planned_invocation(
        &self,
        step_id: &str,
    ) -> Result<Option<xgeny_workgraph::PlannedInvocationMaterialRecord>, StoreError> {
        self.inner.load_planned_invocation(step_id)
    }
}

struct FailingReservationStore(MemoryRunStore);

impl FailingReservationStore {
    fn new() -> Self {
        Self(MemoryRunStore::new())
    }
}

impl RunStore for FailingReservationStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        if matches!(&event.body, RunEventBody::ModelCallReserved { .. }) {
            return Err(StoreError::Corrupt(
                "injected reservation append failure".to_owned(),
            ));
        }
        self.0.append(expected, event)
    }

    fn append_with_plan_inputs(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: Vec<xgeny_workgraph::PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        self.0.append_with_plan_inputs(expected, event, inputs)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        self.0.load()
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        self.0.load_current()
    }
}

fn budget(
    model_turns: u32,
    planned_steps: u32,
    tool_calls: u32,
    context_bytes: u64,
) -> AgentLoopBudget {
    AgentLoopBudget::new(model_turns, planned_steps, tool_calls, context_bytes)
        .expect("test budget should validate")
}

fn seed_event(event_id: &str, run_id: &str, body: RunEventBody) -> RunEvent {
    RunEvent {
        event_id: event_id.to_owned(),
        run_id: run_id.to_owned(),
        authority: AUTHORITY.to_owned(),
        authority_epoch: AUTHORITY_EPOCH,
        recorded_at: "2026-08-30T00:00:00Z".to_owned(),
        body,
    }
}

fn create_run<S: RunStore>(store: &mut S, run_id: &str) -> RunState {
    create_run_with_goal(store, run_id, "complete a durable two-step local task")
}

fn create_run_with_goal<S: RunStore>(store: &mut S, run_id: &str, goal: &str) -> RunState {
    store
        .append(
            ExpectedHead::Empty,
            seed_event(
                "seed-run-created",
                run_id,
                RunEventBody::RunCreated {
                    goal: goal.to_owned(),
                },
            ),
        )
        .expect("Run should initialize")
        .state
}

fn configure_instance<S: RunStore>(
    store: &mut S,
    run_id: &str,
    agent_loop: AgentLoop,
) -> AgentLoop {
    let mut events = DeterministicEvents;
    let mut planner = ScriptedPlanner::default();
    let mut materializer = RecordingMaterializer::default();
    let tick = agent_loop
        .tick(
            store,
            &mut events,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("AgentLoop should configure");
    assert!(matches!(tick, AgentLoopTick::Configured { .. }));
    assert_eq!(planner.calls, 0);
    let tick = agent_loop
        .tick(
            store,
            &mut events,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("model-call lifecycle should configure");
    assert!(matches!(
        tick,
        AgentLoopTick::ModelCallLifecycleConfigured { .. }
    ));
    assert_eq!(planner.calls, 0);
    agent_loop
}

fn configure<S: RunStore>(store: &mut S, run_id: &str, loop_budget: &AgentLoopBudget) -> AgentLoop {
    configure_instance(store, run_id, AgentLoop::new(loop_budget.clone()))
}

fn definition_fixture(id: &str) -> CapabilityDefinitionBody {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
    ))
    .expect("definition fixture should deserialize");
    let ProtocolDocument::CapabilityDefinition(mut definition) = document else {
        panic!("expected CapabilityDefinition fixture")
    };
    id.clone_into(&mut definition.metadata.id);
    "Write marker".clone_into(&mut definition.metadata.display_name);
    definition.spec.input_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["path", "marker"],
        "properties": {
            "path": {"type": "string", "minLength": 1},
            "marker": {
                "type": "string",
                "allOf": [{"minLength": 1}, {"maxLength": 256}]
            }
        },
        "additionalProperties": false
    });
    definition.spec.output_schema = json!({
        "type": "object",
        "required": ["digest"],
        "properties": {
            "digest": {"type": "string", "pattern": "^sha256:[a-f0-9]{64}$"}
        },
        "additionalProperties": false
    });
    definition.spec.effect.class = EffectClass::Idempotent;
    "filesystem.write".clone_into(&mut definition.spec.effect.resource_selectors[0].scope);
    definition.spec.effect.critical_actions.clear();
    definition.spec.execution.styles = vec![ExecutionStyle::Sync];
    definition.spec.execution.idempotency_key_supported = true;
    *definition
}

fn read_only_definition_fixture(id: &str) -> CapabilityDefinitionBody {
    let mut definition = definition_fixture(id);
    definition.spec.effect.class = EffectClass::ReadOnly;
    "filesystem.read".clone_into(&mut definition.spec.effect.resource_selectors[0].scope);
    definition.spec.execution.idempotency_key_supported = false;
    definition
}

fn registry_with(definition: CapabilityDefinitionBody) -> xgeny_runtime::CapabilityRegistry {
    let mut registry = xgeny_runtime::CapabilityRegistry::new();
    registry
        .register_schema_validated_definition(definition)
        .expect("definition should register");
    registry
}

fn capability(definition: &CapabilityDefinitionBody) -> CapabilityRef {
    CapabilityRef {
        capability_id: definition.metadata.id.clone(),
        contract_version: definition.metadata.contract_version.clone(),
    }
}

fn proposed_step(
    key: &str,
    depends_on: Vec<PlanDependency>,
    capability: CapabilityRef,
    path: &str,
    marker: &str,
) -> ProposedPlanStep {
    ProposedPlanStep::new(
        key,
        format!("perform {key}"),
        depends_on,
        capability,
        json!({"path": path, "marker": marker}),
    )
}

fn manual_step(step_id: &str, status: StepStatus, attempts: u32) -> StepState {
    let completed = status == StepStatus::Completed;
    StepState {
        step_id: step_id.to_owned(),
        objective: format!("continue {step_id}"),
        depends_on: Vec::new(),
        planned_invocation: None,
        status,
        attempts,
        intent: None,
        effect_evidence_digest: None,
        output_record_digest: None,
        execution_receipt_id: completed.then(|| format!("receipt-{step_id}")),
        execution_receipt_digest: completed.then(|| format!("sha256:{}", "c".repeat(64))),
        uncertainty_reason: None,
        reconciliation_evidence_digest: None,
    }
}

fn projected_state(
    run_id: &str,
    loop_budget: AgentLoopBudget,
    accepted_model_turns: u32,
    steps: Vec<StepState>,
) -> RunState {
    let model_call_budget =
        ModelCallBudget::new(loop_budget.max_model_turns).expect("call budget should validate");
    RunState {
        run_id: run_id.to_owned(),
        authority: AUTHORITY.to_owned(),
        authority_epoch: AUTHORITY_EPOCH,
        goal: "continue from durable state".to_owned(),
        revision: 11,
        journal_sequence: 11,
        journal_head_digest: format!("sha256:{}", "a".repeat(64)),
        steps: steps
            .into_iter()
            .map(|step| (step.step_id.clone(), step))
            .collect::<BTreeMap<_, _>>(),
        authorization_consumption: BTreeMap::new(),
        agent_loop: Some(AgentLoopState {
            budget: loop_budget,
            accepted_model_turns,
            model_calls: Some(ModelCallLifecycleState {
                budget: model_call_budget,
                reserved_calls: accepted_model_turns,
                settled_calls: accepted_model_turns,
                unknown_calls: 0,
                active_call: None,
            }),
            completion_candidate: None,
        }),
    }
}

fn assert_workgraph_unchanged(before: &RunState, after: &RunState) {
    assert_eq!(after.run_id, before.run_id);
    assert_eq!(after.authority, before.authority);
    assert_eq!(after.authority_epoch, before.authority_epoch);
    assert_eq!(after.goal, before.goal);
    assert_eq!(after.steps, before.steps);
    assert_eq!(
        after.authorization_consumption,
        before.authorization_consumption
    );
    assert_eq!(
        after
            .agent_loop
            .as_ref()
            .map(|state| state.accepted_model_turns),
        before
            .agent_loop
            .as_ref()
            .map(|state| state.accepted_model_turns)
    );
    assert_eq!(
        after
            .agent_loop
            .as_ref()
            .and_then(|state| state.completion_candidate.as_ref()),
        before
            .agent_loop
            .as_ref()
            .and_then(|state| state.completion_candidate.as_ref())
    );
}

fn assert_model_call_counters(
    state: &RunState,
    reserved: u32,
    settled: u32,
    unknown: u32,
    accepted: u32,
) {
    let loop_state = state
        .agent_loop
        .as_ref()
        .expect("loop should be configured");
    let calls = loop_state
        .model_calls
        .as_ref()
        .expect("model-call lifecycle should be configured");
    assert_eq!(calls.reserved_calls, reserved);
    assert_eq!(calls.settled_calls, settled);
    assert_eq!(calls.unknown_calls, unknown);
    assert_eq!(loop_state.accepted_model_turns, accepted);
}

fn accepted_proposal_digest(state: &RunState) -> String {
    let snapshot = serde_json::to_value(state).expect("state should serialize");
    snapshot["steps"]
        .as_object()
        .and_then(|steps| steps.values().next())
        .and_then(|step| step["plannedInvocation"]["proposalDigest"].as_str())
        .expect("accepted Step should expose proposal digest")
        .to_owned()
}

fn capture_empty_context(
    run_id: &str,
    registry: &xgeny_runtime::CapabilityRegistry,
    max_context_bytes: u64,
) -> xgeny_runtime::PlanningContext {
    let loop_budget = budget(2, 4, 4, max_context_bytes);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let mut planner = ScriptedPlanner::returning(PlanProposal::completion_candidate(
        "empty Run is not complete",
    ));
    let result = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("context should be offered to planner");
    assert!(matches!(
        result,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::CompletionWithoutReceiptCompletedPlan,
            ..
        }
    ));
    planner
        .contexts
        .pop()
        .expect("planner should capture context")
}

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end assertion spans event and sidecar atomicity.
fn memory_plan_is_atomic_redacted_and_frontier_first() {
    let run_id = "run-agent-loop-memory";
    let definition = definition_fixture("xgeny.test/write-marker");
    let capability = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(4, 8, 8, 262_144);
    let reservation_committed = Rc::new(Cell::new(false));
    let mut store = ReservationObservingStore::new(Rc::clone(&reservation_committed));
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store
        .load_current()
        .expect("state should load")
        .expect("Run should exist");

    let proposal = PlanProposal::plan(vec![
        proposed_step(
            "b",
            vec![PlanDependency::proposed("a")],
            capability.clone(),
            CANONICAL_PATH,
            "second",
        ),
        proposed_step("a", Vec::new(), capability, RAW_ALIAS, RAW_SENTINEL),
    ]);
    assert!(!format!("{proposal:?}").contains(RAW_SENTINEL));
    let mut planner = ScriptedPlanner::returning(proposal);
    planner.reservation_observed = Some(reservation_committed);
    let mut materializer = RecordingMaterializer::default();
    let mut events = DeterministicEvents;
    let tick = agent_loop
        .tick(
            &mut store,
            &mut events,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("valid plan should commit");
    let AgentLoopTick::PlanAccepted { step_ids, head } = tick else {
        panic!("expected accepted plan")
    };
    assert_eq!(step_ids.len(), 2);
    assert_eq!(head.sequence, before.journal_sequence + 2);
    assert_eq!(planner.calls, 1);
    assert_eq!(planner.call_ids.len(), 1);
    assert!(planner.call_ids[0].starts_with("model-call-"));
    assert!(planner.request_digests[0].starts_with("sha256:"));
    assert!(!planner.request_debugs[0].contains(RAW_SENTINEL));
    assert!(!planner.request_debugs[0].contains(CANONICAL_PATH));
    assert_eq!(materializer.calls, 2);
    assert_eq!(materializer.captured[0].step_id, step_ids[0]);
    assert_eq!(
        materializer.captured[0].normalized_arguments["path"],
        CANONICAL_PATH
    );
    assert!(
        materializer.captured[0]
            .material_digest
            .starts_with("sha256:")
    );
    for step_id in &step_ids {
        assert!(
            store
                .load_planned_invocation(step_id)
                .expect("sidecar lookup should succeed")
                .is_some(),
            "every accepted Step must have its atomic sidecar"
        );
    }
    let snapshot = store
        .load()
        .expect("store should audit")
        .expect("Run should exist");
    assert_eq!(snapshot.records.len(), 5);
    assert!(matches!(
        snapshot
            .records
            .get(snapshot.records.len() - 2)
            .map(|record| &record.event.body),
        Some(RunEventBody::ModelCallReserved { reservation })
            if reservation.call_id() == planner.call_ids[0]
    ));
    assert!(matches!(
        snapshot.records.last().map(|record| &record.event.body),
        Some(RunEventBody::PlanAccepted { steps, .. }) if steps.len() == 2
    ));
    let journal = store.export_jsonl().expect("journal should export");
    assert!(
        !journal
            .windows(RAW_SENTINEL.len())
            .any(|window| window == RAW_SENTINEL.as_bytes())
    );
    assert!(
        !serde_json::to_string(&snapshot.state)
            .expect("state should serialize")
            .contains(RAW_SENTINEL)
    );
    assert_model_call_counters(&snapshot.state, 1, 1, 0, 1);

    let mut should_not_plan = ScriptedPlanner::default();
    let next = agent_loop
        .tick(
            &mut store,
            &mut events,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut should_not_plan,
            &mut materializer,
        )
        .expect("frontier selection should succeed");
    assert!(matches!(
        next,
        AgentLoopTick::ActionRequired {
            action,
            ..
        } if action.step_id == step_ids[0] && action.action == ContinuationAction::Admit
    ));
    assert_eq!(should_not_plan.calls, 0, "frontier must preempt planning");
}

#[test]
#[allow(clippy::too_many_lines)] // Shares one WorkGraph across three durable call outcomes.
fn invalid_timeout_and_materializer_failure_settle_without_mutating_workgraph() {
    let run_id = "run-agent-loop-failures";
    let definition = definition_fixture("xgeny.test/failure-marker");
    let capability = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(4, 8, 8, 262_144);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().expect("state should load").unwrap();
    let mut events = DeterministicEvents;

    let mut invalid = ScriptedPlanner::returning(PlanProposal::plan(vec![
        proposed_step(
            "a",
            vec![PlanDependency::proposed("b")],
            capability.clone(),
            CANONICAL_PATH,
            "a",
        ),
        proposed_step(
            "b",
            vec![PlanDependency::proposed("a")],
            capability.clone(),
            CANONICAL_PATH,
            "b",
        ),
    ]));
    let mut materializer = RecordingMaterializer::default();
    let rejected = agent_loop
        .tick(
            &mut store,
            &mut events,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut invalid,
            &mut materializer,
        )
        .expect("invalid plan should be classified");
    assert!(matches!(
        rejected,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::DependencyCycle,
            ..
        }
    ));
    assert_eq!(materializer.calls, 0);
    let after_invalid = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after_invalid);
    assert_model_call_counters(&after_invalid, 1, 1, 0, 0);

    let mut timed_out = ScriptedPlanner::failing(PlannerPortFailure::Timeout);
    let timeout = agent_loop
        .tick(
            &mut store,
            &mut events,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut timed_out,
            &mut materializer,
        )
        .expect("timeout should be classified");
    assert!(matches!(
        timeout,
        AgentLoopTick::PlannerUnavailable {
            failure: PlannerPortFailure::Timeout,
            ..
        }
    ));
    let after_timeout = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after_timeout);
    assert_model_call_counters(&after_timeout, 2, 1, 1, 0);
    let timed_out_call_id = timed_out.call_ids[0].clone();
    let abandoned = agent_loop
        .abandon_model_call(
            &mut store,
            &mut events,
            &FixedLease(run_id.to_owned()),
            &timed_out_call_id,
        )
        .expect("trusted recovery should explicitly discard the timed-out call");
    assert!(matches!(
        abandoned,
        AgentLoopTick::ModelCallAbandoned { call_id, .. } if call_id == timed_out_call_id
    ));
    assert_model_call_counters(&store.load_current().unwrap().unwrap(), 2, 2, 1, 0);

    let mut valid = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "a",
        Vec::new(),
        capability,
        CANONICAL_PATH,
        RAW_SENTINEL,
    )]));
    materializer.failure = Some(PlanMaterializerFailure::PersistenceFailed);
    let unavailable = agent_loop
        .tick(
            &mut store,
            &mut events,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut valid,
            &mut materializer,
        )
        .expect("materializer failure should be classified");
    assert!(matches!(
        unavailable,
        AgentLoopTick::MaterializerUnavailable {
            failure: PlanMaterializerFailure::PersistenceFailed,
            ..
        }
    ));
    let after_materializer = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after_materializer);
    assert_model_call_counters(&after_materializer, 3, 3, 1, 0);
    assert!(
        !store
            .export_jsonl()
            .expect("journal should export")
            .windows(RAW_SENTINEL.len())
            .any(|window| window == RAW_SENTINEL.as_bytes())
    );
}

#[test]
fn invalid_provider_response_is_durably_rejected_without_an_accepted_turn() {
    let run_id = "run-agent-loop-invalid-provider-response";
    let loop_budget = budget(2, 2, 2, 32_768);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();
    let mut planner = ScriptedPlanner::failing(PlannerPortFailure::InvalidResponse);
    let tick = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("invalid response should settle with a closed rejection");
    assert!(matches!(
        tick,
        AgentLoopTick::PlannerUnavailable {
            failure: PlannerPortFailure::InvalidResponse,
            ..
        }
    ));
    let after = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after);
    assert_model_call_counters(&after, 1, 1, 0, 0);
    assert!(
        after
            .agent_loop
            .as_ref()
            .unwrap()
            .model_calls
            .as_ref()
            .unwrap()
            .active_call
            .is_none()
    );
}

#[test]
fn oversized_raw_proposal_is_rejected_before_resolution_or_materialization() {
    let run_id = "run-agent-loop-oversized";
    let definition = definition_fixture("xgeny.test/oversized-marker");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(2, 4, 4, 262_144);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();
    let resolver = CanonicalResolver::default();
    let mut materializer = RecordingMaterializer::default();
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "oversized",
        Vec::new(),
        cap,
        CANONICAL_PATH,
        &"x".repeat(300_000),
    )]));
    let result = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &resolver,
            &mut planner,
            &mut materializer,
        )
        .expect("oversized proposal should be classified");
    assert!(matches!(
        result,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::ProposalTooLarge,
            ..
        }
    ));
    assert_eq!(resolver.calls.get(), 0);
    assert_eq!(materializer.calls, 0);
    let after = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after);
    assert_model_call_counters(&after, 1, 1, 0, 0);
}

#[test]
#[allow(clippy::too_many_lines)] // Covers both batch and cross-turn identity collisions.
fn duplicate_semantic_actions_in_batch_or_replan_never_materialize() {
    let run_id = "run-agent-loop-duplicate-batch";
    let definition = definition_fixture("xgeny.test/duplicate-marker");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(4, 8, 8, 262_144);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![
        proposed_step(
            "first",
            Vec::new(),
            cap.clone(),
            CANONICAL_PATH,
            "same-action",
        ),
        proposed_step("second", Vec::new(), cap.clone(), RAW_ALIAS, "same-action"),
    ]));
    let mut materializer = RecordingMaterializer::default();
    let batch = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("duplicate batch should be classified");
    assert!(matches!(
        batch,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::DuplicateSemanticAction,
            ..
        }
    ));
    assert_eq!(materializer.calls, 0);
    let after = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after);
    assert_model_call_counters(&after, 1, 1, 0, 0);

    let replan_run = "run-agent-loop-duplicate-replan";
    let mut initial_store = MemoryRunStore::new();
    create_run(&mut initial_store, replan_run);
    let replan_loop = configure(&mut initial_store, replan_run, &loop_budget);
    let mut initial_planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "original",
        Vec::new(),
        cap.clone(),
        CANONICAL_PATH,
        "same-action",
    )]));
    let initial = replan_loop
        .tick(
            &mut initial_store,
            &mut DeterministicEvents,
            &FixedLease(replan_run.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut initial_planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("initial action should plan");
    let AgentLoopTick::PlanAccepted { step_ids, .. } = initial else {
        panic!("expected initial plan")
    };
    let mut completed_state = initial_store.load_current().unwrap().unwrap();
    let completed = completed_state
        .steps
        .get_mut(&step_ids[0])
        .expect("planned Step should project");
    completed.status = StepStatus::Completed;
    completed.execution_receipt_id = Some("receipt-duplicate-replan".to_owned());
    completed.execution_receipt_digest = Some(format!("sha256:{}", "d".repeat(64)));
    let mut replan_store = ProjectionStore::new(completed_state.clone());
    let mut duplicate_planner =
        ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
            "duplicate",
            Vec::new(),
            cap,
            RAW_ALIAS,
            "same-action",
        )]));
    let replan = replan_loop
        .tick(
            &mut replan_store,
            &mut DeterministicEvents,
            &FixedLease(replan_run.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut duplicate_planner,
            &mut materializer,
        )
        .expect("duplicate replan should be classified");
    assert!(matches!(
        replan,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::DuplicateSemanticAction,
            ..
        }
    ));
    assert_eq!(materializer.calls, 0);
    let after_replan = replan_store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&completed_state, &after_replan);
    assert_model_call_counters(&after_replan, 2, 2, 0, 1);
}

#[test]
fn sqlite_reopen_restores_atomic_plan_without_calling_planner() {
    let directory = tempdir().expect("temp directory should exist");
    let database = directory.path().join("run.sqlite3");
    let run_id = "run-agent-loop-sqlite";
    let definition = definition_fixture("xgeny.test/sqlite-marker");
    let capability = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(3, 4, 4, 262_144);
    let accepted_step_id;
    {
        let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
        create_run(&mut store, run_id);
        let agent_loop = configure(&mut store, run_id, &loop_budget);
        let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
            "persisted",
            Vec::new(),
            capability,
            CANONICAL_PATH,
            RAW_SENTINEL,
        )]));
        let mut materializer = RecordingMaterializer::default();
        let tick = agent_loop
            .tick(
                &mut store,
                &mut DeterministicEvents,
                &FixedLease(run_id.to_owned()),
                &registry,
                &CanonicalResolver::default(),
                &mut planner,
                &mut materializer,
            )
            .expect("plan should persist");
        let AgentLoopTick::PlanAccepted { step_ids, .. } = tick else {
            panic!("expected accepted plan")
        };
        accepted_step_id = step_ids[0].clone();
    }

    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
    assert!(
        reopened
            .load_planned_invocation(&accepted_step_id)
            .expect("planned sidecar should load")
            .is_some()
    );
    let mut planner = ScriptedPlanner::default();
    let tick = AgentLoop::new(loop_budget)
        .tick(
            &mut reopened,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("reopened frontier should derive");
    assert!(matches!(
        tick,
        AgentLoopTick::ActionRequired { action, .. }
            if action.step_id == accepted_step_id && action.action == ContinuationAction::Admit
    ));
    assert_eq!(planner.calls, 0);
    assert!(
        !reopened
            .export_jsonl()
            .expect("journal should export")
            .windows(RAW_SENTINEL.len())
            .any(|window| window == RAW_SENTINEL.as_bytes())
    );
}

#[test]
fn read_only_plan_uses_distinct_profile_without_idempotency_support() {
    let run_id = "run-agent-loop-read-only";
    let definition = read_only_definition_fixture("xgeny.test/read-only-marker");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(2, 2, 2, 262_144);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "read",
        Vec::new(),
        cap,
        CANONICAL_PATH,
        RAW_SENTINEL,
    )]));

    let tick = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("read-only plan should be accepted");
    assert!(matches!(tick, AgentLoopTick::PlanAccepted { .. }));
    let state = store.load_current().unwrap().unwrap();
    let planned_binding = state
        .steps
        .values()
        .next()
        .and_then(|step| step.planned_invocation.as_ref())
        .expect("accepted Step should retain its binding");
    assert_eq!(
        planned_binding.execution_profile(),
        PlannedExecutionProfile::LocalSyncReadOnlyV1
    );
}

#[test]
fn sqlite_reopen_marks_interrupted_reservation_unknown_without_recalling_planner() {
    let directory = tempdir().expect("temp directory should exist");
    let database = directory.path().join("interrupted.sqlite3");
    let run_id = "run-agent-loop-interrupted";
    let loop_budget = budget(2, 2, 2, 32_768);
    let agent_loop = AgentLoop::new(loop_budget.clone());
    {
        let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
        create_run(&mut store, run_id);
        configure_instance(&mut store, run_id, agent_loop.clone());
        let mut panic_planner = ScriptedPlanner::default();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = agent_loop.tick(
                &mut store,
                &mut DeterministicEvents,
                &FixedLease(run_id.to_owned()),
                &xgeny_runtime::CapabilityRegistry::new(),
                &CanonicalResolver::default(),
                &mut panic_planner,
                &mut RecordingMaterializer::default(),
            );
        }));
        assert!(
            panic.is_err(),
            "the scripted planner should interrupt after reservation"
        );
        assert_eq!(panic_planner.calls, 1);
        let reserved = store.load_current().unwrap().unwrap();
        assert_model_call_counters(&reserved, 1, 0, 0, 0);
        assert!(matches!(
            reserved
                .agent_loop
                .as_ref()
                .unwrap()
                .model_calls
                .as_ref()
                .unwrap()
                .active_call
                .as_ref()
                .map(|call| call.status),
            Some(xgeny_workgraph::ModelCallStatus::Reserved)
        ));
    }

    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
    let mut should_not_call = ScriptedPlanner::default();
    let recovery = agent_loop
        .tick(
            &mut reopened,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut should_not_call,
            &mut RecordingMaterializer::default(),
        )
        .expect("orphan reservation should become Unknown");
    let call_id = match recovery {
        AgentLoopTick::ModelCallRecoveryRequired {
            call_id,
            reason: xgeny_workgraph::ModelCallUnknownReason::Interrupted,
            newly_recorded: true,
            ..
        } => call_id,
        other => panic!("expected newly recorded recovery requirement, got {other:?}"),
    };
    assert_eq!(should_not_call.calls, 0);
    let unknown = reopened.load_current().unwrap().unwrap();
    assert_model_call_counters(&unknown, 1, 0, 1, 0);

    let repeated = agent_loop
        .tick(
            &mut reopened,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut should_not_call,
            &mut RecordingMaterializer::default(),
        )
        .expect("Unknown call should remain paused");
    assert!(matches!(
        repeated,
        AgentLoopTick::ModelCallRecoveryRequired {
            newly_recorded: false,
            ..
        }
    ));
    assert_eq!(should_not_call.calls, 0);
    assert_eq!(reopened.load_current().unwrap().unwrap(), unknown);

    let abandoned = agent_loop
        .abandon_model_call(
            &mut reopened,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &call_id,
        )
        .expect("trusted recovery should abandon Unknown call");
    assert!(matches!(
        abandoned,
        AgentLoopTick::ModelCallAbandoned { .. }
    ));
    assert_model_call_counters(&reopened.load_current().unwrap().unwrap(), 1, 1, 1, 0);
}

#[test]
fn lost_reservation_commit_ack_never_calls_provider_and_reopens_for_recovery() {
    let directory = tempdir().expect("temp directory should exist");
    let database = directory.path().join("reservation-ack-loss.sqlite3");
    let run_id = "run-agent-loop-reservation-ack-loss";
    let loop_budget = budget(2, 2, 2, 32_768);
    let agent_loop = AgentLoop::new(loop_budget.clone());
    let mut planner = ScriptedPlanner::returning(PlanProposal::completion_candidate("unused"));
    {
        let sqlite = SqliteRunStore::open(&database).expect("SQLite should open");
        let mut store = CommitAckLossStore::new(sqlite, AckLossStage::Reservation);
        create_run(&mut store, run_id);
        configure_instance(&mut store, run_id, agent_loop.clone());
        let error = agent_loop
            .tick(
                &mut store,
                &mut DeterministicEvents,
                &FixedLease(run_id.to_owned()),
                &xgeny_runtime::CapabilityRegistry::new(),
                &CanonicalResolver::default(),
                &mut planner,
                &mut RecordingMaterializer::default(),
            )
            .expect_err("lost reservation acknowledgement must surface as a store error");
        assert!(matches!(
            &error,
            AgentLoopError::Store(StoreError::Corrupt(message)) if message == RAW_ACK_LOSS
        ));
        assert_eq!(planner.calls, 0);
        let reserved = store.load_current().unwrap().unwrap();
        assert_model_call_counters(&reserved, 1, 0, 0, 0);
        assert!(
            !store
                .inner
                .export_jsonl()
                .expect("journal should export")
                .windows(RAW_ACK_LOSS.len())
                .any(|window| window == RAW_ACK_LOSS.as_bytes())
        );
    }

    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
    let mut should_not_call = ScriptedPlanner::default();
    let recovery = AgentLoop::new(loop_budget)
        .tick(
            &mut reopened,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut should_not_call,
            &mut RecordingMaterializer::default(),
        )
        .expect("committed reservation should recover after reopen");
    assert!(matches!(
        recovery,
        AgentLoopTick::ModelCallRecoveryRequired {
            reason: ModelCallUnknownReason::Interrupted,
            newly_recorded: true,
            ..
        }
    ));
    assert_eq!(should_not_call.calls, 0);
    assert_model_call_counters(&reopened.load_current().unwrap().unwrap(), 1, 0, 1, 0);
}

#[test]
fn lost_atomic_plan_commit_ack_reopens_as_accepted_without_recalling_provider() {
    let directory = tempdir().expect("temp directory should exist");
    let database = directory.path().join("plan-ack-loss.sqlite3");
    let run_id = "run-agent-loop-plan-ack-loss";
    let definition = definition_fixture("xgeny.test/plan-ack-loss");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(2, 4, 4, 262_144);
    let agent_loop = AgentLoop::new(loop_budget.clone());
    let accepted_step_id;
    {
        let sqlite = SqliteRunStore::open(&database).expect("SQLite should open");
        let mut store = CommitAckLossStore::new(sqlite, AckLossStage::AcceptedPlan);
        create_run(&mut store, run_id);
        configure_instance(&mut store, run_id, agent_loop.clone());
        let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
            "committed",
            Vec::new(),
            cap,
            CANONICAL_PATH,
            RAW_SENTINEL,
        )]));
        let error = agent_loop
            .tick(
                &mut store,
                &mut DeterministicEvents,
                &FixedLease(run_id.to_owned()),
                &registry,
                &CanonicalResolver::default(),
                &mut planner,
                &mut RecordingMaterializer::default(),
            )
            .expect_err("lost Plan acknowledgement must surface as a store error");
        assert!(matches!(
            &error,
            AgentLoopError::Store(StoreError::Corrupt(message)) if message == RAW_ACK_LOSS
        ));
        assert_eq!(planner.calls, 1);
        let committed = store.load_current().unwrap().unwrap();
        assert_model_call_counters(&committed, 1, 1, 0, 1);
        assert_eq!(committed.steps.len(), 1);
        accepted_step_id = committed
            .steps
            .keys()
            .next()
            .expect("accepted Step should project")
            .clone();
        assert!(
            store
                .load_planned_invocation(&accepted_step_id)
                .expect("sidecar lookup should succeed")
                .is_some()
        );
        assert!(
            !store
                .inner
                .export_jsonl()
                .expect("journal should export")
                .windows(RAW_ACK_LOSS.len())
                .any(|window| window == RAW_ACK_LOSS.as_bytes())
        );
    }

    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
    let mut should_not_call = ScriptedPlanner::default();
    let resumed = AgentLoop::new(loop_budget)
        .tick(
            &mut reopened,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut should_not_call,
            &mut RecordingMaterializer::default(),
        )
        .expect("reopened accepted Plan should expose its frontier");
    assert!(matches!(
        resumed,
        AgentLoopTick::ActionRequired { action, .. }
            if action.step_id == accepted_step_id && action.action == ContinuationAction::Admit
    ));
    assert_eq!(should_not_call.calls, 0);
    assert!(
        reopened
            .load_planned_invocation(&accepted_step_id)
            .expect("reopened sidecar lookup should succeed")
            .is_some()
    );
}

#[test]
fn context_preserves_whole_schemas_steps_and_host_owned_digest_envelope() {
    let run_id = "run-agent-loop-context";
    let definition = definition_fixture("xgeny.test/context-marker");
    let expected_input = definition.spec.input_schema.clone();
    let expected_output = definition.spec.output_schema.clone();
    let registry = registry_with(definition);
    let loop_budget = budget(3, 4, 4, 262_144);
    let completed = manual_step("step-completed", StepStatus::Completed, 1);
    let mut store = ProjectionStore::new(projected_state(
        run_id,
        loop_budget.clone(),
        0,
        vec![completed],
    ));
    let mut planner = ScriptedPlanner::returning(PlanProposal::completion_candidate("done"));
    let tick = AgentLoop::new(loop_budget)
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("completion candidate should commit");
    assert!(matches!(
        tick,
        AgentLoopTick::CompletionCandidate {
            newly_recorded: true,
            ..
        }
    ));
    let context = &planner.contexts[0];
    assert_eq!(context.profile_version(), "xgeny.planning-context/v1");
    assert_eq!(context.steps().len(), 1);
    assert_eq!(context.omitted_steps(), 0);
    assert_eq!(context.steps()[0].step_id(), "step-completed");
    assert_eq!(context.steps()[0].status(), StepStatus::Completed);
    assert_eq!(context.steps()[0].attempts(), 1);
    assert!(context.steps()[0].dependency_released());
    assert_eq!(context.capabilities().len(), 1);
    assert_eq!(context.capabilities()[0].input_schema(), &expected_input);
    assert_eq!(context.capabilities()[0].output_schema(), &expected_output);
    assert_eq!(
        context.capabilities()[0].effect_class(),
        EffectClass::Idempotent
    );
    assert_eq!(
        context.capabilities()[0].execution_styles(),
        &[ExecutionStyle::Sync]
    );
    let canonical = serde_jcs::to_vec(context).expect("context should canonicalize");
    assert_eq!(canonical.len() as u64, context.canonical_size_bytes());
    let envelope = serde_json::to_value(context).expect("context should serialize");
    assert!(envelope.get("contextDigest").is_none());
    assert!(context.context_digest().starts_with("sha256:"));
    assert!(matches!(
        store.last_body,
        Some(RunEventBody::CompletionCandidateRecorded { .. })
    ));
    assert_model_call_counters(&store.load_current().unwrap().unwrap(), 1, 1, 0, 1);
}

#[test]
fn context_whole_item_byte_boundary_and_catalog_order_are_deterministic() {
    let definition = definition_fixture("xgeny.test/boundary-marker");
    let registry = registry_with(definition);
    let generous = capture_empty_context("run-context-boundary", &registry, 262_144);
    assert_eq!(generous.capabilities().len(), 1);
    let exact_size = generous.canonical_size_bytes();

    let exact = capture_empty_context("run-context-boundary", &registry, exact_size);
    assert_eq!(exact.capabilities().len(), 1);
    assert_eq!(exact.canonical_size_bytes(), exact_size);
    let one_byte_short = capture_empty_context(
        "run-context-boundary",
        &registry,
        exact_size.checked_sub(1).expect("context is non-empty"),
    );
    assert!(one_byte_short.capabilities().is_empty());
    assert_eq!(one_byte_short.omitted_capabilities(), 1);
    assert!(one_byte_short.canonical_size_bytes() < exact_size);

    let definition_a = definition_fixture("xgeny.test/a-marker");
    let definition_b = definition_fixture("xgeny.test/b-marker");
    let mut registry_ab = xgeny_runtime::CapabilityRegistry::new();
    registry_ab
        .register_schema_validated_definition(definition_a.clone())
        .unwrap();
    registry_ab
        .register_schema_validated_definition(definition_b.clone())
        .unwrap();
    let mut registry_ba = xgeny_runtime::CapabilityRegistry::new();
    registry_ba
        .register_schema_validated_definition(definition_b)
        .unwrap();
    registry_ba
        .register_schema_validated_definition(definition_a)
        .unwrap();
    let context_ab = capture_empty_context("run-context-order", &registry_ab, 262_144);
    let context_ba = capture_empty_context("run-context-order", &registry_ba, 262_144);
    assert_eq!(context_ab.context_digest(), context_ba.context_digest());
    assert_eq!(context_ab.catalog_digest(), context_ba.catalog_digest());
    assert_eq!(
        context_ab
            .capabilities()
            .iter()
            .map(|summary| summary.capability().capability_id.as_str())
            .collect::<Vec<_>>(),
        vec!["xgeny.test/a-marker", "xgeny.test/b-marker"]
    );
}

#[test]
fn context_base_over_budget_yields_without_calling_planner_or_mutating_run() {
    let run_id = "run-agent-loop-context-exhausted";
    let loop_budget = budget(2, 2, 2, 1);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();
    let mut planner = ScriptedPlanner::default();
    let result = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("context budget exhaustion should be classified");
    assert!(matches!(
        result,
        AgentLoopTick::Quiescent {
            reason: AgentLoopQuiescence::ContextBudgetExceeded,
            ..
        }
    ));
    assert_eq!(planner.calls, 0);
    let after = store.load_current().unwrap().unwrap();
    assert_eq!(after, before);
    assert_model_call_counters(&after, 0, 0, 0, 0);
}

#[test]
fn context_input_hard_limit_yields_before_reservation_or_planner_call() {
    let run_id = "run-agent-loop-context-input-limit";
    let loop_budget = budget(2, 2, 2, 262_144);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let mut definition = definition_fixture("xgeny.test/oversized-planning-source");
    definition.spec.summary = "x".repeat(256 * 1024 + 1);
    let registry = registry_with(definition);
    let before = store.load_current().unwrap().unwrap();
    let mut planner = ScriptedPlanner::default();
    let result = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("context input limit should be classified");
    assert!(matches!(
        result,
        AgentLoopTick::Quiescent {
            reason: AgentLoopQuiescence::ContextInputLimitExceeded,
            ..
        }
    ));
    assert_eq!(planner.calls, 0);
    let after = store.load_current().unwrap().unwrap();
    assert_eq!(after, before);
    assert_model_call_counters(&after, 0, 0, 0, 0);
}

#[test]
fn oversized_goal_yields_before_reservation_or_planner_call() {
    let run_id = "run-agent-loop-oversized-goal";
    let loop_budget = budget(2, 2, 2, 262_144);
    let mut store = MemoryRunStore::new();
    create_run_with_goal(&mut store, run_id, &"x".repeat(256 * 1024 + 1));
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();
    let mut planner = ScriptedPlanner::default();
    let result = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("oversized goal should be classified");
    assert!(matches!(
        result,
        AgentLoopTick::Quiescent {
            reason: AgentLoopQuiescence::ContextInputLimitExceeded,
            ..
        }
    ));
    assert_eq!(planner.calls, 0);
    let after = store.load_current().unwrap().unwrap();
    assert_eq!(after, before);
    assert_model_call_counters(&after, 0, 0, 0, 0);
}

#[test]
fn invalid_planner_identity_or_profile_fails_before_reservation_and_provider_call() {
    let run_id = "run-agent-loop-invalid-planner-metadata";
    let loop_budget = budget(2, 2, 2, 32_768);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();

    let mut invalid_profile = ScriptedPlanner {
        profile_digest_override: Some(format!("sha256:{}", "A".repeat(64))),
        ..ScriptedPlanner::default()
    };
    let error = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut invalid_profile,
            &mut RecordingMaterializer::default(),
        )
        .expect_err("non-canonical profile digest must fail");
    assert!(matches!(
        error,
        xgeny_runtime::AgentLoopError::InvalidPlannerRequestProfileDigest
    ));
    assert_eq!(invalid_profile.calls, 0);
    assert_eq!(store.load_current().unwrap().unwrap(), before);

    let mut invalid_id = ScriptedPlanner {
        planner_id_override: Some("registry/planner-v1".to_owned()),
        ..ScriptedPlanner::default()
    };
    let error = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut invalid_id,
            &mut RecordingMaterializer::default(),
        )
        .expect_err("syntactically invalid planner ID must fail");
    assert!(matches!(
        error,
        xgeny_runtime::AgentLoopError::PlanningContract(_)
    ));
    assert_eq!(invalid_id.calls, 0);
    assert_eq!(store.load_current().unwrap().unwrap(), before);
}

#[test]
fn reservation_append_failure_never_invokes_planner_or_consumes_slot() {
    let run_id = "run-agent-loop-reservation-failure";
    let loop_budget = budget(2, 2, 2, 32_768);
    let mut store = FailingReservationStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();
    let mut planner = ScriptedPlanner::default();
    let error = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect_err("reservation append fault should surface");
    assert!(matches!(
        error,
        xgeny_runtime::AgentLoopError::Store(StoreError::Corrupt(_))
    ));
    assert_eq!(planner.calls, 0);
    let after = store.load_current().unwrap().unwrap();
    assert_eq!(after, before);
    assert_model_call_counters(&after, 0, 0, 0, 0);
}

#[test]
fn omitted_capability_and_step_cannot_be_guessed_by_proposal() {
    let run_id = "run-agent-loop-hidden";
    let mut definition = definition_fixture("xgeny.test/hidden-marker");
    definition.spec.input_schema["description"] = Value::String("schema-a".repeat(8_000));
    let hidden_capability = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(3, 4, 4, 1_024);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "guessed",
        Vec::new(),
        hidden_capability.clone(),
        CANONICAL_PATH,
        "hidden",
    )]));
    let mut materializer = RecordingMaterializer::default();
    let result = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("hidden capability should be rejected");
    assert!(matches!(
        result,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::CapabilityUnavailable,
            ..
        }
    ));
    assert!(planner.contexts[0].capabilities().is_empty());
    assert_eq!(planner.contexts[0].omitted_capabilities(), 1);
    let omitted_catalog_digest = planner.contexts[0].catalog_digest().to_owned();
    assert_eq!(materializer.calls, 0);
    let after = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after);
    assert_model_call_counters(&after, 1, 1, 0, 0);

    let mut alternate_definition = definition_fixture("xgeny.test/hidden-marker");
    alternate_definition.spec.input_schema["description"] = Value::String("schema-b".repeat(8_000));
    let alternate_registry = registry_with(alternate_definition);
    let alternate_context =
        capture_empty_context("run-agent-loop-hidden-catalog", &alternate_registry, 1_024);
    assert!(alternate_context.capabilities().is_empty());
    assert_ne!(
        alternate_context.catalog_digest(),
        omitted_catalog_digest,
        "omission must not remove the full catalog commitment"
    );

    let completed = manual_step("step-hidden", StepStatus::Completed, 1);
    let mut hidden_step_store = ProjectionStore::new(projected_state(
        "run-agent-loop-hidden-step",
        loop_budget.clone(),
        0,
        vec![StepState {
            objective: "objective".repeat(700),
            ..completed
        }],
    ));
    let mut hidden_step_planner =
        ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
            "child",
            vec![PlanDependency::existing("step-hidden")],
            hidden_capability,
            CANONICAL_PATH,
            "child",
        )]));
    let hidden_step_result = AgentLoop::new(loop_budget)
        .tick(
            &mut hidden_step_store,
            &mut DeterministicEvents,
            &FixedLease("run-agent-loop-hidden-step".to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut hidden_step_planner,
            &mut materializer,
        )
        .expect("hidden Step dependency should be rejected");
    assert!(matches!(
        hidden_step_result,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::UnknownDependency,
            ..
        }
    ));
    assert!(hidden_step_planner.contexts[0].steps().is_empty());
    assert_eq!(hidden_step_planner.contexts[0].omitted_steps(), 1);
    assert_eq!(materializer.calls, 0);
}

#[test]
fn completion_candidate_is_durable_and_does_not_complete_run() {
    let run_id = "run-agent-loop-completion";
    let loop_budget = budget(2, 2, 2, 32_768);
    let mut store = ProjectionStore::new(projected_state(
        run_id,
        loop_budget.clone(),
        0,
        vec![manual_step("step-done", StepStatus::Completed, 1)],
    ));
    let mut planner = ScriptedPlanner::returning(PlanProposal::completion_candidate(
        "all receipt-bound work is complete",
    ));
    let agent_loop = AgentLoop::new(loop_budget);
    let first = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("candidate should commit");
    let AgentLoopTick::CompletionCandidate {
        candidate,
        newly_recorded: true,
        head,
    } = first
    else {
        panic!("expected new completion candidate")
    };
    assert_eq!(planner.calls, 1);
    assert!(candidate.candidate_id.starts_with("completion-"));
    assert!(matches!(
        store.last_body,
        Some(RunEventBody::CompletionCandidateRecorded { .. })
    ));
    let second = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("durable candidate should be replayed");
    assert!(matches!(
        second,
        AgentLoopTick::CompletionCandidate {
            newly_recorded: false,
            head: existing_head,
            ..
        } if existing_head == head
    ));
    assert_eq!(planner.calls, 1);
}

#[test]
#[allow(clippy::too_many_lines)] // Exercises three durable counters and recovery exceptions.
fn durable_turn_step_and_tool_budgets_fail_closed_at_exact_boundaries() {
    let run_id = "run-agent-loop-step-budget";
    let definition = definition_fixture("xgeny.test/budget-marker");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let step_budget = budget(2, 1, 2, 262_144);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &step_budget);
    let before = store.load_current().unwrap().unwrap();
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![
        proposed_step("a", Vec::new(), cap.clone(), CANONICAL_PATH, "a"),
        proposed_step("b", Vec::new(), cap, CANONICAL_PATH, "b"),
    ]));
    let rejected = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("step budget should classify");
    assert!(matches!(
        rejected,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::PlannedStepBudgetExceeded,
            ..
        }
    ));
    let after = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after);
    assert_model_call_counters(&after, 1, 1, 0, 0);

    let turn_budget = budget(1, 2, 2, 32_768);
    let mut turn_store = ProjectionStore::new(projected_state(
        "run-agent-loop-turn-budget",
        turn_budget.clone(),
        1,
        Vec::new(),
    ));
    let mut no_planner = ScriptedPlanner::default();
    let turn = AgentLoop::new(turn_budget)
        .tick(
            &mut turn_store,
            &mut DeterministicEvents,
            &FixedLease("run-agent-loop-turn-budget".to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut no_planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("turn budget should classify");
    assert!(matches!(
        turn,
        AgentLoopTick::Quiescent {
            reason: AgentLoopQuiescence::ModelTurnBudgetExhausted,
            ..
        }
    ));
    assert_eq!(no_planner.calls, 0);

    let tool_budget = budget(2, 2, 1, 32_768);
    for (status, expected_action) in [
        (StepStatus::Planned, ContinuationAction::Admit),
        (StepStatus::Executing, ContinuationAction::DriveEffect),
        (StepStatus::EffectUnknown, ContinuationAction::DriveEffect),
        (StepStatus::Reconciling, ContinuationAction::DriveEffect),
        (StepStatus::Validating, ContinuationAction::Verify),
    ] {
        let case_run = format!("run-tool-{status:?}");
        let mut case_store = ProjectionStore::new(projected_state(
            &case_run,
            tool_budget.clone(),
            0,
            vec![manual_step("step-tool", status, 1)],
        ));
        let action = AgentLoop::new(tool_budget.clone())
            .tick(
                &mut case_store,
                &mut DeterministicEvents,
                &FixedLease(case_run),
                &xgeny_runtime::CapabilityRegistry::new(),
                &CanonicalResolver::default(),
                &mut ScriptedPlanner::default(),
                &mut RecordingMaterializer::default(),
            )
            .expect("safety/recovery action should remain available");
        assert!(matches!(
            action,
            AgentLoopTick::ActionRequired { action, .. } if action.action == expected_action
        ));
    }
    let mut intent_store = ProjectionStore::new(projected_state(
        "run-tool-intent",
        tool_budget.clone(),
        0,
        vec![manual_step("step-tool", StepStatus::IntentCommitted, 1)],
    ));
    let blocked = AgentLoop::new(tool_budget)
        .tick(
            &mut intent_store,
            &mut DeterministicEvents,
            &FixedLease("run-tool-intent".to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut ScriptedPlanner::default(),
            &mut RecordingMaterializer::default(),
        )
        .expect("new effect attempt should be budget-gated");
    assert!(matches!(
        blocked,
        AgentLoopTick::Quiescent {
            reason: AgentLoopQuiescence::ToolCallBudgetExhausted,
            ..
        }
    ));
}

#[test]
fn model_call_budget_is_independent_from_accepted_turn_budget() {
    let run_id = "run-agent-loop-call-budget";
    let loop_budget = budget(4, 4, 4, 32_768);
    let call_budget = ModelCallBudget::new(3).expect("call budget should validate");
    let agent_loop = AgentLoop::with_model_call_budget(loop_budget, call_budget);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure_instance(&mut store, run_id, agent_loop);

    for failure in [
        PlannerPortFailure::InvalidResponse,
        PlannerPortFailure::ProviderLimit,
        PlannerPortFailure::ProviderRejected,
    ] {
        let outcome = agent_loop
            .tick(
                &mut store,
                &mut DeterministicEvents,
                &FixedLease(run_id.to_owned()),
                &xgeny_runtime::CapabilityRegistry::new(),
                &CanonicalResolver::default(),
                &mut ScriptedPlanner::failing(failure),
                &mut RecordingMaterializer::default(),
            )
            .expect("typed provider failure should settle");
        assert!(matches!(
            outcome,
            AgentLoopTick::PlannerUnavailable {
                failure: actual,
                ..
            } if actual == failure
        ));
    }
    let after_failures = store.load_current().unwrap().unwrap();
    assert_model_call_counters(&after_failures, 3, 3, 0, 0);

    let mut no_call = ScriptedPlanner::default();
    let exhausted = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut no_call,
            &mut RecordingMaterializer::default(),
        )
        .expect("call budget should fail closed");
    assert!(matches!(
        exhausted,
        AgentLoopTick::Quiescent {
            reason: AgentLoopQuiescence::ModelCallBudgetExhausted,
            ..
        }
    ));
    assert_eq!(no_call.calls, 0);
    assert_eq!(store.load_current().unwrap().unwrap(), after_failures);
}

#[test]
fn semantic_plan_identity_is_order_independent_after_normalization() {
    fn accept(path: &str, marker: &str, reversed: bool) -> (Vec<String>, String) {
        let run_id = "run-agent-loop-stable";
        let definition = definition_fixture("xgeny.test/stable-marker");
        let cap = capability(&definition);
        let registry = registry_with(definition);
        let loop_budget = budget(2, 4, 4, 262_144);
        let mut store = MemoryRunStore::new();
        create_run(&mut store, run_id);
        let agent_loop = configure(&mut store, run_id, &loop_budget);
        let a = proposed_step("a", Vec::new(), cap.clone(), path, marker);
        let b = proposed_step(
            "b",
            vec![PlanDependency::proposed("a")],
            cap,
            CANONICAL_PATH,
            "same-b",
        );
        let steps = if reversed { vec![b, a] } else { vec![a, b] };
        let mut planner = ScriptedPlanner::returning(PlanProposal::plan(steps));
        let tick = agent_loop
            .tick(
                &mut store,
                &mut DeterministicEvents,
                &FixedLease(run_id.to_owned()),
                &registry,
                &CanonicalResolver::default(),
                &mut planner,
                &mut RecordingMaterializer::default(),
            )
            .expect("plan should commit");
        let AgentLoopTick::PlanAccepted { step_ids, .. } = tick else {
            panic!("expected accepted plan")
        };
        let state = store.load_current().unwrap().unwrap();
        (step_ids, accepted_proposal_digest(&state))
    }

    let canonical = accept(CANONICAL_PATH, "same", false);
    let normalized_alias = accept(RAW_ALIAS, "same", true);
    assert_eq!(canonical, normalized_alias);
    let different_semantics = accept(CANONICAL_PATH, "different", false);
    assert_ne!(canonical, different_semantics);
}

#[test]
fn timeout_settlement_head_conflict_reloads_and_marks_the_call_unknown() {
    let run_id = "run-agent-loop-timeout-conflict";
    let loop_budget = budget(2, 4, 4, 32_768);
    let mut store = OutcomeConflictStore::new(OutcomeConflictTarget::Unknown);
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let mut planner = ScriptedPlanner::failing(PlannerPortFailure::Timeout);
    let outcome = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("timeout should be reclassified against the fresh head");
    assert!(matches!(
        outcome,
        AgentLoopTick::ModelCallRecoveryRequired {
            reason: ModelCallUnknownReason::Timeout,
            newly_recorded: true,
            ..
        }
    ));
    assert_eq!(planner.calls, 1);
    let state = store.load_current().unwrap().unwrap();
    assert!(state.steps.contains_key("step-concurrent-outcome"));
    assert_model_call_counters(&state, 1, 0, 1, 0);
}

#[test]
fn known_failure_settlement_head_conflict_closes_the_call_as_stale() {
    let run_id = "run-agent-loop-known-failure-conflict";
    let loop_budget = budget(2, 4, 4, 32_768);
    let mut store = OutcomeConflictStore::new(OutcomeConflictTarget::Settled);
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let mut planner = ScriptedPlanner::failing(PlannerPortFailure::InvalidResponse);
    let outcome = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &xgeny_runtime::CapabilityRegistry::new(),
            &CanonicalResolver::default(),
            &mut planner,
            &mut RecordingMaterializer::default(),
        )
        .expect("known failure should close conservatively against the fresh head");
    assert!(matches!(
        outcome,
        AgentLoopTick::ModelCallRejected {
            reason: ModelCallRejectionReason::StaleHead,
            ..
        }
    ));
    assert_eq!(planner.calls, 1);
    let state = store.load_current().unwrap().unwrap();
    assert!(state.steps.contains_key("step-concurrent-outcome"));
    assert_model_call_counters(&state, 1, 1, 0, 0);
}

#[test]
fn concurrent_unknown_wins_over_a_stale_success_without_a_false_rejection() {
    let run_id = "run-agent-loop-concurrent-unknown";
    let definition = definition_fixture("xgeny.test/concurrent-unknown");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(2, 4, 4, 262_144);
    let mut store = StalePlanOutcomeStore::new(ConcurrentCallOutcome::Unknown);
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "stale",
        Vec::new(),
        cap,
        CANONICAL_PATH,
        RAW_SENTINEL,
    )]));
    let mut materializer = RecordingMaterializer::default();
    let outcome = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("the fresh Unknown outcome should remain authoritative");
    assert!(matches!(
        outcome,
        AgentLoopTick::ModelCallRecoveryRequired {
            reason: ModelCallUnknownReason::Interrupted,
            newly_recorded: false,
            ..
        }
    ));
    assert_eq!(planner.calls, 1);
    assert_eq!(materializer.calls, 1);
    let state = store.load_current().unwrap().unwrap();
    assert!(state.steps.is_empty());
    assert_model_call_counters(&state, 1, 0, 1, 0);
    assert!(
        store
            .load_planned_invocation(&materializer.captured[0].step_id)
            .expect("sidecar lookup should succeed")
            .is_none()
    );
}

#[test]
fn concurrently_settled_call_fails_closed_instead_of_reporting_stale_rejection() {
    let run_id = "run-agent-loop-concurrent-settled";
    let definition = definition_fixture("xgeny.test/concurrent-settled");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(2, 4, 4, 262_144);
    let mut store = StalePlanOutcomeStore::new(ConcurrentCallOutcome::Settled);
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "stale",
        Vec::new(),
        cap,
        CANONICAL_PATH,
        RAW_SENTINEL,
    )]));
    let mut materializer = RecordingMaterializer::default();
    let error = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect_err("a call already settled by another writer has no local stale outcome to add");
    assert!(matches!(error, AgentLoopError::ModelCallNoLongerActive));
    assert_eq!(planner.calls, 1);
    assert_eq!(materializer.calls, 1);
    let state = store.load_current().unwrap().unwrap();
    assert!(state.steps.is_empty());
    assert_model_call_counters(&state, 1, 1, 0, 0);
    assert!(
        store
            .load_planned_invocation(&materializer.captured[0].step_id)
            .expect("sidecar lookup should succeed")
            .is_none()
    );
}

#[test]
fn stale_success_is_never_replayed_on_a_new_head() {
    let run_id = "run-agent-loop-stale-success";
    let definition = definition_fixture("xgeny.test/stale-marker");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let loop_budget = budget(2, 4, 4, 262_144);
    let mut store = StalePlanStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "stale",
        Vec::new(),
        cap,
        CANONICAL_PATH,
        RAW_SENTINEL,
    )]));
    let mut materializer = RecordingMaterializer::default();
    let outcome = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &registry,
            &CanonicalResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("stale result should be discarded safely");
    assert!(matches!(
        outcome,
        AgentLoopTick::ModelCallRejected {
            reason: xgeny_workgraph::ModelCallRejectionReason::StaleHead,
            ..
        }
    ));
    assert_eq!(planner.calls, 1);
    assert_eq!(materializer.calls, 1);
    let state = store.load_current().unwrap().unwrap();
    assert_eq!(state.steps.len(), 1);
    assert!(state.steps.contains_key("step-concurrent"));
    assert_model_call_counters(&state, 1, 1, 0, 0);
    assert!(
        store
            .load_planned_invocation(&materializer.captured[0].step_id)
            .expect("sidecar lookup should verify")
            .is_none(),
        "stale Plan and its sidecar transaction must roll back"
    );
    assert!(
        !store
            .export_jsonl()
            .expect("journal should export")
            .windows(RAW_SENTINEL.len())
            .any(|window| window == RAW_SENTINEL.as_bytes())
    );
}

#[test]
fn critical_action_and_nondeterministic_normalization_are_rejected_before_commit() {
    let run_id = "run-agent-loop-critical";
    let mut critical = definition_fixture("xgeny.test/critical-marker");
    critical
        .spec
        .effect
        .critical_actions
        .push(CriticalAction::ExternalPublishOrMessage);
    let critical_capability = capability(&critical);
    let critical_registry = registry_with(critical);
    let loop_budget = budget(3, 4, 4, 262_144);
    let mut store = MemoryRunStore::new();
    create_run(&mut store, run_id);
    let agent_loop = configure(&mut store, run_id, &loop_budget);
    let before = store.load_current().unwrap().unwrap();
    let resolver = CanonicalResolver::default();
    let mut planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "critical",
        Vec::new(),
        critical_capability,
        CANONICAL_PATH,
        "critical",
    )]));
    let mut materializer = RecordingMaterializer::default();
    let result = agent_loop
        .tick(
            &mut store,
            &mut DeterministicEvents,
            &FixedLease(run_id.to_owned()),
            &critical_registry,
            &resolver,
            &mut planner,
            &mut materializer,
        )
        .expect("critical action should be classified");
    assert!(matches!(
        result,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::CapabilityUnsupported,
            ..
        }
    ));
    assert_eq!(resolver.calls.get(), 0);
    assert_eq!(materializer.calls, 0);
    let after = store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&before, &after);
    assert_model_call_counters(&after, 1, 1, 0, 0);

    let unstable_run = "run-agent-loop-unstable-resolver";
    let definition = definition_fixture("xgeny.test/unstable-marker");
    let cap = capability(&definition);
    let registry = registry_with(definition);
    let mut unstable_store = MemoryRunStore::new();
    create_run(&mut unstable_store, unstable_run);
    let unstable_loop = configure(&mut unstable_store, unstable_run, &loop_budget);
    let unstable_before = unstable_store.load_current().unwrap().unwrap();
    let unstable_resolver = AlternatingResolver::default();
    let mut unstable_planner = ScriptedPlanner::returning(PlanProposal::plan(vec![proposed_step(
        "unstable",
        Vec::new(),
        cap,
        CANONICAL_PATH,
        "unstable",
    )]));
    let unstable = unstable_loop
        .tick(
            &mut unstable_store,
            &mut DeterministicEvents,
            &FixedLease(unstable_run.to_owned()),
            &registry,
            &unstable_resolver,
            &mut unstable_planner,
            &mut materializer,
        )
        .expect("nondeterministic resolver should fail closed");
    assert!(matches!(
        unstable,
        AgentLoopTick::ProposalRejected {
            reason: ProposalRejection::InvocationInvalid,
            ..
        }
    ));
    assert_eq!(unstable_resolver.calls.get(), 2);
    assert_eq!(materializer.calls, 0);
    let unstable_after = unstable_store.load_current().unwrap().unwrap();
    assert_workgraph_unchanged(&unstable_before, &unstable_after);
    assert_model_call_counters(&unstable_after, 1, 1, 0, 0);
}
