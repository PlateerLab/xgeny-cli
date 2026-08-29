use std::cell::Cell;

use serde_json::json;
use tempfile::{TempDir, tempdir};
use xgeny_domain::{
    Architecture, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef, CriticalAction,
    DataBoundary, EffectClass, ExecutionStyle, GrantLifetime, OperatingSystem, Platform,
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
    EventFactoryError, EventMetadata, InvocationAdmission, LocalRunLease, RequiredRouteFeatures,
    RouteOutcome, RouteRequest,
};
use xgeny_workgraph::{RunEvent, RunEventBody, RunState, StepStatus};

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
                },
            ),
        )
        .expect("Step should be planned")
        .state
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
        admitted.normalized_arguments()["path"],
        json!(CANONICAL_PATH)
    );
    assert_eq!(
        admitted.normalized_arguments()["marker"],
        json!(SECRET_SENTINEL)
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
struct LostAcknowledgementStore {
    inner: MemoryRunStore,
    lose_effect_commit_once: bool,
}

impl RunStore for LostAcknowledgementStore {
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
    .expect("invocation should prepare");
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
