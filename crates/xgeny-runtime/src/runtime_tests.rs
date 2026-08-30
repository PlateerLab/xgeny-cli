use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use crate::{
    DriveAction, DurableEffectRuntime, EffectSink, EventFactory, EventFactoryError, EventMetadata,
    ExecutionObservation, LeaseError, LocalRunLease, PreparedEffect, PreparedEffectBinding,
    ReconciliationObservation, RuntimeError, RuntimePolicy,
};
use tempfile::{TempDir, tempdir};
use xgeny_local_store::{
    Commit, ExpectedHead, MemoryRunStore, RunSnapshot, RunStore, SqliteRunStore, StoreError,
};
use xgeny_protocol::{CORE_RECEIPT_INPUT_SUMMARY_V1, CORE_RECEIPT_PROFILE_V1};
use xgeny_workgraph::{
    AuthorizationBinding, AuthorizationUse, EffectClass, EffectIntent, InvocationBinding,
    InvocationMaterialRecord, InvocationMaterialRetention, ReceiptPlacement, ReceiptProvenance,
    ReceiptVerificationRule, ReceiptVerificationStrategy, RunEvent, RunEventBody, RunState,
    SinkGuarantee, StepStatus, authorization_digest, invocation_material_digest,
    invocation_material_retention_digest, once_authorization_id, receipt_provenance_digest,
};

const RUN_ID: &str = "run-1";
const STEP_ID: &str = "step-1";
const EFFECT_ID: &str = "effect-1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Other,
    ExecutionStarted,
    EffectSucceeded,
    EffectFailed,
    EffectUnknown,
    ReconciliationStarted,
    ReconciliationResolved,
    ManualRequired,
}

fn event_kind(body: &RunEventBody) -> EventKind {
    match body {
        RunEventBody::EffectExecutionStarted { .. } => EventKind::ExecutionStarted,
        RunEventBody::EffectSucceeded { .. } => EventKind::EffectSucceeded,
        RunEventBody::EffectFailed { .. } => EventKind::EffectFailed,
        RunEventBody::EffectBecameUnknown { .. } => EventKind::EffectUnknown,
        RunEventBody::ReconciliationStarted { .. } => EventKind::ReconciliationStarted,
        RunEventBody::ReconciliationResolved { .. } => EventKind::ReconciliationResolved,
        RunEventBody::ManualInterventionRequired { .. } => EventKind::ManualRequired,
        _ => EventKind::Other,
    }
}

fn trace_label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Other => "store:other",
        EventKind::ExecutionStarted => "store:execution_started",
        EventKind::EffectSucceeded => "store:effect_succeeded",
        EventKind::EffectFailed => "store:effect_failed",
        EventKind::EffectUnknown => "store:effect_unknown",
        EventKind::ReconciliationStarted => "store:reconciliation_started",
        EventKind::ReconciliationResolved => "store:reconciliation_resolved",
        EventKind::ManualRequired => "store:manual_required",
    }
}

#[derive(Debug)]
struct RecordingStore {
    inner: MemoryRunStore,
    trace: Rc<RefCell<Vec<&'static str>>>,
    fail_once: Option<EventKind>,
}

impl RecordingStore {
    fn new(trace: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            inner: MemoryRunStore::new(),
            trace,
            fail_once: None,
        }
    }

    fn fail_once_on(&mut self, kind: EventKind) {
        self.fail_once = Some(kind);
    }
}

impl RunStore for RecordingStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        let kind = event_kind(&event.body);
        self.trace.borrow_mut().push(trace_label(kind));
        if self.fail_once == Some(kind) {
            self.fail_once = None;
            return Err(StoreError::InjectedFault("runtime contract test"));
        }
        self.inner.append(expected, event)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        self.inner.load()
    }

    fn append_with_invocation_material(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        material: InvocationMaterialRecord,
    ) -> Result<Commit, StoreError> {
        let kind = event_kind(&event.body);
        self.trace.borrow_mut().push(trace_label(kind));
        if self.fail_once == Some(kind) {
            self.fail_once = None;
            return Err(StoreError::InjectedFault("runtime contract test"));
        }
        self.inner
            .append_with_invocation_material(expected, event, material)
    }

    fn load_invocation_material(
        &self,
        effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        self.inner.load_invocation_material(effect_id)
    }
}

#[derive(Debug, Default)]
struct DeterministicEvents {
    _private: (),
}

impl EventFactory for DeterministicEvents {
    fn create_metadata(&mut self, state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        let sequence = state
            .journal_sequence
            .checked_add(1)
            .ok_or_else(|| EventFactoryError::new("journal sequence overflow"))?;
        Ok(EventMetadata {
            event_id: format!("runtime-event-{sequence}"),
            recorded_at: "2026-08-29T00:00:00Z".to_owned(),
        })
    }
}

#[derive(Debug)]
struct FailingEvents;

impl EventFactory for FailingEvents {
    fn create_metadata(&mut self, _state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        Err(EventFactoryError::new("simulated metadata failure"))
    }
}

#[derive(Debug)]
struct InvalidTimestampEvents;

impl EventFactory for InvalidTimestampEvents {
    fn create_metadata(&mut self, _state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        Ok(EventMetadata {
            event_id: "invalid-start-metadata".to_owned(),
            recorded_at: "RAW-START-TIMESTAMP-SENTINEL".to_owned(),
        })
    }
}

#[derive(Debug)]
struct ScriptedSink {
    executions: VecDeque<ExecutionObservation>,
    reconciliations: VecDeque<ReconciliationObservation>,
    execute_calls: usize,
    reconcile_calls: usize,
    trace: Rc<RefCell<Vec<&'static str>>>,
}

#[derive(Debug)]
struct TestPreparedEffect(PreparedEffectBinding);

impl PreparedEffect for TestPreparedEffect {
    fn binding(&self) -> &PreparedEffectBinding {
        &self.0
    }
}

fn prepared_effect<S: RunStore>(store: &S) -> TestPreparedEffect {
    let snapshot = store
        .load()
        .expect("store should load")
        .expect("Run should exist");
    let intent = snapshot.state.steps[STEP_ID]
        .intent
        .as_ref()
        .expect("intent should exist");
    let record = store
        .load_invocation_material(&intent.effect_id)
        .expect("material should load")
        .expect("material should exist");
    TestPreparedEffect(PreparedEffectBinding::from_verified(
        &snapshot.state,
        STEP_ID,
        intent,
        record,
    ))
}

impl ScriptedSink {
    fn new(trace: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            executions: VecDeque::new(),
            reconciliations: VecDeque::new(),
            execute_calls: 0,
            reconcile_calls: 0,
            trace,
        }
    }
}

impl EffectSink for ScriptedSink {
    type Prepared = TestPreparedEffect;

    fn execute(
        &mut self,
        _intent: &EffectIntent,
        _prepared: Self::Prepared,
    ) -> ExecutionObservation {
        self.execute_calls += 1;
        self.trace.borrow_mut().push("sink:execute");
        self.executions
            .pop_front()
            .expect("execution result must be scripted")
    }

    fn reconcile(&mut self, _intent: &EffectIntent) -> ReconciliationObservation {
        self.reconcile_calls += 1;
        self.trace.borrow_mut().push("sink:reconcile");
        self.reconciliations
            .pop_front()
            .expect("reconciliation result must be scripted")
    }
}

fn seed_event(event_id: &str, body: RunEventBody) -> RunEvent {
    RunEvent {
        event_id: event_id.to_owned(),
        run_id: RUN_ID.to_owned(),
        authority: "local:test".to_owned(),
        authority_epoch: 11,
        recorded_at: "2026-08-29T00:00:00Z".to_owned(),
        body,
    }
}

fn append_body<S: RunStore>(
    store: &mut S,
    state: &RunState,
    event_id: &str,
    body: RunEventBody,
) -> RunState {
    store
        .append(ExpectedHead::from_state(state), seed_event(event_id, body))
        .expect("seed event should commit")
        .state
}

fn effect_intent(state: &RunState, guarantee: SinkGuarantee) -> EffectIntent {
    let material_digest = invocation_material_digest(&serde_json::json!({"operation": "test"}))
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
    let mut binding = AuthorizationBinding {
        run_id: state.run_id.clone(),
        step_id: STEP_ID.to_owned(),
        authority: state.authority.clone(),
        authority_epoch: state.authority_epoch,
        issued_at_sequence: state.journal_sequence,
        issued_at_head_digest: state.journal_head_digest.clone(),
        capability_id: invocation.capability_id.clone(),
        contract_version: invocation.contract_version.clone(),
        definition_digest: invocation.definition_digest.clone(),
        instance_id: invocation.instance_id.clone(),
        instance_binding_digest: invocation.instance_binding_digest.clone(),
        action_digest: "sha256:action-1".to_owned(),
        material_digest,
        material_retention_digest,
        policy_evidence_digest: "sha256:policy-1".to_owned(),
        receipt_provenance_digest: None,
    };
    let provenance = ReceiptProvenance {
        profile_version: CORE_RECEIPT_PROFILE_V1.to_owned(),
        tool_output_profile: None,
        invocation_id: "invocation-runtime-test".to_owned(),
        plan_id: "plan-runtime-test".to_owned(),
        policy_decision_id: "decision-runtime-test".to_owned(),
        policy_decision_digest: format!("sha256:{}", "c".repeat(64)),
        executor_id: "xgeny-local".to_owned(),
        executor_placement: ReceiptPlacement::Local,
        executor_platform: crate::local_executor_platform(),
        input_summary: CORE_RECEIPT_INPUT_SUMMARY_V1.to_owned(),
        verification_plan: vec![ReceiptVerificationRule {
            strategy: ReceiptVerificationStrategy::Postcondition,
            required: true,
        }],
    };
    binding.receipt_provenance_digest =
        Some(receipt_provenance_digest(&provenance).expect("provenance should canonicalize"));
    EffectIntent {
        effect_id: EFFECT_ID.to_owned(),
        action_digest: "sha256:action-1".to_owned(),
        invocation,
        effect_class: EffectClass::NonIdempotent,
        idempotency_key: (guarantee != SinkGuarantee::None).then(|| "stable-key-1".to_owned()),
        sink_guarantee: guarantee,
        authorization: AuthorizationUse {
            grant_id: once_authorization_id(&binding.run_id, &binding.action_digest)
                .expect("authorization ID should canonicalize"),
            grant_digest: authorization_digest(&binding, 1)
                .expect("authorization should canonicalize"),
            max_uses: 1,
            binding,
        },
        receipt_provenance: Some(provenance),
    }
}

fn seed_intent<S: RunStore>(store: &mut S, guarantee: SinkGuarantee) -> RunState {
    let created = store
        .append(
            ExpectedHead::Empty,
            seed_event(
                "seed-event-1",
                RunEventBody::RunCreated {
                    goal: "durable effect".to_owned(),
                },
            ),
        )
        .expect("run should be created")
        .state;
    let planned = append_body(
        store,
        &created,
        "seed-event-2",
        RunEventBody::StepPlanned {
            step_id: STEP_ID.to_owned(),
            objective: "perform effect".to_owned(),
            depends_on: Vec::new(),
        },
    );
    let effect = effect_intent(&planned, guarantee);
    let material_digest = invocation_material_digest(&serde_json::json!({"operation": "test"}))
        .expect("material should canonicalize");
    let material = InvocationMaterialRecord::new(
        RUN_ID,
        STEP_ID,
        &effect,
        material_digest,
        InvocationMaterialRetention::Ephemeral,
    )
    .expect("material should bind");
    store
        .append_with_invocation_material(
            ExpectedHead::from_state(&planned),
            seed_event(
                "seed-event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: STEP_ID.to_owned(),
                    intent: Box::new(effect),
                },
            ),
            material,
        )
        .expect("intent and material should commit")
        .state
}

fn seed_executing<S: RunStore>(store: &mut S, guarantee: SinkGuarantee) -> RunState {
    let intent = seed_intent(store, guarantee);
    append_body(
        store,
        &intent,
        "seed-event-4",
        RunEventBody::EffectExecutionStarted {
            step_id: STEP_ID.to_owned(),
            effect_id: EFFECT_ID.to_owned(),
        },
    )
}

fn seed_unknown<S: RunStore>(store: &mut S, guarantee: SinkGuarantee) -> RunState {
    let executing = seed_executing(store, guarantee);
    append_body(
        store,
        &executing,
        "seed-event-5",
        RunEventBody::EffectBecameUnknown {
            step_id: STEP_ID.to_owned(),
            effect_id: EFFECT_ID.to_owned(),
            reason: "lost acknowledgement".to_owned(),
        },
    )
}

fn seed_reconciling<S: RunStore>(store: &mut S) -> RunState {
    let unknown = seed_unknown(store, SinkGuarantee::QueryByKey);
    append_body(
        store,
        &unknown,
        "seed-event-6",
        RunEventBody::ReconciliationStarted {
            step_id: STEP_ID.to_owned(),
            effect_id: EFFECT_ID.to_owned(),
        },
    )
}

fn lease() -> (TempDir, LocalRunLease) {
    let directory = tempdir().expect("temporary run directory should exist");
    let lease = LocalRunLease::try_acquire(RUN_ID, directory.path().join("run.lock"))
        .expect("run lease should be acquired");
    (directory, lease)
}

#[test]
fn effect_call_happens_only_after_start_marker_is_committed() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.executions.push_back(ExecutionObservation::Succeeded {
        evidence_digest: "sha256:receipt-1".to_owned(),
    });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);

    let report = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared))
        .expect("effect should complete");

    assert_eq!(report.action, DriveAction::EffectSucceeded);
    assert_eq!(report.state.steps[STEP_ID].status, StepStatus::Validating);
    assert_eq!(
        trace.borrow().as_slice(),
        [
            "store:execution_started",
            "sink:execute",
            "store:effect_succeeded"
        ]
    );
}

#[test]
fn definite_effect_failure_commits_its_receipt() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.executions.push_back(ExecutionObservation::Failed {
        evidence_digest: "sha256:failure-receipt-1".to_owned(),
    });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);

    let report = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared))
        .expect("definite failure should be committed");

    assert_eq!(report.action, DriveAction::EffectFailed);
    assert_eq!(report.state.steps[STEP_ID].status, StepStatus::Failed);
    assert_eq!(
        report.state.steps[STEP_ID]
            .effect_evidence_digest
            .as_deref(),
        Some("sha256:failure-receipt-1")
    );
    assert_eq!(
        trace.borrow().as_slice(),
        [
            "store:execution_started",
            "sink:execute",
            "store:effect_failed"
        ]
    );
}

#[test]
fn empty_execution_receipt_fails_closed_and_recovers_as_unknown() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.executions.push_back(ExecutionObservation::Succeeded {
        evidence_digest: "  ".to_owned(),
    });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);

    let first = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared));

    assert!(matches!(
        first,
        Err(RuntimeError::InvalidSinkObservation {
            observation: "execution_succeeded",
            field: "evidence_digest"
        })
    ));
    assert_eq!(sink.execute_calls, 1);
    assert_eq!(
        store
            .load()
            .expect("store should load")
            .expect("run should exist")
            .state
            .steps[STEP_ID]
            .status,
        StepStatus::Executing
    );

    let recovered = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("invalid receipt should recover conservatively");
    assert_eq!(recovered.action, DriveAction::ExecutionRecoveredAsUnknown);
    assert_eq!(sink.execute_calls, 1);
}

#[test]
fn failed_start_commit_prevents_physical_effect() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    store.fail_once_on(EventKind::ExecutionStarted);
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);

    let result = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared));

    assert!(matches!(
        result,
        Err(RuntimeError::Store(StoreError::InjectedFault(_)))
    ));
    assert_eq!(sink.execute_calls, 0);
    assert_eq!(
        store
            .load()
            .expect("store should load")
            .expect("run should exist")
            .state
            .steps[STEP_ID]
            .status,
        StepStatus::IntentCommitted
    );
}

#[test]
fn failed_event_metadata_creation_prevents_start_and_physical_effect() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    let mut events = FailingEvents;
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);

    let result = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared));

    assert!(matches!(result, Err(RuntimeError::EventFactory(_))));
    assert_eq!(sink.execute_calls, 0);
    assert!(trace.borrow().is_empty());
}

#[test]
fn invalid_start_timestamp_prevents_started_commit_and_physical_effect() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    let mut events = InvalidTimestampEvents;
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);

    let error = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared))
        .expect_err("invalid start timestamp must fail closed");

    assert!(matches!(error, RuntimeError::EventMetadata(_)));
    let rendered = format!("{error}\n{error:?}");
    assert!(!rendered.contains("RAW-START-TIMESTAMP-SENTINEL"));
    assert_eq!(sink.execute_calls, 0);
    assert!(trace.borrow().is_empty());
    assert_eq!(
        store
            .load()
            .expect("store should load")
            .expect("Run should exist")
            .state
            .steps[STEP_ID]
            .status,
        StepStatus::IntentCommitted
    );
}

#[test]
fn lost_outcome_commit_recovers_to_unknown_without_duplicate_execution() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    store.fail_once_on(EventKind::EffectSucceeded);
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.executions.push_back(ExecutionObservation::Succeeded {
        evidence_digest: "sha256:receipt-1".to_owned(),
    });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);

    let first = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared));
    assert!(matches!(
        first,
        Err(RuntimeError::Store(StoreError::InjectedFault(_)))
    ));
    assert_eq!(sink.execute_calls, 1);
    assert_eq!(
        store
            .load()
            .expect("store should load")
            .expect("run should exist")
            .state
            .steps[STEP_ID]
            .status,
        StepStatus::Executing
    );

    let recovered = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("executing state should recover conservatively");
    assert_eq!(recovered.action, DriveAction::ExecutionRecoveredAsUnknown);
    assert_eq!(
        recovered.state.steps[STEP_ID].status,
        StepStatus::EffectUnknown
    );
    assert_eq!(sink.execute_calls, 1);
}

#[test]
fn ambiguous_live_result_is_committed_as_unknown() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.executions.push_back(ExecutionObservation::Unknown {
        reason: "timeout after request write".to_owned(),
    });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);

    let report = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared))
        .expect("unknown result should be durable");

    assert_eq!(report.action, DriveAction::EffectUnknown);
    assert_eq!(
        report.state.steps[STEP_ID].status,
        StepStatus::EffectUnknown
    );
    assert_eq!(sink.execute_calls, 1);
}

#[test]
fn non_queryable_unknown_effect_requires_manual_intervention() {
    for guarantee in [SinkGuarantee::None, SinkGuarantee::DeduplicateByKey] {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut store = RecordingStore::new(Rc::clone(&trace));
        seed_unknown(&mut store, guarantee);
        let mut sink = ScriptedSink::new(Rc::clone(&trace));
        let mut events = DeterministicEvents::default();
        let (_directory, lease) = lease();

        let report = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
            .drive_step(STEP_ID, None)
            .expect("manual state should be committed");

        assert_eq!(report.action, DriveAction::ManualRequired);
        assert_eq!(
            report.state.steps[STEP_ID].status,
            StepStatus::ManualRequired
        );
        assert_eq!(sink.execute_calls, 0);
        assert_eq!(sink.reconcile_calls, 0);
    }
}

#[test]
fn queryable_unknown_effect_reconciles_as_applied() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_unknown(&mut store, SinkGuarantee::QueryByKey);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.reconciliations
        .push_back(ReconciliationObservation::Applied {
            evidence_digest: "sha256:query-evidence-1".to_owned(),
        });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();

    let report = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("reconciliation should complete");

    assert_eq!(report.action, DriveAction::ReconciliationApplied);
    assert_eq!(report.state.steps[STEP_ID].status, StepStatus::Validating);
    assert_eq!(
        trace.borrow().as_slice(),
        [
            "store:reconciliation_started",
            "sink:reconcile",
            "store:reconciliation_resolved"
        ]
    );
}

#[test]
fn definite_reconciliation_failure_commits_its_evidence() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_unknown(&mut store, SinkGuarantee::QueryByKey);
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.reconciliations
        .push_back(ReconciliationObservation::Failed {
            evidence_digest: "sha256:reconciliation-failure-1".to_owned(),
        });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();

    let report = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("definite reconciliation failure should be committed");

    assert_eq!(report.action, DriveAction::ReconciliationFailed);
    assert_eq!(report.state.steps[STEP_ID].status, StepStatus::Failed);
    assert_eq!(
        report.state.steps[STEP_ID]
            .reconciliation_evidence_digest
            .as_deref(),
        Some("sha256:reconciliation-failure-1")
    );
}

#[test]
fn empty_reconciliation_evidence_keeps_query_safe_to_resume() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_unknown(&mut store, SinkGuarantee::QueryByKey);
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.reconciliations
        .push_back(ReconciliationObservation::Applied {
            evidence_digest: String::new(),
        });
    sink.reconciliations
        .push_back(ReconciliationObservation::Applied {
            evidence_digest: "sha256:query-evidence-after-resume".to_owned(),
        });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();

    let first = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None);
    assert!(matches!(
        first,
        Err(RuntimeError::InvalidSinkObservation {
            observation: "reconciliation_applied",
            field: "evidence_digest"
        })
    ));
    assert_eq!(
        store
            .load()
            .expect("store should load")
            .expect("run should exist")
            .state
            .steps[STEP_ID]
            .status,
        StepStatus::Reconciling
    );

    let resumed = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("read-only reconciliation should be repeatable");
    assert_eq!(resumed.action, DriveAction::ReconciliationApplied);
    assert_eq!(resumed.state.steps[STEP_ID].status, StepStatus::Validating);
    assert_eq!(sink.execute_calls, 0);
    assert_eq!(sink.reconcile_calls, 2);
}

#[test]
fn proved_not_applied_reuses_intent_without_reconsuming_authorization() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_unknown(&mut store, SinkGuarantee::QueryByKey);
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.reconciliations
        .push_back(ReconciliationObservation::NotApplied {
            evidence_digest: "sha256:not-applied-1".to_owned(),
        });
    sink.executions.push_back(ExecutionObservation::Succeeded {
        evidence_digest: "sha256:receipt-2".to_owned(),
    });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let reconciled = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("not-applied evidence should allow resume");
    assert_eq!(reconciled.action, DriveAction::ReconciliationNotApplied);
    assert_eq!(
        reconciled.state.steps[STEP_ID].status,
        StepStatus::IntentCommitted
    );
    let prepared = prepared_effect(&store);
    let executed = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared))
        .expect("same intent should execute after proof");

    assert_eq!(executed.action, DriveAction::EffectSucceeded);
    assert_eq!(executed.state.authorization_consumption.len(), 1);
    assert_eq!(
        executed
            .state
            .authorization_consumption
            .values()
            .next()
            .expect("one authorization should exist")
            .uses,
        1
    );
    assert_eq!(sink.execute_calls, 1);
    assert_eq!(sink.reconcile_calls, 1);
}

#[test]
fn proved_not_applied_still_respects_durable_attempt_limit() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_unknown(&mut store, SinkGuarantee::QueryByKey);
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.reconciliations
        .push_back(ReconciliationObservation::NotApplied {
            evidence_digest: "sha256:not-applied-limit".to_owned(),
        });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let policy = RuntimePolicy::new(NonZeroU32::new(1).expect("one is non-zero"));
    DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .with_policy(policy)
        .drive_step(STEP_ID, None)
        .expect("reconciliation should prove not applied");
    let prepared = prepared_effect(&store);
    let retry = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .with_policy(policy)
        .drive_step(STEP_ID, Some(prepared));

    assert!(matches!(
        retry,
        Err(RuntimeError::ExecutionAttemptLimitReached {
            attempts: 1,
            maximum: 1,
            ..
        })
    ));
    assert_eq!(sink.execute_calls, 0);
}

#[test]
fn restart_during_reconciliation_reissues_only_the_read_only_query() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_reconciling(&mut store);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.reconciliations
        .push_back(ReconciliationObservation::Applied {
            evidence_digest: "sha256:query-evidence-2".to_owned(),
        });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();

    let report = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("reconciliation should resume");

    assert_eq!(report.action, DriveAction::ReconciliationApplied);
    assert_eq!(sink.execute_calls, 0);
    assert_eq!(sink.reconcile_calls, 1);
    assert_eq!(
        trace.borrow().as_slice(),
        ["sink:reconcile", "store:reconciliation_resolved"]
    );
}

#[test]
fn inconclusive_reconciliation_fails_closed_to_manual() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_unknown(&mut store, SinkGuarantee::DeduplicateAndQuery);
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    sink.reconciliations
        .push_back(ReconciliationObservation::Inconclusive {
            reason: "query endpoint unavailable".to_owned(),
        });
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();

    let report = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("inconclusive query should become manual");

    assert_eq!(report.action, DriveAction::ManualRequired);
    assert_eq!(
        report.state.steps[STEP_ID].status,
        StepStatus::ManualRequired
    );
}

#[test]
fn missing_prepared_effect_never_starts_execution() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();

    let result = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None);

    assert!(matches!(
        result,
        Err(RuntimeError::PreparedEffectRequired { .. })
    ));
    assert_eq!(sink.execute_calls, 0);
    assert!(trace.borrow().is_empty());
}

#[test]
fn prepared_effect_from_an_older_head_never_starts_execution() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let prepared = prepared_effect(&store);
    let current = store
        .load()
        .expect("store should load")
        .expect("Run should exist")
        .state;
    append_body(
        &mut store,
        &current,
        "seed-unrelated-step",
        RunEventBody::StepPlanned {
            step_id: "step-unrelated".to_owned(),
            objective: "advance the journal head".to_owned(),
            depends_on: Vec::new(),
        },
    );
    trace.borrow_mut().clear();

    let result = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(prepared));

    assert!(matches!(
        result,
        Err(RuntimeError::PreparedEffectHeadChanged { .. })
    ));
    assert_eq!(sink.execute_calls, 0);
    assert!(trace.borrow().is_empty());
}

#[test]
fn prepared_effect_bound_to_another_step_never_starts_execution() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let mut mismatched = prepared_effect(&store);
    mismatched.0.corrupt_step_id_for_test();

    let result = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(mismatched));

    assert!(matches!(
        result,
        Err(RuntimeError::PreparedEffectBindingMismatch { .. })
    ));
    assert_eq!(sink.execute_calls, 0);
    assert!(trace.borrow().is_empty());
}

#[test]
fn prepared_effect_bound_to_another_effect_never_starts_execution() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    let mut events = DeterministicEvents::default();
    let (_directory, lease) = lease();
    let mut mismatched = prepared_effect(&store);
    mismatched.0.corrupt_effect_id_for_test();

    let result = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, Some(mismatched));

    assert!(matches!(
        result,
        Err(RuntimeError::PreparedEffectBindingMismatch { .. })
    ));
    assert_eq!(sink.execute_calls, 0);
    assert!(trace.borrow().is_empty());
}

#[test]
fn local_run_lease_is_exclusive_and_released_on_drop() {
    let directory = tempdir().expect("temporary run directory should exist");
    let path = directory.path().join("run.lock");
    let first = LocalRunLease::try_acquire(RUN_ID, &path).expect("first lease should succeed");

    assert!(matches!(
        LocalRunLease::try_acquire(RUN_ID, &path),
        Err(LeaseError::AlreadyHeld { .. })
    ));
    drop(first);
    LocalRunLease::try_acquire(RUN_ID, &path).expect("lease should be released after drop");
}

#[test]
fn lease_for_another_run_is_rejected_before_any_effect() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut store = RecordingStore::new(Rc::clone(&trace));
    seed_intent(&mut store, SinkGuarantee::None);
    trace.borrow_mut().clear();
    let mut sink = ScriptedSink::new(Rc::clone(&trace));
    let mut events = DeterministicEvents::default();
    let directory = tempdir().expect("temporary run directory should exist");
    let wrong_lease = LocalRunLease::try_acquire("another-run", directory.path().join("run.lock"))
        .expect("lease should be acquired");
    let prepared = prepared_effect(&store);

    let result = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &wrong_lease)
        .drive_step(STEP_ID, Some(prepared));

    assert!(matches!(result, Err(RuntimeError::LeaseRunMismatch { .. })));
    assert_eq!(sink.execute_calls, 0);
    assert!(trace.borrow().is_empty());
}

#[derive(Debug)]
struct CrashAfterCounterSink {
    counter_path: PathBuf,
}

impl EffectSink for CrashAfterCounterSink {
    type Prepared = TestPreparedEffect;

    fn execute(
        &mut self,
        _intent: &EffectIntent,
        _prepared: Self::Prepared,
    ) -> ExecutionObservation {
        let current: u64 = fs::read_to_string(&self.counter_path)
            .expect("counter should be readable")
            .parse()
            .expect("counter should contain a number");
        fs::write(&self.counter_path, (current + 1).to_string())
            .expect("physical counter effect should be written");
        std::process::exit(86);
    }

    fn reconcile(&mut self, _intent: &EffectIntent) -> ReconciliationObservation {
        panic!("crash sink does not support reconciliation")
    }
}

#[test]
fn process_exit_after_physical_effect_preserves_unknown_without_duplicate() {
    const CHILD_MARKER: &str = "XGENY_EFFECT_CRASH_CHILD";
    const DATABASE_PATH: &str = "XGENY_EFFECT_CRASH_DB";
    const LOCK_PATH: &str = "XGENY_EFFECT_CRASH_LOCK";
    const COUNTER_PATH: &str = "XGENY_EFFECT_CRASH_COUNTER";

    if std::env::var_os(CHILD_MARKER).is_some() {
        let database_path = std::env::var_os(DATABASE_PATH).expect("child database path");
        let lock_path = std::env::var_os(LOCK_PATH).expect("child lock path");
        let counter_path = std::env::var_os(COUNTER_PATH).expect("child counter path");
        let mut store = SqliteRunStore::open(database_path).expect("child store should open");
        let lease =
            LocalRunLease::try_acquire(RUN_ID, lock_path).expect("child lease should acquire");
        let mut sink = CrashAfterCounterSink {
            counter_path: counter_path.into(),
        };
        let mut events = DeterministicEvents::default();
        let prepared = prepared_effect(&store);
        let _never_returns = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
            .drive_step(STEP_ID, Some(prepared));
        panic!("child must exit after applying the physical effect");
    }

    let directory = tempdir().expect("temporary run directory should exist");
    let database_path = directory.path().join("run.db");
    let lock_path = directory.path().join("run.lock");
    let counter_path = directory.path().join("counter.txt");
    {
        let mut store = SqliteRunStore::open(&database_path).expect("store should open");
        seed_intent(&mut store, SinkGuarantee::None);
    }
    fs::write(&counter_path, "0").expect("counter should initialize");

    let status = Command::new(std::env::current_exe().expect("test executable should exist"))
        .args([
            "--exact",
            "runtime_tests::process_exit_after_physical_effect_preserves_unknown_without_duplicate",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER, "1")
        .env(DATABASE_PATH, &database_path)
        .env(LOCK_PATH, &lock_path)
        .env(COUNTER_PATH, &counter_path)
        .status()
        .expect("crash child should start");
    assert_eq!(status.code(), Some(86));
    assert_eq!(
        fs::read_to_string(&counter_path).expect("counter should be readable"),
        "1"
    );

    let mut store = SqliteRunStore::open(&database_path).expect("store should recover");
    assert_eq!(
        store
            .load()
            .expect("store should load")
            .expect("run should exist")
            .state
            .steps[STEP_ID]
            .status,
        StepStatus::Executing
    );
    let lease = LocalRunLease::try_acquire(RUN_ID, &lock_path)
        .expect("crashed process should release lease");
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut sink = ScriptedSink::new(trace);
    let mut events = DeterministicEvents::default();

    let recovered = DurableEffectRuntime::new(&mut store, &mut sink, &mut events, &lease)
        .drive_step(STEP_ID, None)
        .expect("recovery should mark uncertainty");

    assert_eq!(recovered.action, DriveAction::ExecutionRecoveredAsUnknown);
    assert_eq!(
        recovered.state.steps[STEP_ID].status,
        StepStatus::EffectUnknown
    );
    assert_eq!(sink.execute_calls, 0);
    assert_eq!(
        fs::read_to_string(&counter_path).expect("counter should be readable"),
        "1"
    );
}
