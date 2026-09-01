use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::num::NonZeroU32;
use std::process::Command;
use std::rc::Rc;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use xgeny_adapter_filesystem::{
    MAX_READ_TEXT_BYTES, ReadTextAdapter, ReadTextLimits, ReadTextVerifier, WorkspaceId,
    WorkspaceRoot,
};
use xgeny_cli::{
    ApprovalDecision, ApprovalPort, ApprovalPortFailure, DriverOutcome, PlannedRouteFailure,
    PlannedRoutePort, RunDriver,
};
use xgeny_domain::{
    Architecture, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef, DataBoundary,
    EffectClass, GrantLifetime, OperatingSystem, Platform, PolicySource, PolicySourceKind,
    ProtocolDocument, TrustLevel, VerificationResult,
};
use xgeny_local_store::{
    Commit, ExpectedHead, RunPlanningSnapshot, RunSnapshot, RunStore, SqliteRunStore, StoreError,
};
use xgeny_policy::{
    PolicyAllowance, PolicyContribution, PolicyInputs, ResolvedPermissionRequest,
    ResourceResolutionFailure, ResourceResolver,
};
use xgeny_runtime::{
    AdapterEvidenceDigest, AdapterExecutionObservation, AdapterPrepareFailure,
    AdapterPrepareRequest, AdapterReconcileRequest, AdapterReconciliationInconclusiveReason,
    AdapterReconciliationObservation, AdapterToolOutput, AgentLoop, AgentLoopError,
    AgentLoopQuiescence, AgentLoopTick, CapabilityRegistry, EffectAdapter, EffectAdapterRegistry,
    EffectVerifier, EffectVerifierRegistry, EventFactory, EventFactoryError, EventMetadata,
    InvocationMaterialProvider, LocalRunLease, MaterialProviderFailure, MaterialProviderRegistry,
    PlanMaterializationRequest, PlanMaterializer, PlanMaterializerFailure, PlanProposal,
    PlannerCallRequest, PlannerPort, PlannerPortFailure, PreparedAdapterInvocation,
    ProposedPlanStep, RequiredRouteFeatures, RouteRequest, RuleVerificationObservation,
    VerificationPortFailure, VerificationReport, VerificationRequest, VerifiedArtifactDescriptor,
    VerifierOutputDigest,
};
use xgeny_workgraph::{
    AgentLoopBudget, CompletionOutputRecord, EventRecord, PlannedExecutionProfile,
    ReconstructableMaterialReference, RunEvent, RunEventBody, RunState, StepStatus,
    ToolOutputRecord, apply_record,
};

const RUN_ID: &str = "run-cli-driver-read-only";
const RAW_PATH: &str = "RAW-DRIVER-PATH/../README.md";
const CANONICAL_PATH: &str = "workspace:fixture/README.md";
const OUTPUT_DIGEST: &str =
    "sha256:6428eca423933898d1191c9687288aaf33b29cc2e3809bd30452630a3816527e";
const EVIDENCE_DIGEST: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ARTIFACT_DIGEST: &str =
    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const COMPLETION_SUMMARY: &str = "FINAL-CLI-SUMMARY \"quote\" \\ slash\n\t- 한글과 UTF-8 결과";
const RESTART_CHILD_MARKER: &str = "XGENY_CLI_COMPLETION_RESTART_CHILD";
const RESTART_DATABASE_PATH: &str = "XGENY_CLI_COMPLETION_RESTART_DATABASE";
const RESTART_LEASE_PATH: &str = "XGENY_CLI_COMPLETION_RESTART_LEASE";
const RESTART_PROOF_PATH: &str = "XGENY_CLI_COMPLETION_RESTART_PROOF";
const RESTART_PROOF_BYTES: &[u8] = b"xgeny-completion-restart-replayed";
const RESTART_TEST_NAME: &str =
    "sqlite_driver_completes_read_only_plan_with_core_bound_artifact_and_replays";
const REAL_RUN_ID: &str = "run-cli-driver-real-filesystem";
const REAL_FILE_CONTENT: &str = "REAL-FILESYSTEM-ADAPTER-CONTENT \"quote\"\n\t한글과 UTF-8 관찰";
const MVP_CONTEXT_BUDGET_BYTES: u64 = 512 * 1024;

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
    contexts: Rc<RefCell<Vec<Value>>>,
    context_sizes: Rc<RefCell<Vec<u64>>>,
    context_digests: Rc<RefCell<Vec<String>>>,
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
        request: &PlannerCallRequest<'_>,
    ) -> Result<PlanProposal, PlannerPortFailure> {
        self.calls.set(self.calls.get() + 1);
        assert!(!format!("{request:?}").contains("fixture-content"));
        assert!(!format!("{request:?}").contains(REAL_FILE_CONTENT));
        assert!(!format!("{:?}", request.context()).contains("fixture-content"));
        assert!(!format!("{:?}", request.context()).contains(REAL_FILE_CONTENT));
        assert!(
            request
                .context()
                .tool_outputs()
                .iter()
                .all(|output| !format!("{output:?}").contains("fixture-content"))
        );
        self.contexts.borrow_mut().push(
            serde_json::to_value(request.context())
                .expect("planning context should serialize for observation"),
        );
        self.context_sizes
            .borrow_mut()
            .push(request.context().canonical_size_bytes());
        self.context_digests
            .borrow_mut()
            .push(request.context().context_digest().to_owned());
        Ok(self
            .proposals
            .pop_front()
            .expect("planner calls must be scripted"))
    }
}

struct PlanningProjectionStore {
    state: RunState,
    outputs: BTreeMap<String, ToolOutputRecord>,
    completion_output: Option<CompletionOutputRecord>,
}

impl RunStore for PlanningProjectionStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        let actual = ExpectedHead::from_state(&self.state);
        if expected != actual {
            return Err(StoreError::HeadConflict { expected, actual });
        }
        let previous = EventRecord {
            sequence: self.state.journal_sequence,
            previous_digest: None,
            event: RunEvent {
                event_id: "planning-projection-placeholder".to_owned(),
                run_id: self.state.run_id.clone(),
                authority: self.state.authority.clone(),
                authority_epoch: self.state.authority_epoch,
                recorded_at: "2026-08-30T10:00:00Z".to_owned(),
                body: RunEventBody::RunCreated {
                    goal: self.state.goal.clone(),
                },
            },
            digest: self.state.journal_head_digest.clone(),
        };
        let record = EventRecord::next(Some(&previous), event)?;
        let state = apply_record(Some(&self.state), &record)?;
        self.state = state.clone();
        Ok(Commit { record, state })
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        Ok(Some(RunSnapshot {
            records: Vec::new(),
            state: self.state.clone(),
        }))
    }

    fn append_with_completion_output(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        output: CompletionOutputRecord,
    ) -> Result<Commit, StoreError> {
        let commit = self.append(expected, event)?;
        self.completion_output = Some(output);
        Ok(commit)
    }

    fn load_completion_output(
        &self,
        expected: ExpectedHead,
        candidate_id: &str,
    ) -> Result<Option<CompletionOutputRecord>, StoreError> {
        let actual = ExpectedHead::from_state(&self.state);
        if expected != actual {
            return Err(StoreError::HeadConflict { expected, actual });
        }
        Ok(self
            .completion_output
            .as_ref()
            .filter(|output| output.candidate_id() == candidate_id)
            .cloned())
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        Ok(Some(self.state.clone()))
    }

    fn load_planning_snapshot(
        &self,
        expected: ExpectedHead,
        max_output_bytes: u64,
    ) -> Result<Option<RunPlanningSnapshot>, StoreError> {
        let actual = ExpectedHead::from_state(&self.state);
        if expected != actual {
            return Err(StoreError::HeadConflict { expected, actual });
        }
        let total = self.outputs.values().try_fold(0_u64, |bytes, output| {
            bytes.checked_add(output.canonical_size_bytes())
        });
        if total.is_none_or(|bytes| bytes > max_output_bytes) {
            return Err(StoreError::PlanningSnapshotBudgetExceeded);
        }
        Ok(Some(RunPlanningSnapshot::new(
            self.state.clone(),
            self.outputs.clone(),
        )))
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
        if planned.execution_profile() != PlannedExecutionProfile::LocalSyncReadOnlyOccurrenceV1 {
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
        AdapterExecutionObservation::SucceededWithOutput {
            evidence_digest: AdapterEvidenceDigest::new(EVIDENCE_DIGEST)
                .expect("evidence digest should validate"),
            output: AdapterToolOutput::new(json!({
                "content": "fixture-content",
                "digest": ARTIFACT_DIGEST,
            })),
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

struct CountingReadAdapter {
    inner: ReadTextAdapter,
    prepare_calls: Rc<Cell<usize>>,
    execute_calls: Rc<Cell<usize>>,
}

struct CountingReadSession {
    inner: Box<dyn PreparedAdapterInvocation>,
    execute_calls: Rc<Cell<usize>>,
}

impl PreparedAdapterInvocation for CountingReadSession {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        self.execute_calls.set(self.execute_calls.get() + 1);
        self.inner.execute()
    }
}

impl EffectAdapter for CountingReadAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        self.prepare_calls.set(self.prepare_calls.get() + 1);
        let inner = self.inner.prepare(request)?;
        Ok(Box::new(CountingReadSession {
            inner,
            execute_calls: Rc::clone(&self.execute_calls),
        }))
    }

    fn reconcile(
        &mut self,
        request: AdapterReconcileRequest<'_>,
    ) -> AdapterReconciliationObservation {
        self.inner.reconcile(request)
    }
}

struct CountingReadVerifier {
    inner: ReadTextVerifier,
    calls: Rc<Cell<usize>>,
}

impl EffectVerifier for CountingReadVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        self.calls.set(self.calls.get() + 1);
        self.inner.verify(request)
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn real_filesystem_adapter_reaches_next_turn_and_replays_after_sqlite_reopen() {
    let directory = tempdir().expect("temporary test directory should exist");
    let workspace_path = directory.path().join("workspace");
    fs::create_dir(&workspace_path).expect("workspace should create");
    fs::write(workspace_path.join("README.md"), REAL_FILE_CONTENT)
        .expect("workspace fixture should write");
    let workspace = WorkspaceRoot::open_ambient(
        &workspace_path,
        WorkspaceId::new("fixture").expect("workspace ID should validate"),
    )
    .expect("workspace capability should open");
    let resolver = workspace.resolver();

    let definition = definition_fixture();
    let mut instance = instance_fixture(&definition);
    instance.binding = workspace.binding();
    instance.features.cancellable = false;
    assert!(instance.features.sync);
    assert!(!instance.features.task);
    assert!(!instance.features.cancellable);
    assert!(!instance.features.idempotency_query);
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
        .expect("root-bound instance should register");

    let prepare_calls = Rc::new(Cell::new(0));
    let execute_calls = Rc::new(Cell::new(0));
    let verify_calls = Rc::new(Cell::new(0));
    let adapter = workspace.read_text_adapter(ReadTextLimits::default());
    let verifier = adapter.verifier();
    let mut adapters = EffectAdapterRegistry::new();
    adapters
        .register(
            &instance.binding,
            CountingReadAdapter {
                inner: adapter,
                prepare_calls: Rc::clone(&prepare_calls),
                execute_calls: Rc::clone(&execute_calls),
            },
        )
        .expect("read adapter should register");
    let mut verifiers = EffectVerifierRegistry::new();
    verifiers
        .register(
            &instance.binding,
            CountingReadVerifier {
                inner: verifier,
                calls: Rc::clone(&verify_calls),
            },
        )
        .expect("read verifier should register");

    let recipes = RecipeState(Rc::new(RefCell::new(BTreeMap::new())));
    let mut materializer = MemoryRecipeMaterializer {
        state: recipes.clone(),
        next: 0,
    };
    let mut providers = MaterialProviderRegistry::new();
    providers
        .register("test-recipes", MemoryRecipeProvider(recipes))
        .expect("material provider should register");
    let planner_calls = Rc::new(Cell::new(0));
    let planner_contexts = Rc::new(RefCell::new(Vec::new()));
    let mut planner = ScriptedPlanner {
        proposals: VecDeque::from([
            PlanProposal::plan(vec![ProposedPlanStep::new(
                "read",
                "read one real workspace file",
                Vec::new(),
                capability.clone(),
                json!({"path": "README.md"}),
            )]),
            PlanProposal::completion_candidate("REAL-FILESYSTEM-COMPLETION"),
        ]),
        calls: Rc::clone(&planner_calls),
        contexts: Rc::clone(&planner_contexts),
        context_sizes: Rc::new(RefCell::new(Vec::new())),
        context_digests: Rc::new(RefCell::new(Vec::new())),
    };
    let mut routes = ExactReadOnlyRoute {
        capability,
        instance_id: instance.instance_id.clone(),
    };
    let database = directory.path().join("real-run.sqlite3");
    let lease_path = directory.path().join("real-run.lock");
    let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
    store
        .append(
            ExpectedHead::Empty,
            RunEvent {
                event_id: "real-filesystem-seed".to_owned(),
                run_id: REAL_RUN_ID.to_owned(),
                authority: "local:test".to_owned(),
                authority_epoch: 1,
                recorded_at: "2026-08-30T10:00:00Z".to_owned(),
                body: RunEventBody::RunCreated {
                    goal: "read a real local fixture".to_owned(),
                },
            },
        )
        .expect("Run should initialize");
    let lease =
        LocalRunLease::try_acquire(REAL_RUN_ID, &lease_path).expect("Run lease should acquire");
    let budget = AgentLoopBudget::new(2, 1, 1, MVP_CONTEXT_BUDGET_BYTES)
        .expect("MVP context budget should validate");
    let mut approvals = AllowExactRequest;
    let mut events = DeterministicEvents;

    let outcome = RunDriver::new(AgentLoop::new(budget.clone()), NonZeroU32::new(32).unwrap())
        .drive_until_pause(
            &mut store,
            &mut events,
            &lease,
            &registry,
            &resolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut approvals,
        )
        .expect("real filesystem Run should complete");
    let DriverOutcome::CompletionCandidate {
        output: Some(completion),
        ..
    } = outcome
    else {
        panic!("real filesystem Run should return its completion output")
    };
    assert_eq!(completion.summary(), "REAL-FILESYSTEM-COMPLETION");
    assert_eq!(planner_calls.get(), 2);
    assert_eq!(prepare_calls.get(), 1);
    assert_eq!(execute_calls.get(), 1);
    assert_eq!(verify_calls.get(), 1);

    let contexts = planner_contexts.borrow();
    assert_eq!(contexts.len(), 2);
    assert_eq!(contexts[0]["toolOutputs"], json!([]));
    assert_eq!(
        contexts[1]["toolOutputs"][0]["output"]["content"],
        REAL_FILE_CONTENT
    );
    let mut worst_case_context = contexts[1].clone();
    worst_case_context["toolOutputs"][0]["output"]["content"] =
        Value::String("\0".repeat(MAX_READ_TEXT_BYTES));
    let worst_case_context_bytes = u64::try_from(
        serde_json::to_vec(&worst_case_context)
            .expect("worst-case context should serialize")
            .len(),
    )
    .expect("serialized context length should fit u64");
    assert!(
        worst_case_context_bytes <= budget.max_context_bytes,
        "one maximum-size control-heavy read must fit the effective MVP planning-context budget"
    );
    drop(contexts);
    let state = store.load_current().unwrap().unwrap();
    let step = state.steps.values().next().expect("read Step should exist");
    let intent = step
        .intent
        .as_ref()
        .expect("read Step should retain intent");
    let output = store
        .load_tool_output(&intent.effect_id)
        .expect("tool output lookup should succeed")
        .expect("tool output should persist");
    let expected_file_digest = test_sha256_digest(REAL_FILE_CONTENT.as_bytes());
    assert_eq!(output.output()["content"], REAL_FILE_CONTENT);
    assert_eq!(output.output()["digest"], expected_file_digest);
    let receipts = store
        .load_execution_receipts()
        .expect("Receipt lookup should succeed");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].output_digest, output.output_digest());
    assert_eq!(receipts[0].artifacts.len(), 1);
    assert_eq!(receipts[0].artifacts[0].digest, expected_file_digest);
    assert_eq!(
        receipts[0].artifacts[0].size,
        u64::try_from(REAL_FILE_CONTENT.len()).expect("fixture length should fit")
    );
    for public_surface in [
        String::from_utf8(store.export_jsonl().expect("journal should export"))
            .expect("journal should be UTF-8"),
        String::from_utf8(
            store
                .export_execution_receipts_jsonl()
                .expect("Receipts should export"),
        )
        .expect("Receipt export should be UTF-8"),
        serde_json::to_string(&state).expect("projection should serialize"),
    ] {
        assert!(!public_surface.contains("README.md"));
        assert!(!public_surface.contains("workspace:fixture"));
        assert!(!public_surface.contains(REAL_FILE_CONTENT));
        assert!(!public_surface.contains(workspace_path.to_string_lossy().as_ref()));
    }

    drop(store);
    fs::remove_file(workspace_path.join("README.md"))
        .expect("source may be removed after durable completion");
    let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
    let before = reopened.load().unwrap().unwrap();
    let replay = RunDriver::new(AgentLoop::new(budget), NonZeroU32::new(1).unwrap())
        .drive_until_pause(
            &mut reopened,
            &mut events,
            &lease,
            &registry,
            &resolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut approvals,
        )
        .expect("completed Run should replay without file or model access");
    let DriverOutcome::CompletionCandidate {
        output: Some(replayed),
        ..
    } = replay
    else {
        panic!("reopened Run should replay its completion")
    };
    assert_eq!(replayed.summary(), "REAL-FILESYSTEM-COMPLETION");
    assert_eq!(planner_calls.get(), 2, "replay must not recall the planner");
    assert_eq!(
        prepare_calls.get(),
        1,
        "replay must not prepare the adapter"
    );
    assert_eq!(execute_calls.get(), 1, "replay must not reread the file");
    assert_eq!(verify_calls.get(), 1, "replay must not reverify the file");
    assert_eq!(reopened.load().unwrap().unwrap(), before);
}

impl EffectVerifier for ArtifactVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        self.calls.set(self.calls.get() + 1);
        assert_eq!(
            request
                .tool_output()
                .expect("read-only verification must receive the durable output")
                .output()["content"],
            "fixture-content"
        );
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
    if std::env::var_os(RESTART_CHILD_MARKER).is_some() {
        run_completion_restart_child();
        return;
    }

    let directory = tempdir().expect("temporary Run directory should exist");
    let database = directory.path().join("run.sqlite3");
    let lease_path = directory.path().join("run.lock");
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
    let planner_contexts = Rc::new(RefCell::new(Vec::new()));
    let planner_context_sizes = Rc::new(RefCell::new(Vec::new()));
    let planner_context_digests = Rc::new(RefCell::new(Vec::new()));
    let mut planner = ScriptedPlanner {
        proposals: VecDeque::from([
            PlanProposal::plan(vec![ProposedPlanStep::new(
                "read",
                "read one bounded fixture",
                Vec::new(),
                capability.clone(),
                json!({"path": RAW_PATH}),
            )]),
            PlanProposal::completion_candidate(COMPLETION_SUMMARY),
        ]),
        calls: Rc::clone(&planner_calls),
        contexts: Rc::clone(&planner_contexts),
        context_sizes: Rc::clone(&planner_context_sizes),
        context_digests: Rc::clone(&planner_context_digests),
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
    let lease = LocalRunLease::try_acquire(RUN_ID, &lease_path).expect("Run lease should acquire");
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
    assert_eq!(planner_contexts.borrow()[0]["toolOutputs"], json!([]));
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
    assert_eq!(
        planner_calls.get(),
        1,
        "Validating must not invoke the planner"
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
    assert_eq!(
        planner_calls.get(),
        1,
        "verification must not invoke the planner"
    );
    let completed_state = state.clone();
    let completed_step = completed_state
        .steps
        .values()
        .next()
        .expect("completed Step should exist");
    let completed_output = reopened
        .load_tool_output(
            &completed_step
                .intent
                .as_ref()
                .expect("completed Step should retain its intent")
                .effect_id,
        )
        .expect("tool output should load")
        .expect("completed read-only Step should retain exact output");
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
    for public_surface in [
        String::from_utf8(reopened.export_jsonl().expect("journal should export"))
            .expect("journal should be UTF-8"),
        String::from_utf8(
            reopened
                .export_execution_receipts_jsonl()
                .expect("Receipts should export"),
        )
        .expect("Receipt export should be UTF-8"),
        serde_json::to_string(&state).expect("projection should serialize"),
        format!(
            "{:?}",
            reopened
                .load_verification_snapshot(&receipt.step_id)
                .expect("verification snapshot should load")
        ),
        format!(
            "{:?}",
            reopened
                .load_planning_snapshot(ExpectedHead::from_state(&state), budget.max_context_bytes,)
                .expect("planning snapshot should load")
        ),
    ] {
        assert!(!public_surface.contains("fixture-content"));
    }

    let tampered_output = ToolOutputRecord::new(
        RUN_ID,
        completed_step.step_id.clone(),
        completed_step
            .intent
            .as_ref()
            .expect("completed Step should retain its intent"),
        completed_step.attempts,
        completed_step
            .effect_evidence_digest
            .as_deref()
            .expect("completed Step should retain evidence"),
        json!({"content": "fixture-content-tampered", "digest": ARTIFACT_DIGEST}),
    )
    .expect("alternate output should be internally valid");
    let forged_cases = [
        ("missing", BTreeMap::new()),
        (
            "body-digest-mismatch",
            BTreeMap::from([(completed_step.step_id.clone(), tampered_output)]),
        ),
        (
            "step-binding-mismatch",
            BTreeMap::from([("step-wrong-binding".to_owned(), completed_output.clone())]),
        ),
    ];
    for (case, outputs) in forged_cases {
        let forged_calls = Rc::new(Cell::new(0));
        let mut forged_planner = ScriptedPlanner {
            proposals: VecDeque::from([PlanProposal::completion_candidate(
                "must never be requested",
            )]),
            calls: Rc::clone(&forged_calls),
            contexts: Rc::new(RefCell::new(Vec::new())),
            context_sizes: Rc::new(RefCell::new(Vec::new())),
            context_digests: Rc::new(RefCell::new(Vec::new())),
        };
        let mut forged_store = PlanningProjectionStore {
            state: completed_state.clone(),
            outputs,
            completion_output: None,
        };
        let forged_before = forged_store.state.clone();
        let forged = AgentLoop::new(budget.clone()).tick(
            &mut forged_store,
            &mut events,
            &lease,
            &registry,
            &CanonicalResolver,
            &mut forged_planner,
            &mut materializer,
        );
        assert!(
            matches!(forged, Err(AgentLoopError::PlanningSnapshotMismatch)),
            "unexpected forged snapshot result for {case}: {forged:?}"
        );
        assert_eq!(forged_calls.get(), 0, "planner called for {case}");
        assert_eq!(
            forged_store.state, forged_before,
            "state changed for {case}"
        );
    }
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
    let completion_debug = format!("{completion:?}");
    let DriverOutcome::CompletionCandidate {
        candidate: completion_candidate,
        output: Some(completion_output),
    } = completion
    else {
        panic!("new completion must return its exact durable output")
    };
    assert_eq!(
        completion_output.summary().as_bytes(),
        COMPLETION_SUMMARY.as_bytes()
    );
    assert_eq!(
        completion_output.candidate_id(),
        completion_candidate.candidate_id
    );
    assert_eq!(
        Some(completion_output.record_digest()),
        completion_candidate
            .completion_output_record_digest
            .as_deref()
    );
    assert!(!format!("{completion_output:?}").contains(COMPLETION_SUMMARY));
    let completion_snapshot = reopened
        .load()
        .expect("completion snapshot should verify")
        .expect("Run should exist");
    let completion_head = ExpectedHead::from_state(&completion_snapshot.state);
    let completion_event_count = completion_snapshot.records.len();
    for public_surface in [
        String::from_utf8(reopened.export_jsonl().expect("journal should export"))
            .expect("journal should be UTF-8"),
        String::from_utf8(
            reopened
                .export_execution_receipts_jsonl()
                .expect("Receipts should export"),
        )
        .expect("Receipt export should be UTF-8"),
        serde_json::to_string(&completion_snapshot.state).expect("projection should serialize"),
        completion_debug,
    ] {
        assert!(!public_surface.contains(COMPLETION_SUMMARY));
    }
    assert_eq!(planner_calls.get(), 2);
    let contexts = planner_contexts.borrow();
    assert_eq!(contexts.len(), 2);
    let continued = &contexts[1];
    assert_eq!(continued["profileVersion"], "xgeny.planning-context/v2");
    let outputs = continued["toolOutputs"]
        .as_array()
        .expect("toolOutputs should be an array");
    assert_eq!(outputs.len(), 1);
    assert_eq!(
        outputs[0]["output"],
        json!({"content": "fixture-content", "digest": ARTIFACT_DIGEST})
    );
    assert_eq!(outputs[0]["outputDigest"], OUTPUT_DIGEST);
    assert_eq!(outputs[0]["receiptDigest"], receipt.receipt_digest);
    assert_eq!(
        serde_json::to_string(continued)
            .expect("continued context should serialize")
            .matches("fixture-content")
            .count(),
        1
    );
    drop(contexts);
    let generous_context_bytes = planner_context_sizes.borrow()[1];
    let output_map = BTreeMap::from([(
        completed_output.step_id().to_owned(),
        completed_output.clone(),
    )]);
    let completed_intent = completed_step
        .intent
        .as_ref()
        .expect("completed Step should retain its intent");
    let completed_evidence = completed_step
        .effect_evidence_digest
        .as_deref()
        .expect("completed Step should retain evidence");
    let output_with_order = |raw: &str| {
        ToolOutputRecord::new(
            RUN_ID,
            completed_step.step_id.clone(),
            completed_intent,
            completed_step.attempts,
            completed_evidence,
            serde_json::from_str(raw).expect("ordered output should be valid JSON"),
        )
        .expect("ordered output should bind")
    };
    let ordered_ab = output_with_order(r#"{"a":1,"b":2}"#);
    let ordered_ba = output_with_order(r#"{"b":2,"a":1}"#);
    let mutated = output_with_order(r#"{"a":1,"b":3}"#);
    let mut capture_output_context_digest = |output: ToolOutputRecord| {
        let mut candidate_state = completed_state.clone();
        candidate_state
            .steps
            .get_mut(completed_output.step_id())
            .expect("completed Step should exist")
            .output_record_digest = Some(output.record_digest().to_owned());
        let calls = Rc::new(Cell::new(0));
        let digests = Rc::new(RefCell::new(Vec::new()));
        let mut candidate_planner = ScriptedPlanner {
            proposals: VecDeque::from([PlanProposal::completion_candidate("digest")]),
            calls: Rc::clone(&calls),
            contexts: Rc::new(RefCell::new(Vec::new())),
            context_sizes: Rc::new(RefCell::new(Vec::new())),
            context_digests: Rc::clone(&digests),
        };
        let mut candidate_store = PlanningProjectionStore {
            state: candidate_state,
            outputs: BTreeMap::from([(completed_output.step_id().to_owned(), output)]),
            completion_output: None,
        };
        let tick = AgentLoop::new(budget.clone())
            .tick(
                &mut candidate_store,
                &mut events,
                &lease,
                &registry,
                &CanonicalResolver,
                &mut candidate_planner,
                &mut materializer,
            )
            .expect("digest fixture should complete");
        assert!(matches!(tick, AgentLoopTick::CompletionCandidate { .. }));
        assert_eq!(calls.get(), 1);
        digests.borrow()[0].clone()
    };
    let digest_ab = capture_output_context_digest(ordered_ab);
    let digest_ba = capture_output_context_digest(ordered_ba);
    let digest_mutated = capture_output_context_digest(mutated);
    assert_eq!(
        digest_ab, digest_ba,
        "JSON key order must not affect context digest"
    );
    assert_ne!(
        digest_ab, digest_mutated,
        "exact output body changes must affect context digest"
    );
    let mut boundary_tick = |limit: u64| {
        let mut candidate_budget = budget.clone();
        candidate_budget.max_context_bytes = limit;
        let mut candidate_state = completed_state.clone();
        candidate_state
            .agent_loop
            .as_mut()
            .expect("AgentLoop should be configured")
            .budget = candidate_budget.clone();
        let calls = Rc::new(Cell::new(0));
        let mut candidate_planner = ScriptedPlanner {
            proposals: VecDeque::from([PlanProposal::completion_candidate("boundary")]),
            calls: Rc::clone(&calls),
            contexts: Rc::new(RefCell::new(Vec::new())),
            context_sizes: Rc::new(RefCell::new(Vec::new())),
            context_digests: Rc::new(RefCell::new(Vec::new())),
        };
        let mut candidate_store = PlanningProjectionStore {
            state: candidate_state,
            outputs: output_map.clone(),
            completion_output: None,
        };
        let before = candidate_store.state.clone();
        let tick = AgentLoop::new(candidate_budget)
            .tick(
                &mut candidate_store,
                &mut events,
                &lease,
                &registry,
                &CanonicalResolver,
                &mut candidate_planner,
                &mut materializer,
            )
            .expect("context boundary should have a closed outcome");
        (tick, calls.get(), before, candidate_store.state)
    };
    let mut lower = 1_u64;
    let mut upper = generous_context_bytes;
    while lower < upper {
        let middle = lower + (upper - lower) / 2;
        let (tick, calls, _, _) = boundary_tick(middle);
        match tick {
            xgeny_runtime::AgentLoopTick::CompletionCandidate { .. } => {
                assert_eq!(calls, 1);
                upper = middle;
            }
            xgeny_runtime::AgentLoopTick::Quiescent {
                reason: AgentLoopQuiescence::ContextBudgetExceeded,
                ..
            } => {
                assert_eq!(calls, 0);
                lower = middle + 1;
            }
            other => panic!("unexpected context-boundary tick: {other:?}"),
        }
    }
    let exact_context_bytes = lower;
    let (exact_tick, exact_calls, _, _) = boundary_tick(exact_context_bytes);
    assert!(matches!(
        exact_tick,
        xgeny_runtime::AgentLoopTick::CompletionCandidate {
            newly_recorded: true,
            ..
        }
    ));
    assert_eq!(exact_calls, 1);
    let (short_tick, short_calls, short_before, short_after) = boundary_tick(
        exact_context_bytes
            .checked_sub(1)
            .expect("planning context should be non-empty"),
    );
    assert!(matches!(
        short_tick,
        xgeny_runtime::AgentLoopTick::Quiescent {
            reason: AgentLoopQuiescence::ContextBudgetExceeded,
            ..
        }
    ));
    assert_eq!(short_calls, 0);
    assert_eq!(short_after, short_before);
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
    let DriverOutcome::CompletionCandidate {
        candidate: replayed_candidate,
        output: Some(replayed_output),
    } = replay
    else {
        panic!("reopened completion must return its exact durable output")
    };
    assert_eq!(replayed_candidate, completion_candidate);
    assert_eq!(replayed_output, completion_output);
    assert_eq!(
        replayed_output.summary().as_bytes(),
        COMPLETION_SUMMARY.as_bytes()
    );
    let replayed_snapshot = reopened
        .load()
        .expect("replayed snapshot should verify")
        .expect("Run should exist");
    assert_eq!(
        ExpectedHead::from_state(&replayed_snapshot.state),
        completion_head
    );
    assert_eq!(replayed_snapshot.records.len(), completion_event_count);
    assert_eq!(planner_calls.get(), 2);
    assert_eq!(execute_calls.get(), 1);
    assert_eq!(verify_calls.get(), 1);
    drop(reopened);
    drop(lease);

    let restart_proof = directory.path().join("completion-restart.proof");
    let child = Command::new(std::env::current_exe().expect("test executable should resolve"))
        .arg("--exact")
        .arg(RESTART_TEST_NAME)
        .arg("--nocapture")
        .env(RESTART_CHILD_MARKER, "1")
        .env(RESTART_DATABASE_PATH, &database)
        .env(RESTART_LEASE_PATH, &lease_path)
        .env(RESTART_PROOF_PATH, &restart_proof)
        .output()
        .expect("restart child should execute");
    assert!(
        child.status.success(),
        "restart child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(
        fs::read(&restart_proof).expect("restart child must publish its exact replay proof"),
        RESTART_PROOF_BYTES,
        "a successful test process without the selected child test is not sufficient"
    );
    assert_persisted_files_exclude(directory.path(), &[RAW_PATH, CANONICAL_PATH]);
}

fn run_completion_restart_child() {
    let database = std::env::var_os(RESTART_DATABASE_PATH)
        .map(std::path::PathBuf::from)
        .expect("restart database path should be supplied");
    let lease_path = std::env::var_os(RESTART_LEASE_PATH)
        .map(std::path::PathBuf::from)
        .expect("restart lease path should be supplied");
    let proof_path = std::env::var_os(RESTART_PROOF_PATH)
        .map(std::path::PathBuf::from)
        .expect("restart proof path should be supplied");
    let mut store = SqliteRunStore::open(database).expect("restart child should open SQLite");
    let before = store
        .load()
        .expect("restart child snapshot should verify")
        .expect("restart child Run should exist");
    let lease =
        LocalRunLease::try_acquire(RUN_ID, lease_path).expect("restart child lease should acquire");
    let budget = AgentLoopBudget::new(2, 1, 1, 262_144).expect("budget should validate");
    let planner_calls = Rc::new(Cell::new(0));
    let mut planner = ScriptedPlanner {
        proposals: VecDeque::new(),
        calls: Rc::clone(&planner_calls),
        contexts: Rc::new(RefCell::new(Vec::new())),
        context_sizes: Rc::new(RefCell::new(Vec::new())),
        context_digests: Rc::new(RefCell::new(Vec::new())),
    };
    let mut materializer = MemoryRecipeMaterializer {
        state: RecipeState(Rc::new(RefCell::new(BTreeMap::new()))),
        next: 0,
    };
    let mut providers = MaterialProviderRegistry::new();
    let mut adapters = EffectAdapterRegistry::new();
    let mut verifiers = EffectVerifierRegistry::new();
    let mut routes = ExactReadOnlyRoute {
        capability: CapabilityRef {
            capability_id: "restart-unused".to_owned(),
            contract_version: "restart-unused".to_owned(),
        },
        instance_id: "restart-unused".to_owned(),
    };
    let mut approvals = AllowExactRequest;
    let mut events = DeterministicEvents;
    let outcome = RunDriver::new(AgentLoop::new(budget), NonZeroU32::new(1).unwrap())
        .drive_until_pause(
            &mut store,
            &mut events,
            &lease,
            &CapabilityRegistry::new(),
            &CanonicalResolver,
            &mut planner,
            &mut materializer,
            &mut providers,
            &mut adapters,
            &mut verifiers,
            &mut routes,
            &mut approvals,
        )
        .expect("restart child should replay without external calls");
    let DriverOutcome::CompletionCandidate {
        output: Some(output),
        ..
    } = outcome
    else {
        panic!("restart child should recover the exact completion output")
    };
    assert_eq!(output.summary().as_bytes(), COMPLETION_SUMMARY.as_bytes());
    assert_eq!(planner_calls.get(), 0, "restart must not recall the model");
    let after = store
        .load()
        .expect("restart child snapshot should re-verify")
        .expect("restart child Run should remain");
    assert_eq!(
        after, before,
        "restart replay must not append or mutate state"
    );
    fs::write(proof_path, RESTART_PROOF_BYTES).expect("restart child should publish replay proof");
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
    let ProtocolDocument::CapabilityDefinition(definition) = document else {
        panic!("expected definition fixture")
    };
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

fn test_sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("sha256:{encoded}")
}
