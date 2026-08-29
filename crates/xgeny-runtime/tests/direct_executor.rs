use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use xgeny_domain::{
    Architecture, AuthState, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef,
    DataBoundary, EffectClass, ExecutionStyle, GrantLifetime, HealthStatus, OperatingSystem,
    Platform, PolicySource, PolicySourceKind, ProtocolDocument, TrustLevel,
};
use xgeny_local_store::{
    Commit, ExpectedHead, MemoryRunStore, RunSnapshot, RunStore, SqliteRunStore, StoreError,
};
use xgeny_policy::{
    PolicyAllowance, PolicyContribution, PolicyInputs, ResolvedPermissionRequest,
    ResourceResolutionFailure, ResourceResolver,
};
use xgeny_runtime::{
    AdapterEvidenceDigest, AdapterExecutionObservation, AdapterPrepareFailure,
    AdapterPrepareRequest, AdapterReconcileRequest, AdapterReconciliationInconclusiveReason,
    AdapterReconciliationObservation, AdapterRegistryError, AdmissionOutcome, AdmissionRequest,
    CapabilityRegistry, DirectExecutor, DirectExecutorError, DriveAction, EffectAdapter,
    EffectAdapterRegistry, EventFactory, EventFactoryError, EventMetadata, InvocationAdmission,
    InvocationMaterial, InvocationMaterialProvider, InvocationMaterialRecovery, LocalRunLease,
    MaterialProviderFailure, MaterialProviderRegistry, MaterialProviderRegistryError,
    PreparedAdapterInvocation, RequiredRouteFeatures, RouteRequest, RuntimePolicy,
};
use xgeny_workgraph::{
    InvocationMaterialRecord, InvocationMaterialRetention, ReconstructableMaterialReference,
    RunEvent, RunEventBody, RunState, SinkGuarantee, StepStatus,
};

const RUN_ID: &str = "run-direct-1";
const STEP_ID: &str = "step-direct-1";
const AUTHORITY: &str = "local:test";
const AUTHORITY_EPOCH: u64 = 23;
const RAW_PATH: &str = "/workspace/area/../result.txt";
const CANONICAL_PATH: &str = "/workspace/result.txt";
const SECRET_SENTINEL: &str = "DIRECT-EXECUTOR-RAW-MATERIAL-MUST-STAY-EPHEMERAL";
const CRASH_CHILD_MARKER: &str = "XGENY_DIRECT_CRASH_CHILD";
const CRASH_DATABASE_PATH: &str = "XGENY_DIRECT_CRASH_DB";
const CRASH_LOCK_PATH: &str = "XGENY_DIRECT_CRASH_LOCK";
const CRASH_COUNTER_PATH: &str = "XGENY_DIRECT_CRASH_COUNTER";

fn digest(byte: char) -> AdapterEvidenceDigest {
    AdapterEvidenceDigest::new(format!("sha256:{}", byte.to_string().repeat(64)))
        .expect("test digest should be canonical")
}

fn assert_persisted_artifacts_exclude(directory: &Path, sentinels: &[&[u8]]) {
    for entry in fs::read_dir(directory).expect("Run directory should be readable") {
        let path = entry.expect("directory entry should be readable").path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).expect("persisted Run artifact should be readable");
        for sentinel in sentinels {
            assert!(
                !bytes
                    .windows(sentinel.len())
                    .any(|window| window == *sentinel),
                "plaintext material leaked into {}",
                path.display()
            );
        }
    }
}

#[derive(Debug, Default)]
struct CanonicalResolver;

impl ResourceResolver for CanonicalResolver {
    fn resolve(&self, _scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        match resource {
            RAW_PATH | CANONICAL_PATH => Ok(CANONICAL_PATH.to_owned()),
            _ => Err(ResourceResolutionFailure::OutsideHostBoundary),
        }
    }
}

#[derive(Debug, Default)]
struct DeterministicEvents;

impl EventFactory for DeterministicEvents {
    fn create_metadata(&mut self, state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        let next = state
            .journal_sequence
            .checked_add(1)
            .ok_or_else(|| EventFactoryError::new("sequence overflow"))?;
        Ok(EventMetadata {
            event_id: format!("direct-event-{next}"),
            recorded_at: "2026-08-29T15:00:00Z".to_owned(),
        })
    }
}

#[derive(Debug)]
struct RecordingStore {
    inner: MemoryRunStore,
    trace: Rc<RefCell<Vec<&'static str>>>,
    fail_started_once: bool,
    fail_succeeded_once: bool,
    material_available: Rc<Cell<bool>>,
}

impl RecordingStore {
    fn new(trace: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            inner: MemoryRunStore::new(),
            trace,
            fail_started_once: false,
            fail_succeeded_once: false,
            material_available: Rc::new(Cell::new(true)),
        }
    }

    fn record(&mut self, body: &RunEventBody) -> Result<(), StoreError> {
        match body {
            RunEventBody::EffectExecutionStarted { .. } => {
                self.trace.borrow_mut().push("store:started");
                if self.fail_started_once {
                    self.fail_started_once = false;
                    return Err(StoreError::InjectedFault("lost start commit"));
                }
            }
            RunEventBody::EffectSucceeded { .. } => {
                self.trace.borrow_mut().push("store:succeeded");
                if self.fail_succeeded_once {
                    self.fail_succeeded_once = false;
                    return Err(StoreError::InjectedFault("lost effect outcome commit"));
                }
            }
            RunEventBody::EffectBecameUnknown { .. } => {
                self.trace.borrow_mut().push("store:unknown");
            }
            _ => {}
        }
        Ok(())
    }
}

impl RunStore for RecordingStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        self.record(&event.body)?;
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
        self.record(&event.body)?;
        self.inner
            .append_with_invocation_material(expected, event, material)
    }

    fn load_invocation_material(
        &self,
        effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        if !self.material_available.get() {
            return Ok(None);
        }
        self.inner.load_invocation_material(effect_id)
    }
}

#[derive(Debug, Default)]
struct AdapterCounters {
    prepares: Cell<usize>,
    executes: Cell<usize>,
    reconciles: Cell<usize>,
}

struct FakeSession {
    counters: Rc<AdapterCounters>,
    trace: Rc<RefCell<Vec<&'static str>>>,
    observation: AdapterExecutionObservation,
}

impl PreparedAdapterInvocation for FakeSession {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        self.counters.executes.set(self.counters.executes.get() + 1);
        self.trace.borrow_mut().push("adapter:execute");
        self.observation
    }
}

struct FakeAdapter {
    counters: Rc<AdapterCounters>,
    trace: Rc<RefCell<Vec<&'static str>>>,
    executions: VecDeque<AdapterExecutionObservation>,
    reconciliations: VecDeque<AdapterReconciliationObservation>,
    prepare_failure: Option<AdapterPrepareFailure>,
    request_debug: Rc<RefCell<String>>,
    drop_material_after_prepare: Option<Rc<Cell<bool>>>,
}

impl FakeAdapter {
    fn succeeding(counters: Rc<AdapterCounters>, trace: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            counters,
            trace,
            executions: VecDeque::from([
                AdapterExecutionObservation::Succeeded {
                    receipt_digest: digest('a'),
                },
                AdapterExecutionObservation::Succeeded {
                    receipt_digest: digest('b'),
                },
            ]),
            reconciliations: VecDeque::from([AdapterReconciliationObservation::Inconclusive {
                reason: AdapterReconciliationInconclusiveReason::QueryUnavailable,
            }]),
            prepare_failure: None,
            request_debug: Rc::new(RefCell::new(String::new())),
            drop_material_after_prepare: None,
        }
    }
}

impl EffectAdapter for FakeAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        self.counters.prepares.set(self.counters.prepares.get() + 1);
        self.trace.borrow_mut().push("adapter:prepare");
        *self.request_debug.borrow_mut() = format!("{request:?}");
        assert_eq!(
            request.normalized_arguments()["path"],
            json!(CANONICAL_PATH)
        );
        assert_eq!(
            request.normalized_arguments()["marker"],
            json!(SECRET_SENTINEL)
        );
        if let Some(failure) = self.prepare_failure.take() {
            return Err(failure);
        }
        if let Some(material_available) = &self.drop_material_after_prepare {
            material_available.set(false);
        }
        Ok(Box::new(FakeSession {
            counters: Rc::clone(&self.counters),
            trace: Rc::clone(&self.trace),
            observation: self
                .executions
                .pop_front()
                .expect("execution observation should be scripted"),
        }))
    }

    fn reconcile(
        &mut self,
        _request: AdapterReconcileRequest<'_>,
    ) -> AdapterReconciliationObservation {
        self.counters
            .reconciles
            .set(self.counters.reconciles.get() + 1);
        self.trace.borrow_mut().push("adapter:reconcile");
        self.reconciliations
            .pop_front()
            .expect("reconciliation observation should be scripted")
    }
}

#[derive(Debug)]
struct FixedProvider {
    calls: Rc<Cell<usize>>,
    material: Value,
}

impl InvocationMaterialProvider for FixedProvider {
    fn reconstruct(
        &mut self,
        _reference_id: &str,
        _revision: &str,
    ) -> Result<Value, MaterialProviderFailure> {
        self.calls.set(self.calls.get() + 1);
        Ok(self.material.clone())
    }
}

struct CrashSession {
    counter_path: PathBuf,
}

impl PreparedAdapterInvocation for CrashSession {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        let current: u64 = fs::read_to_string(&self.counter_path)
            .expect("counter should be readable")
            .parse()
            .expect("counter should contain a number");
        fs::write(&self.counter_path, (current + 1).to_string())
            .expect("physical fake effect should commit");
        std::process::exit(86);
    }
}

struct CrashAdapter {
    counter_path: PathBuf,
}

impl EffectAdapter for CrashAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        assert_eq!(
            request.normalized_arguments()["marker"],
            json!(SECRET_SENTINEL)
        );
        Ok(Box::new(CrashSession {
            counter_path: self.counter_path.clone(),
        }))
    }

    fn reconcile(
        &mut self,
        _request: AdapterReconcileRequest<'_>,
    ) -> AdapterReconciliationObservation {
        panic!("crash adapter does not reconcile")
    }
}

fn definition_fixture() -> CapabilityDefinitionBody {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
    ))
    .expect("definition fixture should deserialize");
    let ProtocolDocument::CapabilityDefinition(mut definition) = document else {
        panic!("expected CapabilityDefinition fixture")
    };
    "xgeny.fs/write-marker".clone_into(&mut definition.metadata.id);
    "Write marker".clone_into(&mut definition.metadata.display_name);
    definition.spec.input_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["path", "marker"],
        "properties": {
            "path": {"type": "string", "minLength": 1},
            "marker": {"type": "string", "minLength": 1}
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

fn capability(definition: &CapabilityDefinitionBody) -> CapabilityRef {
    CapabilityRef {
        capability_id: definition.metadata.id.clone(),
        contract_version: definition.metadata.contract_version.clone(),
    }
}

fn instance_fixture(definition: &CapabilityDefinitionBody) -> CapabilityInstanceBody {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-instance.local-fs.json"
    ))
    .expect("instance fixture should deserialize");
    let ProtocolDocument::CapabilityInstance(mut instance) = document else {
        panic!("expected CapabilityInstance fixture")
    };
    "local.fs.writer.v1".clone_into(&mut instance.instance_id);
    instance.definition = capability(definition);
    "builtin://test/filesystem-writer".clone_into(&mut instance.binding.binding_ref);
    instance.binding.operation_ref = Some("writeMarker".to_owned());
    instance.binding.protocol_version = Some("1".to_owned());
    *instance
}

fn registry_with(
    definition: &CapabilityDefinitionBody,
    instance: CapabilityInstanceBody,
) -> CapabilityRegistry {
    let mut registry = CapabilityRegistry::new();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    registry
        .register_schema_validated_instance(instance)
        .expect("instance should register");
    registry
}

fn route_request(definition: &CapabilityDefinitionBody) -> RouteRequest {
    RouteRequest {
        capability: capability(definition),
        target_platform: Platform {
            os: OperatingSystem::Linux,
            arch: Architecture::X86_64,
        },
        required_features: RequiredRouteFeatures {
            execution_style: ExecutionStyle::Sync,
            cancellation: false,
            idempotency_key: true,
            idempotency_query: false,
        },
        allowed_trust_levels: vec![TrustLevel::Verified],
        allowed_data_boundaries: vec![DataBoundary::Local],
        trust_preference: Vec::new(),
        data_boundary_preference: Vec::new(),
        preferred_instance_ids: Vec::new(),
        pinned_instance_id: None,
    }
}

fn arguments() -> Value {
    json!({"path": RAW_PATH, "marker": SECRET_SENTINEL})
}

fn canonical_arguments() -> Value {
    json!({"path": CANONICAL_PATH, "marker": SECRET_SENTINEL})
}

fn seed<S: RunStore>(store: &mut S) {
    let created = store
        .append(
            ExpectedHead::Empty,
            RunEvent {
                event_id: "seed-direct-1".to_owned(),
                run_id: RUN_ID.to_owned(),
                authority: AUTHORITY.to_owned(),
                authority_epoch: AUTHORITY_EPOCH,
                recorded_at: "2026-08-29T15:00:00Z".to_owned(),
                body: RunEventBody::RunCreated {
                    goal: "execute one exact fake effect".to_owned(),
                },
            },
        )
        .expect("Run should seed")
        .state;
    store
        .append(
            ExpectedHead::from_state(&created),
            RunEvent {
                event_id: "seed-direct-2".to_owned(),
                run_id: RUN_ID.to_owned(),
                authority: AUTHORITY.to_owned(),
                authority_epoch: AUTHORITY_EPOCH,
                recorded_at: "2026-08-29T15:00:00Z".to_owned(),
                body: RunEventBody::StepPlanned {
                    step_id: STEP_ID.to_owned(),
                    objective: "write one fake marker".to_owned(),
                },
            },
        )
        .expect("Step should seed");
}

fn source(kind: PolicySourceKind, id: &str, byte: char) -> PolicySource {
    PolicySource {
        kind,
        id: id.to_owned(),
        digest: format!("sha256:{}", byte.to_string().repeat(64)),
    }
}

fn allowance(request: &ResolvedPermissionRequest) -> PolicyAllowance {
    PolicyAllowance::from_trusted_evaluation(
        request.requested_scopes().iter().cloned(),
        request.resources().iter().cloned(),
        request.critical_actions().iter().copied(),
        [GrantLifetime::Once],
    )
}

fn allow_inputs(request: &ResolvedPermissionRequest) -> PolicyInputs {
    PolicyInputs::local(
        request,
        PolicyContribution::allow(
            source(PolicySourceKind::Host, "host.local", '1'),
            allowance(request),
        ),
        PolicyContribution::allow(
            source(PolicySourceKind::UserProfile, "profile.default", '2'),
            allowance(request),
        ),
    )
}

fn acquire_lease() -> (TempDir, LocalRunLease) {
    let directory = tempdir().expect("temporary lease directory should exist");
    let lease = LocalRunLease::try_acquire(RUN_ID, directory.path().join("run.lock"))
        .expect("Run lease should acquire");
    (directory, lease)
}

fn admit<S: RunStore>(
    store: &mut S,
    lease: &LocalRunLease,
    registry: &CapabilityRegistry,
    definition: &CapabilityDefinitionBody,
    reconstructable: bool,
) -> Box<xgeny_runtime::AdmittedEffect> {
    let resolver = CanonicalResolver;
    let mut pending = InvocationAdmission::new()
        .prepare(
            store,
            lease,
            registry,
            &resolver,
            AdmissionRequest {
                step_id: STEP_ID.to_owned(),
                route: route_request(definition),
                arguments: arguments(),
            },
        )
        .expect("invocation should prepare");
    if reconstructable {
        pending = pending.with_reconstructable_material(
            ReconstructableMaterialReference::new("run-recipe", "recipe-1", "rev-1")
                .expect("reference should validate"),
        );
    }
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, registry, store, &mut events, lease)
        .expect("invocation should authorize");
    let AdmissionOutcome::Authorized(admitted) = outcome else {
        panic!("exact invocation should authorize")
    };
    admitted
}

fn append_body<S: RunStore>(
    store: &mut S,
    state: &RunState,
    event_id: &str,
    body: RunEventBody,
) -> RunState {
    store
        .append(
            ExpectedHead::from_state(state),
            RunEvent {
                event_id: event_id.to_owned(),
                run_id: RUN_ID.to_owned(),
                authority: AUTHORITY.to_owned(),
                authority_epoch: AUTHORITY_EPOCH,
                recorded_at: "2026-08-29T15:00:00Z".to_owned(),
                body,
            },
        )
        .expect("test event should append")
        .state
}

fn seed_queryable_unknown(
    store: &mut RecordingStore,
    lease: &LocalRunLease,
    registry: &CapabilityRegistry,
    definition: &CapabilityDefinitionBody,
) -> InvocationMaterial {
    let mut source = MemoryRunStore::new();
    seed(&mut source);
    let admitted = admit(&mut source, lease, registry, definition, false);
    let mut intent = admitted.commit().state.steps[STEP_ID]
        .intent
        .as_ref()
        .expect("admission should commit intent")
        .clone();
    let material = (*admitted)
        .into_ephemeral_material()
        .expect("admitted material should verify");
    intent.sink_guarantee = SinkGuarantee::QueryByKey;

    seed(store);
    let planned = store.load().expect("load").expect("Run").state;
    let record = InvocationMaterialRecord::new(
        RUN_ID,
        STEP_ID,
        &intent,
        material.record().material_digest(),
        InvocationMaterialRetention::Ephemeral,
    )
    .expect("queryable intent material should bind");
    assert_eq!(&record, material.record());
    let intent_committed = store
        .append_with_invocation_material(
            ExpectedHead::from_state(&planned),
            RunEvent {
                event_id: "seed-queryable-intent".to_owned(),
                run_id: RUN_ID.to_owned(),
                authority: AUTHORITY.to_owned(),
                authority_epoch: AUTHORITY_EPOCH,
                recorded_at: "2026-08-29T15:00:00Z".to_owned(),
                body: RunEventBody::EffectIntentCommitted {
                    step_id: STEP_ID.to_owned(),
                    intent: Box::new(intent.clone()),
                },
            },
            record,
        )
        .expect("queryable intent should commit")
        .state;
    let executing = append_body(
        store,
        &intent_committed,
        "seed-queryable-started",
        RunEventBody::EffectExecutionStarted {
            step_id: STEP_ID.to_owned(),
            effect_id: intent.effect_id.clone(),
        },
    );
    append_body(
        store,
        &executing,
        "seed-queryable-unknown",
        RunEventBody::EffectBecameUnknown {
            step_id: STEP_ID.to_owned(),
            effect_id: intent.effect_id,
            reason: "seeded_unknown_for_read_only_reconciliation".to_owned(),
        },
    );
    material
}

fn register_fake(
    adapters: &mut EffectAdapterRegistry,
    instance: &CapabilityInstanceBody,
    adapter: FakeAdapter,
) {
    adapters
        .register(&instance.binding, adapter)
        .expect("fake adapter should register");
}

fn run_direct_crash_child_if_requested() -> bool {
    if std::env::var_os(CRASH_CHILD_MARKER).is_none() {
        return false;
    }
    let database_path = std::env::var_os(CRASH_DATABASE_PATH).expect("child database path");
    let lock_path = std::env::var_os(CRASH_LOCK_PATH).expect("child lock path");
    let counter_path: PathBuf = std::env::var_os(CRASH_COUNTER_PATH)
        .expect("child counter path")
        .into();
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    let mut store = SqliteRunStore::open(database_path).expect("child store should open");
    let lease = LocalRunLease::try_acquire(RUN_ID, lock_path).expect("child lease should acquire");
    let mut providers = MaterialProviderRegistry::new();
    providers
        .register(
            "run-recipe",
            FixedProvider {
                calls: Rc::new(Cell::new(0)),
                material: canonical_arguments(),
            },
        )
        .expect("child provider should register");
    let material = InvocationMaterialRecovery::new()
        .recover(
            &store,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut providers,
            STEP_ID,
        )
        .expect("child material should recover");
    let mut adapters = EffectAdapterRegistry::new();
    adapters
        .register(&instance.binding, CrashAdapter { counter_path })
        .expect("child adapter should register");
    let mut events = DeterministicEvents;
    let _never_returns = DirectExecutor::new().drive_step(
        &mut store,
        &mut events,
        &lease,
        &registry,
        &mut adapters,
        STEP_ID,
        Some(&material),
    );
    panic!("child must exit after applying the fake physical effect");
}

#[test]
fn exact_adapter_executes_once_only_after_the_durable_start_marker() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let counters = Rc::new(AdapterCounters::default());
    let rejected_replacement = Rc::new(AdapterCounters::default());
    let mut store = RecordingStore::new(Rc::clone(&trace));
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let admitted = admit(&mut store, &lease, &registry, &definition, false);
    let material = (*admitted)
        .into_ephemeral_material()
        .expect("same-process material should verify");
    trace.borrow_mut().clear();
    let adapter = FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace));
    let request_debug = Rc::clone(&adapter.request_debug);
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(&mut adapters, &instance, adapter);
    assert!(matches!(
        adapters.register(
            &instance.binding,
            FakeAdapter::succeeding(Rc::clone(&rejected_replacement), Rc::clone(&trace)),
        ),
        Err(AdapterRegistryError::DuplicateBinding)
    ));
    let mut events = DeterministicEvents;

    let report = DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut adapters,
            STEP_ID,
            Some(&material),
        )
        .expect("exact fake adapter should execute");

    assert_eq!(report.action, DriveAction::EffectSucceeded);
    assert_eq!(report.state.steps[STEP_ID].status, StepStatus::Validating);
    assert_eq!(counters.prepares.get(), 1);
    assert_eq!(counters.executes.get(), 1);
    assert_eq!(counters.reconciles.get(), 0);
    assert_eq!(rejected_replacement.prepares.get(), 0);
    assert_eq!(rejected_replacement.executes.get(), 0);
    assert_eq!(
        trace.borrow().as_slice(),
        [
            "adapter:prepare",
            "store:started",
            "adapter:execute",
            "store:succeeded"
        ]
    );
    assert!(!request_debug.borrow().contains(SECRET_SENTINEL));
    assert!(!request_debug.borrow().contains(CANONICAL_PATH));
    let journal = String::from_utf8(store.inner.export_jsonl().expect("journal should export"))
        .expect("journal should be UTF-8");
    assert!(!journal.contains(SECRET_SENTINEL));
    assert!(!journal.contains(CANONICAL_PATH));
    assert_eq!(
        report
            .state
            .authorization_consumption
            .values()
            .next()
            .expect("one authorization")
            .uses,
        1
    );
}

#[test]
fn prepare_failure_keeps_intent_committed_and_never_starts_or_executes() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let counters = Rc::new(AdapterCounters::default());
    let mut store = RecordingStore::new(Rc::clone(&trace));
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let admitted = admit(&mut store, &lease, &registry, &definition, false);
    let material = (*admitted).into_ephemeral_material().expect("material");
    trace.borrow_mut().clear();
    let mut adapter = FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace));
    adapter.prepare_failure = Some(AdapterPrepareFailure::ResourceUnavailable);
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(&mut adapters, &instance, adapter);
    let mut events = DeterministicEvents;

    let result = DirectExecutor::new().drive_step(
        &mut store,
        &mut events,
        &lease,
        &registry,
        &mut adapters,
        STEP_ID,
        Some(&material),
    );

    assert!(matches!(
        result,
        Err(DirectExecutorError::AdapterPrepare(
            AdapterPrepareFailure::ResourceUnavailable
        ))
    ));
    assert_eq!(counters.prepares.get(), 1);
    assert_eq!(counters.executes.get(), 0);
    assert_eq!(trace.borrow().as_slice(), ["adapter:prepare"]);
    assert_eq!(
        store.load().expect("load").expect("Run").state.steps[STEP_ID].status,
        StepStatus::IntentCommitted
    );

    trace.borrow_mut().clear();
    let retried = DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut adapters,
            STEP_ID,
            Some(&material),
        )
        .expect("the same borrowed ephemeral material should remain retryable");
    assert_eq!(retried.action, DriveAction::EffectSucceeded);
    assert_eq!(counters.prepares.get(), 2);
    assert_eq!(counters.executes.get(), 1);
    assert_eq!(
        trace.borrow().as_slice(),
        [
            "adapter:prepare",
            "store:started",
            "adapter:execute",
            "store:succeeded"
        ]
    );
    assert_eq!(
        retried
            .state
            .authorization_consumption
            .values()
            .next()
            .expect("one authorization")
            .uses,
        1
    );
}

#[test]
fn material_sidecar_drift_during_prepare_never_starts_or_executes() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let counters = Rc::new(AdapterCounters::default());
    let mut store = RecordingStore::new(Rc::clone(&trace));
    let material_available = Rc::clone(&store.material_available);
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(&mut store, &lease, &registry, &definition, false))
        .into_ephemeral_material()
        .expect("material");
    trace.borrow_mut().clear();
    let mut adapter = FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace));
    adapter.drop_material_after_prepare = Some(material_available);
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(&mut adapters, &instance, adapter);
    let mut events = DeterministicEvents;

    let result = DirectExecutor::new().drive_step(
        &mut store,
        &mut events,
        &lease,
        &registry,
        &mut adapters,
        STEP_ID,
        Some(&material),
    );

    assert!(matches!(result, Err(DirectExecutorError::Runtime(_))));
    assert_eq!(counters.prepares.get(), 1);
    assert_eq!(counters.executes.get(), 0);
    assert_eq!(trace.borrow().as_slice(), ["adapter:prepare"]);
    assert_eq!(
        store.load().expect("load").expect("Run").state.steps[STEP_ID].status,
        StepStatus::IntentCommitted
    );
}

#[test]
fn failed_start_commit_preserves_ephemeral_material_for_a_fresh_prepare_retry() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let counters = Rc::new(AdapterCounters::default());
    let mut store = RecordingStore::new(Rc::clone(&trace));
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(&mut store, &lease, &registry, &definition, false))
        .into_ephemeral_material()
        .expect("material");
    store.fail_started_once = true;
    trace.borrow_mut().clear();
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(
        &mut adapters,
        &instance,
        FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace)),
    );
    let mut events = DeterministicEvents;

    let first = DirectExecutor::new().drive_step(
        &mut store,
        &mut events,
        &lease,
        &registry,
        &mut adapters,
        STEP_ID,
        Some(&material),
    );

    assert!(matches!(first, Err(DirectExecutorError::Runtime(_))));
    assert_eq!(counters.prepares.get(), 1);
    assert_eq!(counters.executes.get(), 0);
    assert_eq!(
        store.load().expect("load").expect("Run").state.steps[STEP_ID].status,
        StepStatus::IntentCommitted
    );

    trace.borrow_mut().clear();
    let retried = DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut adapters,
            STEP_ID,
            Some(&material),
        )
        .expect("the retained material should support a fresh one-shot session");
    assert_eq!(retried.action, DriveAction::EffectSucceeded);
    assert_eq!(counters.prepares.get(), 2);
    assert_eq!(counters.executes.get(), 1);
    assert_eq!(
        trace.borrow().as_slice(),
        [
            "adapter:prepare",
            "store:started",
            "adapter:execute",
            "store:succeeded"
        ]
    );
}

#[test]
fn adapter_registry_is_exact_and_duplicate_registration_never_replaces() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let trace = Rc::new(RefCell::new(Vec::new()));
    let first = Rc::new(AdapterCounters::default());
    let second = Rc::new(AdapterCounters::default());
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(
        &mut adapters,
        &instance,
        FakeAdapter::succeeding(Rc::clone(&first), Rc::clone(&trace)),
    );

    assert!(matches!(
        adapters.register(
            &instance.binding,
            FakeAdapter::succeeding(Rc::clone(&second), Rc::clone(&trace)),
        ),
        Err(AdapterRegistryError::DuplicateBinding)
    ));
    assert_eq!(adapters.len(), 1);
    assert_eq!(first.prepares.get(), 0);
    assert_eq!(second.prepares.get(), 0);
    assert!(!format!("{adapters:?}").contains("filesystem-writer"));
}

#[test]
fn operation_and_protocol_mismatch_do_not_fallback_to_a_nearby_adapter() {
    for mutation in ["binding", "operation", "protocol"] {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let counters = Rc::new(AdapterCounters::default());
        let mut store = RecordingStore::new(Rc::clone(&trace));
        let definition = definition_fixture();
        let instance = instance_fixture(&definition);
        let registry = registry_with(&definition, instance.clone());
        seed(&mut store);
        let (_lease_directory, lease) = acquire_lease();
        let material = (*admit(&mut store, &lease, &registry, &definition, false))
            .into_ephemeral_material()
            .expect("material");
        let mut nearby = instance.binding.clone();
        match mutation {
            "binding" => nearby.binding_ref.push_str("-nearby"),
            "operation" => nearby.operation_ref = None,
            "protocol" => nearby.protocol_version = Some("2".to_owned()),
            _ => unreachable!(),
        }
        let mut adapters = EffectAdapterRegistry::new();
        adapters
            .register(
                &nearby,
                FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace)),
            )
            .expect("nearby adapter should register under its own exact key");
        let mut events = DeterministicEvents;

        let result = DirectExecutor::new().drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut adapters,
            STEP_ID,
            Some(&material),
        );

        assert!(
            matches!(
                result,
                Err(DirectExecutorError::AdapterNotRegistered { .. })
            ),
            "mutation: {mutation}"
        );
        assert_eq!(counters.prepares.get(), 0, "mutation: {mutation}");
        assert_eq!(counters.executes.get(), 0, "mutation: {mutation}");
        assert!(
            !trace.borrow().contains(&"store:started"),
            "mutation: {mutation}"
        );
    }
}

#[test]
fn dynamic_health_and_credential_states_fail_before_adapter_prepare() {
    for mutation in [
        "health-degraded",
        "health-unavailable",
        "health-unknown",
        "auth-available",
        "auth-required",
        "auth-expired",
        "auth-ref",
    ] {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let counters = Rc::new(AdapterCounters::default());
        let mut store = RecordingStore::new(Rc::clone(&trace));
        let definition = definition_fixture();
        let mut admitted_instance = instance_fixture(&definition);
        if mutation == "auth-ref" {
            admitted_instance.auth.auth_ref = Some("credential-slot".to_owned());
        }
        let admission_registry = registry_with(&definition, admitted_instance.clone());
        seed(&mut store);
        let (_lease_directory, lease) = acquire_lease();
        let material = (*admit(&mut store, &lease, &admission_registry, &definition, false))
            .into_ephemeral_material()
            .expect("material");
        let mut current_instance = admitted_instance.clone();
        match mutation {
            "health-degraded" => current_instance.health.status = HealthStatus::Degraded,
            "health-unavailable" => current_instance.health.status = HealthStatus::Unavailable,
            "health-unknown" => current_instance.health.status = HealthStatus::Unknown,
            "auth-available" => current_instance.auth.state = AuthState::Available,
            "auth-required" => current_instance.auth.state = AuthState::Required,
            "auth-expired" => current_instance.auth.state = AuthState::Expired,
            "auth-ref" => {}
            _ => unreachable!(),
        }
        let current_registry = registry_with(&definition, current_instance);
        let mut adapters = EffectAdapterRegistry::new();
        register_fake(
            &mut adapters,
            &admitted_instance,
            FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace)),
        );
        let mut events = DeterministicEvents;

        let result = DirectExecutor::new().drive_step(
            &mut store,
            &mut events,
            &lease,
            &current_registry,
            &mut adapters,
            STEP_ID,
            Some(&material),
        );

        assert!(
            matches!(
                (mutation, &result),
                (
                    "health-degraded" | "health-unavailable" | "health-unknown",
                    Err(DirectExecutorError::InstanceNotAvailable { .. })
                ) | (
                    "auth-available" | "auth-required" | "auth-expired" | "auth-ref",
                    Err(DirectExecutorError::CredentialWitnessUnavailable)
                )
            ),
            "mutation {mutation} returned {result:?}"
        );
        assert_eq!(counters.prepares.get(), 0, "mutation: {mutation}");
        assert_eq!(counters.executes.get(), 0, "mutation: {mutation}");
        assert!(!trace.borrow().contains(&"store:started"));
    }
}

#[test]
fn definition_and_instance_drift_fail_before_adapter_prepare() {
    for mutation in ["definition", "instance"] {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let counters = Rc::new(AdapterCounters::default());
        let mut store = RecordingStore::new(Rc::clone(&trace));
        let definition = definition_fixture();
        let instance = instance_fixture(&definition);
        let admission_registry = registry_with(&definition, instance.clone());
        seed(&mut store);
        let (_lease_directory, lease) = acquire_lease();
        let material = (*admit(&mut store, &lease, &admission_registry, &definition, false))
            .into_ephemeral_material()
            .expect("material");
        trace.borrow_mut().clear();

        let current_registry = if mutation == "definition" {
            let mut changed_definition = definition.clone();
            changed_definition.spec.input_schema["properties"]["marker"]["minLength"] = json!(2);
            registry_with(&changed_definition, instance_fixture(&changed_definition))
        } else {
            let mut changed_instance = instance.clone();
            changed_instance.binding.operation_ref = Some("differentOperation".to_owned());
            registry_with(&definition, changed_instance)
        };
        let mut adapters = EffectAdapterRegistry::new();
        register_fake(
            &mut adapters,
            &instance,
            FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace)),
        );
        let mut events = DeterministicEvents;

        let result = DirectExecutor::new().drive_step(
            &mut store,
            &mut events,
            &lease,
            &current_registry,
            &mut adapters,
            STEP_ID,
            Some(&material),
        );

        assert!(
            matches!(
                (mutation, &result),
                ("definition", Err(DirectExecutorError::DefinitionChanged))
                    | ("instance", Err(DirectExecutorError::InstanceBindingChanged))
            ),
            "mutation {mutation} returned {result:?}"
        );
        assert_eq!(counters.prepares.get(), 0, "mutation: {mutation}");
        assert_eq!(counters.executes.get(), 0, "mutation: {mutation}");
        assert!(trace.borrow().is_empty(), "mutation: {mutation}");
    }
}

#[test]
fn lost_outcome_commit_resumes_executing_without_prepare_or_duplicate_execution() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let counters = Rc::new(AdapterCounters::default());
    let mut store = RecordingStore::new(Rc::clone(&trace));
    store.fail_succeeded_once = true;
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(&mut store, &lease, &registry, &definition, false))
        .into_ephemeral_material()
        .expect("material");
    trace.borrow_mut().clear();
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(
        &mut adapters,
        &instance,
        FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace)),
    );
    let mut events = DeterministicEvents;

    let first = DirectExecutor::new().drive_step(
        &mut store,
        &mut events,
        &lease,
        &registry,
        &mut adapters,
        STEP_ID,
        Some(&material),
    );
    assert!(matches!(first, Err(DirectExecutorError::Runtime(_))));
    assert_eq!(counters.prepares.get(), 1);
    assert_eq!(counters.executes.get(), 1);
    assert_eq!(
        store.load().expect("load").expect("Run").state.steps[STEP_ID].status,
        StepStatus::Executing
    );

    let resumed = DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &CapabilityRegistry::new(),
            &mut EffectAdapterRegistry::new(),
            STEP_ID,
            None,
        )
        .expect("Executing must recover without any adapter");
    assert_eq!(resumed.action, DriveAction::ExecutionRecoveredAsUnknown);
    assert_eq!(
        resumed.state.steps[STEP_ID].status,
        StepStatus::EffectUnknown
    );
    assert_eq!(counters.prepares.get(), 1);
    assert_eq!(counters.executes.get(), 1);
    assert_eq!(counters.reconciles.get(), 0);
}

#[test]
fn exact_reconciliation_not_applied_reuses_the_same_material_without_new_authorization() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let counters = Rc::new(AdapterCounters::default());
    let mut store = RecordingStore::new(Rc::clone(&trace));
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    let (_lease_directory, lease) = acquire_lease();
    let material = seed_queryable_unknown(&mut store, &lease, &registry, &definition);
    trace.borrow_mut().clear();
    let mut adapter = FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace));
    adapter.reconciliations = VecDeque::from([AdapterReconciliationObservation::NotApplied {
        evidence_digest: digest('c'),
    }]);
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(&mut adapters, &instance, adapter);
    let mut events = DeterministicEvents;

    let reconciled = DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut adapters,
            STEP_ID,
            None,
        )
        .expect("exact adapter should prove the effect was not applied");
    assert_eq!(reconciled.action, DriveAction::ReconciliationNotApplied);
    assert_eq!(
        reconciled.state.steps[STEP_ID].status,
        StepStatus::IntentCommitted
    );
    assert_eq!(counters.reconciles.get(), 1);
    assert_eq!(counters.prepares.get(), 0);
    assert_eq!(counters.executes.get(), 0);
    assert_eq!(trace.borrow().as_slice(), ["adapter:reconcile"]);

    trace.borrow_mut().clear();
    let executed = DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut adapters,
            STEP_ID,
            Some(&material),
        )
        .expect("the retained material should create a fresh one-shot session");
    assert_eq!(executed.action, DriveAction::EffectSucceeded);
    assert_eq!(counters.prepares.get(), 1);
    assert_eq!(counters.executes.get(), 1);
    assert_eq!(counters.reconciles.get(), 1);
    assert_eq!(
        executed
            .state
            .authorization_consumption
            .values()
            .next()
            .expect("one authorization")
            .uses,
        1
    );
}

#[test]
fn attempt_limit_blocks_adapter_prepare_at_the_direct_executor_boundary() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let counters = Rc::new(AdapterCounters::default());
    let mut store = RecordingStore::new(Rc::clone(&trace));
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    let (_lease_directory, lease) = acquire_lease();
    let material = seed_queryable_unknown(&mut store, &lease, &registry, &definition);
    let mut adapter = FakeAdapter::succeeding(Rc::clone(&counters), Rc::clone(&trace));
    adapter.reconciliations = VecDeque::from([AdapterReconciliationObservation::NotApplied {
        evidence_digest: digest('d'),
    }]);
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(&mut adapters, &instance, adapter);
    let mut events = DeterministicEvents;
    DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut adapters,
            STEP_ID,
            None,
        )
        .expect("reconciliation should prove not applied");
    assert_eq!(counters.prepares.get(), 0);
    trace.borrow_mut().clear();
    let policy = RuntimePolicy::new(NonZeroU32::new(1).expect("one is non-zero"));

    let result = DirectExecutor::new().with_policy(policy).drive_step(
        &mut store,
        &mut events,
        &lease,
        &registry,
        &mut adapters,
        STEP_ID,
        Some(&material),
    );

    assert!(matches!(
        result,
        Err(DirectExecutorError::Runtime(
            xgeny_runtime::RuntimeError::ExecutionAttemptLimitReached {
                attempts: 1,
                maximum: 1,
                ..
            }
        ))
    ));
    assert_eq!(counters.prepares.get(), 0);
    assert_eq!(counters.executes.get(), 0);
    assert!(trace.borrow().is_empty());
}

#[test]
fn direct_executor_process_exit_after_effect_never_blindly_retries() {
    if run_direct_crash_child_if_requested() {
        return;
    }

    let directory = tempdir().expect("temporary Run directory should exist");
    let database_path = directory.path().join("run.db");
    let lock_path = directory.path().join("run.lock");
    let counter_path = directory.path().join("counter.txt");
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance);
    {
        let mut store = SqliteRunStore::open(&database_path).expect("store should open");
        let lease =
            LocalRunLease::try_acquire(RUN_ID, &lock_path).expect("parent lease should acquire");
        seed(&mut store);
        drop(admit(&mut store, &lease, &registry, &definition, true));
    }
    fs::write(&counter_path, "0").expect("counter should initialize");

    let status = Command::new(std::env::current_exe().expect("test executable should exist"))
        .args([
            "--exact",
            "direct_executor_process_exit_after_effect_never_blindly_retries",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD_MARKER, "1")
        .env(CRASH_DATABASE_PATH, &database_path)
        .env(CRASH_LOCK_PATH, &lock_path)
        .env(CRASH_COUNTER_PATH, &counter_path)
        .status()
        .expect("crash child should start");
    assert_eq!(status.code(), Some(86));
    assert_eq!(
        fs::read_to_string(&counter_path).expect("counter should be readable"),
        "1"
    );

    let mut store = SqliteRunStore::open(&database_path).expect("store should recover");
    assert_eq!(
        store.load().expect("load").expect("Run").state.steps[STEP_ID].status,
        StepStatus::Executing
    );
    let lease =
        LocalRunLease::try_acquire(RUN_ID, &lock_path).expect("crashed child released its lease");
    let mut events = DeterministicEvents;
    let recovered = DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &CapabilityRegistry::new(),
            &mut EffectAdapterRegistry::new(),
            STEP_ID,
            None,
        )
        .expect("Executing recovery must not need provider or adapter");
    assert_eq!(recovered.action, DriveAction::ExecutionRecoveredAsUnknown);
    assert_eq!(
        recovered.state.steps[STEP_ID].status,
        StepStatus::EffectUnknown
    );
    assert_eq!(
        fs::read_to_string(&counter_path).expect("counter should be readable"),
        "1"
    );
}

#[test]
fn sqlite_restart_recovers_material_then_executes_without_plaintext_persistence() {
    let directory = tempdir().expect("temporary Run directory should exist");
    let database = directory.path().join("run.db");
    let lock = directory.path().join("run.lock");
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    let lease = LocalRunLease::try_acquire(RUN_ID, &lock).expect("lease should acquire");
    {
        let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
        seed(&mut store);
        drop(admit(&mut store, &lease, &registry, &definition, true));
    }

    let provider_calls = Rc::new(Cell::new(0));
    let rejected_provider_calls = Rc::new(Cell::new(0));
    let mut providers = MaterialProviderRegistry::new();
    providers
        .register(
            "run-recipe",
            FixedProvider {
                calls: Rc::clone(&provider_calls),
                material: canonical_arguments(),
            },
        )
        .expect("provider should register");
    assert!(matches!(
        providers.register(
            "run-recipe",
            FixedProvider {
                calls: Rc::clone(&rejected_provider_calls),
                material: json!({"path": CANONICAL_PATH, "marker": "wrong-provider"}),
            },
        ),
        Err(MaterialProviderRegistryError::DuplicateProvider)
    ));
    let mut store = SqliteRunStore::open(&database).expect("SQLite should reopen");
    let material = InvocationMaterialRecovery::new()
        .recover(
            &store,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut providers,
            STEP_ID,
        )
        .expect("material should reconstruct after restart");
    let trace = Rc::new(RefCell::new(Vec::new()));
    let counters = Rc::new(AdapterCounters::default());
    let mut adapters = EffectAdapterRegistry::new();
    register_fake(
        &mut adapters,
        &instance,
        FakeAdapter::succeeding(Rc::clone(&counters), trace),
    );
    let mut events = DeterministicEvents;

    let report = DirectExecutor::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut adapters,
            STEP_ID,
            Some(&material),
        )
        .expect("recovered material should execute exactly once");

    assert_eq!(report.action, DriveAction::EffectSucceeded);
    assert_eq!(provider_calls.get(), 1);
    assert_eq!(rejected_provider_calls.get(), 0);
    assert_eq!(counters.prepares.get(), 1);
    assert_eq!(counters.executes.get(), 1);

    let sentinels = [
        SECRET_SENTINEL.as_bytes(),
        RAW_PATH.as_bytes(),
        CANONICAL_PATH.as_bytes(),
    ];
    #[cfg(not(windows))]
    assert_persisted_artifacts_exclude(directory.path(), &sentinels);
    drop(store);
    drop(lease);
    assert_persisted_artifacts_exclude(directory.path(), &sentinels);
}

#[test]
fn provider_registry_rejects_invalid_and_duplicate_ids_without_replacement() {
    let calls = Rc::new(Cell::new(0));
    let provider = || FixedProvider {
        calls: Rc::clone(&calls),
        material: canonical_arguments(),
    };
    let mut providers = MaterialProviderRegistry::new();
    providers
        .register("run-recipe", provider())
        .expect("first provider should register");
    assert!(matches!(
        providers.register("run-recipe", provider()),
        Err(MaterialProviderRegistryError::DuplicateProvider)
    ));
    assert!(matches!(
        providers.register("../secret", provider()),
        Err(MaterialProviderRegistryError::InvalidProviderId)
    ));
    assert!(matches!(
        providers.register(".", provider()),
        Err(MaterialProviderRegistryError::InvalidProviderId)
    ));
    assert_eq!(providers.len(), 1);
    assert!(!format!("{providers:?}").contains("run-recipe"));
}

#[test]
fn invalid_adapter_evidence_is_rejected_without_echoing_the_candidate() {
    let candidate = format!("sha256:{SECRET_SENTINEL}");
    let error = AdapterEvidenceDigest::new(candidate).expect_err("candidate must be rejected");
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains(SECRET_SENTINEL));
}
