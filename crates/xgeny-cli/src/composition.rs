use std::env;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use xgeny_adapter_filesystem::{
    APPLY_PATCH_CAPABILITY_ID, APPLY_PATCH_CONTRACT_VERSION, FILESYSTEM_READ_SCOPE,
    FILESYSTEM_WRITE_SCOPE, LIST_DIRECTORY_CAPABILITY_ID, LIST_DIRECTORY_CONTRACT_VERSION,
    MAX_APPLY_PATCH_BYTES, MAX_APPLY_PATCH_EDITS, MAX_DIRECTORY_SCAN_ENTRIES,
    MAX_LIST_DIRECTORY_ENTRIES, MAX_QUERY_OUTPUT_CANONICAL_BYTES, MAX_SEARCH_FILE_BYTES,
    MAX_SEARCH_MATCHES, MAX_SEARCH_PREVIEW_BYTES, MAX_SEARCH_QUERY_BYTES,
    MAX_SEARCH_QUERY_UNICODE_SCALARS, MAX_SEARCH_TOTAL_BYTES, MAX_SEARCH_VISITED_ENTRIES,
    MAX_WRITE_ATOMIC_BYTES, READ_TEXT_CAPABILITY_ID, READ_TEXT_CONTRACT_VERSION, ReadTextLimits,
    SEARCH_TEXT_CAPABILITY_ID, SEARCH_TEXT_CONTRACT_VERSION, STAT_CAPABILITY_ID,
    STAT_CONTRACT_VERSION, WRITE_ATOMIC_CAPABILITY_ID, WRITE_ATOMIC_CONTRACT_VERSION, WorkspaceId,
    WorkspaceResourceResolver, WorkspaceRoot,
};
use xgeny_adapter_process::{
    MAX_CAPTURE_BYTES, MAX_PROCESS_TIMEOUT_MS, MIN_CAPTURE_BYTES, PROCESS_EXECUTE_CAPABILITY_ID,
    PROCESS_EXECUTE_CONTRACT_VERSION, PROCESS_EXECUTE_SCOPE, ProcessResourceResolver,
};
use xgeny_domain::{
    Architecture, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef, DataBoundary,
    EffectClass, GrantLifetime, OperatingSystem, Platform, PolicySource, PolicySourceKind,
    ProtocolDocument, TrustLevel,
};
use xgeny_local_store::{ExpectedHead, RunStore, SqliteRunStore};
use xgeny_policy::{
    PolicyAllowance, PolicyContribution, PolicyInputs, ResolvedPermissionRequest,
    ResourceResolutionFailure, ResourceResolver,
};
use xgeny_provider_openai::{
    BearerCredential, OpenAiModelCheckFailure, OpenAiModelChecker, OpenAiPlanner,
    OpenAiPlannerConfig, OpenAiPlannerConfigError,
};
use xgeny_runtime::{
    AgentLoop, AgentLoopQuiescence, AgentLoopTick, CapabilityRegistry, DirectExecutor,
    EffectAdapterRegistry, EffectVerifierRegistry, EventFactory, EventFactoryError, EventMetadata,
    LocalRunLease, MaterialProviderRegistry, PlanMaterializationRequest, PlanMaterializer,
    PlanMaterializerFailure, PlanProposal, PlannerCallRequest, PlannerPort, PlannerPortFailure,
    PlanningConstraint, RequiredRouteFeatures, RouteRequest,
};
use xgeny_workgraph::{
    CompletionOutputRecord, ModelCallStatus, PlannedExecutionProfile,
    ReconstructableMaterialReference, RunEvent, RunEventBody, RunState, StepStatus,
    derive_frontier,
};

use crate::allow_file::{ALLOW_FILE_PROVIDER_ID, AllowFileCatalog};
use crate::allow_path::WorkspaceReadAuthorization;
use crate::allow_process::{
    PROCESS_EXECUTABLE_CATALOG_PROFILE, ProcessExecutionAuthorization, ProcessTooling,
    SAFE_PROCESS_ENVIRONMENT_PROFILE,
};
use crate::driver::{
    ApprovalDecision, ApprovalPort, ApprovalPortFailure, DriverOutcome, PlannedRouteFailure,
    PlannedRoutePort, RunDriver,
};
use crate::manifest::{ManifestBudget, RunManifest};
use crate::material_catalog::{
    MAX_RECIPE_BYTES, PROCESS_MATERIAL_PROVIDER_ID, PROCESS_RECIPE_DOMAIN,
    PROCESS_RECIPE_FORMAT_VERSION, ProcessMaterialProvider, ProcessMaterializer,
    RunMaterialCatalog, WORKSPACE_READ_MATERIAL_CATALOG_SCHEMA_VERSION,
    WORKSPACE_READ_MATERIAL_PROVIDER_ID, WORKSPACE_READ_RECIPE_DOMAIN,
    WORKSPACE_READ_RECIPE_FORMAT_VERSION, WorkspaceReadMaterialProvider, WorkspaceReadMaterializer,
};
use crate::run_layout::{RunLayout, discover_state_root, generate_run_id};

const WORKSPACE_ID: &str = "primary";
const WORKSPACE_IDENTITY_PROFILE: &str = "xgeny.fs.workspace-root-identity.v1";
const DEFAULT_PLANNER_ID: &str = "xgeny.cli.openai";
const MAX_GOAL_BYTES: usize = 16 * 1024;
const MAX_TICKS: u32 = 1_024;
const MAX_OUTPUT_TOKENS: u32 = 1_024;
const MODEL_TIMEOUT: Duration = Duration::from_secs(60);
const MODEL_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCAL_EXECUTION_PROFILE_DOMAIN: &str = "xgeny.cli.local-execution-profile/v1";
const WORKSPACE_DISCOVERY_PROFILE_DOMAIN: &str =
    "xgeny.cli.workspace-filesystem-execution-profile/v3";
const ROUTE_PROFILE: &str = "xgeny.cli.exact-read-only-route/v1";
const MATERIALIZER_PROFILE: &str = "xgeny.cli.allow-file-materializer/v1";
const APPROVAL_PROFILE: &str = "xgeny.cli.exact-catalog-read-approval/v1";
const HOST_POLICY_PROFILE: &str = "xgeny.cli.host-exact-catalog-read/v1";
const WORKSPACE_HOST_POLICY_PROFILE: &str = "xgeny.cli.host-workspace-descendant-fs/v3";
const USER_READ_POLICY_PROFILE: &str = "xgeny.cli.explicit-allow-read-flag/v1";
const USER_WRITE_POLICY_PROFILE: &str = "xgeny.cli.explicit-allow-write-flag/v1";
const WORKSPACE_ROUTE_PROFILE: &str = "xgeny.cli.workspace-filesystem-route/v3";
const WORKSPACE_MATERIALIZER_PROFILE: &str = "xgeny.cli.durable-workspace-fs-materializer/v3";
const WORKSPACE_APPROVAL_PROFILE: &str = "xgeny.cli.workspace-fs-approval/v3";
const WORKSPACE_PLANNING_CONSTRAINT_PROFILE: &str = "xgeny.cli.workspace-fs-scope-constraint/v3";
const PROCESS_EXECUTION_PROFILE_DOMAIN: &str = "xgeny.cli.process-execution-profile/v1";
const PROCESS_ROUTE_PROFILE: &str = "xgeny.cli.process-route/v1";
const PROCESS_MATERIALIZER_PROFILE: &str = "xgeny.cli.durable-process-materializer/v1";
const PROCESS_APPROVAL_PROFILE: &str = "xgeny.cli.process-approval/v1";
const PROCESS_HOST_POLICY_PROFILE: &str = "xgeny.cli.host-process-catalog/v1";
const USER_EXECUTE_POLICY_PROFILE: &str = "xgeny.cli.explicit-allow-execute-flag/v1";
const PROCESS_PLANNING_CONSTRAINT_PROFILE: &str = "xgeny.cli.process-catalog-constraint/v1";

/// One new local Run invocation. Remote model transfer and local reads are separate decisions.
#[allow(clippy::struct_excessive_bools)] // Four independent process-local consent gates.
pub struct LocalRunRequest {
    pub goal: String,
    pub workspace: PathBuf,
    pub base_url: String,
    pub planner_id: String,
    pub model: String,
    pub tokenizer: String,
    pub allow_files: Vec<String>,
    pub allow_dirs: Vec<String>,
    pub allow_executables: Vec<String>,
    pub allow_remote_model_egress: bool,
    pub allow_read: bool,
    pub allow_write: bool,
    pub allow_execute: bool,
    pub max_ticks: u32,
}

impl LocalRunRequest {
    #[must_use]
    pub fn with_defaults(
        goal: String,
        workspace: PathBuf,
        base_url: String,
        model: String,
        tokenizer: String,
        allow_files: Vec<String>,
    ) -> Self {
        Self {
            goal,
            workspace,
            base_url,
            planner_id: DEFAULT_PLANNER_ID.to_owned(),
            model,
            tokenizer,
            allow_files,
            allow_dirs: Vec::new(),
            allow_executables: Vec::new(),
            allow_remote_model_egress: false,
            allow_read: false,
            allow_write: false,
            allow_execute: false,
            max_ticks: 32,
        }
    }
}

/// One continuation invocation. Completed Runs intentionally need no provider or workspace fields.
#[allow(clippy::struct_excessive_bools)] // Four independent process-local consent gates.
pub struct LocalResumeRequest {
    pub run_id: String,
    pub workspace: Option<PathBuf>,
    pub base_url: Option<String>,
    pub allow_files: Vec<String>,
    pub allow_dirs: Vec<String>,
    pub allow_executables: Vec<String>,
    pub allow_remote_model_egress: bool,
    pub allow_read: bool,
    pub allow_write: bool,
    pub allow_execute: bool,
    pub max_ticks: u32,
}

/// One explicit, non-durable model-catalog connectivity check.
pub struct ModelCheckRequest {
    pub base_url: String,
    pub model: String,
    pub tokenizer: String,
}

/// Stable, redacted failure classes returned by `xgeny model check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCheckError {
    Configuration,
    InvalidBaseUrl,
    InvalidBasePath,
    InvalidModel,
    InvalidTokenizer,
    InvalidCredential,
    InsecureCredentialTransport,
    InsecureEndpointTransport,
    AuthenticationRejected,
    RateLimited,
    Timeout,
    RequestRejected,
    Unavailable,
    InvalidResponse,
    ModelNotAdvertised,
}

impl ModelCheckError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Configuration => "configuration_invalid",
            Self::InvalidBaseUrl => "base_url_invalid",
            Self::InvalidBasePath => "base_url_must_end_in_v1",
            Self::InvalidModel => "model_invalid",
            Self::InvalidTokenizer => "tokenizer_invalid",
            Self::InvalidCredential => "api_key_invalid",
            Self::InsecureCredentialTransport => "api_key_requires_https",
            Self::InsecureEndpointTransport => "plaintext_endpoint_must_be_loopback",
            Self::AuthenticationRejected => "authentication_rejected",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::RequestRejected => "request_rejected",
            Self::Unavailable => "provider_unavailable",
            Self::InvalidResponse => "invalid_model_catalog",
            Self::ModelNotAdvertised => "model_not_advertised",
        }
    }

    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Configuration
            | Self::InvalidBaseUrl
            | Self::InvalidBasePath
            | Self::InvalidModel
            | Self::InvalidTokenizer
            | Self::InvalidCredential
            | Self::InsecureCredentialTransport
            | Self::InsecureEndpointTransport => 64,
            Self::InvalidResponse | Self::ModelNotAdvertised => 65,
            Self::AuthenticationRejected => 77,
            Self::RateLimited | Self::Timeout | Self::RequestRejected | Self::Unavailable => 69,
        }
    }
}

/// Stable, path-free result returned to the binary presentation layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalCommandResult {
    Completed {
        run_id: String,
        summary: String,
    },
    Paused {
        run_id: Option<String>,
        reason: PauseReason,
    },
    Rejected {
        run_id: String,
        reason: RejectionReason,
    },
    RecoveryRequired {
        run_id: String,
        reason: RecoveryReason,
    },
}

/// Check catalog access and exact model advertisement without Run state.
///
/// This sends exactly one `GET /v1/models`. It does not send a prompt or inference request and
/// never opens a workspace or local Run store. Any authentication enforced only by the inference
/// endpoint remains unverified until the first `run`.
///
/// # Errors
///
/// Returns only stable, redacted configuration, transport, status, and response classes.
pub fn check_openai_model(request: ModelCheckRequest) -> Result<(), ModelCheckError> {
    let ModelCheckRequest {
        base_url,
        model,
        tokenizer,
    } = request;
    let config = OpenAiPlannerConfig::new(&base_url, DEFAULT_PLANNER_ID, &model, &tokenizer)
        .and_then(|config| config.with_timeout(MODEL_CHECK_TIMEOUT))
        .map_err(map_model_check_config)?;
    let credential = provider_credential(&config).map_err(map_model_check_config)?;
    OpenAiModelChecker::new(config, credential)
        .map_err(map_model_check_config)?
        .check()
        .map_err(map_model_check_failure)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    RemoteModelEgressConsentRequired,
    ReadApprovalRequired,
    WriteApprovalRequired,
    ExecuteApprovalRequired,
    TickBudgetExhausted,
    Quiescent,
}

impl PauseReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::RemoteModelEgressConsentRequired => "remote_model_egress_consent_required",
            Self::ReadApprovalRequired => "read_approval_required",
            Self::WriteApprovalRequired => "write_approval_required",
            Self::ExecuteApprovalRequired => "execute_approval_required",
            Self::TickBudgetExhausted => "tick_budget_exhausted",
            Self::Quiescent => "quiescent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    ApprovalDenied,
    AdmissionRejected,
    ProposalRejected,
    MaterialRejected,
    ModelRejected,
    FailedWork,
}

impl RejectionReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ApprovalDenied => "approval_denied",
            Self::AdmissionRejected => "admission_rejected",
            Self::ProposalRejected => "proposal_rejected",
            Self::MaterialRejected => "material_rejected",
            Self::ModelRejected => "model_rejected",
            Self::FailedWork => "failed_work",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryReason {
    ModelCallUnknown,
    EffectOutcomeUnknown,
}

impl RecoveryReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ModelCallUnknown => "model_call_unknown",
            Self::EffectOutcomeUnknown => "effect_outcome_unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicRunError {
    Configuration,
    Busy,
    Integrity,
    Internal,
}

impl PublicRunError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Configuration => "configuration_mismatch",
            Self::Busy => "run_busy",
            Self::Integrity => "run_integrity_failure",
            Self::Internal => "internal_safety_failure",
        }
    }

    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Configuration => 64,
            Self::Busy => 75,
            Self::Integrity | Self::Internal => 70,
        }
    }
}

#[derive(Clone)]
enum LocalReadCatalog {
    ExactFiles(AllowFileCatalog),
    Workspace(WorkspaceReadAuthorization),
}

impl LocalReadCatalog {
    fn build(
        workspace: &WorkspaceRoot,
        allow_files: &[String],
        allow_dirs: &[String],
    ) -> Result<Self, PublicRunError> {
        if allow_dirs.is_empty() {
            return AllowFileCatalog::new(&workspace.resolver(), allow_files)
                .map(Self::ExactFiles)
                .map_err(|_| PublicRunError::Configuration);
        }
        WorkspaceReadAuthorization::new(&workspace.resolver(), allow_files, allow_dirs)
            .map(Self::Workspace)
            .map_err(|_| PublicRunError::Configuration)
    }

    fn catalog_digest(&self) -> &str {
        match self {
            Self::ExactFiles(catalog) => catalog.catalog_digest(),
            Self::Workspace(authorization) => authorization.catalog_digest(),
        }
    }

    const fn workspace_discovery(&self) -> bool {
        matches!(self, Self::Workspace(_))
    }

    const fn workspace_authorization(&self) -> Option<&WorkspaceReadAuthorization> {
        match self {
            Self::ExactFiles(_) => None,
            Self::Workspace(authorization) => Some(authorization),
        }
    }
}

/// Create and drive a new one-workspace, read-text prototype Run.
///
/// # Errors
///
/// Returns only a fixed public failure class. Paths, provider bodies, and credentials are never
/// included in the returned value.
pub fn run_local(request: LocalRunRequest) -> Result<LocalCommandResult, PublicRunError> {
    run_local_with_started(request, |_| {})
}

/// Create and drive a new Run, announcing its durable identifier before provider I/O.
///
/// The callback runs only after the manifest and initial `RunCreated` event are durable. A caller
/// can therefore use the identifier for crash recovery without announcing a phantom Run.
///
/// # Errors
///
/// Returns only a fixed public failure class. Paths, provider bodies, and credentials are never
/// included in the returned value.
pub fn run_local_with_started<F>(
    request: LocalRunRequest,
    on_started: F,
) -> Result<LocalCommandResult, PublicRunError>
where
    F: FnOnce(&str),
{
    validate_max_ticks(request.max_ticks)?;
    if request.allow_execute && request.allow_executables.is_empty() {
        return Err(PublicRunError::Configuration);
    }
    if !request.allow_remote_model_egress {
        return Ok(LocalCommandResult::Paused {
            run_id: None,
            reason: PauseReason::RemoteModelEgressConsentRequired,
        });
    }
    validate_goal(&request.goal)?;

    let workspace_id = WorkspaceId::new(WORKSPACE_ID).map_err(|_| PublicRunError::Internal)?;
    let workspace = WorkspaceRoot::open_ambient(&request.workspace, workspace_id.clone())
        .map_err(|_| PublicRunError::Configuration)?;
    let identity = workspace
        .physical_identity()
        .map_err(|_| PublicRunError::Configuration)?;
    let catalog = LocalReadCatalog::build(&workspace, &request.allow_files, &request.allow_dirs)?;
    let process =
        ProcessTooling::build(&request.workspace, WORKSPACE_ID, &request.allow_executables)
            .map_err(|_| PublicRunError::Configuration)?;
    if request.allow_execute && process.is_none() {
        return Err(PublicRunError::Configuration);
    }
    let planning_constraints_required = catalog.workspace_discovery() || process.is_some();
    let config = planner_config(
        &request.base_url,
        &request.planner_id,
        &request.model,
        &request.tokenizer,
        planning_constraints_required,
    )?;
    let local_execution_profile_digest =
        local_execution_profile_digest(&workspace, &catalog, process.as_ref())?;
    let run_id = generate_run_id().map_err(|_| PublicRunError::Internal)?;
    let manifest = RunManifest::new(
        &run_id,
        &workspace_id,
        WORKSPACE_IDENTITY_PROFILE,
        identity.as_str(),
        config.planner_id(),
        config.model(),
        &request.tokenizer,
        config.request_profile_digest(),
        catalog.catalog_digest(),
        &local_execution_profile_digest,
        if planning_constraints_required {
            ManifestBudget::workspace_discovery()
        } else {
            ManifestBudget::default()
        },
    )
    .map_err(|_| PublicRunError::Configuration)?;
    let planner = remote_planner(config)?;
    let state_root = discover_state_root().map_err(|_| PublicRunError::Configuration)?;
    let layout = RunLayout::create(&state_root, manifest.run_id()).map_err(map_layout_create)?;
    layout
        .write_manifest(&manifest)
        .map_err(|_| PublicRunError::Integrity)?;
    if planning_constraints_required {
        RunMaterialCatalog::create(&layout.material_catalog_path(), manifest.run_id())
            .map_err(|_| PublicRunError::Integrity)?;
    }
    let lease = acquire_lease(&layout, manifest.run_id())?;
    let mut store =
        SqliteRunStore::create(layout.database_path()).map_err(|_| PublicRunError::Integrity)?;
    seed_run(&mut store, &manifest, request.goal)?;
    on_started(manifest.run_id());
    continue_incomplete(
        &mut store,
        &lease,
        &manifest,
        &workspace,
        Some(planner),
        catalog,
        process.as_ref(),
        &layout.material_catalog_path(),
        request.allow_read,
        request.allow_write,
        request.allow_execute,
        request.max_ticks,
    )
}

/// Resume a Run, first attempting provider-free durable completion replay.
///
/// # Errors
///
/// Returns a fixed public failure class on invalid configuration, contention, or corrupt state.
#[allow(clippy::too_many_lines)]
pub fn resume_local(request: LocalResumeRequest) -> Result<LocalCommandResult, PublicRunError> {
    validate_max_ticks(request.max_ticks)?;
    let state_root = discover_state_root().map_err(|_| PublicRunError::Configuration)?;
    let layout = RunLayout::existing(&state_root, &request.run_id)
        .map_err(|_| PublicRunError::Configuration)?;
    let lease = acquire_lease(&layout, &request.run_id)?;
    let manifest = layout
        .read_manifest()
        .map_err(|_| PublicRunError::Integrity)?;
    let store = SqliteRunStore::open_existing_read_only(layout.database_path())
        .map_err(|_| PublicRunError::Integrity)?;
    let state = store
        .load_current()
        .map_err(|_| PublicRunError::Integrity)?
        .ok_or(PublicRunError::Integrity)?;
    verify_manifest_state(&manifest, &state)?;

    if let Some(output) = load_offline_completion(&store, &state)? {
        return Ok(LocalCommandResult::Completed {
            run_id: state.run_id,
            summary: output.summary().to_owned(),
        });
    }
    if has_unknown_model_call(&state) {
        return Ok(LocalCommandResult::RecoveryRequired {
            run_id: state.run_id,
            reason: RecoveryReason::ModelCallUnknown,
        });
    }
    if has_reserved_model_call(&state) {
        drop(store);
        let mut store = reopen_writable_verified(&layout, &manifest, &state)?;
        mark_reserved_model_call_unknown(&mut store, &lease, &manifest)?;
        return Ok(LocalCommandResult::RecoveryRequired {
            run_id: state.run_id,
            reason: RecoveryReason::ModelCallUnknown,
        });
    }
    if let Some(step_id) = executing_step_id(&state) {
        drop(store);
        let mut store = reopen_writable_verified(&layout, &manifest, &state)?;
        mark_executing_effect_unknown(&mut store, &lease, &step_id)?;
        return Ok(LocalCommandResult::RecoveryRequired {
            run_id: state.run_id,
            reason: RecoveryReason::EffectOutcomeUnknown,
        });
    }
    if has_effect_uncertainty(&state) {
        return Ok(LocalCommandResult::RecoveryRequired {
            run_id: state.run_id,
            reason: RecoveryReason::EffectOutcomeUnknown,
        });
    }
    let local_action_available = derive_frontier(&state)
        .map_err(|_| PublicRunError::Integrity)?
        .next_action()
        .is_some();
    if !request.allow_remote_model_egress && !local_action_available {
        return Ok(LocalCommandResult::Paused {
            run_id: Some(state.run_id),
            reason: PauseReason::RemoteModelEgressConsentRequired,
        });
    }

    let workspace_path = request.workspace.ok_or(PublicRunError::Configuration)?;
    let workspace = WorkspaceRoot::open_ambient(
        &workspace_path,
        manifest
            .workspace_id()
            .map_err(|_| PublicRunError::Integrity)?,
    )
    .map_err(|_| PublicRunError::Configuration)?;
    let identity = workspace
        .physical_identity()
        .map_err(|_| PublicRunError::Configuration)?;
    if manifest.workspace_identity_profile() != WORKSPACE_IDENTITY_PROFILE
        || manifest.workspace_identity_digest() != identity.as_str()
    {
        return Err(PublicRunError::Configuration);
    }
    let catalog = LocalReadCatalog::build(&workspace, &request.allow_files, &request.allow_dirs)?;
    let process = ProcessTooling::build(&workspace_path, WORKSPACE_ID, &request.allow_executables)
        .map_err(|_| PublicRunError::Configuration)?;
    if request.allow_execute && process.is_none() {
        return Err(PublicRunError::Configuration);
    }
    if manifest.local_execution_profile_digest()
        != local_execution_profile_digest(&workspace, &catalog, process.as_ref())?
    {
        return Err(PublicRunError::Configuration);
    }
    if manifest.allow_file_catalog_digest() != catalog.catalog_digest() {
        return Err(PublicRunError::Configuration);
    }
    if !manifest.remote_model_egress() {
        return Err(PublicRunError::Configuration);
    }
    let planner = if request.allow_remote_model_egress {
        let base_url = request.base_url.ok_or(PublicRunError::Configuration)?;
        let config = planner_config(
            &base_url,
            manifest.planner_id(),
            manifest.model(),
            manifest.tokenizer(),
            catalog.workspace_discovery() || process.is_some(),
        )?;
        if manifest.request_profile_digest() != config.request_profile_digest() {
            return Err(PublicRunError::Configuration);
        }
        Some(remote_planner(config)?)
    } else {
        None
    };
    drop(store);
    let mut store = reopen_writable_verified(&layout, &manifest, &state)?;
    continue_incomplete(
        &mut store,
        &lease,
        &manifest,
        &workspace,
        planner,
        catalog,
        process.as_ref(),
        &layout.material_catalog_path(),
        request.allow_read,
        request.allow_write,
        request.allow_execute,
        request.max_ticks,
    )
}

fn reopen_writable_verified(
    layout: &RunLayout,
    manifest: &RunManifest,
    preflight_state: &RunState,
) -> Result<SqliteRunStore, PublicRunError> {
    let store = SqliteRunStore::open_existing(layout.database_path())
        .map_err(|_| PublicRunError::Integrity)?;
    let reopened_state = store
        .load_current()
        .map_err(|_| PublicRunError::Integrity)?
        .ok_or(PublicRunError::Integrity)?;
    verify_manifest_state(manifest, &reopened_state)?;
    if reopened_state != *preflight_state {
        return Err(PublicRunError::Integrity);
    }
    Ok(store)
}

enum LocalReadMaterializer {
    ExactFiles(AllowFileCatalog),
    Workspace(WorkspaceReadMaterializer),
}

impl PlanMaterializer for LocalReadMaterializer {
    fn materialize(
        &mut self,
        request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        match self {
            Self::ExactFiles(materializer) => materializer.materialize(request),
            Self::Workspace(materializer) => materializer.materialize(request),
        }
    }
}

struct LocalMaterializer {
    filesystem: LocalReadMaterializer,
    process: Option<ProcessMaterializer>,
}

impl PlanMaterializer for LocalMaterializer {
    fn materialize(
        &mut self,
        request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        if request.capability().capability_id == PROCESS_EXECUTE_CAPABILITY_ID
            && request.capability().contract_version == PROCESS_EXECUTE_CONTRACT_VERSION
        {
            return self
                .process
                .as_mut()
                .ok_or(PlanMaterializerFailure::Rejected)?
                .materialize(request);
        }
        self.filesystem.materialize(request)
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn continue_incomplete(
    store: &mut SqliteRunStore,
    lease: &LocalRunLease,
    manifest: &RunManifest,
    workspace: &WorkspaceRoot,
    planner: Option<OpenAiPlanner>,
    catalog: LocalReadCatalog,
    process: Option<&ProcessTooling>,
    material_catalog_path: &Path,
    allow_read: bool,
    allow_write: bool,
    allow_execute: bool,
    max_ticks: u32,
) -> Result<LocalCommandResult, PublicRunError> {
    let model_egress_allowed = planner.is_some();
    let mut planner = if let Some(planner) = planner {
        LocalPlanner::Remote(Box::new(planner))
    } else {
        LocalPlanner::Disabled {
            planner_id: manifest.planner_id().to_owned(),
            request_profile_digest: manifest.request_profile_digest().to_owned(),
        }
    };
    let LocalToolProduct {
        capabilities,
        mut adapters,
        mut verifiers,
        mut route,
    } = local_tool_product(workspace, &catalog, process)?;
    let mut planning_constraints = catalog
        .workspace_authorization()
        .map(|authorization| {
            PlanningConstraint::new("workspace.fs-scope", authorization.planner_scope_hint())
        })
        .transpose()
        .map_err(|_| PublicRunError::Internal)?
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(process) = process {
        planning_constraints.push(
            PlanningConstraint::new(
                "process.executable-catalog",
                process.authorization().planner_scope_hint(),
            )
            .map_err(|_| PublicRunError::Internal)?,
        );
    }
    let resolver = LocalResourceResolver {
        filesystem: workspace.resolver(),
        process: process.map(|process| process.workspace().resolver()),
    };
    let mut providers = MaterialProviderRegistry::new();
    let (filesystem_materializer, approval_catalog) = match catalog {
        LocalReadCatalog::ExactFiles(catalog) => {
            let materializer = LocalReadMaterializer::ExactFiles(catalog.clone());
            providers
                .register(ALLOW_FILE_PROVIDER_ID, catalog.clone())
                .map_err(|_| PublicRunError::Internal)?;
            (materializer, ApprovalCatalog::ExactFiles(catalog))
        }
        LocalReadCatalog::Workspace(authorization) => {
            let materializer_catalog =
                RunMaterialCatalog::open_existing(material_catalog_path, manifest.run_id())
                    .map_err(|_| PublicRunError::Integrity)?;
            let provider_catalog =
                RunMaterialCatalog::open_existing(material_catalog_path, manifest.run_id())
                    .map_err(|_| PublicRunError::Integrity)?;
            let materializer = LocalReadMaterializer::Workspace(WorkspaceReadMaterializer::new(
                authorization.clone(),
                materializer_catalog,
            ));
            providers
                .register(
                    WORKSPACE_READ_MATERIAL_PROVIDER_ID,
                    WorkspaceReadMaterialProvider::new(authorization.clone(), provider_catalog),
                )
                .map_err(|_| PublicRunError::Internal)?;
            (materializer, ApprovalCatalog::Workspace(authorization))
        }
    };
    let process_materializer = if let Some(process) = process {
        let materializer_catalog =
            RunMaterialCatalog::open_existing(material_catalog_path, manifest.run_id())
                .map_err(|_| PublicRunError::Integrity)?;
        let provider_catalog =
            RunMaterialCatalog::open_existing(material_catalog_path, manifest.run_id())
                .map_err(|_| PublicRunError::Integrity)?;
        providers
            .register(
                PROCESS_MATERIAL_PROVIDER_ID,
                ProcessMaterialProvider::new(process.workspace().clone(), provider_catalog),
            )
            .map_err(|_| PublicRunError::Internal)?;
        Some(ProcessMaterializer::new(
            process.workspace().clone(),
            materializer_catalog,
        ))
    } else {
        None
    };
    let mut materializer = LocalMaterializer {
        filesystem: filesystem_materializer,
        process: process_materializer,
    };
    let mut approvals = ExplicitLocalApproval {
        allow_read,
        allow_write,
        allow_execute,
        run_id: manifest.run_id().to_owned(),
        catalog: approval_catalog,
        process: process.map(|process| process.authorization().clone()),
    };
    let mut events = HostEventFactory;
    let mut loop_runtime = AgentLoop::with_model_call_budget(
        manifest
            .budget()
            .agent_loop()
            .map_err(|_| PublicRunError::Integrity)?,
        manifest
            .budget()
            .model_calls()
            .map_err(|_| PublicRunError::Integrity)?,
    );
    if !planning_constraints.is_empty() {
        loop_runtime = loop_runtime
            .with_planning_constraints(planning_constraints)
            .map_err(|_| PublicRunError::Internal)?;
    }
    let driver = RunDriver::new(
        loop_runtime,
        NonZeroU32::new(max_ticks).ok_or(PublicRunError::Configuration)?,
    );
    let Ok(outcome) = driver.drive_until_pause_with_model_egress(
        store,
        &mut events,
        lease,
        &capabilities,
        &resolver,
        &mut planner,
        &mut materializer,
        &mut providers,
        &mut adapters,
        &mut verifiers,
        &mut route,
        &mut approvals,
        model_egress_allowed,
    ) else {
        return classify_durable_boundary_after_driver_error(store, manifest);
    };
    map_driver_outcome(manifest.run_id(), outcome)
}

fn classify_durable_boundary_after_driver_error(
    store: &SqliteRunStore,
    manifest: &RunManifest,
) -> Result<LocalCommandResult, PublicRunError> {
    let state = store
        .load_current()
        .map_err(|_| PublicRunError::Integrity)?
        .ok_or(PublicRunError::Integrity)?;
    verify_manifest_state(manifest, &state)?;
    let reason = if has_unknown_model_call(&state) || has_reserved_model_call(&state) {
        Some(RecoveryReason::ModelCallUnknown)
    } else if executing_step_id(&state).is_some() || has_effect_uncertainty(&state) {
        Some(RecoveryReason::EffectOutcomeUnknown)
    } else {
        None
    };
    reason.map_or(Err(PublicRunError::Internal), |reason| {
        Ok(LocalCommandResult::RecoveryRequired {
            run_id: state.run_id,
            reason,
        })
    })
}

enum LocalPlanner {
    Remote(Box<OpenAiPlanner>),
    Disabled {
        planner_id: String,
        request_profile_digest: String,
    },
}

impl PlannerPort for LocalPlanner {
    fn planner_id(&self) -> &str {
        match self {
            Self::Remote(planner) => planner.planner_id(),
            Self::Disabled { planner_id, .. } => planner_id,
        }
    }

    fn request_profile_digest(&self) -> &str {
        match self {
            Self::Remote(planner) => planner.request_profile_digest(),
            Self::Disabled {
                request_profile_digest,
                ..
            } => request_profile_digest,
        }
    }

    fn plan(
        &mut self,
        request: &PlannerCallRequest<'_>,
    ) -> Result<PlanProposal, PlannerPortFailure> {
        match self {
            Self::Remote(planner) => planner.plan(request),
            Self::Disabled { .. } => Err(PlannerPortFailure::Unavailable),
        }
    }
}

fn load_offline_completion(
    store: &SqliteRunStore,
    state: &RunState,
) -> Result<Option<CompletionOutputRecord>, PublicRunError> {
    let Some(candidate) = state
        .agent_loop
        .as_ref()
        .and_then(|agent| agent.completion_candidate.as_ref())
    else {
        return Ok(None);
    };
    let expected_record_digest = candidate
        .completion_output_record_digest
        .as_deref()
        .ok_or(PublicRunError::Integrity)?;
    let output = store
        .load_completion_output(ExpectedHead::from_state(state), &candidate.candidate_id)
        .map_err(|_| PublicRunError::Integrity)?
        .ok_or(PublicRunError::Integrity)?;
    let decision = output.decision().map_err(|_| PublicRunError::Integrity)?;
    output
        .verify_for(
            &state.run_id,
            &decision,
            &candidate.candidate_id,
            &candidate.summary_digest,
        )
        .map_err(|_| PublicRunError::Integrity)?;
    if output.record_digest() != expected_record_digest
        || output.context_digest() != candidate.context_digest
        || output.proposal_digest() != candidate.proposal_digest
    {
        return Err(PublicRunError::Integrity);
    }
    Ok(Some(output))
}

fn seed_run(
    store: &mut SqliteRunStore,
    manifest: &RunManifest,
    goal: String,
) -> Result<(), PublicRunError> {
    let recorded_at = now_rfc3339()?;
    store
        .append(
            ExpectedHead::Empty,
            RunEvent {
                event_id: format!("event-{}-1", manifest.run_id()),
                run_id: manifest.run_id().to_owned(),
                authority: manifest.authority(),
                authority_epoch: 1,
                recorded_at,
                body: RunEventBody::RunCreated { goal },
            },
        )
        .map_err(|_| PublicRunError::Integrity)?;
    Ok(())
}

fn verify_manifest_state(manifest: &RunManifest, state: &RunState) -> Result<(), PublicRunError> {
    manifest.verify().map_err(|_| PublicRunError::Integrity)?;
    if state.run_id != manifest.run_id()
        || state.authority != manifest.authority()
        || state.authority_epoch != 1
    {
        return Err(PublicRunError::Integrity);
    }
    if let Some(agent) = &state.agent_loop {
        if agent.budget
            != manifest
                .budget()
                .agent_loop()
                .map_err(|_| PublicRunError::Integrity)?
        {
            return Err(PublicRunError::Integrity);
        }
        if let Some(model_calls) = &agent.model_calls
            && model_calls.budget
                != manifest
                    .budget()
                    .model_calls()
                    .map_err(|_| PublicRunError::Integrity)?
        {
            return Err(PublicRunError::Integrity);
        }
    }
    Ok(())
}

fn has_unknown_model_call(state: &RunState) -> bool {
    state
        .agent_loop
        .as_ref()
        .and_then(|agent| agent.model_calls.as_ref())
        .and_then(|lifecycle| lifecycle.active_call.as_ref())
        .is_some_and(|call| matches!(call.status, ModelCallStatus::Unknown { .. }))
}

fn has_reserved_model_call(state: &RunState) -> bool {
    state
        .agent_loop
        .as_ref()
        .and_then(|agent| agent.model_calls.as_ref())
        .and_then(|lifecycle| lifecycle.active_call.as_ref())
        .is_some_and(|call| matches!(call.status, ModelCallStatus::Reserved))
}

fn executing_step_id(state: &RunState) -> Option<String> {
    state
        .steps
        .values()
        .find(|step| step.status == StepStatus::Executing)
        .map(|step| step.step_id.clone())
}

fn has_effect_uncertainty(state: &RunState) -> bool {
    state.steps.values().any(|step| {
        matches!(
            step.status,
            StepStatus::EffectUnknown | StepStatus::Reconciling | StepStatus::ManualRequired
        )
    })
}

fn mark_executing_effect_unknown(
    store: &mut SqliteRunStore,
    lease: &LocalRunLease,
    step_id: &str,
) -> Result<(), PublicRunError> {
    let capabilities = CapabilityRegistry::new();
    let mut adapters = EffectAdapterRegistry::new();
    DirectExecutor::new()
        .drive_step(
            store,
            &mut HostEventFactory,
            lease,
            &capabilities,
            &mut adapters,
            step_id,
            None,
        )
        .map_err(|_| PublicRunError::Integrity)?;
    let state = store
        .load_current()
        .map_err(|_| PublicRunError::Integrity)?
        .ok_or(PublicRunError::Integrity)?;
    if state.steps.get(step_id).map(|step| step.status) != Some(StepStatus::EffectUnknown) {
        return Err(PublicRunError::Integrity);
    }
    Ok(())
}

fn mark_reserved_model_call_unknown(
    store: &mut SqliteRunStore,
    lease: &LocalRunLease,
    manifest: &RunManifest,
) -> Result<(), PublicRunError> {
    let runtime = AgentLoop::with_model_call_budget(
        manifest
            .budget()
            .agent_loop()
            .map_err(|_| PublicRunError::Integrity)?,
        manifest
            .budget()
            .model_calls()
            .map_err(|_| PublicRunError::Integrity)?,
    );
    let tick = runtime
        .tick(
            store,
            &mut HostEventFactory,
            lease,
            &CapabilityRegistry::new(),
            &RejectResolver,
            &mut UnusedPlanner,
            &mut RejectMaterializer,
        )
        .map_err(|_| PublicRunError::Integrity)?;
    if !matches!(tick, AgentLoopTick::ModelCallRecoveryRequired { .. }) {
        return Err(PublicRunError::Integrity);
    }
    Ok(())
}

struct RejectResolver;

impl ResourceResolver for RejectResolver {
    fn resolve(&self, _scope: &str, _resource: &str) -> Result<String, ResourceResolutionFailure> {
        Err(ResourceResolutionFailure::OutsideHostBoundary)
    }
}

struct UnusedPlanner;

impl PlannerPort for UnusedPlanner {
    fn planner_id(&self) -> &'static str {
        "xgeny.cli.unused-recovery"
    }

    fn request_profile_digest(&self) -> &'static str {
        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
    }

    fn plan(
        &mut self,
        _request: &PlannerCallRequest<'_>,
    ) -> Result<PlanProposal, PlannerPortFailure> {
        Err(PlannerPortFailure::Unavailable)
    }
}

struct RejectMaterializer;

impl PlanMaterializer for RejectMaterializer {
    fn materialize(
        &mut self,
        _request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        Err(PlanMaterializerFailure::Rejected)
    }
}

fn planner_config(
    base_url: &str,
    planner_id: &str,
    model: &str,
    tokenizer: &str,
    planning_constraints_required: bool,
) -> Result<OpenAiPlannerConfig, PublicRunError> {
    let config = OpenAiPlannerConfig::new(base_url, planner_id, model, tokenizer)
        .and_then(|config| config.with_max_output_tokens(MAX_OUTPUT_TOKENS))
        .and_then(|config| config.with_timeout(MODEL_TIMEOUT))
        .map_err(map_provider_config)?;
    if planning_constraints_required {
        config
            .with_planning_constraints_required()
            .map_err(map_provider_config)
    } else {
        Ok(config)
    }
}

fn remote_planner(config: OpenAiPlannerConfig) -> Result<OpenAiPlanner, PublicRunError> {
    let credential = provider_credential(&config).map_err(map_provider_config)?;
    OpenAiPlanner::new(config, credential).map_err(map_provider_config)
}

fn provider_credential(
    config: &OpenAiPlannerConfig,
) -> Result<Option<BearerCredential>, OpenAiPlannerConfigError> {
    let credential = if config.accepts_bearer_credential() {
        match env::var("XGENY_OPENAI_API_KEY") {
            Ok(value) => Some(BearerCredential::new(&value)?),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                return Err(OpenAiPlannerConfigError::InvalidCredential);
            }
        }
    } else {
        None
    };
    Ok(credential)
}

fn map_provider_config(_error: OpenAiPlannerConfigError) -> PublicRunError {
    PublicRunError::Configuration
}

fn map_model_check_failure(error: OpenAiModelCheckFailure) -> ModelCheckError {
    match error {
        OpenAiModelCheckFailure::AuthenticationRejected => ModelCheckError::AuthenticationRejected,
        OpenAiModelCheckFailure::RateLimited => ModelCheckError::RateLimited,
        OpenAiModelCheckFailure::Timeout => ModelCheckError::Timeout,
        OpenAiModelCheckFailure::RequestRejected => ModelCheckError::RequestRejected,
        OpenAiModelCheckFailure::Unavailable => ModelCheckError::Unavailable,
        OpenAiModelCheckFailure::InvalidResponse => ModelCheckError::InvalidResponse,
        OpenAiModelCheckFailure::ModelNotAdvertised => ModelCheckError::ModelNotAdvertised,
    }
}

fn map_model_check_config(error: OpenAiPlannerConfigError) -> ModelCheckError {
    match error {
        OpenAiPlannerConfigError::InvalidBaseUrl => ModelCheckError::InvalidBaseUrl,
        OpenAiPlannerConfigError::InvalidBasePath => ModelCheckError::InvalidBasePath,
        OpenAiPlannerConfigError::InvalidProfileField("model") => ModelCheckError::InvalidModel,
        OpenAiPlannerConfigError::InvalidProfileField("tokenizer") => {
            ModelCheckError::InvalidTokenizer
        }
        OpenAiPlannerConfigError::InvalidCredential => ModelCheckError::InvalidCredential,
        OpenAiPlannerConfigError::InsecureCredentialTransport => {
            ModelCheckError::InsecureCredentialTransport
        }
        OpenAiPlannerConfigError::InsecureEndpointTransport => {
            ModelCheckError::InsecureEndpointTransport
        }
        OpenAiPlannerConfigError::InvalidPlannerId
        | OpenAiPlannerConfigError::InvalidProfileField(_)
        | OpenAiPlannerConfigError::InvalidLimit(_)
        | OpenAiPlannerConfigError::ProfileDigest => ModelCheckError::Configuration,
    }
}

fn validate_goal(goal: &str) -> Result<(), PublicRunError> {
    if goal.is_empty()
        || goal.len() > MAX_GOAL_BYTES
        || goal.chars().any(|character| character == '\0')
    {
        return Err(PublicRunError::Configuration);
    }
    Ok(())
}

fn validate_max_ticks(max_ticks: u32) -> Result<(), PublicRunError> {
    if max_ticks == 0 || max_ticks > MAX_TICKS {
        return Err(PublicRunError::Configuration);
    }
    Ok(())
}

fn acquire_lease(layout: &RunLayout, run_id: &str) -> Result<LocalRunLease, PublicRunError> {
    LocalRunLease::try_acquire(run_id, layout.lease_path()).map_err(|error| match error {
        xgeny_runtime::LeaseError::AlreadyHeld => PublicRunError::Busy,
        xgeny_runtime::LeaseError::Io => PublicRunError::Internal,
    })
}

fn map_layout_create(error: crate::run_layout::RunLayoutError) -> PublicRunError {
    match error {
        crate::run_layout::RunLayoutError::InvalidRunId => PublicRunError::Configuration,
        crate::run_layout::RunLayoutError::AlreadyExists
        | crate::run_layout::RunLayoutError::Unavailable
        | crate::run_layout::RunLayoutError::InvalidManifest => PublicRunError::Internal,
    }
}

fn map_driver_outcome(
    run_id: &str,
    outcome: DriverOutcome,
) -> Result<LocalCommandResult, PublicRunError> {
    let run_id = run_id.to_owned();
    Ok(match outcome {
        DriverOutcome::CompletionCandidate {
            output: Some(output),
            ..
        } => LocalCommandResult::Completed {
            run_id,
            summary: output.summary().to_owned(),
        },
        DriverOutcome::ApprovalPending {
            effect_class: EffectClass::ReadOnly,
            ..
        } => LocalCommandResult::Paused {
            run_id: Some(run_id),
            reason: PauseReason::ReadApprovalRequired,
        },
        DriverOutcome::ApprovalPending {
            effect_class: EffectClass::Idempotent,
            ..
        } => LocalCommandResult::Paused {
            run_id: Some(run_id),
            reason: PauseReason::WriteApprovalRequired,
        },
        DriverOutcome::ApprovalPending {
            effect_class: EffectClass::NonIdempotent,
            ..
        } => LocalCommandResult::Paused {
            run_id: Some(run_id),
            reason: PauseReason::ExecuteApprovalRequired,
        },
        DriverOutcome::CompletionCandidate { output: None, .. }
        | DriverOutcome::ApprovalPending { .. } => {
            return Err(PublicRunError::Integrity);
        }
        DriverOutcome::TickBudgetExhausted => LocalCommandResult::Paused {
            run_id: Some(run_id),
            reason: PauseReason::TickBudgetExhausted,
        },
        DriverOutcome::Quiescent(reason) => match reason {
            AgentLoopQuiescence::FailedSteps => LocalCommandResult::Rejected {
                run_id,
                reason: RejectionReason::FailedWork,
            },
            AgentLoopQuiescence::ManualIntervention => LocalCommandResult::RecoveryRequired {
                run_id,
                reason: RecoveryReason::EffectOutcomeUnknown,
            },
            _ => LocalCommandResult::Paused {
                run_id: Some(run_id),
                reason: PauseReason::Quiescent,
            },
        },
        DriverOutcome::ApprovalDenied { .. } => LocalCommandResult::Rejected {
            run_id,
            reason: RejectionReason::ApprovalDenied,
        },
        DriverOutcome::AdmissionNotAuthorized { .. } => LocalCommandResult::Rejected {
            run_id,
            reason: RejectionReason::AdmissionRejected,
        },
        DriverOutcome::ProposalRejected(_) => LocalCommandResult::Rejected {
            run_id,
            reason: RejectionReason::ProposalRejected,
        },
        DriverOutcome::MaterializerUnavailable(_) => LocalCommandResult::Rejected {
            run_id,
            reason: RejectionReason::MaterialRejected,
        },
        DriverOutcome::PlannerUnavailable(failure) => match failure {
            PlannerPortFailure::Timeout | PlannerPortFailure::Unavailable => {
                LocalCommandResult::RecoveryRequired {
                    run_id,
                    reason: RecoveryReason::ModelCallUnknown,
                }
            }
            _ => LocalCommandResult::Rejected {
                run_id,
                reason: RejectionReason::ModelRejected,
            },
        },
        DriverOutcome::ModelCallRecoveryRequired { .. } => LocalCommandResult::RecoveryRequired {
            run_id,
            reason: RecoveryReason::ModelCallUnknown,
        },
        DriverOutcome::ModelCallRejected(_) => LocalCommandResult::Rejected {
            run_id,
            reason: RejectionReason::ModelRejected,
        },
        DriverOutcome::ModelEgressRequired => LocalCommandResult::Paused {
            run_id: Some(run_id),
            reason: PauseReason::RemoteModelEgressConsentRequired,
        },
    })
}

struct LocalToolProduct {
    capabilities: CapabilityRegistry,
    adapters: EffectAdapterRegistry,
    verifiers: EffectVerifierRegistry,
    route: ExactLocalRoute,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalExecutionProfileDescriptor<'a> {
    domain: &'static str,
    definition: &'a CapabilityDefinitionBody,
    instance: &'a CapabilityInstanceBody,
    adapter_max_bytes: usize,
    route_profile: &'static str,
    materializer_profile: &'static str,
    material_provider_id: &'static str,
    approval_profile: &'static str,
    host_policy_profile: &'static str,
    user_read_policy_profile: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDiscoveryProfileDescriptor<'a> {
    domain: &'static str,
    definitions: Vec<&'a CapabilityDefinitionBody>,
    instances: Vec<&'a CapabilityInstanceBody>,
    limits: DiscoveryLimitsDescriptor,
    route_profile: &'static str,
    materializer_profile: &'static str,
    material_provider_id: &'static str,
    material_catalog_schema_version: i64,
    material_recipe_domain: &'static str,
    material_recipe_format_version: u32,
    material_recipe_max_bytes: usize,
    planning_constraint_profile: &'static str,
    approval_profile: &'static str,
    host_policy_profile: &'static str,
    user_read_policy_profile: &'static str,
    user_write_policy_profile: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryLimitsDescriptor {
    read_text_bytes: usize,
    write_atomic_bytes: usize,
    apply_patch_bytes: usize,
    apply_patch_edits: usize,
    list_entries: usize,
    directory_scan_entries: usize,
    search_query_bytes: usize,
    search_query_unicode_scalars: usize,
    search_matches: usize,
    search_visited_entries: usize,
    search_file_bytes: usize,
    search_total_bytes: usize,
    search_preview_bytes: usize,
    query_output_canonical_bytes: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessExecutionProfileDescriptor<'a> {
    domain: &'static str,
    filesystem_profile_digest: &'a str,
    definition: &'a CapabilityDefinitionBody,
    instance: &'a CapabilityInstanceBody,
    executable_authorization_digest: &'a str,
    executable_catalog_profile: &'static str,
    environment_profile: &'static str,
    minimum_capture_bytes: usize,
    maximum_capture_bytes: usize,
    maximum_timeout_ms: u64,
    route_profile: &'static str,
    materializer_profile: &'static str,
    material_provider_id: &'static str,
    material_catalog_schema_version: i64,
    material_recipe_domain: &'static str,
    material_recipe_format_version: u32,
    material_recipe_max_bytes: usize,
    planning_constraint_profile: &'static str,
    approval_profile: &'static str,
    host_policy_profile: &'static str,
    user_execute_policy_profile: &'static str,
}

fn local_execution_profile_digest(
    workspace: &WorkspaceRoot,
    catalog: &LocalReadCatalog,
    process: Option<&ProcessTooling>,
) -> Result<String, PublicRunError> {
    let filesystem_profile_digest = filesystem_execution_profile_digest(workspace, catalog)?;
    let Some(process) = process else {
        return Ok(filesystem_profile_digest);
    };
    let (definition, instance) = process_spec(process.workspace())?;
    let canonical = serde_jcs::to_vec(&ProcessExecutionProfileDescriptor {
        domain: PROCESS_EXECUTION_PROFILE_DOMAIN,
        filesystem_profile_digest: &filesystem_profile_digest,
        definition: &definition,
        instance: &instance,
        executable_authorization_digest: process.authorization().catalog_digest(),
        executable_catalog_profile: PROCESS_EXECUTABLE_CATALOG_PROFILE,
        environment_profile: SAFE_PROCESS_ENVIRONMENT_PROFILE,
        minimum_capture_bytes: MIN_CAPTURE_BYTES,
        maximum_capture_bytes: MAX_CAPTURE_BYTES,
        maximum_timeout_ms: MAX_PROCESS_TIMEOUT_MS,
        route_profile: PROCESS_ROUTE_PROFILE,
        materializer_profile: PROCESS_MATERIALIZER_PROFILE,
        material_provider_id: PROCESS_MATERIAL_PROVIDER_ID,
        material_catalog_schema_version: WORKSPACE_READ_MATERIAL_CATALOG_SCHEMA_VERSION,
        material_recipe_domain: PROCESS_RECIPE_DOMAIN,
        material_recipe_format_version: PROCESS_RECIPE_FORMAT_VERSION,
        material_recipe_max_bytes: MAX_RECIPE_BYTES,
        planning_constraint_profile: PROCESS_PLANNING_CONSTRAINT_PROFILE,
        approval_profile: PROCESS_APPROVAL_PROFILE,
        host_policy_profile: PROCESS_HOST_POLICY_PROFILE,
        user_execute_policy_profile: USER_EXECUTE_POLICY_PROFILE,
    })
    .map_err(|_| PublicRunError::Internal)?;
    Ok(sha256_digest(&canonical))
}

fn filesystem_execution_profile_digest(
    workspace: &WorkspaceRoot,
    catalog: &LocalReadCatalog,
) -> Result<String, PublicRunError> {
    if catalog.workspace_authorization().is_some() {
        return workspace_discovery_profile_digest(workspace);
    }
    let (definition, instance) = read_text_spec(workspace)?;
    let canonical = serde_jcs::to_vec(&LocalExecutionProfileDescriptor {
        domain: LOCAL_EXECUTION_PROFILE_DOMAIN,
        definition: &definition,
        instance: &instance,
        adapter_max_bytes: ReadTextLimits::default().max_bytes(),
        route_profile: ROUTE_PROFILE,
        materializer_profile: MATERIALIZER_PROFILE,
        material_provider_id: ALLOW_FILE_PROVIDER_ID,
        approval_profile: APPROVAL_PROFILE,
        host_policy_profile: HOST_POLICY_PROFILE,
        user_read_policy_profile: USER_READ_POLICY_PROFILE,
    })
    .map_err(|_| PublicRunError::Internal)?;
    Ok(sha256_digest(&canonical))
}

fn workspace_discovery_profile_digest(workspace: &WorkspaceRoot) -> Result<String, PublicRunError> {
    let specs = workspace_discovery_specs(workspace)?;
    let canonical = serde_jcs::to_vec(&WorkspaceDiscoveryProfileDescriptor {
        domain: WORKSPACE_DISCOVERY_PROFILE_DOMAIN,
        definitions: specs.iter().map(|(definition, _)| definition).collect(),
        instances: specs.iter().map(|(_, instance)| instance).collect(),
        limits: DiscoveryLimitsDescriptor {
            read_text_bytes: ReadTextLimits::default().max_bytes(),
            write_atomic_bytes: MAX_WRITE_ATOMIC_BYTES,
            apply_patch_bytes: MAX_APPLY_PATCH_BYTES,
            apply_patch_edits: MAX_APPLY_PATCH_EDITS,
            list_entries: MAX_LIST_DIRECTORY_ENTRIES,
            directory_scan_entries: MAX_DIRECTORY_SCAN_ENTRIES,
            search_query_bytes: MAX_SEARCH_QUERY_BYTES,
            search_query_unicode_scalars: MAX_SEARCH_QUERY_UNICODE_SCALARS,
            search_matches: MAX_SEARCH_MATCHES,
            search_visited_entries: MAX_SEARCH_VISITED_ENTRIES,
            search_file_bytes: MAX_SEARCH_FILE_BYTES,
            search_total_bytes: MAX_SEARCH_TOTAL_BYTES,
            search_preview_bytes: MAX_SEARCH_PREVIEW_BYTES,
            query_output_canonical_bytes: MAX_QUERY_OUTPUT_CANONICAL_BYTES,
        },
        route_profile: WORKSPACE_ROUTE_PROFILE,
        materializer_profile: WORKSPACE_MATERIALIZER_PROFILE,
        material_provider_id: WORKSPACE_READ_MATERIAL_PROVIDER_ID,
        material_catalog_schema_version: WORKSPACE_READ_MATERIAL_CATALOG_SCHEMA_VERSION,
        material_recipe_domain: WORKSPACE_READ_RECIPE_DOMAIN,
        material_recipe_format_version: WORKSPACE_READ_RECIPE_FORMAT_VERSION,
        material_recipe_max_bytes: MAX_RECIPE_BYTES,
        planning_constraint_profile: WORKSPACE_PLANNING_CONSTRAINT_PROFILE,
        approval_profile: WORKSPACE_APPROVAL_PROFILE,
        host_policy_profile: WORKSPACE_HOST_POLICY_PROFILE,
        user_read_policy_profile: USER_READ_POLICY_PROFILE,
        user_write_policy_profile: USER_WRITE_POLICY_PROFILE,
    })
    .map_err(|_| PublicRunError::Internal)?;
    Ok(sha256_digest(&canonical))
}

fn read_text_spec(
    workspace: &WorkspaceRoot,
) -> Result<(CapabilityDefinitionBody, CapabilityInstanceBody), PublicRunError> {
    filesystem_spec(
        include_str!(
            "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
        ),
        READ_TEXT_CAPABILITY_ID,
        READ_TEXT_CONTRACT_VERSION,
        "local.fs.builtin.v1",
        workspace.binding(),
    )
}

fn process_spec(
    workspace: &xgeny_adapter_process::ProcessWorkspace,
) -> Result<(CapabilityDefinitionBody, CapabilityInstanceBody), PublicRunError> {
    filesystem_spec(
        include_str!(
            "../../../protocol/fixtures/v1alpha1/valid/capability-definition.process-execute.json"
        ),
        PROCESS_EXECUTE_CAPABILITY_ID,
        PROCESS_EXECUTE_CONTRACT_VERSION,
        "local.process.execute.builtin.v1",
        workspace.binding(),
    )
}

fn filesystem_spec(
    definition_json: &str,
    capability_id: &str,
    contract_version: &str,
    instance_id: &str,
    binding: xgeny_domain::InstanceBinding,
) -> Result<(CapabilityDefinitionBody, CapabilityInstanceBody), PublicRunError> {
    let document: ProtocolDocument =
        serde_json::from_str(definition_json).map_err(|_| PublicRunError::Internal)?;
    let ProtocolDocument::CapabilityDefinition(definition) = document else {
        return Err(PublicRunError::Internal);
    };
    let definition: CapabilityDefinitionBody = *definition;
    if definition.metadata.id != capability_id
        || definition.metadata.contract_version != contract_version
    {
        return Err(PublicRunError::Internal);
    }
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-instance.local-fs.json"
    ))
    .map_err(|_| PublicRunError::Internal)?;
    let ProtocolDocument::CapabilityInstance(instance) = document else {
        return Err(PublicRunError::Internal);
    };
    let mut instance: CapabilityInstanceBody = *instance;
    instance_id.clone_into(&mut instance.instance_id);
    instance.definition = CapabilityRef {
        capability_id: definition.metadata.id.clone(),
        contract_version: definition.metadata.contract_version.clone(),
    };
    instance.platform = Platform {
        os: host_os()?,
        arch: host_arch()?,
    };
    instance.binding = binding;
    instance.features.cancellable = false;
    Ok((definition, instance))
}

fn workspace_discovery_specs(
    workspace: &WorkspaceRoot,
) -> Result<Vec<(CapabilityDefinitionBody, CapabilityInstanceBody)>, PublicRunError> {
    Ok(vec![
        filesystem_spec(
            include_str!(
                "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-apply-patch.json"
            ),
            APPLY_PATCH_CAPABILITY_ID,
            APPLY_PATCH_CONTRACT_VERSION,
            "local.fs.apply-patch.builtin.v1",
            workspace.apply_patch_binding(),
        )?,
        filesystem_spec(
            include_str!(
                "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-list-directory.json"
            ),
            LIST_DIRECTORY_CAPABILITY_ID,
            LIST_DIRECTORY_CONTRACT_VERSION,
            "local.fs.list-directory.builtin.v1",
            workspace.list_directory_binding(),
        )?,
        read_text_spec(workspace)?,
        filesystem_spec(
            include_str!(
                "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-search-text.json"
            ),
            SEARCH_TEXT_CAPABILITY_ID,
            SEARCH_TEXT_CONTRACT_VERSION,
            "local.fs.search-text.builtin.v1",
            workspace.search_text_binding(),
        )?,
        filesystem_spec(
            include_str!(
                "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-stat.json"
            ),
            STAT_CAPABILITY_ID,
            STAT_CONTRACT_VERSION,
            "local.fs.stat.builtin.v1",
            workspace.stat_binding(),
        )?,
        filesystem_spec(
            include_str!(
                "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-write-atomic.json"
            ),
            WRITE_ATOMIC_CAPABILITY_ID,
            WRITE_ATOMIC_CONTRACT_VERSION,
            "local.fs.write-atomic.builtin.v1",
            workspace.write_atomic_binding(),
        )?,
    ])
}

fn local_tool_product(
    workspace: &WorkspaceRoot,
    catalog: &LocalReadCatalog,
    process: Option<&ProcessTooling>,
) -> Result<LocalToolProduct, PublicRunError> {
    let workspace_discovery = catalog.workspace_discovery();
    let mut specs = if workspace_discovery {
        workspace_discovery_specs(workspace)?
    } else {
        vec![read_text_spec(workspace)?]
    };
    if let Some(process) = process {
        specs.push(process_spec(process.workspace())?);
    }
    let mut capabilities = CapabilityRegistry::new();
    let mut routes = Vec::with_capacity(specs.len());
    for (definition, instance) in specs {
        routes.push((instance.definition.clone(), instance.instance_id.clone()));
        capabilities
            .register_schema_validated_definition(definition)
            .map_err(|_| PublicRunError::Internal)?;
        capabilities
            .register_schema_validated_instance(instance)
            .map_err(|_| PublicRunError::Internal)?;
    }
    let mut adapters = EffectAdapterRegistry::new();
    let read_adapter = workspace.read_text_adapter(ReadTextLimits::default());
    let read_verifier = read_adapter.verifier();
    adapters
        .register(&workspace.binding(), read_adapter)
        .map_err(|_| PublicRunError::Internal)?;
    let mut verifiers = EffectVerifierRegistry::new();
    verifiers
        .register(&workspace.binding(), read_verifier)
        .map_err(|_| PublicRunError::Internal)?;
    if workspace_discovery {
        let list_adapter = workspace.list_directory_adapter();
        let list_verifier = list_adapter.verifier();
        adapters
            .register(&workspace.list_directory_binding(), list_adapter)
            .map_err(|_| PublicRunError::Internal)?;
        verifiers
            .register(&workspace.list_directory_binding(), list_verifier)
            .map_err(|_| PublicRunError::Internal)?;

        let search_adapter = workspace.search_text_adapter();
        let search_verifier = search_adapter.verifier();
        adapters
            .register(&workspace.search_text_binding(), search_adapter)
            .map_err(|_| PublicRunError::Internal)?;
        verifiers
            .register(&workspace.search_text_binding(), search_verifier)
            .map_err(|_| PublicRunError::Internal)?;

        let stat_adapter = workspace.stat_adapter();
        let stat_verifier = stat_adapter.verifier();
        adapters
            .register(&workspace.stat_binding(), stat_adapter)
            .map_err(|_| PublicRunError::Internal)?;
        verifiers
            .register(&workspace.stat_binding(), stat_verifier)
            .map_err(|_| PublicRunError::Internal)?;

        let write_adapter = workspace.write_atomic_adapter();
        let write_verifier = write_adapter.verifier();
        adapters
            .register(&workspace.write_atomic_binding(), write_adapter)
            .map_err(|_| PublicRunError::Internal)?;
        verifiers
            .register(&workspace.write_atomic_binding(), write_verifier)
            .map_err(|_| PublicRunError::Internal)?;

        let patch_adapter = workspace.apply_patch_adapter();
        let patch_verifier = patch_adapter.verifier();
        adapters
            .register(&workspace.apply_patch_binding(), patch_adapter)
            .map_err(|_| PublicRunError::Internal)?;
        verifiers
            .register(&workspace.apply_patch_binding(), patch_verifier)
            .map_err(|_| PublicRunError::Internal)?;
    }
    if let Some(process) = process {
        let process_adapter = process.workspace().adapter();
        let process_verifier = process_adapter.verifier();
        adapters
            .register(&process.workspace().binding(), process_adapter)
            .map_err(|_| PublicRunError::Internal)?;
        verifiers
            .register(&process.workspace().binding(), process_verifier)
            .map_err(|_| PublicRunError::Internal)?;
    }
    Ok(LocalToolProduct {
        capabilities,
        adapters,
        verifiers,
        route: ExactLocalRoute { routes },
    })
}

struct LocalResourceResolver {
    filesystem: WorkspaceResourceResolver,
    process: Option<ProcessResourceResolver>,
}

impl ResourceResolver for LocalResourceResolver {
    fn resolve(&self, scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        if scope == PROCESS_EXECUTE_SCOPE {
            return self
                .process
                .as_ref()
                .ok_or(ResourceResolutionFailure::UnsupportedScope)?
                .resolve(scope, resource);
        }
        self.filesystem.resolve(scope, resource)
    }
}

struct ExactLocalRoute {
    routes: Vec<(CapabilityRef, String)>,
}

impl PlannedRoutePort for ExactLocalRoute {
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
        let once = (planned.capability_id() == WRITE_ATOMIC_CAPABILITY_ID
            && planned.contract_version() == WRITE_ATOMIC_CONTRACT_VERSION)
            || (planned.capability_id() == APPLY_PATCH_CAPABILITY_ID
                && planned.contract_version() == APPLY_PATCH_CONTRACT_VERSION)
            || (planned.capability_id() == PROCESS_EXECUTE_CAPABILITY_ID
                && planned.contract_version() == PROCESS_EXECUTE_CONTRACT_VERSION);
        if !local_profile_matches_effect_kind(once, planned.execution_profile()) {
            return Err(PlannedRouteFailure::Rejected);
        }
        let (capability, instance_id) = self
            .routes
            .iter()
            .find(|(capability, _)| {
                planned.capability_id() == capability.capability_id
                    && planned.contract_version() == capability.contract_version
            })
            .ok_or(PlannedRouteFailure::Rejected)?;
        Ok(RouteRequest {
            capability: capability.clone(),
            target_platform: Platform {
                os: host_os().map_err(|_| PlannedRouteFailure::Unavailable)?,
                arch: host_arch().map_err(|_| PlannedRouteFailure::Unavailable)?,
            },
            required_features: RequiredRouteFeatures {
                execution_style: xgeny_domain::ExecutionStyle::Sync,
                cancellation: false,
                idempotency_key: once,
                idempotency_query: false,
            },
            allowed_trust_levels: vec![TrustLevel::Verified],
            allowed_data_boundaries: vec![DataBoundary::Local],
            trust_preference: Vec::new(),
            data_boundary_preference: Vec::new(),
            preferred_instance_ids: Vec::new(),
            pinned_instance_id: Some(instance_id.clone()),
        })
    }
}

fn local_profile_matches_effect_kind(once: bool, profile: PlannedExecutionProfile) -> bool {
    matches!(
        (once, profile),
        (
            true,
            PlannedExecutionProfile::LocalSyncOnceV1
                | PlannedExecutionProfile::LocalSyncOnceOccurrenceV1
        ) | (
            false,
            PlannedExecutionProfile::LocalSyncReadOnlyV1
                | PlannedExecutionProfile::LocalSyncReadOnlyOccurrenceV1
        )
    )
}

enum ApprovalCatalog {
    ExactFiles(AllowFileCatalog),
    Workspace(WorkspaceReadAuthorization),
}

impl ApprovalCatalog {
    const fn host_policy_profile(&self) -> &'static str {
        match self {
            Self::ExactFiles(_) => HOST_POLICY_PROFILE,
            Self::Workspace(_) => WORKSPACE_HOST_POLICY_PROFILE,
        }
    }
}

struct ExplicitLocalApproval {
    allow_read: bool,
    allow_write: bool,
    allow_execute: bool,
    run_id: String,
    catalog: ApprovalCatalog,
    process: Option<ProcessExecutionAuthorization>,
}

impl ApprovalPort for ExplicitLocalApproval {
    fn decide(
        &mut self,
        request: &ResolvedPermissionRequest,
    ) -> Result<ApprovalDecision, ApprovalPortFailure> {
        let Some((host_policy_profile, user_policy_profile, approved)) =
            self.authorized_action_profiles(request)
        else {
            return Ok(ApprovalDecision::Denied);
        };
        if !approved {
            return Ok(ApprovalDecision::Pending);
        }
        let allowance = || {
            PolicyAllowance::from_trusted_evaluation(
                request.requested_scopes().iter().cloned(),
                request.resources().iter().cloned(),
                request.critical_actions().iter().copied(),
                [GrantLifetime::Once],
            )
        };
        Ok(ApprovalDecision::Approved(Box::new(PolicyInputs::local(
            request,
            PolicyContribution::allow(
                policy_source(
                    PolicySourceKind::Host,
                    "xgeny-cli-host",
                    host_policy_profile,
                ),
                allowance(),
            ),
            PolicyContribution::allow(
                policy_source(
                    PolicySourceKind::UserProfile,
                    "xgeny-cli-user",
                    user_policy_profile,
                ),
                allowance(),
            ),
        ))))
    }
}

impl ExplicitLocalApproval {
    fn authorized_action_profiles(
        &self,
        request: &ResolvedPermissionRequest,
    ) -> Option<(&'static str, &'static str, bool)> {
        if request.effect_class() == EffectClass::NonIdempotent {
            return self.authorized_process_profiles(request);
        }
        let expected_scope = match request.effect_class() {
            EffectClass::ReadOnly => FILESYSTEM_READ_SCOPE,
            EffectClass::Idempotent => FILESYSTEM_WRITE_SCOPE,
            _ => return None,
        };
        let exact = request.run_id() == self.run_id
            && request.requested_lifetime() == GrantLifetime::Once
            && request.requested_scopes().len() == 1
            && request
                .requested_scopes()
                .first()
                .is_some_and(|scope| scope == expected_scope)
            && request.resources().len() == 1
            && request.resources().first().is_some_and(|resource| {
                resource.scope() == expected_scope
                    && match &self.catalog {
                        ApprovalCatalog::ExactFiles(catalog) => {
                            request.effect_class() == EffectClass::ReadOnly
                                && request.capability().capability_id == READ_TEXT_CAPABILITY_ID
                                && request.capability().contract_version
                                    == READ_TEXT_CONTRACT_VERSION
                                && catalog
                                    .contains_canonical_resource(resource.canonical_resource())
                        }
                        ApprovalCatalog::Workspace(authorization) => authorization
                            .authorizes_resource(
                                request.capability(),
                                resource.canonical_resource(),
                            ),
                    }
            })
            && request.critical_actions().is_empty();
        exact.then(|| {
            let (user_profile, approved) = match request.effect_class() {
                EffectClass::ReadOnly => (USER_READ_POLICY_PROFILE, self.allow_read),
                EffectClass::Idempotent => (USER_WRITE_POLICY_PROFILE, self.allow_write),
                _ => unreachable!("effect class was closed above"),
            };
            (self.catalog.host_policy_profile(), user_profile, approved)
        })
    }

    fn authorized_process_profiles(
        &self,
        request: &ResolvedPermissionRequest,
    ) -> Option<(&'static str, &'static str, bool)> {
        let authorization = self.process.as_ref()?;
        let exact = request.run_id() == self.run_id
            && request.requested_lifetime() == GrantLifetime::Once
            && request.requested_scopes().len() == 1
            && request
                .requested_scopes()
                .first()
                .is_some_and(|scope| scope == PROCESS_EXECUTE_SCOPE)
            && request.resources().len() == 1
            && request.resources().first().is_some_and(|resource| {
                resource.scope() == PROCESS_EXECUTE_SCOPE
                    && authorization.authorizes_resource(resource.canonical_resource())
            })
            && request.capability().capability_id == PROCESS_EXECUTE_CAPABILITY_ID
            && request.capability().contract_version == PROCESS_EXECUTE_CONTRACT_VERSION
            && request.critical_actions().is_empty();
        exact.then_some((
            PROCESS_HOST_POLICY_PROFILE,
            USER_EXECUTE_POLICY_PROFILE,
            self.allow_execute,
        ))
    }
}

fn policy_source(kind: PolicySourceKind, id: &str, profile: &str) -> PolicySource {
    PolicySource {
        kind,
        id: id.to_owned(),
        digest: sha256_digest(profile.as_bytes()),
    }
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}

struct HostEventFactory;

impl EventFactory for HostEventFactory {
    fn create_metadata(&mut self, state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        let sequence = state
            .journal_sequence
            .checked_add(1)
            .ok_or_else(|| EventFactoryError::new("sequence_overflow"))?;
        let recorded_at = now_rfc3339().map_err(|_| EventFactoryError::new("clock_failure"))?;
        Ok(EventMetadata {
            event_id: format!("event-{}-{sequence}", state.run_id),
            recorded_at,
        })
    }
}

fn now_rfc3339() -> Result<String, PublicRunError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| PublicRunError::Internal)
}

fn host_os() -> Result<OperatingSystem, PublicRunError> {
    match env::consts::OS {
        "linux" => Ok(OperatingSystem::Linux),
        "macos" => Ok(OperatingSystem::Macos),
        "windows" => Ok(OperatingSystem::Windows),
        _ => Err(PublicRunError::Configuration),
    }
}

fn host_arch() -> Result<Architecture, PublicRunError> {
    match env::consts::ARCH {
        "x86_64" => Ok(Architecture::X86_64),
        "aarch64" => Ok(Architecture::Aarch64),
        _ => Err(PublicRunError::Configuration),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;
    use xgeny_domain::{CriticalAction, ResourceSelector};
    use xgeny_policy::PermissionRequestResolver;

    use super::*;

    #[test]
    fn local_route_profile_gate_accepts_new_occurrences_and_legacy_resume_profiles() {
        assert!(local_profile_matches_effect_kind(
            true,
            PlannedExecutionProfile::LocalSyncOnceOccurrenceV1
        ));
        assert!(local_profile_matches_effect_kind(
            true,
            PlannedExecutionProfile::LocalSyncOnceV1
        ));
        assert!(local_profile_matches_effect_kind(
            false,
            PlannedExecutionProfile::LocalSyncReadOnlyOccurrenceV1
        ));
        assert!(local_profile_matches_effect_kind(
            false,
            PlannedExecutionProfile::LocalSyncReadOnlyV1
        ));
        assert!(!local_profile_matches_effect_kind(
            true,
            PlannedExecutionProfile::LocalSyncReadOnlyOccurrenceV1
        ));
        assert!(!local_profile_matches_effect_kind(
            false,
            PlannedExecutionProfile::LocalSyncOnceV1
        ));
    }

    #[test]
    fn no_egress_consent_returns_before_workspace_or_state_access() {
        let request = LocalRunRequest {
            goal: "read a file".to_owned(),
            workspace: PathBuf::from("/definitely/not/a/workspace"),
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            planner_id: DEFAULT_PLANNER_ID.to_owned(),
            model: "model".to_owned(),
            tokenizer: "tokenizer".to_owned(),
            allow_files: vec!["README.md".to_owned()],
            allow_dirs: Vec::new(),
            allow_executables: Vec::new(),
            allow_remote_model_egress: false,
            allow_read: true,
            allow_write: false,
            allow_execute: false,
            max_ticks: 32,
        };
        assert_eq!(
            run_local(request).unwrap(),
            LocalCommandResult::Paused {
                run_id: None,
                reason: PauseReason::RemoteModelEgressConsentRequired,
            }
        );
    }

    #[test]
    fn product_registry_is_bound_to_the_exact_workspace() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("README.md"), "fixture").unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        let workspace =
            WorkspaceRoot::open_ambient(directory.path(), WorkspaceId::new(WORKSPACE_ID).unwrap())
                .unwrap();
        let exact_catalog =
            LocalReadCatalog::build(&workspace, &["README.md".to_owned()], &[]).unwrap();
        let product = local_tool_product(&workspace, &exact_catalog, None).unwrap();
        assert_eq!(product.capabilities.definitions().count(), 1);
        assert!(
            product
                .capabilities
                .instance("local.fs.builtin.v1")
                .is_some()
        );
        assert_eq!(product.adapters.len(), 1);
        assert!(format!("{:?}", product.verifiers).contains("verifier_count: 1"));

        let discovery_catalog =
            LocalReadCatalog::build(&workspace, &[], &[".".to_owned()]).unwrap();
        let discovery = local_tool_product(&workspace, &discovery_catalog, None).unwrap();
        assert_eq!(discovery.capabilities.definitions().count(), 6);
        assert_eq!(discovery.capabilities.instances().count(), 6);
        assert_eq!(discovery.adapters.len(), 6);
        assert!(format!("{:?}", discovery.verifiers).contains("verifier_count: 6"));
        let scoped_catalog = LocalReadCatalog::build(&workspace, &[], &["src".to_owned()]).unwrap();
        let scoped = local_tool_product(&workspace, &scoped_catalog, None).unwrap();
        let root_definitions = discovery
            .capabilities
            .definitions()
            .cloned()
            .collect::<Vec<_>>();
        let scoped_definitions = scoped
            .capabilities
            .definitions()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(root_definitions, scoped_definitions);
        assert_eq!(
            local_execution_profile_digest(&workspace, &discovery_catalog, None).unwrap(),
            local_execution_profile_digest(&workspace, &scoped_catalog, None).unwrap()
        );
        assert_ne!(
            discovery_catalog
                .workspace_authorization()
                .unwrap()
                .catalog_digest(),
            scoped_catalog
                .workspace_authorization()
                .unwrap()
                .catalog_digest()
        );

        let executable = std::env::current_exe().unwrap();
        let process_specification = format!("test-helper={}", executable.display());
        let process =
            ProcessTooling::build(directory.path(), WORKSPACE_ID, [&process_specification])
                .unwrap()
                .unwrap();
        let process_product =
            local_tool_product(&workspace, &discovery_catalog, Some(&process)).unwrap();
        assert_eq!(process_product.capabilities.definitions().count(), 7);
        assert_eq!(process_product.capabilities.instances().count(), 7);
        assert_eq!(process_product.adapters.len(), 7);
        assert!(format!("{:?}", process_product.verifiers).contains("verifier_count: 7"));
        assert_eq!(
            filesystem_execution_profile_digest(&workspace, &discovery_catalog).unwrap(),
            local_execution_profile_digest(&workspace, &discovery_catalog, None).unwrap()
        );
        assert_ne!(
            local_execution_profile_digest(&workspace, &discovery_catalog, None).unwrap(),
            local_execution_profile_digest(&workspace, &discovery_catalog, Some(&process)).unwrap()
        );
    }

    #[test]
    fn read_flag_approves_only_the_exact_catalog_request_shape() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("README.md"), "fixture").unwrap();
        let workspace =
            WorkspaceRoot::open_ambient(directory.path(), WorkspaceId::new(WORKSPACE_ID).unwrap())
                .unwrap();
        let catalog = AllowFileCatalog::new(&workspace.resolver(), ["README.md"]).unwrap();
        let mut approval = ExplicitLocalApproval {
            allow_read: true,
            allow_write: false,
            allow_execute: false,
            run_id: "run-0123456789abcdef0123456789abcdef".to_owned(),
            catalog: ApprovalCatalog::ExactFiles(catalog),
            process: None,
        };
        let ProtocolDocument::CapabilityDefinition(definition) =
            serde_json::from_str(include_str!(
                "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
            ))
            .unwrap()
        else {
            panic!("fixture must be a CapabilityDefinition");
        };
        let definition = *definition;
        let resolver = PermissionRequestResolver::new(PassthroughResolver);
        let arguments = json!({"path": "workspace:primary/README.md"});

        let exact = resolver
            .resolve_invocation(
                "request-1",
                "run-0123456789abcdef0123456789abcdef",
                "step-1",
                &definition,
                &arguments,
                GrantLifetime::Once,
            )
            .unwrap();
        assert!(matches!(
            approval.decide(exact.permission_request()).unwrap(),
            ApprovalDecision::Approved(_)
        ));

        let wrong_lifetime = resolver
            .resolve_invocation(
                "request-2",
                "run-0123456789abcdef0123456789abcdef",
                "step-1",
                &definition,
                &arguments,
                GrantLifetime::Run,
            )
            .unwrap();
        assert!(matches!(
            approval
                .decide(wrong_lifetime.permission_request())
                .unwrap(),
            ApprovalDecision::Denied
        ));

        let mut extra_resource_definition = definition.clone();
        extra_resource_definition
            .spec
            .effect
            .resource_selectors
            .push(ResourceSelector {
                scope: "filesystem.metadata".to_owned(),
                argument_pointer: "/path".to_owned(),
            });
        let extra_resource = resolver
            .resolve_invocation(
                "request-3",
                "run-0123456789abcdef0123456789abcdef",
                "step-1",
                &extra_resource_definition,
                &arguments,
                GrantLifetime::Once,
            )
            .unwrap();
        assert!(matches!(
            approval
                .decide(extra_resource.permission_request())
                .unwrap(),
            ApprovalDecision::Denied
        ));

        let mut critical_definition = definition;
        critical_definition
            .spec
            .effect
            .critical_actions
            .push(CriticalAction::ExternalPublishOrMessage);
        let critical = resolver
            .resolve_invocation(
                "request-4",
                "run-0123456789abcdef0123456789abcdef",
                "step-1",
                &critical_definition,
                &arguments,
                GrantLifetime::Once,
            )
            .unwrap();
        assert!(matches!(
            approval.decide(critical.permission_request()).unwrap(),
            ApprovalDecision::Denied
        ));
    }

    #[test]
    fn execute_flag_approves_only_the_exact_process_catalog_request_shape() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("README.md"), "fixture").unwrap();
        let executable = std::env::current_exe().unwrap();
        let specification = format!("test-helper={}", executable.display());
        let process = ProcessTooling::build(directory.path(), WORKSPACE_ID, [&specification])
            .unwrap()
            .unwrap();
        let filesystem_workspace =
            WorkspaceRoot::open_ambient(directory.path(), WorkspaceId::new(WORKSPACE_ID).unwrap())
                .unwrap();
        let filesystem_catalog =
            AllowFileCatalog::new(&filesystem_workspace.resolver(), ["README.md"]).unwrap();
        let mut approval = ExplicitLocalApproval {
            allow_read: false,
            allow_write: false,
            allow_execute: false,
            run_id: "run-0123456789abcdef0123456789abcdef".to_owned(),
            catalog: ApprovalCatalog::ExactFiles(filesystem_catalog),
            process: Some(process.authorization().clone()),
        };
        let ProtocolDocument::CapabilityDefinition(definition) =
            serde_json::from_str(include_str!(
                "../../../protocol/fixtures/v1alpha1/valid/capability-definition.process-execute.json"
            ))
            .unwrap()
        else {
            panic!("fixture must be a CapabilityDefinition");
        };
        let resolver = PermissionRequestResolver::new(process.workspace().resolver());
        let arguments = json!({
            "executable": "test-helper",
            "args": [],
            "cwd": ".",
            "env": {},
            "timeoutMs": 30000,
            "maxOutputBytes": 32768
        });
        let exact = resolver
            .resolve_invocation(
                "request-1",
                "run-0123456789abcdef0123456789abcdef",
                "step-1",
                &definition,
                &arguments,
                GrantLifetime::Once,
            )
            .unwrap();
        assert!(matches!(
            approval.decide(exact.permission_request()).unwrap(),
            ApprovalDecision::Pending
        ));
        approval.allow_execute = true;
        assert!(matches!(
            approval.decide(exact.permission_request()).unwrap(),
            ApprovalDecision::Approved(_)
        ));

        let wrong_lifetime = resolver
            .resolve_invocation(
                "request-2",
                "run-0123456789abcdef0123456789abcdef",
                "step-1",
                &definition,
                &arguments,
                GrantLifetime::Run,
            )
            .unwrap();
        assert!(matches!(
            approval
                .decide(wrong_lifetime.permission_request())
                .unwrap(),
            ApprovalDecision::Denied
        ));
    }

    struct PassthroughResolver;

    impl ResourceResolver for PassthroughResolver {
        fn resolve(
            &self,
            _scope: &str,
            resource: &str,
        ) -> Result<String, ResourceResolutionFailure> {
            Ok(resource.to_owned())
        }
    }
}
