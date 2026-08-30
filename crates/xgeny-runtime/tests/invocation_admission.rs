use std::cell::Cell;
use std::fs;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use xgeny_domain::{
    Architecture, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef, CriticalAction,
    DataBoundary, EffectClass, ExecutionStyle, GrantLifetime, OperatingSystem, Placement, Platform,
    PolicySource, PolicySourceKind, ProtocolDocument, TrustLevel,
};
use xgeny_local_store::{
    Commit, ExpectedHead, MemoryRunStore, RunSnapshot, RunStore, SqliteRunStore, StoreError,
};
use xgeny_policy::{
    BrokerError, InvocationResolutionError, PolicyAllowance, PolicyContribution, PolicyInputs,
    ResolvedPermissionRequest, ResourceResolutionFailure, ResourceResolver,
};
use xgeny_runtime::{
    AdmissionError, AdmissionOutcome, AdmissionRequest, CapabilityRegistry, EventFactory,
    EventFactoryError, EventMetadata, InvocationAdmission, InvocationMaterialProvider,
    InvocationMaterialRecovery, LocalRunLease, MaterialProviderFailure, MaterialProviderRegistry,
    MaterialRecoveryError, PlannedAdmissionRequest, RequiredRouteFeatures, RouteOutcome,
    RouteRequest,
};
use xgeny_workgraph::{
    AcceptedPlanStep, AgentLoopBudget, DependencyBlockReason, ExpectedPlanningTurn,
    InvocationMaterialRecord, InvocationMaterialRetention, InvocationMaterialUnavailableReason,
    PlannedExecutionProfile, PlannedInvocationBinding, PlannedInvocationMaterialRecord,
    PlannedInvocationSpec, ReconstructableMaterialReference, RunEvent, RunEventBody, RunState,
    StepStatus, invocation_material_digest,
};

const RUN_ID: &str = "run-admission-1";
const STEP_ID: &str = "step-admission-1";
const OTHER_STEP_ID: &str = "step-admission-2";
const AUTHORITY: &str = "local:test";
const AUTHORITY_EPOCH: u64 = 17;
const RAW_ALIAS: &str = "/workspace/area/../output.txt";
const CANONICAL_PATH: &str = "/workspace/output.txt";
const SECRET_SENTINEL: &str = "RAW-ARGUMENT-MUST-NOT-BE-JOURNALED";

#[derive(Debug, Default)]
struct CanonicalResolver {
    calls: Cell<usize>,
}

impl ResourceResolver for CanonicalResolver {
    fn resolve(&self, _scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        self.calls.set(self.calls.get() + 1);
        match resource {
            RAW_ALIAS | "/workspace/./output.txt" | CANONICAL_PATH => Ok(CANONICAL_PATH.to_owned()),
            "reject://outside" => Err(ResourceResolutionFailure::OutsideHostBoundary),
            other => Ok(other.to_owned()),
        }
    }
}

#[derive(Debug)]
struct FixedProvider {
    material: Value,
    failure: Rc<Cell<Option<MaterialProviderFailure>>>,
    calls: Rc<Cell<usize>>,
}

#[derive(Debug)]
struct FixedMaterialProvider {
    registry: MaterialProviderRegistry,
    failure: Rc<Cell<Option<MaterialProviderFailure>>>,
    calls: Rc<Cell<usize>>,
}

impl FixedMaterialProvider {
    fn available(provider_id: &str, material: Value) -> Self {
        let failure = Rc::new(Cell::new(None));
        let calls = Rc::new(Cell::new(0));
        let provider = FixedProvider {
            material,
            failure: Rc::clone(&failure),
            calls: Rc::clone(&calls),
        };
        let mut registry = MaterialProviderRegistry::new();
        registry
            .register(provider_id, provider)
            .expect("fixture provider identifier should register");
        Self {
            registry,
            failure,
            calls,
        }
    }
}

impl Deref for FixedMaterialProvider {
    type Target = MaterialProviderRegistry;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl DerefMut for FixedMaterialProvider {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.registry
    }
}

impl InvocationMaterialProvider for FixedProvider {
    fn reconstruct(
        &mut self,
        _reference_id: &str,
        _revision: &str,
    ) -> Result<Value, MaterialProviderFailure> {
        self.calls.set(self.calls.get() + 1);
        if let Some(failure) = self.failure.get() {
            return Err(failure);
        }
        Ok(self.material.clone())
    }
}

fn assert_sqlite_artifacts_exclude(directory: &std::path::Path, sentinels: &[&[u8]]) {
    for entry in fs::read_dir(directory).expect("run directory should be readable") {
        let path = entry.expect("directory entry should be readable").path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).expect("SQLite artifacts should be readable");
        for sentinel in sentinels {
            assert!(
                !bytes
                    .windows(sentinel.len())
                    .any(|window| window == *sentinel),
                "plaintext invocation material leaked into {}",
                path.display()
            );
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
            event_id: format!("admission-event-{next}"),
            recorded_at: "2026-08-29T12:00:00Z".to_owned(),
        })
    }
}

struct InvalidTimestampEvents;

impl EventFactory for InvalidTimestampEvents {
    fn create_metadata(&mut self, state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        Ok(EventMetadata {
            event_id: format!("invalid-policy-event-{}", state.journal_sequence + 1),
            recorded_at: "RAW-POLICY-SENTINEL".to_owned(),
        })
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
    *instance
}

fn registry_with(
    definition: &CapabilityDefinitionBody,
    instances: impl IntoIterator<Item = CapabilityInstanceBody>,
) -> CapabilityRegistry {
    let mut registry = CapabilityRegistry::new();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    for instance in instances {
        registry
            .register_schema_validated_instance(instance)
            .expect("instance should register");
    }
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

fn arguments(path: &str, marker: &str) -> serde_json::Value {
    json!({"path": path, "marker": marker})
}

fn seed_event(event_id: &str, run_id: &str, body: RunEventBody) -> RunEvent {
    RunEvent {
        event_id: event_id.to_owned(),
        run_id: run_id.to_owned(),
        authority: AUTHORITY.to_owned(),
        authority_epoch: AUTHORITY_EPOCH,
        recorded_at: "2026-08-29T12:00:00Z".to_owned(),
        body,
    }
}

fn seed<S: RunStore>(store: &mut S, run_id: &str, step_id: &str) -> RunState {
    let created = store
        .append(
            ExpectedHead::Empty,
            seed_event(
                "seed-event-1",
                run_id,
                RunEventBody::RunCreated {
                    goal: "admit one exact effect".to_owned(),
                },
            ),
        )
        .expect("Run should be created")
        .state;
    store
        .append(
            ExpectedHead::from_state(&created),
            seed_event(
                "seed-event-2",
                run_id,
                RunEventBody::StepPlanned {
                    step_id: step_id.to_owned(),
                    objective: "write one marker".to_owned(),
                    depends_on: Vec::new(),
                },
            ),
        )
        .expect("Step should be planned")
        .state
}

fn planning_facts(
    registry: &CapabilityRegistry,
    resolver: &CanonicalResolver,
    definition: &CapabilityDefinitionBody,
    invocation_arguments: Value,
) -> (String, String, String) {
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        registry,
        resolver,
        definition,
        invocation_arguments,
    )
    .expect("planning fixture should normalize through admission facts");
    (
        pending.definition_digest().to_owned(),
        pending.action_digest().to_owned(),
        pending.material_digest().to_owned(),
    )
}

fn seed_accepted_plan<S: RunStore>(
    store: &mut S,
    definition: &CapabilityDefinitionBody,
    definition_digest: String,
    action_digest: String,
    material_digest: String,
) -> (RunState, PlannedInvocationBinding) {
    let created = store
        .append(
            ExpectedHead::Empty,
            seed_event(
                "planned-seed-event-1",
                RUN_ID,
                RunEventBody::RunCreated {
                    goal: "admit one durable planned effect".to_owned(),
                },
            ),
        )
        .expect("Run should be created")
        .state;
    let configured = store
        .append(
            ExpectedHead::from_state(&created),
            seed_event(
                "planned-seed-event-2",
                RUN_ID,
                RunEventBody::AgentLoopConfigured {
                    budget: AgentLoopBudget::new(2, 2, 2, 16_384).expect("budget should validate"),
                },
            ),
        )
        .expect("agent loop should configure")
        .state;
    let proposal_digest = format!("sha256:{}", "a".repeat(64));
    let spec = PlannedInvocationSpec::new(
        definition.metadata.id.clone(),
        definition.metadata.contract_version.clone(),
        definition_digest,
        action_digest,
        material_digest,
        PlannedExecutionProfile::LocalSyncOnceV1,
        "linux",
        "x86_64",
    )
    .expect("planned invocation facts should validate");
    let reference = ReconstructableMaterialReference::new("run-recipe", "recipe-1", "rev-1")
        .expect("reference should validate");
    let (binding, input) =
        PlannedInvocationMaterialRecord::bind(RUN_ID, STEP_ID, &proposal_digest, spec, reference)
            .expect("planned input should bind");
    let decision =
        ExpectedPlanningTurn::new(1, format!("sha256:{}", "b".repeat(64)), proposal_digest)
            .expect("planning turn should bind");
    let state = store
        .append_with_plan_inputs(
            ExpectedHead::from_state(&configured),
            seed_event(
                "planned-seed-event-3",
                RUN_ID,
                RunEventBody::PlanAccepted {
                    decision,
                    steps: vec![AcceptedPlanStep {
                        step_id: STEP_ID.to_owned(),
                        objective: "write one marker".to_owned(),
                        depends_on: Vec::new(),
                        invocation: binding.clone(),
                    }],
                },
            ),
            vec![input],
        )
        .expect("accepted plan and input should commit atomically")
        .state;
    (state, binding)
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
        [request.requested_lifetime()],
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

fn ask_inputs(request: &ResolvedPermissionRequest) -> PolicyInputs {
    PolicyInputs::local(
        request,
        PolicyContribution::allow(
            source(PolicySourceKind::Host, "host.local", '1'),
            allowance(request),
        ),
        PolicyContribution::ask(
            source(PolicySourceKind::UserProfile, "profile.default", '2'),
            "approval_required",
        ),
    )
}

fn deny_inputs(request: &ResolvedPermissionRequest) -> PolicyInputs {
    PolicyInputs::local(
        request,
        PolicyContribution::allow(
            source(PolicySourceKind::Host, "host.local", '1'),
            allowance(request),
        ),
        PolicyContribution::deny(
            source(PolicySourceKind::UserProfile, "profile.default", '2'),
            "profile_denied",
        ),
    )
}

fn prepare<S: RunStore>(
    store: &S,
    lease: &LocalRunLease,
    registry: &CapabilityRegistry,
    resolver: &CanonicalResolver,
    definition: &CapabilityDefinitionBody,
    invocation_arguments: serde_json::Value,
) -> Result<xgeny_runtime::PendingInvocation, AdmissionError> {
    prepare_for_step(
        store,
        lease,
        registry,
        resolver,
        definition,
        STEP_ID,
        invocation_arguments,
    )
}

fn prepare_for_step<S: RunStore>(
    store: &S,
    lease: &LocalRunLease,
    registry: &CapabilityRegistry,
    resolver: &CanonicalResolver,
    definition: &CapabilityDefinitionBody,
    step_id: &str,
    invocation_arguments: serde_json::Value,
) -> Result<xgeny_runtime::PendingInvocation, AdmissionError> {
    InvocationAdmission::new().prepare(
        store,
        lease,
        registry,
        resolver,
        AdmissionRequest {
            step_id: step_id.to_owned(),
            route: route_request(definition),
            arguments: invocation_arguments,
        },
    )
}

fn acquire_lease(run_id: &str) -> (TempDir, LocalRunLease) {
    let directory = tempdir().expect("temporary directory should exist");
    let lease = LocalRunLease::try_acquire(run_id, directory.path().join("run.lock"))
        .expect("Run lease should be acquired");
    (directory, lease)
}

fn commit_reconstructable<S: RunStore>(
    store: &mut S,
    lease: &LocalRunLease,
    registry: &CapabilityRegistry,
    resolver: &CanonicalResolver,
    definition: &CapabilityDefinitionBody,
    marker: &str,
) -> Box<xgeny_runtime::AdmittedEffect> {
    let pending = prepare(
        store,
        lease,
        registry,
        resolver,
        definition,
        arguments(CANONICAL_PATH, marker),
    )
    .expect("invocation should prepare")
    .with_reconstructable_material(
        ReconstructableMaterialReference::new("run-recipe", "recipe-1", "rev-1")
            .expect("reference should validate"),
    );
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, registry, store, &mut events, lease)
        .expect("reconstructable intent should commit");
    let AdmissionOutcome::Authorized(admitted) = outcome else {
        panic!("invocation should authorize")
    };
    admitted
}

#[test]
fn accepted_plan_reconstructs_after_sqlite_reopen_and_commits_the_exact_plan_id() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let expected_arguments = arguments(CANONICAL_PATH, SECRET_SENTINEL);
    let (definition_digest, action_digest, material_digest) = planning_facts(
        &registry,
        &resolver,
        &definition,
        expected_arguments.clone(),
    );
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("planned-run.db");
    let binding = {
        let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
        seed_accepted_plan(
            &mut store,
            &definition,
            definition_digest,
            action_digest,
            material_digest,
        )
        .1
    };

    let mut store = SqliteRunStore::open(&path).expect("SQLite should reopen");
    let (_lease_directory, lease) = acquire_lease(RUN_ID);
    let mut providers = FixedMaterialProvider::available("run-recipe", expected_arguments);
    let pending = InvocationMaterialRecovery::new()
        .prepare_planned_admission(
            &store,
            &lease,
            &registry,
            &resolver,
            &mut providers,
            PlannedAdmissionRequest::new(STEP_ID, route_request(&definition)),
        )
        .expect("accepted input should reconstruct and prepare");
    assert_eq!(providers.calls.get(), 1);
    assert!(!format!("{pending:?}").contains(SECRET_SENTINEL));
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, &registry, &mut store, &mut events, &lease)
        .expect("planned invocation should authorize");
    let AdmissionOutcome::Authorized(admitted) = outcome else {
        panic!("planned invocation should be authorized")
    };
    let step = &admitted.commit().state.steps[STEP_ID];
    assert_eq!(step.status, StepStatus::IntentCommitted);
    assert_eq!(
        step.intent
            .as_ref()
            .and_then(|intent| intent.receipt_provenance.as_ref())
            .map(|provenance| provenance.plan_id.as_str()),
        Some(binding.plan_id())
    );
    drop(store);
    assert_sqlite_artifacts_exclude(directory.path(), &[SECRET_SENTINEL.as_bytes()]);
}

#[test]
fn direct_planned_prepare_pins_the_accepted_recipe_and_ignores_reference_replacement() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let exact_arguments = arguments(CANONICAL_PATH, SECRET_SENTINEL);
    let (definition_digest, action_digest, material_digest) =
        planning_facts(&registry, &resolver, &definition, exact_arguments.clone());
    let mut store = MemoryRunStore::new();
    seed_accepted_plan(
        &mut store,
        &definition,
        definition_digest,
        action_digest,
        material_digest,
    );
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = InvocationAdmission::new()
        .prepare(
            &store,
            &lease,
            &registry,
            &resolver,
            AdmissionRequest {
                step_id: STEP_ID.to_owned(),
                route: route_request(&definition),
                arguments: exact_arguments,
            },
        )
        .expect("direct planned preparation should validate")
        .with_reconstructable_material(
            ReconstructableMaterialReference::new("other-provider", "other-recipe", "rev-9")
                .expect("replacement reference should be well formed"),
        );
    let inputs = allow_inputs(pending.permission_request());
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(
            pending,
            &inputs,
            &registry,
            &mut store,
            &mut DeterministicEvents,
            &lease,
        )
        .expect("planned invocation should authorize");
    let AdmissionOutcome::Authorized(admitted) = outcome else {
        panic!("planned invocation should be authorized")
    };
    assert!(matches!(
        admitted.material_record().retention(),
        InvocationMaterialRetention::ReconstructableReference(reference)
            if reference.provider_id() == "run-recipe"
                && reference.reference_id() == "recipe-1"
                && reference.revision() == "rev-1"
    ));
}

#[test]
fn planned_definition_drift_is_rejected_before_recipe_or_resource_access() {
    let definition = definition_fixture();
    let original_instance = instance_fixture(&definition);
    let original_registry = registry_with(&definition, [original_instance]);
    let original_resolver = CanonicalResolver::default();
    let exact_arguments = arguments(CANONICAL_PATH, SECRET_SENTINEL);
    let (definition_digest, action_digest, material_digest) = planning_facts(
        &original_registry,
        &original_resolver,
        &definition,
        exact_arguments.clone(),
    );
    let mut store = MemoryRunStore::new();
    let planned = seed_accepted_plan(
        &mut store,
        &definition,
        definition_digest,
        action_digest,
        material_digest,
    )
    .0;

    let mut drifted = definition.clone();
    drifted.spec.summary.push_str(" drifted");
    let drifted_instance = instance_fixture(&drifted);
    let drifted_registry = registry_with(&drifted, [drifted_instance]);
    let resolver = CanonicalResolver::default();
    let (_directory, lease) = acquire_lease(RUN_ID);
    let mut providers = FixedMaterialProvider::available("run-recipe", exact_arguments);

    let result = InvocationMaterialRecovery::new().prepare_planned_admission(
        &store,
        &lease,
        &drifted_registry,
        &resolver,
        &mut providers,
        PlannedAdmissionRequest::new(STEP_ID, route_request(&drifted)),
    );

    assert!(matches!(
        result,
        Err(MaterialRecoveryError::Admission(
            AdmissionError::DefinitionChanged
        ))
    ));
    assert_eq!(providers.calls.get(), 0);
    assert_eq!(resolver.calls.get(), 0);
    assert_eq!(
        store.load_current().expect("Run should load"),
        Some(planned)
    );
}

#[test]
fn reconstructed_arguments_that_differ_from_the_accepted_plan_leave_the_run_unchanged() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let expected_arguments = arguments(CANONICAL_PATH, SECRET_SENTINEL);
    let (definition_digest, action_digest, material_digest) =
        planning_facts(&registry, &resolver, &definition, expected_arguments);
    let mut store = MemoryRunStore::new();
    let (planned, _) = seed_accepted_plan(
        &mut store,
        &definition,
        definition_digest,
        action_digest,
        material_digest,
    );
    let (_directory, lease) = acquire_lease(RUN_ID);
    let mut providers =
        FixedMaterialProvider::available("run-recipe", arguments(CANONICAL_PATH, "changed"));

    let result = InvocationMaterialRecovery::new().prepare_planned_admission(
        &store,
        &lease,
        &registry,
        &resolver,
        &mut providers,
        PlannedAdmissionRequest::new(STEP_ID, route_request(&definition)),
    );

    assert!(matches!(
        result,
        Err(MaterialRecoveryError::Admission(
            AdmissionError::PlannedInvocationMismatch {
                step_id,
                field: "action_digest" | "plan_input_digest",
            }
        )) if step_id == STEP_ID
    ));
    let after = store
        .load_current()
        .expect("Run should load")
        .expect("Run should exist");
    assert_eq!(after, planned);
    assert_eq!(after.steps[STEP_ID].status, StepStatus::Planned);
}

#[test]
fn wrong_planned_route_is_rejected_before_material_provider_access() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let expected_arguments = arguments(CANONICAL_PATH, SECRET_SENTINEL);
    let (definition_digest, action_digest, material_digest) = planning_facts(
        &registry,
        &resolver,
        &definition,
        expected_arguments.clone(),
    );
    let mut store = MemoryRunStore::new();
    seed_accepted_plan(
        &mut store,
        &definition,
        definition_digest,
        action_digest,
        material_digest,
    );
    let (_directory, lease) = acquire_lease(RUN_ID);
    let mut providers = FixedMaterialProvider::available("run-recipe", expected_arguments);
    let mut route = route_request(&definition);
    route.capability.capability_id.push_str(".other");

    let result = InvocationMaterialRecovery::new().prepare_planned_admission(
        &store,
        &lease,
        &registry,
        &resolver,
        &mut providers,
        PlannedAdmissionRequest::new(STEP_ID, route),
    );

    assert!(matches!(
        result,
        Err(MaterialRecoveryError::Admission(
            AdmissionError::PlannedInvocationMismatch {
                step_id,
                field: "capability_id",
            }
        )) if step_id == STEP_ID
    ));
    assert_eq!(providers.calls.get(), 0);
}

#[test]
fn unreleased_planned_dependency_is_rejected_before_material_provider_access() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let first_arguments = arguments(CANONICAL_PATH, "first");
    let first_facts = planning_facts(&registry, &resolver, &definition, first_arguments);
    let second_arguments = arguments(CANONICAL_PATH, "second");
    let second_facts = planning_facts(&registry, &resolver, &definition, second_arguments.clone());
    let mut store = MemoryRunStore::new();
    let (first_planned, _) = seed_accepted_plan(
        &mut store,
        &definition,
        first_facts.0,
        first_facts.1,
        first_facts.2,
    );
    let proposal_digest = format!("sha256:{}", "c".repeat(64));
    let spec = PlannedInvocationSpec::new(
        definition.metadata.id.clone(),
        definition.metadata.contract_version.clone(),
        second_facts.0,
        second_facts.1,
        second_facts.2,
        PlannedExecutionProfile::LocalSyncOnceV1,
        "linux",
        "x86_64",
    )
    .expect("planned invocation facts should validate");
    let (binding, input) = PlannedInvocationMaterialRecord::bind(
        RUN_ID,
        OTHER_STEP_ID,
        &proposal_digest,
        spec,
        ReconstructableMaterialReference::new("run-recipe", "recipe-2", "rev-1")
            .expect("reference should validate"),
    )
    .expect("dependent planned input should bind");
    let second_planned = store
        .append_with_plan_inputs(
            ExpectedHead::from_state(&first_planned),
            seed_event(
                "planned-seed-event-4",
                RUN_ID,
                RunEventBody::PlanAccepted {
                    decision: ExpectedPlanningTurn::new(
                        2,
                        format!("sha256:{}", "d".repeat(64)),
                        proposal_digest,
                    )
                    .expect("second planning turn should bind"),
                    steps: vec![AcceptedPlanStep {
                        step_id: OTHER_STEP_ID.to_owned(),
                        objective: "wait for the first Step".to_owned(),
                        depends_on: vec![STEP_ID.to_owned()],
                        invocation: binding,
                    }],
                },
            ),
            vec![input],
        )
        .expect("dependent plan should commit")
        .state;
    let (_directory, lease) = acquire_lease(RUN_ID);
    let mut providers = FixedMaterialProvider::available("run-recipe", second_arguments);

    let result = InvocationMaterialRecovery::new().prepare_planned_admission(
        &store,
        &lease,
        &registry,
        &resolver,
        &mut providers,
        PlannedAdmissionRequest::new(OTHER_STEP_ID, route_request(&definition)),
    );

    assert!(matches!(
        result,
        Err(MaterialRecoveryError::Admission(
            AdmissionError::StepDependencyNotReleased {
                step_id,
                dependency_id,
                ..
            }
        )) if step_id == OTHER_STEP_ID && dependency_id == STEP_ID
    ));
    assert_eq!(providers.calls.get(), 0);
    assert_eq!(
        store.load_current().expect("Run should load"),
        Some(second_planned)
    );
}

#[test]
fn exact_arguments_are_resolved_authorized_and_atomically_committed() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance.clone()]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);

    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(RAW_ALIAS, SECRET_SENTINEL),
    )
    .expect("valid invocation should prepare");
    assert_eq!(resolver.calls.get(), 1);
    assert_eq!(
        pending.permission_request().resources()[0].canonical_resource(),
        CANONICAL_PATH
    );
    let pending_debug = format!("{pending:?}");
    assert!(!pending_debug.contains(SECRET_SENTINEL));
    assert!(!pending_debug.contains(RAW_ALIAS));
    assert!(!pending_debug.contains(CANONICAL_PATH));
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, &registry, &mut store, &mut events, &lease)
        .expect("exact allow should commit");
    let AdmissionOutcome::Authorized(admitted) = outcome else {
        panic!("exact allow should authorize")
    };

    assert_eq!(admitted.selected_instance_id(), instance.instance_id);
    assert_eq!(
        admitted.material_record().material_digest(),
        invocation_material_digest(&arguments(CANONICAL_PATH, SECRET_SENTINEL))
            .expect("canonical admitted material should hash")
    );
    let state = &admitted.commit().state;
    let step = &state.steps[STEP_ID];
    assert_eq!(step.status, StepStatus::IntentCommitted);
    let intent = step.intent.as_ref().expect("intent should be durable");
    assert_eq!(intent.action_digest, admitted.action_digest());
    assert_eq!(intent.invocation.instance_id, instance.instance_id);
    assert_eq!(
        intent.invocation.instance_binding_digest,
        admitted.instance_binding_digest()
    );
    assert_eq!(intent.authorization.max_uses, 1);
    assert_eq!(intent.authorization.binding.run_id, RUN_ID);
    assert_eq!(intent.authorization.binding.step_id, STEP_ID);
    assert_eq!(intent.authorization.binding.authority, AUTHORITY);
    assert_eq!(
        intent.authorization.binding.authority_epoch,
        AUTHORITY_EPOCH
    );
    assert_eq!(state.authorization_consumption.len(), 1);
    assert_eq!(
        state.authorization_consumption[&intent.authorization.grant_id].uses,
        1
    );

    let journal = String::from_utf8(store.export_jsonl().expect("journal should export"))
        .expect("journal should be UTF-8");
    assert!(!journal.contains(SECRET_SENTINEL));
    assert!(!journal.contains(RAW_ALIAS));
    assert!(!journal.contains(CANONICAL_PATH));
    assert!(!format!("{admitted:?}").contains(SECRET_SENTINEL));
    assert!(!format!("{admitted:?}").contains(CANONICAL_PATH));
    let material = (*admitted)
        .into_ephemeral_material()
        .expect("same-process admitted arguments should become opaque material");
    assert_eq!(
        material.record().material_digest(),
        invocation_material_digest(&arguments(CANONICAL_PATH, SECRET_SENTINEL))
            .expect("ephemeral material should retain the committed digest")
    );
    assert!(!format!("{material:?}").contains(SECRET_SENTINEL));
    assert!(!format!("{material:?}").contains(CANONICAL_PATH));
}

#[test]
fn dependency_must_be_receipt_released_before_admission_preparation() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    let root = seed(&mut store, RUN_ID, STEP_ID);
    store
        .append(
            ExpectedHead::from_state(&root),
            seed_event(
                "seed-dependent-step",
                RUN_ID,
                RunEventBody::StepPlanned {
                    step_id: OTHER_STEP_ID.to_owned(),
                    objective: "run only after the verified parent".to_owned(),
                    depends_on: vec![STEP_ID.to_owned()],
                },
            ),
        )
        .expect("dependent Step should plan in topological order");
    let (_directory, lease) = acquire_lease(RUN_ID);

    let result = prepare_for_step(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        OTHER_STEP_ID,
        arguments(RAW_ALIAS, SECRET_SENTINEL),
    );

    assert!(matches!(
        result,
        Err(AdmissionError::StepDependencyNotReleased {
            step_id,
            dependency_id,
            reason: DependencyBlockReason::NotCompleted,
        }) if step_id == OTHER_STEP_ID && dependency_id == STEP_ID
    ));
    assert_eq!(
        resolver.calls.get(),
        0,
        "dependency gating must precede argument/resource resolution"
    );
    assert!(store.load().expect("store should load").is_some());
}

struct CurrentOnlyStore {
    state: RunState,
}

impl RunStore for CurrentOnlyStore {
    fn append(&mut self, _expected: ExpectedHead, _event: RunEvent) -> Result<Commit, StoreError> {
        Err(StoreError::InjectedFault("append must not be called"))
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        Err(StoreError::InjectedFault("full load must not be called"))
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        Ok(Some(self.state.clone()))
    }
}

#[test]
fn corrupt_unknown_dependency_is_rejected_before_resource_resolution_without_panicking() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let mut source = MemoryRunStore::new();
    let mut state = seed(&mut source, RUN_ID, STEP_ID);
    state
        .steps
        .get_mut(STEP_ID)
        .expect("Step should exist")
        .depends_on = vec!["missing-step".to_owned()];
    let store = CurrentOnlyStore { state };
    let (_directory, lease) = acquire_lease(RUN_ID);

    let result = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(RAW_ALIAS, SECRET_SENTINEL),
    );

    assert!(matches!(
        result,
        Err(AdmissionError::StepDependencyUnknown {
            step_id,
            dependency_id,
        }) if step_id == STEP_ID && dependency_id == "missing-step"
    ));
    assert_eq!(resolver.calls.get(), 0);
}

#[test]
fn invalid_event_timestamp_is_rejected_without_echo_or_intent_commit() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(RAW_ALIAS, SECRET_SENTINEL),
    )
    .expect("invocation should prepare");
    let inputs = allow_inputs(pending.permission_request());

    let error = InvocationAdmission::new()
        .authorize_and_commit(
            pending,
            &inputs,
            &registry,
            &mut store,
            &mut InvalidTimestampEvents,
            &lease,
        )
        .expect_err("invalid event timestamp must fail closed");

    assert!(matches!(error, AdmissionError::EventMetadata(_)));
    let rendered = format!("{error}\n{error:?}");
    assert!(!rendered.contains("RAW-POLICY-SENTINEL"));
    let snapshot = store.load().expect("store should load").expect("Run");
    assert_eq!(snapshot.state.steps[STEP_ID].status, StepStatus::Planned);
}

#[test]
fn protocol_invalid_policy_decision_is_rejected_without_echo_or_intent_commit() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let oversized_resource = format!("RAW-POLICY-SENTINEL-{}", "x".repeat(4096));
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(&oversized_resource, SECRET_SENTINEL),
    )
    .expect("trusted resolver may return a value beyond the protocol document limit");
    let inputs = allow_inputs(pending.permission_request());

    let error = InvocationAdmission::new()
        .authorize_and_commit(
            pending,
            &inputs,
            &registry,
            &mut store,
            &mut DeterministicEvents,
            &lease,
        )
        .expect_err("protocol-invalid PolicyDecision must fail closed");

    assert!(matches!(error, AdmissionError::PolicyDecisionInvalid));
    let rendered = format!("{error}\n{error:?}");
    assert!(!rendered.contains("RAW-POLICY-SENTINEL"));
    let snapshot = store.load().expect("store should load").expect("Run");
    assert_eq!(snapshot.state.steps[STEP_ID].status, StepStatus::Planned);
}

#[test]
fn non_local_instance_is_rejected_before_receipt_provenance_is_issued() {
    let definition = definition_fixture();
    let mut instance = instance_fixture(&definition);
    instance.placement = Placement::Remote;
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(RAW_ALIAS, SECRET_SENTINEL),
    )
    .expect("remote candidate may be routed before execution provenance is issued");
    let inputs = allow_inputs(pending.permission_request());

    let result = InvocationAdmission::new().authorize_and_commit(
        pending,
        &inputs,
        &registry,
        &mut store,
        &mut DeterministicEvents,
        &lease,
    );

    assert!(matches!(
        result,
        Err(AdmissionError::UnsupportedExecutorPlacement {
            placement: Placement::Remote
        })
    ));
    let snapshot = store.load().expect("store should load").expect("Run");
    assert_eq!(snapshot.state.steps[STEP_ID].status, StepStatus::Planned);
}

#[test]
fn semantic_action_identity_is_canonical_and_independent_of_run_or_step() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();

    let mut first_store = MemoryRunStore::new();
    seed(&mut first_store, RUN_ID, STEP_ID);
    let (_first_directory, first_lease) = acquire_lease(RUN_ID);
    let first = prepare(
        &first_store,
        &first_lease,
        &registry,
        &resolver,
        &definition,
        json!({"path": RAW_ALIAS, "marker": "same"}),
    )
    .expect("first invocation should prepare");
    let reordered = prepare(
        &first_store,
        &first_lease,
        &registry,
        &resolver,
        &definition,
        json!({"marker": "same", "path": "/workspace/./output.txt"}),
    )
    .expect("reordered invocation should prepare");
    let changed = prepare(
        &first_store,
        &first_lease,
        &registry,
        &resolver,
        &definition,
        json!({"marker": "changed", "path": CANONICAL_PATH}),
    )
    .expect("changed invocation should prepare");

    let mut other_step_store = MemoryRunStore::new();
    seed(&mut other_step_store, RUN_ID, OTHER_STEP_ID);
    let (_other_step_directory, other_step_lease) = acquire_lease(RUN_ID);
    let other_step = prepare_for_step(
        &other_step_store,
        &other_step_lease,
        &registry,
        &resolver,
        &definition,
        OTHER_STEP_ID,
        json!({"marker": "same", "path": CANONICAL_PATH}),
    )
    .expect("other Step invocation should prepare");

    let second_run_id = "run-admission-2";
    let mut second_store = MemoryRunStore::new();
    seed(&mut second_store, second_run_id, STEP_ID);
    let (_second_directory, second_lease) = acquire_lease(second_run_id);
    let second_run = prepare(
        &second_store,
        &second_lease,
        &registry,
        &resolver,
        &definition,
        json!({"marker": "same", "path": CANONICAL_PATH}),
    )
    .expect("second Run invocation should prepare");

    assert_eq!(first.action_digest(), reordered.action_digest());
    assert_eq!(first.action_digest(), other_step.action_digest());
    assert_eq!(first.action_digest(), second_run.action_digest());
    assert_ne!(first.action_digest(), changed.action_digest());
}

#[test]
fn policy_inputs_for_another_invocation_cannot_authorize_the_pending_action() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    let initial = seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);

    let allowed = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, "allowed"),
    )
    .expect("allowed request should prepare");
    let attempted = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, "attempted"),
    )
    .expect("attempted request should prepare before policy");
    let wrong_inputs = allow_inputs(allowed.permission_request());
    let mut events = DeterministicEvents;

    let result = InvocationAdmission::new().authorize_and_commit(
        attempted,
        &wrong_inputs,
        &registry,
        &mut store,
        &mut events,
        &lease,
    );
    assert!(matches!(
        result,
        Err(AdmissionError::Broker(BrokerError::PolicyRequestMismatch))
    ));
    assert_eq!(
        store
            .load()
            .expect("store should load")
            .expect("Run exists")
            .state,
        initial
    );
}

#[test]
fn definition_drift_after_policy_preparation_is_rejected_without_state_change() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    let initial = seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, "definition-drift"),
    )
    .expect("invocation should prepare");
    let inputs = allow_inputs(pending.permission_request());

    let mut changed = definition.clone();
    changed.spec.summary.push_str(" changed");
    let changed_registry = registry_with(&changed, [instance_fixture(&changed)]);
    let mut events = DeterministicEvents;
    assert!(matches!(
        InvocationAdmission::new().authorize_and_commit(
            pending,
            &inputs,
            &changed_registry,
            &mut store,
            &mut events,
            &lease,
        ),
        Err(AdmissionError::DefinitionChanged)
    ));
    assert_eq!(
        store.load().expect("store load").expect("Run exists").state,
        initial
    );
}

#[test]
fn canonical_arguments_over_one_mebibyte_are_rejected_before_policy() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);

    assert!(matches!(
        prepare(
            &store,
            &lease,
            &registry,
            &resolver,
            &definition,
            arguments(CANONICAL_PATH, &"x".repeat(1024 * 1024)),
        ),
        Err(AdmissionError::ArgumentsTooLarge {
            maximum: 1_048_576,
            ..
        })
    ));
}

#[test]
fn resolver_normalized_arguments_must_still_conform_to_the_definition_schema() {
    let mut definition = definition_fixture();
    definition.spec.input_schema["properties"]["path"] = json!({
        "type": "string",
        "const": RAW_ALIAS
    });
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);

    assert!(matches!(
        prepare(
            &store,
            &lease,
            &registry,
            &resolver,
            &definition,
            arguments(RAW_ALIAS, "schema-recheck"),
        ),
        Err(AdmissionError::ArgumentsDoNotConform)
    ));
}

#[test]
fn selector_shape_and_resolver_failures_are_closed_before_policy() {
    let resolver = CanonicalResolver::default();
    let attempt = |definition: &CapabilityDefinitionBody, invalid| {
        let registry = registry_with(definition, [instance_fixture(definition)]);
        let mut store = MemoryRunStore::new();
        seed(&mut store, RUN_ID, STEP_ID);
        let (_directory, lease) = acquire_lease(RUN_ID);
        prepare(&store, &lease, &registry, &resolver, definition, invalid)
    };

    let mut missing = definition_fixture();
    missing.spec.input_schema = json!({
        "type": "object",
        "required": ["marker"],
        "properties": {"marker": {"type": "string"}},
        "additionalProperties": false
    });
    assert!(matches!(
        attempt(&missing, json!({"marker": "missing path"})),
        Err(AdmissionError::Resolution(
            InvocationResolutionError::MissingResourceArgument { .. }
        ))
    ));

    let mut non_string = definition_fixture();
    non_string.spec.input_schema["properties"]["path"] = json!({});
    assert!(matches!(
        attempt(
            &non_string,
            json!({"path": [CANONICAL_PATH], "marker": "not string"}),
        ),
        Err(AdmissionError::Resolution(
            InvocationResolutionError::ResourceArgumentMustBeString { .. }
        ))
    ));

    let mut empty = definition_fixture();
    empty.spec.input_schema["properties"]["path"] = json!({"type": "string"});
    assert!(matches!(
        attempt(&empty, json!({"path": "", "marker": "empty"})),
        Err(AdmissionError::Resolution(
            InvocationResolutionError::EmptyResourceArgument { .. }
        ))
    ));

    let rejected = definition_fixture();
    assert!(matches!(
        attempt(
            &rejected,
            json!({"path": "reject://outside", "marker": "rejected"}),
        ),
        Err(AdmissionError::Resolution(
            InvocationResolutionError::ResolverRejected { .. }
        ))
    ));
}

#[test]
fn ask_and_deny_policy_never_commit_an_intent() {
    for verdict in ["ask", "deny"] {
        let definition = definition_fixture();
        let registry = registry_with(&definition, [instance_fixture(&definition)]);
        let resolver = CanonicalResolver::default();
        let mut store = MemoryRunStore::new();
        let initial = seed(&mut store, RUN_ID, STEP_ID);
        let (_directory, lease) = acquire_lease(RUN_ID);
        let pending = prepare(
            &store,
            &lease,
            &registry,
            &resolver,
            &definition,
            arguments(CANONICAL_PATH, verdict),
        )
        .expect("invocation should prepare");
        let inputs = match verdict {
            "ask" => ask_inputs(pending.permission_request()),
            "deny" => deny_inputs(pending.permission_request()),
            _ => unreachable!(),
        };
        let mut events = DeterministicEvents;
        let outcome = InvocationAdmission::new()
            .authorize_and_commit(pending, &inputs, &registry, &mut store, &mut events, &lease)
            .expect("policy non-allow should be represented by Router outcome");
        assert!(matches!(outcome, AdmissionOutcome::NotAuthorized(_)));
        assert_eq!(
            store.load().expect("store load").expect("Run exists").state,
            initial
        );
    }
}

#[test]
fn critical_and_managed_policy_never_commit_an_intent() {
    let mut critical_definition = definition_fixture();
    critical_definition.spec.effect.critical_actions = vec![CriticalAction::ProductionDeploy];
    let critical_registry = registry_with(
        &critical_definition,
        [instance_fixture(&critical_definition)],
    );
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    let initial = seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let critical = prepare(
        &store,
        &lease,
        &critical_registry,
        &resolver,
        &critical_definition,
        arguments(CANONICAL_PATH, "critical"),
    )
    .expect("critical invocation should prepare for interaction");
    let critical_inputs = allow_inputs(critical.permission_request());
    let mut events = DeterministicEvents;
    let critical_outcome = InvocationAdmission::new()
        .authorize_and_commit(
            critical,
            &critical_inputs,
            &critical_registry,
            &mut store,
            &mut events,
            &lease,
        )
        .expect("critical action should request interaction");
    assert!(matches!(
        critical_outcome,
        AdmissionOutcome::NotAuthorized(RouteOutcome::InteractionRequired { .. })
    ));
    assert_eq!(
        store.load().expect("store load").expect("Run exists").state,
        initial
    );

    let pending = prepare(
        &store,
        &lease,
        &critical_registry,
        &resolver,
        &critical_definition,
        arguments(CANONICAL_PATH, "managed"),
    )
    .expect("managed candidate should prepare");
    let request = pending.permission_request();
    let managed = PolicyInputs::managed(
        request,
        PolicyContribution::allow(
            source(PolicySourceKind::Host, "host.local", '1'),
            allowance(request),
        ),
        PolicyContribution::allow(
            source(PolicySourceKind::UserProfile, "profile.default", '2'),
            allowance(request),
        ),
        PolicyContribution::allow(
            source(PolicySourceKind::ManagedLease, "lease.current", '3'),
            allowance(request),
        ),
    );
    assert!(matches!(
        InvocationAdmission::new().authorize_and_commit(
            pending,
            &managed,
            &critical_registry,
            &mut store,
            &mut events,
            &lease,
        ),
        Err(AdmissionError::ManagedPolicyUnsupported)
    ));
}

#[test]
fn stale_run_head_and_wrong_lease_cannot_consume_prepared_policy() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    let state = seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, "stale"),
    )
    .expect("invocation should prepare");
    let inputs = allow_inputs(pending.permission_request());
    store
        .append(
            ExpectedHead::from_state(&state),
            seed_event(
                "seed-event-3",
                RUN_ID,
                RunEventBody::StepPlanned {
                    step_id: "unrelated-step".to_owned(),
                    objective: "advance the head".to_owned(),
                    depends_on: Vec::new(),
                },
            ),
        )
        .expect("unrelated Step should advance head");
    let mut events = DeterministicEvents;
    assert!(matches!(
        InvocationAdmission::new().authorize_and_commit(
            pending,
            &inputs,
            &registry,
            &mut store,
            &mut events,
            &lease,
        ),
        Err(AdmissionError::RunHeadChanged)
    ));

    let (_wrong_directory, wrong_lease) = acquire_lease("another-run");
    assert!(matches!(
        prepare(
            &store,
            &wrong_lease,
            &registry,
            &resolver,
            &definition,
            arguments(CANONICAL_PATH, "wrong lease"),
        ),
        Err(AdmissionError::LeaseRunMismatch { .. })
    ));
}

#[test]
fn sqlite_restart_preserves_exactly_one_intent_and_no_raw_arguments() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let directory = tempdir().expect("temporary directory should exist");
    let database = directory.path().join("run.db");
    let lease = LocalRunLease::try_acquire(RUN_ID, directory.path().join("run.lock"))
        .expect("Run lease should be acquired");
    let expected_state = {
        let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
        seed(&mut store, RUN_ID, STEP_ID);
        let pending = prepare(
            &store,
            &lease,
            &registry,
            &resolver,
            &definition,
            arguments(RAW_ALIAS, SECRET_SENTINEL),
        )
        .expect("invocation should prepare");
        let inputs = allow_inputs(pending.permission_request());
        let mut events = DeterministicEvents;
        let outcome = InvocationAdmission::new()
            .authorize_and_commit(pending, &inputs, &registry, &mut store, &mut events, &lease)
            .expect("admission should commit");
        let AdmissionOutcome::Authorized(admitted) = outcome else {
            panic!("admission should authorize")
        };
        admitted.commit().state.clone()
    };

    let reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
    let snapshot = reopened
        .load()
        .expect("replay should verify")
        .expect("Run should exist");
    assert_eq!(snapshot.state, expected_state);
    assert_eq!(snapshot.records.len(), 3);
    assert_eq!(snapshot.state.authorization_consumption.len(), 1);
    assert_eq!(
        snapshot
            .state
            .authorization_consumption
            .values()
            .next()
            .expect("one authorization")
            .uses,
        1
    );
    let journal = String::from_utf8(reopened.export_jsonl().expect("journal export"))
        .expect("journal should be UTF-8");
    assert!(!journal.contains(SECRET_SENTINEL));
    assert!(!journal.contains(RAW_ALIAS));
    assert!(!journal.contains(CANONICAL_PATH));
}

#[derive(Debug)]
struct LostAcknowledgementStore<S> {
    inner: S,
    lose_effect_commit_once: bool,
}

impl<S: RunStore> RunStore for LostAcknowledgementStore<S> {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        let lose_ack = self.lose_effect_commit_once
            && matches!(&event.body, RunEventBody::EffectIntentCommitted { .. });
        let commit = self.inner.append(expected, event)?;
        if lose_ack {
            self.lose_effect_commit_once = false;
            Err(StoreError::InjectedFault("lost admission acknowledgement"))
        } else {
            Ok(commit)
        }
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
        let lose_ack = self.lose_effect_commit_once
            && matches!(&event.body, RunEventBody::EffectIntentCommitted { .. });
        let commit = self
            .inner
            .append_with_invocation_material(expected, event, material)?;
        if lose_ack {
            self.lose_effect_commit_once = false;
            Err(StoreError::InjectedFault("lost admission acknowledgement"))
        } else {
            Ok(commit)
        }
    }

    fn load_invocation_material(
        &self,
        effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        self.inner.load_invocation_material(effect_id)
    }
}

#[test]
fn lost_commit_acknowledgement_does_not_mint_a_second_budget() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = LostAcknowledgementStore {
        inner: MemoryRunStore::new(),
        lose_effect_commit_once: true,
    };
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, "lost-ack"),
    )
    .expect("invocation should prepare")
    .with_reconstructable_material(
        ReconstructableMaterialReference::new("run-recipe", "recipe-lost-ack", "rev-1")
            .expect("reference should be a bounded opaque identifier"),
    );
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    assert!(matches!(
        InvocationAdmission::new().authorize_and_commit(
            pending,
            &inputs,
            &registry,
            &mut store,
            &mut events,
            &lease,
        ),
        Err(AdmissionError::Store(StoreError::InjectedFault(_)))
    ));

    let recovered = store
        .load()
        .expect("store should load")
        .expect("Run should exist")
        .state;
    assert_eq!(recovered.steps[STEP_ID].status, StepStatus::IntentCommitted);
    assert_eq!(recovered.authorization_consumption.len(), 1);
    assert_eq!(
        recovered
            .authorization_consumption
            .values()
            .next()
            .expect("one authorization")
            .uses,
        1
    );
    assert!(matches!(
        prepare(
            &store,
            &lease,
            &registry,
            &resolver,
            &definition,
            arguments(CANONICAL_PATH, "lost-ack"),
        ),
        Err(AdmissionError::StepNotPlanned { .. })
    ));

    let mut provider =
        FixedMaterialProvider::available("run-recipe", arguments(CANONICAL_PATH, "lost-ack"));
    let material = InvocationMaterialRecovery::new()
        .recover(&store, &lease, &registry, &resolver, &mut provider, STEP_ID)
        .expect("lost acknowledgement should retain a recoverable material descriptor");
    assert_eq!(
        material.record().material_digest(),
        invocation_material_digest(&arguments(CANONICAL_PATH, "lost-ack"))
            .expect("recovered material should match the committed digest")
    );
    assert_eq!(provider.calls.get(), 1);
    assert_eq!(
        store
            .load()
            .expect("store should load")
            .expect("Run should exist")
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
fn sqlite_lost_ack_reopens_with_one_recoverable_material_and_no_plaintext_arguments() {
    let directory = tempdir().expect("temporary directory should exist");
    let database = directory.path().join("복구 테스트 run.db");
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let (_lease_directory, lease) = acquire_lease(RUN_ID);

    {
        let inner = SqliteRunStore::open(&database).expect("SQLite should open");
        let mut store = LostAcknowledgementStore {
            inner,
            lose_effect_commit_once: true,
        };
        seed(&mut store, RUN_ID, STEP_ID);
        let pending = prepare(
            &store,
            &lease,
            &registry,
            &resolver,
            &definition,
            arguments(CANONICAL_PATH, SECRET_SENTINEL),
        )
        .expect("invocation should prepare")
        .with_reconstructable_material(
            ReconstructableMaterialReference::new("run-recipe", "recipe-sqlite", "rev-1")
                .expect("reference should validate"),
        );
        let inputs = allow_inputs(pending.permission_request());
        let mut events = DeterministicEvents;
        assert!(matches!(
            InvocationAdmission::new().authorize_and_commit(
                pending,
                &inputs,
                &registry,
                &mut store,
                &mut events,
                &lease,
            ),
            Err(AdmissionError::Store(StoreError::InjectedFault(_)))
        ));
    }

    let reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
    let snapshot = reopened
        .load()
        .expect("replay should verify")
        .expect("Run should exist");
    let intent = snapshot.state.steps[STEP_ID]
        .intent
        .as_ref()
        .expect("intent should be committed");
    assert_eq!(snapshot.records.len(), 3);
    assert_eq!(snapshot.state.authorization_consumption.len(), 1);
    assert_eq!(
        snapshot
            .state
            .authorization_consumption
            .values()
            .next()
            .expect("one authorization")
            .uses,
        1
    );
    let record = reopened
        .load_invocation_material(&intent.effect_id)
        .expect("material lookup should work")
        .expect("one material record should exist");
    record
        .verify_for(RUN_ID, STEP_ID, intent)
        .expect("material should remain bound after reopen");

    let mut provider =
        FixedMaterialProvider::available("run-recipe", arguments(CANONICAL_PATH, SECRET_SENTINEL));
    let material = InvocationMaterialRecovery::new()
        .recover(
            &reopened,
            &lease,
            &registry,
            &resolver,
            &mut provider,
            STEP_ID,
        )
        .expect("reopened material should reconstruct");
    assert_eq!(
        material.record().material_digest(),
        invocation_material_digest(&arguments(CANONICAL_PATH, SECRET_SENTINEL))
            .expect("recovered material should match the committed digest")
    );
    assert!(!format!("{material:?}").contains(SECRET_SENTINEL));
    assert!(!format!("{material:?}").contains(CANONICAL_PATH));
    assert_eq!(provider.calls.get(), 1);

    let sentinels = [SECRET_SENTINEL.as_bytes(), CANONICAL_PATH.as_bytes()];
    #[cfg(not(windows))]
    assert_sqlite_artifacts_exclude(directory.path(), &sentinels);
    drop(reopened);
    assert_sqlite_artifacts_exclude(directory.path(), &sentinels);
}

#[test]
fn ephemeral_material_loss_is_explicitly_closed_to_manual_without_an_effect_start() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, SECRET_SENTINEL),
    )
    .expect("ephemeral invocation should prepare");
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, &registry, &mut store, &mut events, &lease)
        .expect("ephemeral intent should commit");
    let AdmissionOutcome::Authorized(admitted) = outcome else {
        panic!("ephemeral invocation should authorize")
    };
    let effect_id = admitted.effect_id().to_owned();
    drop(admitted);

    let mut provider = FixedMaterialProvider::available(
        "unused-provider",
        arguments(CANONICAL_PATH, SECRET_SENTINEL),
    );
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &registry,
            &resolver,
            &mut provider,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::EphemeralMaterialUnavailable)
    ));
    assert_eq!(provider.calls.get(), 0);

    let commit = InvocationMaterialRecovery::new()
        .mark_unavailable(
            &mut store,
            &mut events,
            &lease,
            STEP_ID,
            InvocationMaterialUnavailableReason::EphemeralMaterialLost,
        )
        .expect("permanent ephemeral loss should be durable");
    let step = &commit.state.steps[STEP_ID];
    assert_eq!(step.status, StepStatus::ManualRequired);
    assert_eq!(
        step.uncertainty_reason.as_deref(),
        Some("ephemeral_material_lost")
    );
    assert_eq!(
        commit
            .state
            .authorization_consumption
            .values()
            .next()
            .expect("one authorization")
            .uses,
        1
    );
    assert_eq!(
        step.intent
            .as_ref()
            .expect("intent remains auditable")
            .effect_id,
        effect_id
    );
    assert_eq!(
        commit
            .state
            .steps
            .values()
            .filter(|step| step.status == StepStatus::Executing)
            .count(),
        0
    );
    let journal = String::from_utf8(store.export_jsonl().expect("journal should export"))
        .expect("journal should be UTF-8");
    assert!(journal.contains("ephemeral_material_lost"));
    assert!(!journal.contains(SECRET_SENTINEL));
    assert!(!journal.contains(CANONICAL_PATH));
}

#[test]
fn reconstructable_material_revalidates_provider_payload_and_binding_before_returning() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, [instance]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, "original-marker"),
    )
    .expect("invocation should prepare")
    .with_reconstructable_material(
        ReconstructableMaterialReference::new("run-recipe", "recipe-1", "rev-1")
            .expect("reference should validate"),
    );
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, &registry, &mut store, &mut events, &lease)
        .expect("reconstructable intent should commit");
    let AdmissionOutcome::Authorized(admitted) = outcome else {
        panic!("invocation should authorize")
    };
    let record_debug = format!("{:?}", admitted.material_record());
    assert!(!record_debug.contains("recipe-1"));
    drop(admitted);

    let (_wrong_lease_directory, wrong_lease) = acquire_lease("another-run");
    let mut lease_blocked_provider = FixedMaterialProvider::available(
        "run-recipe",
        arguments(CANONICAL_PATH, "original-marker"),
    );
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &wrong_lease,
            &registry,
            &resolver,
            &mut lease_blocked_provider,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::LeaseRunMismatch { .. })
    ));
    assert_eq!(lease_blocked_provider.calls.get(), 0);

    let mut wrong_provider = FixedMaterialProvider::available(
        "different-provider",
        arguments(CANONICAL_PATH, "original-marker"),
    );
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &registry,
            &resolver,
            &mut wrong_provider,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::ProviderNotRegistered)
    ));
    assert_eq!(wrong_provider.calls.get(), 0);

    let mut changed_payload =
        FixedMaterialProvider::available("run-recipe", arguments(CANONICAL_PATH, "changed-marker"));
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &registry,
            &resolver,
            &mut changed_payload,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::MaterialDigestMismatch)
    ));
    assert_eq!(changed_payload.calls.get(), 1);
    let snapshot = store
        .load()
        .expect("store should load")
        .expect("Run should exist");
    assert_eq!(snapshot.records.len(), 3);
    assert_eq!(
        snapshot.state.steps[STEP_ID].status,
        StepStatus::IntentCommitted
    );
}

#[test]
fn material_provider_failures_are_fixed_and_do_not_expose_invocation_data() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    drop(commit_reconstructable(
        &mut store,
        &lease,
        &registry,
        &resolver,
        &definition,
        SECRET_SENTINEL,
    ));
    let mut provider =
        FixedMaterialProvider::available("run-recipe", arguments(CANONICAL_PATH, SECRET_SENTINEL));
    provider
        .failure
        .set(Some(MaterialProviderFailure::Unavailable));

    let error = InvocationMaterialRecovery::new()
        .recover(&store, &lease, &registry, &resolver, &mut provider, STEP_ID)
        .expect_err("provider failure must close recovery");
    let rendered = format!("{error} {error:?}");
    assert!(matches!(
        error,
        MaterialRecoveryError::Provider(MaterialProviderFailure::Unavailable)
    ));
    assert!(!rendered.contains(SECRET_SENTINEL));
    assert!(!rendered.contains(CANONICAL_PATH));
    assert!(!rendered.contains("recipe-1"));
}

#[test]
fn recovery_rejects_definition_and_instance_drift_before_provider_access() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    drop(commit_reconstructable(
        &mut store,
        &lease,
        &registry,
        &resolver,
        &definition,
        "binding-drift",
    ));

    let mut changed_definition = definition.clone();
    changed_definition.spec.input_schema["properties"]["marker"]["minLength"] = json!(2);
    let changed_definition_registry =
        registry_with(&changed_definition, [instance_fixture(&changed_definition)]);
    let mut provider =
        FixedMaterialProvider::available("run-recipe", arguments(CANONICAL_PATH, "binding-drift"));
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &changed_definition_registry,
            &resolver,
            &mut provider,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::DefinitionChanged)
    ));
    assert_eq!(provider.calls.get(), 0);

    let mut changed_instance = instance_fixture(&definition);
    "builtin://test/retargeted-writer".clone_into(&mut changed_instance.binding.binding_ref);
    let changed_instance_registry = registry_with(&definition, [changed_instance]);
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &changed_instance_registry,
            &resolver,
            &mut provider,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::InstanceBindingChanged)
    ));
    assert_eq!(provider.calls.get(), 0);
}

#[test]
fn recovery_revalidates_schema_size_and_resource_resolution() {
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    drop(commit_reconstructable(
        &mut store,
        &lease,
        &registry,
        &resolver,
        &definition,
        "revalidate",
    ));

    let mut invalid_schema =
        FixedMaterialProvider::available("run-recipe", json!({"path": CANONICAL_PATH}));
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &registry,
            &resolver,
            &mut invalid_schema,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::Admission(
            AdmissionError::ArgumentsDoNotConform
        ))
    ));

    let mut oversized = FixedMaterialProvider::available(
        "run-recipe",
        arguments(CANONICAL_PATH, &"x".repeat(1_048_576)),
    );
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &registry,
            &resolver,
            &mut oversized,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::Admission(
            AdmissionError::ArgumentsTooLarge { .. }
        ))
    ));

    let mut rejected_resource =
        FixedMaterialProvider::available("run-recipe", arguments("reject://outside", "revalidate"));
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &registry,
            &resolver,
            &mut rejected_resource,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::InvocationResolution(
            InvocationResolutionError::ResolverRejected { .. }
        ))
    ));
}

#[test]
fn credential_identity_retarget_is_detected_before_material_provider_access() {
    let definition = definition_fixture();
    let mut admitted_instance = instance_fixture(&definition);
    admitted_instance.auth.auth_ref = Some("principal-a".to_owned());
    let registry = registry_with(&definition, [admitted_instance.clone()]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, "credential-binding"),
    )
    .expect("invocation should prepare")
    .with_reconstructable_material(
        ReconstructableMaterialReference::new("run-recipe", "recipe-credential", "rev-1")
            .expect("reference should validate"),
    );
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, &registry, &mut store, &mut events, &lease)
        .expect("intent should commit");
    assert!(matches!(outcome, AdmissionOutcome::Authorized(_)));

    admitted_instance.auth.auth_ref = Some("principal-b".to_owned());
    let changed_registry = registry_with(&definition, [admitted_instance]);
    let mut provider = FixedMaterialProvider::available(
        "run-recipe",
        arguments(CANONICAL_PATH, "credential-binding"),
    );
    assert!(matches!(
        InvocationMaterialRecovery::new().recover(
            &store,
            &lease,
            &changed_registry,
            &resolver,
            &mut provider,
            STEP_ID,
        ),
        Err(MaterialRecoveryError::InstanceBindingChanged)
    ));
    assert_eq!(provider.calls.get(), 0);
}

#[test]
fn unsupported_effect_classes_and_non_once_policy_are_closed() {
    for effect_class in [
        EffectClass::ReadOnly,
        EffectClass::Compensatable,
        EffectClass::Unknown,
    ] {
        let mut definition = definition_fixture();
        definition.spec.effect.class = effect_class;
        let registry = registry_with(&definition, [instance_fixture(&definition)]);
        let resolver = CanonicalResolver::default();
        let mut store = MemoryRunStore::new();
        seed(&mut store, RUN_ID, STEP_ID);
        let (_directory, lease) = acquire_lease(RUN_ID);
        assert!(matches!(
            prepare(
                &store,
                &lease,
                &registry,
                &resolver,
                &definition,
                arguments(CANONICAL_PATH, "unsupported"),
            ),
            Err(AdmissionError::UnsupportedEffectClass { .. })
        ));
    }

    // The derived request lifetime is fixed to Once; a policy layer offering only Run cannot
    // broaden it and therefore produces no authorization.
    let definition = definition_fixture();
    let registry = registry_with(&definition, [instance_fixture(&definition)]);
    let resolver = CanonicalResolver::default();
    let mut store = MemoryRunStore::new();
    let initial = seed(&mut store, RUN_ID, STEP_ID);
    let (_directory, lease) = acquire_lease(RUN_ID);
    let pending = prepare(
        &store,
        &lease,
        &registry,
        &resolver,
        &definition,
        arguments(CANONICAL_PATH, "lifetime"),
    )
    .expect("invocation should prepare");
    let request = pending.permission_request();
    let run_only_allowance = || {
        PolicyAllowance::from_trusted_evaluation(
            request.requested_scopes().iter().cloned(),
            request.resources().iter().cloned(),
            request.critical_actions().iter().copied(),
            [GrantLifetime::Run],
        )
    };
    let inputs = PolicyInputs::local(
        request,
        PolicyContribution::allow(
            source(PolicySourceKind::Host, "host.local", '1'),
            run_only_allowance(),
        ),
        PolicyContribution::allow(
            source(PolicySourceKind::UserProfile, "profile.default", '2'),
            run_only_allowance(),
        ),
    );
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, &registry, &mut store, &mut events, &lease)
        .expect("lifetime coverage mismatch should be auditable");
    assert!(matches!(
        outcome,
        AdmissionOutcome::NotAuthorized(RouteOutcome::Blocked { .. })
    ));
    assert_eq!(
        store.load().expect("store load").expect("Run exists").state,
        initial
    );
}
