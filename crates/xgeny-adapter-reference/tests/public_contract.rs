use std::cell::Cell;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use jsonschema::{Draft, Registry};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};
use xgeny_adapter_reference::{
    MarkerLimits, PreopenedMarkerAdapter, PreopenedMarkerVerifier, REFERENCE_CAPABILITY_ID,
    REFERENCE_CONTRACT_VERSION, ReferenceAdapterConfigError,
};
use xgeny_domain::{
    Architecture, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef, DataBoundary,
    EffectClass, ExecutionStyle, GrantLifetime, OperatingSystem, Platform, PolicySource,
    PolicySourceKind, ProtocolDocument, ReceiptStatus, TrustLevel, VerificationStrategy,
};
use xgeny_local_store::{
    Commit, ExpectedHead, MemoryRunStore, RunSnapshot, RunStore, SqliteRunStore, StoreError,
};
use xgeny_policy::{
    PolicyAllowance, PolicyContribution, PolicyInputs, ResolvedPermissionRequest,
    ResourceResolutionFailure, ResourceResolver,
};
use xgeny_runtime::{
    AdapterExecutionObservation, AdapterPrepareFailure, AdapterPrepareRequest,
    AdapterReconcileRequest, AdapterReconciliationObservation, AdmissionOutcome, AdmissionRequest,
    CapabilityRegistry, DirectExecutor, DirectExecutorError, DriveAction, EffectAdapter,
    EffectAdapterRegistry, EffectVerifierRegistry, EventFactory, EventFactoryError, EventMetadata,
    InvocationAdmission, InvocationMaterialProvider, InvocationMaterialRecovery, LocalRunLease,
    MaterialProviderFailure, MaterialProviderRegistry, PreparedAdapterInvocation,
    RequiredRouteFeatures, RouteRequest, VerificationRunner,
};
use xgeny_workgraph::{
    InvocationMaterialRecord, ReconstructableMaterialReference, RunEvent, RunEventBody, RunState,
    StepStatus,
};

const RUN_ID: &str = "run-reference-adapter-1";
const STEP_ID: &str = "step-reference-adapter-1";
const AUTHORITY: &str = "local:test";
const AUTHORITY_EPOCH: u64 = 31;
const RAW_TARGET: &str = "fixture-alias";
const CANONICAL_TARGET: &str = "fixture-primary";
const MARKER_SENTINEL: &str = "REFERENCE-ADAPTER-RAW-MARKER-MUST-NOT-PERSIST";
const CRASH_CHILD_MARKER: &str = "XGENY_REFERENCE_ADAPTER_CRASH_CHILD";
const CRASH_DATABASE_PATH: &str = "XGENY_REFERENCE_ADAPTER_CRASH_DB";
const CRASH_LOCK_PATH: &str = "XGENY_REFERENCE_ADAPTER_CRASH_LOCK";
const CRASH_TARGET_PATH: &str = "XGENY_REFERENCE_ADAPTER_CRASH_TARGET";

#[derive(Debug, Default)]
struct FixtureResolver;

impl ResourceResolver for FixtureResolver {
    fn resolve(&self, scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        if scope != "fixture.write" {
            return Err(ResourceResolutionFailure::UnsupportedScope);
        }
        match resource {
            RAW_TARGET | CANONICAL_TARGET => Ok(CANONICAL_TARGET.to_owned()),
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
            event_id: format!("reference-event-{next}"),
            recorded_at: "2026-08-29T20:00:00Z".to_owned(),
        })
    }
}

struct ObservingStore {
    inner: MemoryRunStore,
    target_probe: File,
    fail_started_once: bool,
    fail_succeeded_once: bool,
    started_commits: Cell<usize>,
    succeeded_commits: Cell<usize>,
}

impl ObservingStore {
    fn new(target_probe: File) -> Self {
        Self {
            inner: MemoryRunStore::new(),
            target_probe,
            fail_started_once: false,
            fail_succeeded_once: false,
            started_commits: Cell::new(0),
            succeeded_commits: Cell::new(0),
        }
    }

    fn observe(&mut self, event: &RunEvent) -> Result<(), StoreError> {
        match event.body {
            RunEventBody::EffectExecutionStarted { .. } => {
                assert_eq!(
                    target_len(&self.target_probe),
                    0,
                    "prepare mutated the preopened file"
                );
                self.started_commits.set(self.started_commits.get() + 1);
                if self.fail_started_once {
                    self.fail_started_once = false;
                    return Err(StoreError::InjectedFault("reference start commit"));
                }
            }
            RunEventBody::EffectSucceeded { .. } => {
                assert!(
                    target_len(&self.target_probe) > 0,
                    "success was committed before physical file I/O"
                );
                self.succeeded_commits.set(self.succeeded_commits.get() + 1);
                if self.fail_succeeded_once {
                    self.fail_succeeded_once = false;
                    return Err(StoreError::InjectedFault("reference outcome commit"));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl RunStore for ObservingStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        self.observe(&event)?;
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
        self.observe(&event)?;
        self.inner
            .append_with_invocation_material(expected, event, material)
    }

    fn load_invocation_material(
        &self,
        effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        self.inner.load_invocation_material(effect_id)
    }

    fn append_with_execution_receipt(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        receipt: xgeny_domain::ExecutionReceiptBody,
    ) -> Result<Commit, StoreError> {
        self.observe(&event)?;
        self.inner
            .append_with_execution_receipt(expected, event, receipt)
    }

    fn load_execution_receipts(
        &self,
    ) -> Result<Vec<xgeny_domain::ExecutionReceiptBody>, StoreError> {
        self.inner.load_execution_receipts()
    }
}

struct Fixture {
    _directory: TempDir,
    target: File,
    probe: File,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempdir().expect("temporary fixture directory should exist");
        let path = directory.path().join("preopened-target.bin");
        let target = open_target(&path);
        let probe = target
            .try_clone()
            .expect("preopened target should be clonable for observation");
        Self {
            _directory: directory,
            target,
            probe,
        }
    }
}

fn open_target(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .expect("fixture target should open")
}

fn target_len(file: &File) -> u64 {
    file.metadata().expect("target metadata should load").len()
}

fn target_bytes(file: &mut File) -> Vec<u8> {
    file.seek(SeekFrom::Start(0))
        .expect("target should seek for inspection");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .expect("target should be readable for inspection");
    bytes
}

fn assert_bytes_exclude(bytes: &[u8], sentinels: &[&[u8]]) {
    for sentinel in sentinels {
        assert!(
            !bytes
                .windows(sentinel.len())
                .any(|window| window == *sentinel)
        );
    }
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}

fn assert_directory_excludes(directory: &Path, sentinels: &[&[u8]]) {
    for entry in std::fs::read_dir(directory).expect("artifact directory should be readable") {
        let path = entry.expect("artifact entry should be readable").path();
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).expect("closed artifact should be readable");
        for sentinel in sentinels {
            assert!(
                !bytes
                    .windows(sentinel.len())
                    .any(|window| window == *sentinel),
                "raw invocation material leaked into {}",
                path.display()
            );
        }
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
    REFERENCE_CAPABILITY_ID.clone_into(&mut definition.metadata.id);
    REFERENCE_CONTRACT_VERSION.clone_into(&mut definition.metadata.contract_version);
    "Commit fixture marker".clone_into(&mut definition.metadata.display_name);
    definition.extensions.clear();
    definition.required_extensions.clear();
    definition.metadata.labels.clear();
    "Commit canonical evidence to a host-preopened conformance fixture."
        .clone_into(&mut definition.spec.summary);
    definition.spec.input_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["targetRef", "marker"],
        "properties": {
            "targetRef": {"type": "string", "minLength": 1},
            "marker": {"type": "string", "minLength": 1}
        },
        "additionalProperties": false
    });
    definition.spec.output_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false
    });
    definition.spec.effect.class = EffectClass::Idempotent;
    "fixture.write".clone_into(&mut definition.spec.effect.resource_selectors[0].scope);
    "/targetRef".clone_into(&mut definition.spec.effect.resource_selectors[0].argument_pointer);
    definition.spec.effect.critical_actions.clear();
    definition.spec.required_capabilities.clear();
    definition.spec.execution.styles = vec![ExecutionStyle::Sync];
    definition.spec.execution.cancellable = false;
    definition.spec.execution.idempotency_key_supported = true;
    definition.spec.execution.default_timeout_ms = 1_000;
    definition.spec.execution.max_timeout_ms = 5_000;
    definition.spec.verification.truncate(1);
    definition.spec.verification[0].strategy = VerificationStrategy::Postcondition;
    definition.spec.verification[0].required = true;
    definition.spec.verification[0].description = Some(
        "Reported evidence digest matches the canonical bytes in the preopened fixture.".to_owned(),
    );
    if let Some(discovery) = &mut definition.spec.discovery {
        discovery.keywords = vec!["fixture".to_owned(), "conformance".to_owned()];
        discovery.examples = vec!["Commit one reference conformance marker.".to_owned()];
        discovery.details_ref = None;
    }
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
    "reference.preopened.marker.v1".clone_into(&mut instance.instance_id);
    instance.definition = capability(definition);
    instance.platform = Platform {
        os: OperatingSystem::Any,
        arch: Architecture::Any,
    };
    instance.features.cancellable = false;
    instance.features.idempotency_query = false;
    instance.hints = None;
    "builtin://reference/preopened-marker".clone_into(&mut instance.binding.binding_ref);
    instance.binding.operation_ref = Some("commitMarker".to_owned());
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

fn assert_protocol_schema(document: &ProtocolDocument, schema_source: &str) {
    let common: Value = serde_json::from_str(include_str!(
        "../../../protocol/schema/v1alpha1/common.schema.json"
    ))
    .expect("common schema should parse");
    let schema: Value = serde_json::from_str(schema_source).expect("document schema should parse");
    let registry = Registry::new()
        .add(
            "https://schemas.xgeny.dev/v1alpha1/common.schema.json",
            common,
        )
        .expect("common schema should register")
        .prepare()
        .expect("offline schema registry should prepare");
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_registry(&registry)
        .offline()
        .should_validate_formats(true)
        .build(&schema)
        .expect("document schema should compile offline");
    let value = serde_json::to_value(document).expect("protocol document should serialize");
    validator
        .validate(&value)
        .expect("reference protocol document should satisfy its bundled schema");
}

fn current_platform() -> Platform {
    let os = if cfg!(target_os = "linux") {
        OperatingSystem::Linux
    } else if cfg!(target_os = "macos") {
        OperatingSystem::Macos
    } else if cfg!(target_os = "windows") {
        OperatingSystem::Windows
    } else {
        panic!("unsupported conformance test OS")
    };
    let arch = if cfg!(target_arch = "x86_64") {
        Architecture::X86_64
    } else if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else {
        panic!("unsupported conformance test architecture")
    };
    Platform { os, arch }
}

fn route_request(definition: &CapabilityDefinitionBody) -> RouteRequest {
    RouteRequest {
        capability: capability(definition),
        target_platform: current_platform(),
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

fn seed<S: RunStore>(store: &mut S) {
    let created = store
        .append(
            ExpectedHead::Empty,
            RunEvent {
                event_id: "reference-seed-1".to_owned(),
                run_id: RUN_ID.to_owned(),
                authority: AUTHORITY.to_owned(),
                authority_epoch: AUTHORITY_EPOCH,
                recorded_at: "2026-08-29T20:00:00Z".to_owned(),
                body: RunEventBody::RunCreated {
                    goal: "verify one external reference adapter".to_owned(),
                },
            },
        )
        .expect("Run should seed")
        .state;
    store
        .append(
            ExpectedHead::from_state(&created),
            RunEvent {
                event_id: "reference-seed-2".to_owned(),
                run_id: RUN_ID.to_owned(),
                authority: AUTHORITY.to_owned(),
                authority_epoch: AUTHORITY_EPOCH,
                recorded_at: "2026-08-29T20:00:00Z".to_owned(),
                body: RunEventBody::StepPlanned {
                    step_id: STEP_ID.to_owned(),
                    objective: "commit a marker through a preopened handle".to_owned(),
                    depends_on: Vec::new(),
                },
            },
        )
        .expect("Step should seed");
}

fn acquire_lease() -> (TempDir, LocalRunLease) {
    let directory = tempdir().expect("temporary lease directory should exist");
    let lease = LocalRunLease::try_acquire(RUN_ID, directory.path().join("run.lock"))
        .expect("Run lease should acquire");
    (directory, lease)
}

fn admit(
    store: &mut impl RunStore,
    lease: &LocalRunLease,
    registry: &CapabilityRegistry,
    definition: &CapabilityDefinitionBody,
    marker: &str,
    reconstructable: bool,
) -> Box<xgeny_runtime::AdmittedEffect> {
    let mut pending = InvocationAdmission::new()
        .prepare(
            store,
            lease,
            registry,
            &FixtureResolver,
            AdmissionRequest {
                step_id: STEP_ID.to_owned(),
                route: route_request(definition),
                arguments: json!({"targetRef": RAW_TARGET, "marker": marker}),
            },
        )
        .expect("invocation should prepare");
    if reconstructable {
        pending = pending.with_reconstructable_material(
            ReconstructableMaterialReference::new("fixture-recipe", "marker-1", "rev-1")
                .expect("reference should validate"),
        );
    }
    let inputs = allow_inputs(pending.permission_request());
    let mut events = DeterministicEvents;
    let outcome = InvocationAdmission::new()
        .authorize_and_commit(pending, &inputs, registry, store, &mut events, lease)
        .expect("invocation should authorize");
    let AdmissionOutcome::Authorized(admitted) = outcome else {
        panic!("reference invocation should authorize")
    };
    admitted
}

struct FixedMaterialProvider;

impl InvocationMaterialProvider for FixedMaterialProvider {
    fn reconstruct(
        &mut self,
        reference_id: &str,
        revision: &str,
    ) -> Result<Value, MaterialProviderFailure> {
        if reference_id != "marker-1" {
            return Err(MaterialProviderFailure::NotFound);
        }
        if revision != "rev-1" {
            return Err(MaterialProviderFailure::RevisionChanged);
        }
        Ok(json!({
            "targetRef": CANONICAL_TARGET,
            "marker": MARKER_SENTINEL
        }))
    }
}

struct ExitAfterIoSession {
    inner: Box<dyn PreparedAdapterInvocation>,
}

impl PreparedAdapterInvocation for ExitAfterIoSession {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        match self.inner.execute() {
            AdapterExecutionObservation::Succeeded { .. } => std::process::exit(86),
            observation => observation,
        }
    }
}

struct ExitAfterIoAdapter {
    inner: PreopenedMarkerAdapter,
}

impl EffectAdapter for ExitAfterIoAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        self.inner.prepare(request).map(|inner| {
            Box::new(ExitAfterIoSession { inner }) as Box<dyn PreparedAdapterInvocation>
        })
    }

    fn reconcile(
        &mut self,
        request: AdapterReconcileRequest<'_>,
    ) -> AdapterReconciliationObservation {
        self.inner.reconcile(request)
    }
}

fn run_crash_child_if_requested() -> bool {
    if std::env::var_os(CRASH_CHILD_MARKER).is_none() {
        return false;
    }
    let database_path = std::env::var_os(CRASH_DATABASE_PATH).expect("child database path");
    let lock_path = std::env::var_os(CRASH_LOCK_PATH).expect("child lock path");
    let target_path: PathBuf = std::env::var_os(CRASH_TARGET_PATH)
        .expect("child target path")
        .into();
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    let mut store = SqliteRunStore::open(database_path).expect("child store should open");
    let lease = LocalRunLease::try_acquire(RUN_ID, lock_path).expect("child lease should acquire");
    let mut providers = MaterialProviderRegistry::new();
    providers
        .register("fixture-recipe", FixedMaterialProvider)
        .expect("child provider should register");
    let material = InvocationMaterialRecovery::new()
        .recover(
            &store,
            &lease,
            &registry,
            &FixtureResolver,
            &mut providers,
            STEP_ID,
        )
        .expect("child material should recover");
    let target = OpenOptions::new()
        .read(true)
        .write(true)
        .open(target_path)
        .expect("child preopened target should open");
    let inner = PreopenedMarkerAdapter::new(
        target,
        instance.binding.clone(),
        CANONICAL_TARGET,
        MarkerLimits::new(128).expect("child limit"),
    )
    .expect("child reference adapter should construct");
    let mut adapters = EffectAdapterRegistry::new();
    adapters
        .register(&instance.binding, ExitAfterIoAdapter { inner })
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
    panic!("child must exit after the preopened file effect")
}

fn register_reference(
    adapters: &mut EffectAdapterRegistry,
    instance: &CapabilityInstanceBody,
    target: File,
    max_marker_bytes: usize,
) -> PreopenedMarkerVerifier {
    let adapter = PreopenedMarkerAdapter::new(
        target,
        instance.binding.clone(),
        CANONICAL_TARGET,
        MarkerLimits::new(max_marker_bytes).expect("test limit should validate"),
    )
    .expect("reference adapter should construct");
    let rendered = format!("{adapter:?}");
    assert!(!rendered.contains(CANONICAL_TARGET));
    assert!(!rendered.contains(&instance.binding.binding_ref));
    let verifier = adapter.verifier();
    adapters
        .register(&instance.binding, adapter)
        .expect("reference adapter should register");
    verifier
}

#[test]
fn reference_documents_satisfy_the_bundled_protocol_schemas() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    assert_protocol_schema(
        &ProtocolDocument::CapabilityDefinition(Box::new(definition)),
        include_str!("../../../protocol/schema/v1alpha1/capability-definition.schema.json"),
    );
    assert_protocol_schema(
        &ProtocolDocument::CapabilityInstance(Box::new(instance)),
        include_str!("../../../protocol/schema/v1alpha1/capability-instance.schema.json"),
    );
}

#[test]
fn invalid_configuration_is_fixed_and_does_not_echo_the_target() {
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    for candidate in [
        "",
        ".",
        "..",
        "../DO-NOT-ECHO-PREOPENED-TARGET",
        "nested/target",
        r"nested\target",
        "C:target",
    ] {
        let fixture = Fixture::new();
        let error = PreopenedMarkerAdapter::new(
            fixture.target,
            instance.binding.clone(),
            candidate,
            MarkerLimits::new(64).expect("limit"),
        )
        .expect_err("path-like target references must be rejected");

        assert_eq!(error, ReferenceAdapterConfigError::InvalidTargetReference);
        assert_eq!(
            error.to_string(),
            "reference adapter target reference is invalid"
        );
        assert_eq!(format!("{error:?}"), "InvalidTargetReference");
    }
    assert!(matches!(
        MarkerLimits::new(0),
        Err(ReferenceAdapterConfigError::InvalidMarkerLimit)
    ));
    assert!(matches!(
        MarkerLimits::new(64 * 1024 + 1),
        Err(ReferenceAdapterConfigError::InvalidMarkerLimit)
    ));
}

#[test]
fn public_contract_executes_real_io_only_between_started_and_success() {
    let fixture = Fixture::new();
    let mut store = ObservingStore::new(fixture.probe);
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(
        &mut store,
        &lease,
        &registry,
        &definition,
        MARKER_SENTINEL,
        false,
    ))
    .into_ephemeral_material()
    .expect("material should verify");
    let mut adapters = EffectAdapterRegistry::new();
    let verifier = register_reference(&mut adapters, &instance, fixture.target, 128);
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
        .expect("reference adapter should execute");

    assert_eq!(report.action, DriveAction::EffectSucceeded);
    assert_eq!(report.state.steps[STEP_ID].status, StepStatus::Validating);
    assert_eq!(store.started_commits.get(), 1);
    assert_eq!(store.succeeded_commits.get(), 1);
    assert!(target_len(&store.target_probe) > 0);
    let evidence = target_bytes(&mut store.target_probe);
    let intent = report.state.steps[STEP_ID]
        .intent
        .as_ref()
        .expect("successful effect should retain its durable intent");
    let expected_evidence = serde_jcs::to_vec(&json!({
        "domain": "xgeny.reference-marker-evidence/v1",
        "effectId": intent.effect_id,
        "actionDigest": intent.action_digest,
        "materialDigest": intent.authorization.binding.material_digest,
        "instanceId": instance.instance_id,
        "instanceBindingDigest": intent.invocation.instance_binding_digest,
        "idempotencyKey": intent.idempotency_key.as_deref().expect("stable key")
    }))
    .expect("expected evidence should canonicalize");
    assert_eq!(evidence, expected_evidence);
    assert_bytes_exclude(
        &evidence,
        &[
            MARKER_SENTINEL.as_bytes(),
            CANONICAL_TARGET.as_bytes(),
            RAW_TARGET.as_bytes(),
        ],
    );
    assert_eq!(
        report.state.steps[STEP_ID]
            .effect_evidence_digest
            .as_deref(),
        Some(sha256_digest(&evidence).as_str())
    );
    let journal = String::from_utf8(store.inner.export_jsonl().expect("journal export"))
        .expect("journal should be UTF-8");
    assert!(!journal.contains(MARKER_SENTINEL));
    assert!(!journal.contains(CANONICAL_TARGET));
    assert!(!journal.contains(RAW_TARGET));
    assert!(!format!("{adapters:?}").contains(CANONICAL_TARGET));
    assert!(!format!("{adapters:?}").contains(RAW_TARGET));

    let mut verifiers = EffectVerifierRegistry::new();
    verifiers
        .register(&instance.binding, verifier)
        .expect("reference verifier should register");
    let terminal_report = VerificationRunner::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut verifiers,
            STEP_ID,
        )
        .expect("reference evidence should verify");
    assert_eq!(terminal_report.action, DriveAction::VerificationPassed);
    assert_eq!(
        terminal_report.state.steps[STEP_ID].status,
        StepStatus::Completed
    );
    let receipts = store
        .load_execution_receipts()
        .expect("reference Receipt should load");
    assert_eq!(receipts.len(), 1);
    let receipt_json = serde_json::to_string(&receipts[0]).expect("Receipt should serialize");
    assert!(!receipt_json.contains(MARKER_SENTINEL));
    assert!(!receipt_json.contains(CANONICAL_TARGET));
    assert!(!receipt_json.contains(RAW_TARGET));
}

#[test]
fn sqlite_validating_restart_runs_only_the_read_only_reference_verifier() {
    let run_directory = tempdir().expect("temporary Run directory should exist");
    let database_path = run_directory.path().join("run.db");
    let fixture = Fixture::new();
    let mut probe = fixture.probe;
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    let (_lease_directory, lease) = acquire_lease();
    let verifier = {
        let mut store = SqliteRunStore::open(&database_path).expect("SQLite should open");
        seed(&mut store);
        let material = (*admit(
            &mut store,
            &lease,
            &registry,
            &definition,
            MARKER_SENTINEL,
            false,
        ))
        .into_ephemeral_material()
        .expect("material should verify");
        let mut adapters = EffectAdapterRegistry::new();
        let verifier = register_reference(&mut adapters, &instance, fixture.target, 128);
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
            .expect("reference effect should execute");
        assert_eq!(report.state.steps[STEP_ID].status, StepStatus::Validating);
        drop(adapters);
        verifier
    };
    let evidence_before_restart = target_bytes(&mut probe);

    let mut reopened = SqliteRunStore::open(&database_path).expect("SQLite should reopen");
    let mut verifiers = EffectVerifierRegistry::new();
    verifiers
        .register(&instance.binding, verifier)
        .expect("reference verifier should register");
    let mut events = DeterministicEvents;
    let report = VerificationRunner::new()
        .drive_step(
            &mut reopened,
            &mut events,
            &lease,
            &registry,
            &mut verifiers,
            STEP_ID,
        )
        .expect("restart should resume verification");

    assert_eq!(report.action, DriveAction::VerificationPassed);
    assert_eq!(report.state.steps[STEP_ID].status, StepStatus::Completed);
    assert_eq!(target_bytes(&mut probe), evidence_before_restart);
    assert_eq!(
        reopened
            .load_execution_receipts()
            .expect("Receipt should load")
            .len(),
        1
    );
    drop(reopened);
    assert_directory_excludes(
        run_directory.path(),
        &[
            MARKER_SENTINEL.as_bytes(),
            CANONICAL_TARGET.as_bytes(),
            RAW_TARGET.as_bytes(),
        ],
    );
}

#[test]
fn target_tampering_before_verification_fails_without_reexecuting_the_effect() {
    let fixture = Fixture::new();
    let mut store = ObservingStore::new(fixture.probe);
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(
        &mut store,
        &lease,
        &registry,
        &definition,
        MARKER_SENTINEL,
        false,
    ))
    .into_ephemeral_material()
    .expect("material should verify");
    let mut adapters = EffectAdapterRegistry::new();
    let verifier = register_reference(&mut adapters, &instance, fixture.target, 128);
    let mut events = DeterministicEvents;
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
        .expect("reference effect should execute");
    assert_eq!(executed.state.steps[STEP_ID].status, StepStatus::Validating);

    store
        .target_probe
        .seek(SeekFrom::Start(0))
        .expect("probe should seek");
    store
        .target_probe
        .set_len(0)
        .expect("probe should truncate");
    store
        .target_probe
        .write_all(b"tampered-after-effect")
        .expect("test tampering should write");
    store
        .target_probe
        .sync_all()
        .expect("tampering should sync");
    let tampered = target_bytes(&mut store.target_probe);

    let mut verifiers = EffectVerifierRegistry::new();
    verifiers
        .register(&instance.binding, verifier)
        .expect("reference verifier should register");
    let terminal_report = VerificationRunner::new()
        .drive_step(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &mut verifiers,
            STEP_ID,
        )
        .expect("tampering should produce a failed Receipt");
    assert_eq!(terminal_report.action, DriveAction::VerificationFailed);
    assert_eq!(
        terminal_report.state.steps[STEP_ID].status,
        StepStatus::Failed
    );
    assert_eq!(target_bytes(&mut store.target_probe), tampered);
    let receipts = store
        .load_execution_receipts()
        .expect("Receipt should load");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].status, ReceiptStatus::Failed);
}

#[test]
fn preopened_io_failure_becomes_fixed_unknown_without_os_error_text() {
    let directory = tempdir().expect("temporary fixture directory should exist");
    let path = directory.path().join("read-only-target.bin");
    drop(open_target(&path));
    let target = OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("read-only fixture handle should open");
    let probe = target.try_clone().expect("target probe should clone");
    let mut store = ObservingStore::new(probe);
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(
        &mut store,
        &lease,
        &registry,
        &definition,
        MARKER_SENTINEL,
        false,
    ))
    .into_ephemeral_material()
    .expect("material should verify");
    let mut adapters = EffectAdapterRegistry::new();
    register_reference(&mut adapters, &instance, target, 128);
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
        .expect("post-start I/O failure should be a durable unknown observation");

    assert_eq!(report.action, DriveAction::EffectUnknown);
    assert_eq!(
        report.state.steps[STEP_ID].status,
        StepStatus::EffectUnknown
    );
    assert_eq!(target_len(&store.target_probe), 0);
    let journal = String::from_utf8(store.inner.export_jsonl().expect("journal export"))
        .expect("journal should be UTF-8");
    assert!(journal.contains("adapter_response_unverifiable"));
    assert!(!journal.contains(&path.display().to_string()));
    assert!(!journal.contains(MARKER_SENTINEL));
}

#[test]
fn failed_started_commit_keeps_the_preopened_target_untouched_and_retryable() {
    let fixture = Fixture::new();
    let mut store = ObservingStore::new(fixture.probe);
    store.fail_started_once = true;
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(
        &mut store,
        &lease,
        &registry,
        &definition,
        MARKER_SENTINEL,
        false,
    ))
    .into_ephemeral_material()
    .expect("material should verify");
    let mut adapters = EffectAdapterRegistry::new();
    register_reference(&mut adapters, &instance, fixture.target, 128);
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
    assert_eq!(target_len(&store.target_probe), 0);
    assert_eq!(
        store.load().expect("load").expect("Run").state.steps[STEP_ID].status,
        StepStatus::IntentCommitted
    );

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
        .expect("fresh session should retry after a failed start commit");
    assert_eq!(retried.action, DriveAction::EffectSucceeded);
    assert!(target_len(&store.target_probe) > 0);
}

#[test]
fn adapter_limit_failure_is_closed_before_started_and_io() {
    let fixture = Fixture::new();
    let mut store = ObservingStore::new(fixture.probe);
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(
        &mut store,
        &lease,
        &registry,
        &definition,
        MARKER_SENTINEL,
        false,
    ))
    .into_ephemeral_material()
    .expect("material should verify");
    let mut adapters = EffectAdapterRegistry::new();
    register_reference(&mut adapters, &instance, fixture.target, 8);
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
            AdapterPrepareFailure::InvalidMaterial
        ))
    ));
    assert_eq!(store.started_commits.get(), 0);
    assert_eq!(target_len(&store.target_probe), 0);
}

#[test]
fn nearby_operation_or_protocol_binding_never_receives_the_reference_invocation() {
    for mismatch in ["operation", "protocol"] {
        let fixture = Fixture::new();
        let mut store = ObservingStore::new(fixture.probe);
        let definition = definition_fixture();
        let instance = instance_fixture(&definition);
        let registry = registry_with(&definition, instance.clone());
        seed(&mut store);
        let (_lease_directory, lease) = acquire_lease();
        let material = (*admit(
            &mut store,
            &lease,
            &registry,
            &definition,
            MARKER_SENTINEL,
            false,
        ))
        .into_ephemeral_material()
        .expect("material should verify");
        let mut nearby = instance.binding.clone();
        match mismatch {
            "operation" => nearby.operation_ref = Some("nearbyOperation".to_owned()),
            "protocol" => nearby.protocol_version = Some("2".to_owned()),
            _ => unreachable!("test table is exhaustive"),
        }
        let adapter = PreopenedMarkerAdapter::new(
            fixture.target,
            nearby.clone(),
            CANONICAL_TARGET,
            MarkerLimits::new(128).expect("limit"),
        )
        .expect("nearby adapter should construct");
        let mut adapters = EffectAdapterRegistry::new();
        adapters
            .register(&nearby, adapter)
            .expect("nearby binding should register independently");
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
            Err(DirectExecutorError::AdapterNotRegistered { .. })
        ));
        assert_eq!(store.started_commits.get(), 0);
        assert_eq!(target_len(&store.target_probe), 0);
    }
}

#[test]
fn adapter_rechecks_its_configured_binding_after_exact_registry_dispatch() {
    let fixture = Fixture::new();
    let mut store = ObservingStore::new(fixture.probe);
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(
        &mut store,
        &lease,
        &registry,
        &definition,
        MARKER_SENTINEL,
        false,
    ))
    .into_ephemeral_material()
    .expect("material should verify");
    let mut configured_binding = instance.binding.clone();
    configured_binding.operation_ref = Some("misconfiguredOperation".to_owned());
    let adapter = PreopenedMarkerAdapter::new(
        fixture.target,
        configured_binding,
        CANONICAL_TARGET,
        MarkerLimits::new(128).expect("limit"),
    )
    .expect("misconfigured adapter should construct");
    let mut adapters = EffectAdapterRegistry::new();
    adapters
        .register(&instance.binding, adapter)
        .expect("host registry key should register");
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
            AdapterPrepareFailure::UnsupportedProtocol
        ))
    ));
    assert_eq!(store.started_commits.get(), 0);
    assert_eq!(target_len(&store.target_probe), 0);
}

#[test]
fn lost_outcome_never_reexecutes_the_preopened_effect() {
    let fixture = Fixture::new();
    let mut store = ObservingStore::new(fixture.probe);
    store.fail_succeeded_once = true;
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance.clone());
    seed(&mut store);
    let (_lease_directory, lease) = acquire_lease();
    let material = (*admit(
        &mut store,
        &lease,
        &registry,
        &definition,
        MARKER_SENTINEL,
        false,
    ))
    .into_ephemeral_material()
    .expect("material should verify");
    let mut adapters = EffectAdapterRegistry::new();
    register_reference(&mut adapters, &instance, fixture.target, 128);
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
    assert_eq!(
        store.load().expect("load").expect("Run").state.steps[STEP_ID].status,
        StepStatus::Executing
    );
    let applied_size = target_len(&store.target_probe);
    assert!(applied_size > 0);

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
        .expect("Executing recovery should not need the adapter");
    assert_eq!(recovered.action, DriveAction::ExecutionRecoveredAsUnknown);
    assert_eq!(target_len(&store.target_probe), applied_size);
}

#[test]
fn sqlite_process_exit_after_reference_io_never_reexecutes() {
    if run_crash_child_if_requested() {
        return;
    }

    let directory = tempdir().expect("temporary Run directory should exist");
    let database_path = directory.path().join("run.db");
    let lock_path = directory.path().join("run.lock");
    let target_path = directory.path().join("preopened-target.bin");
    drop(open_target(&target_path));

    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let registry = registry_with(&definition, instance);
    {
        let mut store = SqliteRunStore::open(&database_path).expect("parent store should open");
        let lease =
            LocalRunLease::try_acquire(RUN_ID, &lock_path).expect("parent lease should acquire");
        seed(&mut store);
        drop(admit(
            &mut store,
            &lease,
            &registry,
            &definition,
            MARKER_SENTINEL,
            true,
        ));
    }

    let status = Command::new(std::env::current_exe().expect("test executable should exist"))
        .args([
            "--exact",
            "sqlite_process_exit_after_reference_io_never_reexecutes",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD_MARKER, "1")
        .env(CRASH_DATABASE_PATH, &database_path)
        .env(CRASH_LOCK_PATH, &lock_path)
        .env(CRASH_TARGET_PATH, &target_path)
        .status()
        .expect("crash child should start");
    assert_eq!(status.code(), Some(86));

    let sentinels: [&[u8]; 3] = [
        MARKER_SENTINEL.as_bytes(),
        RAW_TARGET.as_bytes(),
        CANONICAL_TARGET.as_bytes(),
    ];
    let wal_path = directory.path().join("run.db-wal");
    assert!(
        wal_path.metadata().is_ok_and(|metadata| metadata.len() > 0),
        "crash test must observe an uncheckpointed SQLite WAL before recovery"
    );
    assert_directory_excludes(directory.path(), &sentinels);

    let applied = std::fs::read(&target_path).expect("applied evidence should be readable");
    assert!(!applied.is_empty());
    for sentinel in sentinels {
        assert!(
            !applied
                .windows(sentinel.len())
                .any(|window| window == sentinel),
            "physical evidence must not contain raw invocation material"
        );
    }

    {
        let mut store = SqliteRunStore::open(&database_path).expect("store should recover");
        assert_eq!(
            store.load().expect("load").expect("Run").state.steps[STEP_ID].status,
            StepStatus::Executing
        );
        let lease = LocalRunLease::try_acquire(RUN_ID, &lock_path)
            .expect("crashed child should release its lease");
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
    }

    assert_eq!(
        std::fs::read(&target_path).expect("evidence should remain readable"),
        applied,
        "recovery must not execute the physical effect again"
    );
    assert_directory_excludes(directory.path(), &sentinels);
}
