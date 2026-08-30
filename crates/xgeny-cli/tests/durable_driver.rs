use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::num::NonZeroU32;
use std::rc::Rc;

use serde_json::{Value, json};
use tempfile::tempdir;
use xgeny_cli::{
    ApprovalDecision, ApprovalPort, ApprovalPortFailure, DriverOutcome, PlannedRouteFailure,
    PlannedRoutePort, RunDriver,
};
use xgeny_domain::{
    Architecture, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef, DataBoundary,
    EffectClass, GrantLifetime, OperatingSystem, Platform, PolicySource, PolicySourceKind,
    ProtocolDocument, TrustLevel, VerificationResult,
};
use xgeny_local_store::{ExpectedHead, RunStore, SqliteRunStore};
use xgeny_policy::{
    PolicyAllowance, PolicyContribution, PolicyInputs, ResolvedPermissionRequest,
    ResourceResolutionFailure, ResourceResolver,
};
use xgeny_runtime::{
    AdapterEvidenceDigest, AdapterExecutionObservation, AdapterPrepareFailure,
    AdapterPrepareRequest, AdapterReconcileRequest, AdapterReconciliationInconclusiveReason,
    AdapterReconciliationObservation, AgentLoop, CapabilityRegistry, EffectAdapter,
    EffectAdapterRegistry, EffectVerifier, EffectVerifierRegistry, EventFactory, EventFactoryError,
    EventMetadata, InvocationMaterialProvider, LocalRunLease, MaterialProviderFailure,
    MaterialProviderRegistry, PlanMaterializationRequest, PlanMaterializer,
    PlanMaterializerFailure, PlanProposal, PlannerCallRequest, PlannerPort, PlannerPortFailure,
    PreparedAdapterInvocation, ProposedPlanStep, RequiredRouteFeatures, RouteRequest,
    RuleVerificationObservation, VerificationPortFailure, VerificationReport, VerificationRequest,
    VerifiedArtifactDescriptor, VerifierOutputDigest,
};
use xgeny_workgraph::{
    AgentLoopBudget, PlannedExecutionProfile, ReconstructableMaterialReference, RunEvent,
    RunEventBody, RunState, StepStatus,
};

const RUN_ID: &str = "run-cli-driver-read-only";
const RAW_PATH: &str = "RAW-DRIVER-PATH/../README.md";
const CANONICAL_PATH: &str = "workspace:fixture/README.md";
const OUTPUT_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EVIDENCE_DIGEST: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ARTIFACT_DIGEST: &str =
    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

#[derive(Debug, Default)]
struct DeterministicEvents;

impl EventFactory for DeterministicEvents {
    fn create_metadata(&mut self, state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        Ok(EventMetadata {
            event_id: format!("driver-event-{}", state.journal_sequence + 1),
            recorded_at: "2026-08-30T10:00:00Z".to_owned(),
        })
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

struct ScriptedPlanner {
    proposals: VecDeque<PlanProposal>,
    calls: Rc<Cell<usize>>,
}

impl PlannerPort for ScriptedPlanner {
    fn planner_id(&self) -> &'static str {
        "xgeny.test.cli-driver"
    }

    fn request_profile_digest(&self) -> &'static str {
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
    }

    fn plan(
        &mut self,
        _request: &PlannerCallRequest<'_>,
    ) -> Result<PlanProposal, PlannerPortFailure> {
        self.calls.set(self.calls.get() + 1);
        Ok(self
            .proposals
            .pop_front()
            .expect("planner calls must be scripted"))
    }
}

#[derive(Clone)]
struct RecipeState(Rc<RefCell<BTreeMap<String, Value>>>);

struct MemoryRecipeMaterializer {
    state: RecipeState,
    next: usize,
}

impl PlanMaterializer for MemoryRecipeMaterializer {
    fn materialize(
        &mut self,
        request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        self.next += 1;
        let reference_id = format!("recipe-{}", self.next);
        self.state
            .0
            .borrow_mut()
            .insert(reference_id.clone(), request.normalized_arguments().clone());
        ReconstructableMaterialReference::new("test-recipes", reference_id, "rev-1")
            .map_err(|_| PlanMaterializerFailure::PersistenceFailed)
    }
}

struct MemoryRecipeProvider(RecipeState);

impl InvocationMaterialProvider for MemoryRecipeProvider {
    fn reconstruct(
        &mut self,
        reference_id: &str,
        revision: &str,
    ) -> Result<Value, MaterialProviderFailure> {
        if revision != "rev-1" {
            return Err(MaterialProviderFailure::RevisionChanged);
        }
        self.0
            .0
            .borrow()
            .get(reference_id)
            .cloned()
            .ok_or(MaterialProviderFailure::NotFound)
    }
}

struct ExactReadOnlyRoute {
    capability: CapabilityRef,
    instance_id: String,
}

impl PlannedRoutePort for ExactReadOnlyRoute {
    fn route_for(
        &mut self,
        state: &RunState,
        step_id: &str,
    ) -> Result<RouteRequest, PlannedRouteFailure> {
        let planned = state
            .steps
            .get(step_id)
            .and_then(|step| step.planned_invocation.as_ref())
            .ok_or(PlannedRouteFailure::Rejected)?;
        if planned.execution_profile() != PlannedExecutionProfile::LocalSyncReadOnlyV1 {
            return Err(PlannedRouteFailure::Rejected);
        }
        Ok(RouteRequest {
            capability: self.capability.clone(),
            target_platform: Platform {
                os: host_os(),
                arch: host_arch(),
            },
            required_features: RequiredRouteFeatures {
                execution_style: xgeny_domain::ExecutionStyle::Sync,
                cancellation: false,
                idempotency_key: false,
                idempotency_query: false,
            },
            allowed_trust_levels: vec![TrustLevel::Verified],
            allowed_data_boundaries: vec![DataBoundary::Local],
            trust_preference: Vec::new(),
            data_boundary_preference: Vec::new(),
            preferred_instance_ids: Vec::new(),
            pinned_instance_id: Some(self.instance_id.clone()),
        })
    }
}

#[derive(Default)]
struct AllowExactRequest;

impl ApprovalPort for AllowExactRequest {
    fn decide(
        &mut self,
        request: &ResolvedPermissionRequest,
    ) -> Result<ApprovalDecision, ApprovalPortFailure> {
        let allowance = || {
            PolicyAllowance::from_trusted_evaluation(
                request.requested_scopes().iter().cloned(),
                request.resources().iter().cloned(),
                request.critical_actions().iter().copied(),
                [GrantLifetime::Once],
            )
        };
        let decision = ApprovalDecision::Approved(Box::new(PolicyInputs::local(
            request,
            PolicyContribution::allow(
                policy_source(PolicySourceKind::Host, "host", '1'),
                allowance(),
            ),
            PolicyContribution::allow(
                policy_source(PolicySourceKind::UserProfile, "profile", '2'),
                allowance(),
            ),
        )));
        let debug = format!("{decision:?}");
        assert!(!debug.contains(RAW_PATH));
        assert!(!debug.contains(CANONICAL_PATH));
        Ok(decision)
    }
}

struct PendingApproval;

impl ApprovalPort for PendingApproval {
    fn decide(
        &mut self,
        _request: &ResolvedPermissionRequest,
    ) -> Result<ApprovalDecision, ApprovalPortFailure> {
        Ok(ApprovalDecision::Pending)
    }
}

struct DeniedApproval;

impl ApprovalPort for DeniedApproval {
    fn decide(
        &mut self,
        _request: &ResolvedPermissionRequest,
    ) -> Result<ApprovalDecision, ApprovalPortFailure> {
        Ok(ApprovalDecision::Denied)
    }
}

struct ReadSession {
    calls: Rc<Cell<usize>>,
}

impl PreparedAdapterInvocation for ReadSession {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        self.calls.set(self.calls.get() + 1);
        AdapterExecutionObservation::Succeeded {
            evidence_digest: AdapterEvidenceDigest::new(EVIDENCE_DIGEST)
                .expect("evidence digest should validate"),
        }
    }
}

struct ReadOnlyAdapter {
    prepare_calls: Rc<Cell<usize>>,
    execute_calls: Rc<Cell<usize>>,
}

impl EffectAdapter for ReadOnlyAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        self.prepare_calls.set(self.prepare_calls.get() + 1);
        assert_eq!(
            request.intent().effect_class,
            xgeny_workgraph::EffectClass::ReadOnly
        );
        assert_eq!(request.intent().idempotency_key, None);
        assert_eq!(request.normalized_arguments()["path"], CANONICAL_PATH);
        Ok(Box::new(ReadSession {
            calls: Rc::clone(&self.execute_calls),
        }))
    }

    fn reconcile(
        &mut self,
        _request: AdapterReconcileRequest<'_>,
    ) -> AdapterReconciliationObservation {
        AdapterReconciliationObservation::Inconclusive {
            reason: AdapterReconciliationInconclusiveReason::StableKeyUnsupported,
        }
    }
}

struct ArtifactVerifier {
    calls: Rc<Cell<usize>>,
}

impl EffectVerifier for ArtifactVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        self.calls.set(self.calls.get() + 1);
        let rules = request
            .definition()
            .spec
            .verification
            .iter()
            .map(|rule| {
                RuleVerificationObservation::new(
                    rule.strategy,
                    VerificationResult::Passed,
                    Some(
                        AdapterEvidenceDigest::new(EVIDENCE_DIGEST)
                            .expect("evidence should validate"),
                    ),
                )
            })
            .collect();
        VerificationReport::new(
            VerifierOutputDigest::new(OUTPUT_DIGEST).expect("output digest should validate"),
            rules,
        )
        .with_artifacts(vec![
            VerifiedArtifactDescriptor::new(
                "artifact-read-output",
                Some("read-output.json"),
                "application/json",
                128,
                ARTIFACT_DIGEST,
            )
            .expect("artifact should validate"),
        ])
        .map_err(|_| VerificationPortFailure::ResponseUnverifiable)
    }
}

#[test]
#[allow(clippy::too_many_lines)] // Keep one vertical-slice setup and all postconditions together.
fn sqlite_driver_completes_read_only_plan_with_core_bound_artifact_and_replays() {
    let directory = tempdir().expect("temporary Run directory should exist");
    let database = directory.path().join("run.sqlite3");
    let definition = definition_fixture();
    let instance = instance_fixture(&definition);
    let capability = CapabilityRef {
        capability_id: definition.metadata.id.clone(),
        contract_version: definition.metadata.contract_version.clone(),
    };
    let mut registry = CapabilityRegistry::new();
    registry
        .register_schema_validated_definition(definition)
        .expect("definition should register");
    registry
        .register_schema_validated_instance(instance.clone())
        .expect("instance should register");

    let prepare_calls = Rc::new(Cell::new(0));
    let execute_calls = Rc::new(Cell::new(0));
    let verify_calls = Rc::new(Cell::new(0));
    let mut adapters = EffectAdapterRegistry::new();
    adapters
        .register(
            &instance.binding,
            ReadOnlyAdapter {
                prepare_calls: Rc::clone(&prepare_calls),
                execute_calls: Rc::clone(&execute_calls),
            },
        )
        .expect("adapter should register");
    let mut verifiers = EffectVerifierRegistry::new();
    verifiers
        .register(
            &instance.binding,
            ArtifactVerifier {
                calls: Rc::clone(&verify_calls),
            },
        )
        .expect("verifier should register");

    let recipes = RecipeState(Rc::new(RefCell::new(BTreeMap::new())));
    let mut materializer = MemoryRecipeMaterializer {
        state: recipes.clone(),
        next: 0,
    };
    let mut providers = MaterialProviderRegistry::new();
    providers
        .register("test-recipes", MemoryRecipeProvider(recipes))
        .expect("provider should register");
    let planner_calls = Rc::new(Cell::new(0));
    let mut planner = ScriptedPlanner {
        proposals: VecDeque::from([
            PlanProposal::plan(vec![ProposedPlanStep::new(
                "read",
                "read one bounded fixture",
                Vec::new(),
                capability.clone(),
                json!({"path": RAW_PATH}),
            )]),
            PlanProposal::completion_candidate("test-only completion"),
        ]),
        calls: Rc::clone(&planner_calls),
    };
    let mut routes = ExactReadOnlyRoute {
        capability,
        instance_id: instance.instance_id.clone(),
    };
    let mut events = DeterministicEvents;
    let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
    store
        .append(
            ExpectedHead::Empty,
            RunEvent {
                event_id: "driver-seed".to_owned(),
                run_id: RUN_ID.to_owned(),
                authority: "local:test".to_owned(),
                authority_epoch: 1,
                recorded_at: "2026-08-30T10:00:00Z".to_owned(),
                body: RunEventBody::RunCreated {
                    goal: "read a local fixture".to_owned(),
                },
            },
        )
        .expect("Run should initialize");
    let lease = LocalRunLease::try_acquire(RUN_ID, directory.path().join("run.lock"))
        .expect("Run lease should acquire");
    let budget = AgentLoopBudget::new(2, 1, 1, 262_144).expect("budget should validate");

    let mut pending = PendingApproval;
    let outcome = RunDriver::new(AgentLoop::new(budget.clone()), NonZeroU32::new(16).unwrap())
        .drive_until_pause(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut pending,
        )
        .expect("driver should yield for explicit approval");
    assert!(matches!(outcome, DriverOutcome::ApprovalPending { .. }));
    assert_eq!(planner_calls.get(), 1);
    assert_eq!(prepare_calls.get(), 0);
    assert_eq!(execute_calls.get(), 0);
    assert_eq!(verify_calls.get(), 0);
    assert_eq!(
        store
            .load_current()
            .unwrap()
            .unwrap()
            .steps
            .values()
            .next()
            .unwrap()
            .status,
        StepStatus::Planned
    );

    let mut denied = DeniedApproval;
    let outcome = RunDriver::new(AgentLoop::new(budget.clone()), NonZeroU32::new(1).unwrap())
        .drive_until_pause(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut denied,
        )
        .expect("driver should yield after explicit denial");
    assert!(matches!(outcome, DriverOutcome::ApprovalDenied { .. }));
    assert_eq!(execute_calls.get(), 0);

    let mut approvals = AllowExactRequest;
    let outcome = RunDriver::new(AgentLoop::new(budget.clone()), NonZeroU32::new(1).unwrap())
        .drive_until_pause(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut approvals,
        )
        .expect("one admission tick should commit only the intent");
    assert!(matches!(outcome, DriverOutcome::TickBudgetExhausted));
    assert_eq!(
        store
            .load_current()
            .unwrap()
            .unwrap()
            .steps
            .values()
            .next()
            .unwrap()
            .status,
        StepStatus::IntentCommitted
    );
    assert_eq!(execute_calls.get(), 0);
    #[cfg(not(target_os = "windows"))]
    assert_persisted_files_exclude(directory.path(), &[RAW_PATH, CANONICAL_PATH]);
    drop(store);

    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen at intent");
    let outcome = RunDriver::new(AgentLoop::new(budget.clone()), NonZeroU32::new(1).unwrap())
        .drive_until_pause(
            &mut reopened,
            &mut events,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut approvals,
        )
        .expect("reopened intent should execute exactly once");
    assert!(matches!(outcome, DriverOutcome::TickBudgetExhausted));
    assert_eq!(prepare_calls.get(), 1);
    assert_eq!(execute_calls.get(), 1);
    assert_eq!(verify_calls.get(), 0);
    assert_eq!(
        reopened
            .load_current()
            .unwrap()
            .unwrap()
            .steps
            .values()
            .next()
            .unwrap()
            .status,
        StepStatus::Validating
    );
    drop(reopened);

    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen at validation");
    let outcome = RunDriver::new(AgentLoop::new(budget.clone()), NonZeroU32::new(1).unwrap())
        .drive_until_pause(
            &mut reopened,
            &mut events,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut approvals,
        )
        .expect("reopened validation should commit one Receipt");
    assert!(matches!(outcome, DriverOutcome::TickBudgetExhausted));
    assert_eq!(execute_calls.get(), 1);
    assert_eq!(verify_calls.get(), 1);
    let state = reopened.load_current().unwrap().unwrap();
    assert_eq!(state.steps.len(), 1);
    assert_eq!(
        state.steps.values().next().unwrap().status,
        StepStatus::Completed
    );
    let receipts = reopened.load_execution_receipts().unwrap();
    assert_eq!(receipts.len(), 1);
    let receipt = &receipts[0];
    assert_eq!(receipt.effect.class, EffectClass::ReadOnly);
    assert_eq!(receipt.effect.idempotency_key, None);
    assert_eq!(receipt.output_digest, OUTPUT_DIGEST);
    assert_eq!(receipt.artifacts.len(), 1);
    let artifact = &receipt.artifacts[0];
    assert_eq!(artifact.digest, ARTIFACT_DIGEST);
    let provenance = artifact
        .provenance
        .as_ref()
        .expect("Core must attach provenance");
    assert_eq!(provenance.run_id, RUN_ID);
    assert_eq!(provenance.step_id, receipt.step_id);
    assert_eq!(
        provenance.receipt_id.as_deref(),
        Some(receipt.receipt_id.as_str())
    );
    drop(reopened);

    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen after Receipt");
    let completion = RunDriver::new(AgentLoop::new(budget.clone()), NonZeroU32::new(16).unwrap())
        .drive_until_pause(
            &mut reopened,
            &mut events,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut approvals,
        )
        .expect("completed Step should allow the scripted completion candidate");
    assert!(matches!(completion, DriverOutcome::CompletionCandidate(_)));
    assert_eq!(planner_calls.get(), 2);
    assert_eq!(execute_calls.get(), 1);
    assert_eq!(verify_calls.get(), 1);
    drop(reopened);

    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen for replay");
    let replay = RunDriver::new(AgentLoop::new(budget), NonZeroU32::new(16).unwrap())
        .drive_until_pause(
            &mut reopened,
            &mut events,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut approvals,
        )
        .expect("completed Run should replay without external calls");
    assert!(matches!(replay, DriverOutcome::CompletionCandidate(_)));
    assert_eq!(planner_calls.get(), 2);
    assert_eq!(execute_calls.get(), 1);
    assert_eq!(verify_calls.get(), 1);
    drop(reopened);
    drop(lease);
    assert_persisted_files_exclude(directory.path(), &[RAW_PATH, CANONICAL_PATH]);
}

fn assert_persisted_files_exclude(directory: &std::path::Path, sentinels: &[&str]) {
    for entry in fs::read_dir(directory).expect("Run directory should be readable") {
        let path = entry.expect("entry should be readable").path();
        if path.is_file() {
            let bytes = fs::read(&path).expect("persisted file should be readable");
            for sentinel in sentinels {
                assert!(
                    !bytes
                        .windows(sentinel.len())
                        .any(|window| window == sentinel.as_bytes()),
                    "plaintext invocation material leaked into {}",
                    path.display()
                );
            }
        }
    }
}

fn definition_fixture() -> CapabilityDefinitionBody {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
    ))
    .expect("definition fixture should deserialize");
    let ProtocolDocument::CapabilityDefinition(mut definition) = document else {
        panic!("expected definition fixture")
    };
    definition.spec.execution.idempotency_key_supported = false;
    *definition
}

fn instance_fixture(definition: &CapabilityDefinitionBody) -> CapabilityInstanceBody {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-instance.local-fs.json"
    ))
    .expect("instance fixture should deserialize");
    let ProtocolDocument::CapabilityInstance(mut instance) = document else {
        panic!("expected instance fixture")
    };
    instance.definition = CapabilityRef {
        capability_id: definition.metadata.id.clone(),
        contract_version: definition.metadata.contract_version.clone(),
    };
    instance.platform.os = host_os();
    instance.platform.arch = host_arch();
    *instance
}

fn host_os() -> OperatingSystem {
    match std::env::consts::OS {
        "linux" => OperatingSystem::Linux,
        "macos" => OperatingSystem::Macos,
        "windows" => OperatingSystem::Windows,
        other => panic!("unsupported CI operating system: {other}"),
    }
}

fn host_arch() -> Architecture {
    match std::env::consts::ARCH {
        "x86_64" => Architecture::X86_64,
        "aarch64" => Architecture::Aarch64,
        other => panic!("unsupported CI architecture: {other}"),
    }
}

fn policy_source(kind: PolicySourceKind, id: &str, byte: char) -> PolicySource {
    PolicySource {
        kind,
        id: id.to_owned(),
        digest: format!("sha256:{}", byte.to_string().repeat(64)),
    }
}
