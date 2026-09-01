use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use jsonschema::Draft;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_domain::{
    Architecture, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef, Decision,
    EffectClass as DomainEffectClass, ExecutionStyle, Grant, GrantLifetime, OperatingSystem,
    Placement, PolicyDecisionBody, PolicySource, ProtocolDocument, ResolvedResource,
    VerificationStrategy,
};
use xgeny_local_store::{Commit, ExpectedHead, RunStore, StoreError};
use xgeny_policy::{
    BoundPolicyEvaluation, BrokerError, BrokerOutcome, InvocationResolutionError, PermissionBroker,
    PermissionRequestResolver, PolicyInputs, ResolvedPermissionRequest, ResourceResolver,
};
use xgeny_protocol::{
    CORE_RECEIPT_INPUT_SUMMARY_V1, CORE_RECEIPT_PROFILE_V1, CORE_RECEIPT_PROFILE_V2, ProtocolError,
    canonical_digest, validate_policy_decision,
};
use xgeny_workgraph::{
    AuthorizationBinding, AuthorizationDigestError, AuthorizationUse, DependencyBlockReason,
    EffectClass as WorkGraphEffectClass, EffectIntent, InvocationBinding, InvocationMaterialError,
    InvocationMaterialRecord, InvocationMaterialRetention, PlannedExecutionProfile,
    ReceiptPlacement, ReceiptProvenance, ReceiptVerificationRule, ReceiptVerificationStrategy,
    ReconstructableMaterialReference, RunEvent, RunEventBody, RunState, SinkGuarantee, StepStatus,
    authorization_digest, dependency_release_block_reason, invocation_material_digest,
    invocation_material_retention_digest, once_authorization_id, receipt_provenance_digest,
};

use crate::{
    CapabilityRegistry, CapabilityRouter, EventFactory, EventFactoryError, LOCAL_EXECUTOR_ID,
    RouteInputError, RouteOutcome, RouteRequest, RunLease, local_executor_platform,
};

const MAX_ARGUMENTS_SIZE_BYTES: usize = 1024 * 1024;
const ONCE_MAX_USES: u32 = 1;

/// Exact invocation material from which permission resources and semantic action identity are
/// derived. Arguments are intentionally omitted from `Debug` output.
pub struct AdmissionRequest {
    pub step_id: String,
    pub route: RouteRequest,
    pub arguments: Value,
}

impl fmt::Debug for AdmissionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmissionRequest")
            .field("step_id", &self.step_id)
            .field("route", &self.route)
            .field("arguments", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRunBinding {
    run_id: String,
    authority: String,
    authority_epoch: u64,
    journal_sequence: u64,
    journal_head_digest: String,
}

impl PendingRunBinding {
    fn from_state(state: &RunState) -> Self {
        Self {
            run_id: state.run_id.clone(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            journal_sequence: state.journal_sequence,
            journal_head_digest: state.journal_head_digest.clone(),
        }
    }

    fn matches(&self, state: &RunState) -> bool {
        self.run_id == state.run_id
            && self.authority == state.authority
            && self.authority_epoch == state.authority_epoch
            && self.journal_sequence == state.journal_sequence
            && self.journal_head_digest == state.journal_head_digest
    }
}

/// Prepared, model-free invocation whose permission request was derived from its exact arguments.
///
/// This value is policy input, not execution authority. It cannot be deserialized or cloned and
/// must be consumed by [`InvocationAdmission::authorize_and_commit`].
#[must_use = "a pending invocation must be authorized and committed or explicitly discarded"]
pub struct PendingInvocation {
    run_binding: PendingRunBinding,
    route: RouteRequest,
    normalized_arguments: Value,
    permission_request: ResolvedPermissionRequest,
    definition_digest: String,
    action_digest: String,
    material_digest: String,
    material_retention: InvocationMaterialRetention,
    plan_id: Option<String>,
}

impl PendingInvocation {
    /// Exact host-derived request to use when trusted policy layers build their contributions.
    #[must_use]
    pub const fn permission_request(&self) -> &ResolvedPermissionRequest {
        &self.permission_request
    }

    /// Instance-independent semantic action identity used for replay convergence.
    #[must_use]
    pub fn action_digest(&self) -> &str {
        &self.action_digest
    }

    /// Exact immutable Capability Definition identity used during preparation.
    #[must_use]
    pub fn definition_digest(&self) -> &str {
        &self.definition_digest
    }

    /// Canonical normalized-argument identity used by durable material bindings.
    #[must_use]
    pub fn material_digest(&self) -> &str {
        &self.material_digest
    }

    /// Select a secret-free, immutable recipe reference for restart reconstruction.
    ///
    /// The host creates the typed reference. Models and adapters cannot supply raw paths, URLs,
    /// credentials, or bearer tokens through this API. A durably planned invocation is already
    /// pinned to its atomically committed plan-input reference, so this method cannot replace it.
    pub fn with_reconstructable_material(
        mut self,
        reference: ReconstructableMaterialReference,
    ) -> Self {
        if self.plan_id.is_none() {
            self.material_retention =
                InvocationMaterialRetention::ReconstructableReference(reference);
        }
        self
    }
}

impl fmt::Debug for PendingInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingInvocation")
            .field("run_binding", &self.run_binding)
            .field("route", &self.route)
            .field("normalized_arguments", &"<redacted>")
            .field("permission_request", &"<resolved/redacted>")
            .field("definition_digest", &self.definition_digest)
            .field("action_digest", &self.action_digest)
            .field("material_digest", &self.material_digest)
            .field("material_retention", &self.material_retention)
            .field("plan_id", &self.plan_id)
            .finish()
    }
}

/// Successfully admitted and durably committed invocation material for a trusted adapter.
///
/// Raw arguments remain ephemeral and are never written to the Run journal by this type.
#[must_use = "an admitted effect must be prepared by its selected trusted adapter or discarded"]
pub struct AdmittedEffect {
    normalized_arguments: Value,
    capability: CapabilityRef,
    selected_instance_id: String,
    definition_digest: String,
    instance_binding_digest: String,
    action_digest: String,
    effect_id: String,
    material_record: InvocationMaterialRecord,
    commit: Commit,
}

impl AdmittedEffect {
    #[must_use]
    pub(crate) const fn normalized_arguments(&self) -> &Value {
        &self.normalized_arguments
    }

    #[must_use]
    pub const fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    #[must_use]
    pub fn selected_instance_id(&self) -> &str {
        &self.selected_instance_id
    }

    #[must_use]
    pub fn definition_digest(&self) -> &str {
        &self.definition_digest
    }

    #[must_use]
    pub fn instance_binding_digest(&self) -> &str {
        &self.instance_binding_digest
    }

    #[must_use]
    pub fn action_digest(&self) -> &str {
        &self.action_digest
    }

    #[must_use]
    pub fn effect_id(&self) -> &str {
        &self.effect_id
    }

    #[must_use]
    pub const fn material_record(&self) -> &InvocationMaterialRecord {
        &self.material_record
    }

    #[must_use]
    pub const fn commit(&self) -> &Commit {
        &self.commit
    }
}

impl fmt::Debug for AdmittedEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedEffect")
            .field("normalized_arguments", &"<redacted>")
            .field("capability", &self.capability)
            .field("selected_instance_id", &self.selected_instance_id)
            .field("definition_digest", &self.definition_digest)
            .field("instance_binding_digest", &self.instance_binding_digest)
            .field("action_digest", &self.action_digest)
            .field("effect_id", &self.effect_id)
            .field("material_record", &self.material_record)
            .field("journal_sequence", &self.commit.state.journal_sequence)
            .finish()
    }
}

/// Admission either commits one exact effect or returns the Router's auditable non-selection.
#[must_use = "an admission outcome must be handled before any execution is attempted"]
#[derive(Debug)]
pub enum AdmissionOutcome {
    Authorized(Box<AdmittedEffect>),
    NotAuthorized(RouteOutcome),
}

/// Two-phase, I/O-free policy preparation followed by a single durable authorization commit.
#[derive(Debug, Default, Clone, Copy)]
pub struct InvocationAdmission;

impl InvocationAdmission {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Load the verified Run state and derive policy resources from exact invocation arguments.
    ///
    /// # Errors
    ///
    /// Fails closed for a missing or stale Run/Step, unsupported effect semantics, invalid input
    /// schema, oversized arguments, or resource resolution failure.
    pub fn prepare<S, R, L>(
        &self,
        store: &S,
        lease: &L,
        registry: &CapabilityRegistry,
        resolver: &R,
        request: AdmissionRequest,
    ) -> Result<PendingInvocation, AdmissionError>
    where
        S: RunStore,
        R: ResourceResolver,
        L: RunLease,
    {
        let state = store
            .load_current()?
            .ok_or(AdmissionError::RunNotInitialized)?;
        verify_lease(lease, &state)?;
        require_admission_ready_step(&state, &request.step_id)?;
        verify_unplanned_read_only_boundary(
            &state,
            &request.step_id,
            &request.route.capability,
            registry,
        )?;
        verify_planned_route_binding(&state, &request.step_id, &request.route)?;
        verify_planned_definition_binding(&state, &request.step_id, registry)?;
        let planned_input = verify_planned_input_sidecar(store, &state, &request.step_id)?;

        let facts = prepare_invocation_facts(
            &state.run_id,
            &request.step_id,
            &request.route.capability,
            &request.arguments,
            registry,
            resolver,
        )?;
        let plan_id = verify_planned_admission_binding(
            &state,
            &request,
            &facts.definition_digest,
            &facts.action_digest,
            &facts.material_digest,
        )?;
        let material_retention = match (&plan_id, planned_input.as_ref()) {
            (Some(_), Some(input)) => {
                InvocationMaterialRetention::ReconstructableReference(input.reference().clone())
            }
            (None, None) => InvocationMaterialRetention::Ephemeral,
            _ => {
                return Err(AdmissionError::PlannedInvocationMismatch {
                    step_id: request.step_id.clone(),
                    field: "plan_input_sidecar",
                });
            }
        };

        Ok(PendingInvocation {
            run_binding: PendingRunBinding::from_state(&state),
            route: request.route,
            normalized_arguments: facts.normalized_arguments,
            permission_request: facts.permission_request,
            definition_digest: facts.definition_digest,
            action_digest: facts.action_digest,
            material_digest: facts.material_digest,
            material_retention,
            plan_id,
        })
    }

    /// Evaluate the exact prepared request, rerun deterministic routing, issue a one-shot binding,
    /// and atomically commit its `EffectIntent` plus use budget.
    ///
    /// # Errors
    ///
    /// Fails closed if the Run head, Definition, policy mode, Router result, or authorization
    /// binding changed since preparation. `Ask` and `Deny` are returned as `NotAuthorized` and do
    /// not append an event.
    pub fn authorize_and_commit<S, F, L>(
        &self,
        pending: PendingInvocation,
        policy_inputs: &PolicyInputs,
        registry: &CapabilityRegistry,
        store: &mut S,
        events: &mut F,
        lease: &L,
    ) -> Result<AdmissionOutcome, AdmissionError>
    where
        S: RunStore,
        F: EventFactory,
        L: RunLease,
    {
        if policy_inputs.is_managed() {
            return Err(AdmissionError::ManagedPolicyUnsupported);
        }
        let state = store
            .load_current()?
            .ok_or(AdmissionError::RunNotInitialized)?;
        verify_lease(lease, &state)?;
        if !pending.run_binding.matches(&state) {
            return Err(AdmissionError::RunHeadChanged);
        }
        let step_id = pending.permission_request.step_id();
        require_admission_ready_step(&state, step_id)?;
        let planned_input = verify_planned_input_sidecar(store, &state, step_id)?;
        verify_pending_planned_material(&state, step_id, &pending, planned_input.as_ref())?;

        let definition = registry
            .definition(&pending.route.capability)
            .ok_or_else(|| AdmissionError::DefinitionNotFound {
                capability_id: pending.route.capability.capability_id.clone(),
                contract_version: pending.route.capability.contract_version.clone(),
            })?;
        if definition_contract_digest(definition)? != pending.definition_digest {
            return Err(AdmissionError::DefinitionChanged);
        }

        let evaluation =
            PermissionBroker::new().evaluate_bound(&pending.permission_request, policy_inputs)?;
        let route = CapabilityRouter::new().route(registry, &pending.route, Some(&evaluation))?;
        let selected_instance_id = match &route {
            RouteOutcome::Selected {
                selected_instance_id,
                ..
            } => selected_instance_id.clone(),
            RouteOutcome::InteractionRequired { .. } | RouteOutcome::Blocked { .. } => {
                return Ok(AdmissionOutcome::NotAuthorized(route));
            }
        };
        let instance = registry.instance(&selected_instance_id).ok_or_else(|| {
            AdmissionError::SelectedInstanceMissing {
                instance_id: selected_instance_id.clone(),
            }
        })?;
        if instance.placement != Placement::Local {
            return Err(AdmissionError::UnsupportedExecutorPlacement {
                placement: instance.placement,
            });
        }

        let metadata = events.create_metadata(&state)?;
        metadata.validate()?;
        let issued = issue_once_effect(
            &pending,
            &state,
            definition,
            instance,
            &selected_instance_id,
            &evaluation,
            &metadata.recorded_at,
        )?;
        let event = RunEvent {
            event_id: metadata.event_id,
            run_id: state.run_id.clone(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            recorded_at: metadata.recorded_at,
            body: RunEventBody::EffectIntentCommitted {
                step_id: pending.permission_request.step_id().to_owned(),
                intent: Box::new(issued.intent),
            },
        };
        let material_record = issued.material_record.clone();
        let commit = store.append_with_invocation_material(
            ExpectedHead::from_state(&state),
            event,
            issued.material_record,
        )?;

        Ok(AdmissionOutcome::Authorized(Box::new(AdmittedEffect {
            normalized_arguments: pending.normalized_arguments,
            capability: pending.route.capability,
            selected_instance_id,
            definition_digest: pending.definition_digest,
            instance_binding_digest: issued.instance_binding_digest,
            action_digest: pending.action_digest,
            effect_id: issued.effect_id,
            material_record,
            commit,
        })))
    }
}

fn verify_unplanned_read_only_boundary(
    state: &RunState,
    step_id: &str,
    capability: &CapabilityRef,
    registry: &CapabilityRegistry,
) -> Result<(), AdmissionError> {
    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| AdmissionError::StepNotFound(step_id.to_owned()))?;
    if step.planned_invocation.is_some() {
        return Ok(());
    }
    let definition =
        registry
            .definition(capability)
            .ok_or_else(|| AdmissionError::DefinitionNotFound {
                capability_id: capability.capability_id.clone(),
                contract_version: capability.contract_version.clone(),
            })?;
    if definition.spec.effect.class == DomainEffectClass::ReadOnly {
        return Err(AdmissionError::UnplannedReadOnlyUnsupported);
    }
    Ok(())
}

pub(crate) struct PreparedInvocationFacts {
    pub(crate) normalized_arguments: Value,
    pub(crate) permission_request: ResolvedPermissionRequest,
    pub(crate) definition_digest: String,
    pub(crate) action_digest: String,
    pub(crate) material_digest: String,
}

impl fmt::Debug for PreparedInvocationFacts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedInvocationFacts")
            .field("normalized_arguments", &"<redacted>")
            .field("permission_request", &"<resolved/redacted>")
            .field("definition_digest", &self.definition_digest)
            .field("action_digest", &self.action_digest)
            .field("material_digest", &self.material_digest)
            .finish()
    }
}

/// Normalize and bind invocation facts without policy, routing, persistence, or execution.
///
/// Planning callers must supply a deterministic, side-effect-free `ResourceResolver`: no network
/// access, credential dereference, filesystem mutation, or permission change is allowed here.
/// Admission repeats the same derivation from reconstructed input before authority is issued.
pub(crate) fn prepare_invocation_facts<R: ResourceResolver>(
    run_id: &str,
    step_id: &str,
    capability: &CapabilityRef,
    arguments: &Value,
    registry: &CapabilityRegistry,
    resolver: &R,
) -> Result<PreparedInvocationFacts, AdmissionError> {
    let definition =
        registry
            .definition(capability)
            .ok_or_else(|| AdmissionError::DefinitionNotFound {
                capability_id: capability.capability_id.clone(),
                contract_version: capability.contract_version.clone(),
            })?;
    map_effect_class(definition.spec.effect.class)?;
    if definition.spec.effect.class != DomainEffectClass::ReadOnly
        && !definition.spec.execution.idempotency_key_supported
    {
        return Err(AdmissionError::DefinitionDoesNotSupportIdempotencyKey);
    }
    validate_arguments(definition, arguments)?;

    let request_identity = digest_serializable(&RequestIdentityDigestInput {
        domain: "xgeny.permission-request/v1",
        run_id,
        step_id,
        capability,
        arguments,
    })?;
    let request_id = content_id("permission", &request_identity);
    let derived_request = PermissionRequestResolver::new(resolver).resolve_invocation(
        &request_id,
        run_id,
        step_id,
        definition,
        arguments,
        GrantLifetime::Once,
    )?;
    validate_arguments(definition, derived_request.normalized_arguments())?;

    let definition_digest = definition_contract_digest(definition)?;
    let action_digest = semantic_action_digest(
        capability,
        &definition_digest,
        definition.spec.effect.class,
        derived_request.normalized_arguments(),
        derived_request.permission_request(),
    )?;
    let material_digest = invocation_material_digest(derived_request.normalized_arguments())?;
    Ok(PreparedInvocationFacts {
        normalized_arguments: derived_request.normalized_arguments().clone(),
        permission_request: derived_request.permission_request().clone(),
        definition_digest,
        action_digest,
        material_digest,
    })
}

struct IssuedEffect {
    intent: EffectIntent,
    instance_binding_digest: String,
    effect_id: String,
    material_record: InvocationMaterialRecord,
}

#[allow(clippy::too_many_lines)] // Keep the authorization and Receipt digest bindings adjacent.
fn issue_once_effect(
    pending: &PendingInvocation,
    state: &RunState,
    definition: &CapabilityDefinitionBody,
    instance: &CapabilityInstanceBody,
    selected_instance_id: &str,
    evaluation: &BoundPolicyEvaluation,
    decided_at: &str,
) -> Result<IssuedEffect, AdmissionError> {
    let BrokerOutcome::Allow {
        provisional_authorization,
        sources,
    } = evaluation.outcome()
    else {
        return Err(AdmissionError::SelectedRouteWithoutPolicyAllow);
    };
    if provisional_authorization.lifetime() != GrantLifetime::Once
        || pending.permission_request.requested_lifetime() != GrantLifetime::Once
    {
        return Err(AdmissionError::NonOnceAuthorization);
    }
    if !provisional_authorization.critical_actions().is_empty() {
        return Err(AdmissionError::CriticalAuthorizationUnsupported);
    }

    let effect_identity_digest = digest_serializable(&EffectIdentityDigestInput {
        domain: "xgeny.effect.once/v1",
        run_id: &state.run_id,
        action_digest: &pending.action_digest,
    })?;
    let policy_evidence_digest = policy_evidence_digest(
        &pending.permission_request,
        provisional_authorization,
        sources,
    )?;
    let (policy_decision_id, policy_decision_digest) = policy_decision_commitment(
        &pending.permission_request,
        provisional_authorization,
        sources,
        &policy_evidence_digest,
        decided_at,
    )?;
    let receipt_provenance = build_receipt_provenance(
        &effect_identity_digest,
        pending.plan_id.as_deref(),
        policy_decision_id,
        policy_decision_digest,
        instance,
        definition,
    );
    let receipt_provenance_digest = receipt_provenance_digest(&receipt_provenance)?;
    let instance_binding_digest = executable_binding_digest(instance)?;
    let invocation = InvocationBinding {
        capability_id: pending.route.capability.capability_id.clone(),
        contract_version: pending.route.capability.contract_version.clone(),
        definition_digest: pending.definition_digest.clone(),
        instance_id: selected_instance_id.to_owned(),
        instance_binding_digest: instance_binding_digest.clone(),
    };
    let binding = AuthorizationBinding {
        run_id: state.run_id.clone(),
        step_id: pending.permission_request.step_id().to_owned(),
        authority: state.authority.clone(),
        authority_epoch: state.authority_epoch,
        issued_at_sequence: state.journal_sequence,
        issued_at_head_digest: state.journal_head_digest.clone(),
        capability_id: invocation.capability_id.clone(),
        contract_version: invocation.contract_version.clone(),
        definition_digest: invocation.definition_digest.clone(),
        instance_id: invocation.instance_id.clone(),
        instance_binding_digest: invocation.instance_binding_digest.clone(),
        action_digest: pending.action_digest.clone(),
        material_digest: pending.material_digest.clone(),
        material_retention_digest: invocation_material_retention_digest(
            &pending.material_retention,
        )?,
        policy_evidence_digest,
        receipt_provenance_digest: Some(receipt_provenance_digest),
    };
    let grant_digest = authorization_digest(&binding, ONCE_MAX_USES)?;
    let effect_id = content_id("effect", &effect_identity_digest);
    let effect_class = map_effect_class(definition.spec.effect.class)?;
    let intent = EffectIntent {
        effect_id: effect_id.clone(),
        action_digest: pending.action_digest.clone(),
        invocation,
        effect_class,
        idempotency_key: (effect_class != WorkGraphEffectClass::ReadOnly)
            .then(|| content_id("xgeny", &effect_identity_digest)),
        sink_guarantee: SinkGuarantee::None,
        authorization: AuthorizationUse {
            grant_id: once_authorization_id(&state.run_id, &pending.action_digest)?,
            grant_digest,
            max_uses: ONCE_MAX_USES,
            binding,
        },
        receipt_provenance: Some(receipt_provenance),
    };
    let material_record = InvocationMaterialRecord::new(
        &state.run_id,
        pending.permission_request.step_id(),
        &intent,
        &pending.material_digest,
        pending.material_retention.clone(),
    )?;

    Ok(IssuedEffect {
        intent,
        instance_binding_digest,
        effect_id,
        material_record,
    })
}

fn build_receipt_provenance(
    effect_identity_digest: &str,
    planned_plan_id: Option<&str>,
    policy_decision_id: String,
    policy_decision_digest: String,
    instance: &CapabilityInstanceBody,
    definition: &CapabilityDefinitionBody,
) -> ReceiptProvenance {
    let durable_tool_output = definition.spec.effect.class == DomainEffectClass::ReadOnly
        || (matches!(
            definition.spec.effect.class,
            DomainEffectClass::Idempotent | DomainEffectClass::NonIdempotent
        ) && definition.spec.execution.durable_tool_output);
    ReceiptProvenance {
        profile_version: if durable_tool_output {
            CORE_RECEIPT_PROFILE_V2
        } else {
            CORE_RECEIPT_PROFILE_V1
        }
        .to_owned(),
        tool_output_profile: durable_tool_output
            .then(|| xgeny_workgraph::TOOL_OUTPUT_PROFILE_V1.to_owned()),
        invocation_id: content_id("invocation", effect_identity_digest),
        plan_id: planned_plan_id
            .map_or_else(|| content_id("plan", effect_identity_digest), str::to_owned),
        policy_decision_id,
        policy_decision_digest,
        executor_id: LOCAL_EXECUTOR_ID.to_owned(),
        executor_placement: receipt_placement(instance.placement),
        executor_platform: local_executor_platform(),
        input_summary: CORE_RECEIPT_INPUT_SUMMARY_V1.to_owned(),
        verification_plan: definition
            .spec
            .verification
            .iter()
            .map(|rule| ReceiptVerificationRule {
                strategy: receipt_verification_strategy(rule.strategy),
                required: rule.required,
            })
            .collect(),
    }
}

fn policy_decision_commitment(
    request: &ResolvedPermissionRequest,
    authorization: &xgeny_policy::ProvisionalAuthorization,
    sources: &[PolicySource],
    evidence_digest: &str,
    decided_at: &str,
) -> Result<(String, String), AdmissionError> {
    let decision_id = content_id("decision", evidence_digest);
    let resources = authorization
        .resources()
        .iter()
        .map(|resource| ResolvedResource {
            scope: resource.scope().to_owned(),
            resource: resource.canonical_resource().to_owned(),
            normalized: true,
            metadata: BTreeMap::new(),
        })
        .collect();
    let decision = PolicyDecisionBody {
        api_version: request.api_version().to_owned(),
        extensions: BTreeMap::new(),
        required_extensions: Vec::new(),
        decision_id: decision_id.clone(),
        request_id: request.request_id().to_owned(),
        decision: Decision::Allow,
        policy_sources: sources.to_vec(),
        grant: Some(Grant {
            lifetime: authorization.lifetime(),
            expires_at: None,
            scopes: authorization.scopes().to_vec(),
            resources,
            critical_actions: authorization.critical_actions().to_vec(),
        }),
        interaction_id: None,
        deny_reasons: Vec::new(),
        decided_at: decided_at.to_owned(),
    };
    validate_policy_decision(&decision).map_err(|_| AdmissionError::PolicyDecisionInvalid)?;
    let value = serde_json::to_value(ProtocolDocument::PolicyDecision(Box::new(decision)))?;
    Ok((decision_id, canonical_digest(&value)?))
}

const fn receipt_placement(placement: Placement) -> ReceiptPlacement {
    match placement {
        Placement::Local => ReceiptPlacement::Local,
        Placement::Device => ReceiptPlacement::Device,
        Placement::Remote => ReceiptPlacement::Remote,
    }
}

const fn receipt_verification_strategy(
    strategy: VerificationStrategy,
) -> ReceiptVerificationStrategy {
    match strategy {
        VerificationStrategy::OutputSchema => ReceiptVerificationStrategy::OutputSchema,
        VerificationStrategy::Postcondition => ReceiptVerificationStrategy::Postcondition,
        VerificationStrategy::ArtifactDigest => ReceiptVerificationStrategy::ArtifactDigest,
        VerificationStrategy::Receipt => ReceiptVerificationStrategy::Receipt,
        VerificationStrategy::Human => ReceiptVerificationStrategy::Human,
    }
}

fn verify_lease<L: RunLease>(lease: &L, state: &RunState) -> Result<(), AdmissionError> {
    if lease.run_id() != state.run_id {
        return Err(AdmissionError::LeaseRunMismatch {
            lease_run_id: lease.run_id().to_owned(),
            state_run_id: state.run_id.clone(),
        });
    }
    Ok(())
}

pub(crate) fn require_admission_ready_step(
    state: &RunState,
    step_id: &str,
) -> Result<(), AdmissionError> {
    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| AdmissionError::StepNotFound(step_id.to_owned()))?;
    if step.status != StepStatus::Planned {
        return Err(AdmissionError::StepNotPlanned {
            step_id: step_id.to_owned(),
            actual: step.status,
        });
    }
    let mut first_blocker: Option<(&str, DependencyBlockReason)> = None;
    for dependency_id in &step.depends_on {
        let dependency = state.steps.get(dependency_id).ok_or_else(|| {
            AdmissionError::StepDependencyUnknown {
                step_id: step_id.to_owned(),
                dependency_id: dependency_id.clone(),
            }
        })?;
        if let Some(reason) = dependency_release_block_reason(dependency) {
            let candidate = (dependency_id.as_str(), reason);
            if first_blocker
                .as_ref()
                .is_none_or(|current| candidate.0 < current.0)
            {
                first_blocker = Some(candidate);
            }
        }
    }
    if let Some((dependency_id, reason)) = first_blocker {
        return Err(AdmissionError::StepDependencyNotReleased {
            step_id: step_id.to_owned(),
            dependency_id: dependency_id.to_owned(),
            reason,
        });
    }
    Ok(())
}

fn verify_planned_input_sidecar<S: RunStore>(
    store: &S,
    state: &RunState,
    step_id: &str,
) -> Result<Option<xgeny_workgraph::PlannedInvocationMaterialRecord>, AdmissionError> {
    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| AdmissionError::StepNotFound(step_id.to_owned()))?;
    let Some(binding) = &step.planned_invocation else {
        return Ok(None);
    };
    let input = store
        .load_planned_invocation(step_id)?
        .ok_or_else(|| AdmissionError::PlannedInputMissing(step_id.to_owned()))?;
    input.verify_for(&state.run_id, step_id, binding)?;
    Ok(Some(input))
}

fn verify_pending_planned_material(
    state: &RunState,
    step_id: &str,
    pending: &PendingInvocation,
    input: Option<&xgeny_workgraph::PlannedInvocationMaterialRecord>,
) -> Result<(), AdmissionError> {
    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| AdmissionError::StepNotFound(step_id.to_owned()))?;
    let Some(planned) = &step.planned_invocation else {
        if pending.plan_id.is_some() || input.is_some() {
            return Err(AdmissionError::PlannedInvocationMismatch {
                step_id: step_id.to_owned(),
                field: "plan_input_sidecar",
            });
        }
        return Ok(());
    };
    if pending.plan_id.as_deref() != Some(planned.plan_id()) {
        return Err(AdmissionError::PlannedInvocationMismatch {
            step_id: step_id.to_owned(),
            field: "plan_id",
        });
    }
    let input = input.ok_or_else(|| AdmissionError::PlannedInputMissing(step_id.to_owned()))?;
    if !matches!(
        &pending.material_retention,
        InvocationMaterialRetention::ReconstructableReference(reference)
            if reference == input.reference()
    ) {
        return Err(AdmissionError::PlannedInvocationMismatch {
            step_id: step_id.to_owned(),
            field: "material_retention.reference",
        });
    }
    Ok(())
}

fn verify_planned_admission_binding(
    state: &RunState,
    request: &AdmissionRequest,
    definition_digest: &str,
    action_digest: &str,
    material_digest: &str,
) -> Result<Option<String>, AdmissionError> {
    verify_planned_route_binding(state, &request.step_id, &request.route)?;
    let step = state
        .steps
        .get(&request.step_id)
        .ok_or_else(|| AdmissionError::StepNotFound(request.step_id.clone()))?;
    let Some(planned) = &step.planned_invocation else {
        return Ok(None);
    };
    let checks = [
        (
            "definition_digest",
            planned.definition_digest() == definition_digest,
        ),
        ("action_digest", planned.action_digest() == action_digest),
        (
            "plan_input_digest",
            planned.plan_input_digest() == material_digest,
        ),
    ];
    if let Some((field, _)) = checks.into_iter().find(|(_, matches)| !matches) {
        return Err(AdmissionError::PlannedInvocationMismatch {
            step_id: request.step_id.clone(),
            field,
        });
    }
    Ok(Some(planned.plan_id().to_owned()))
}

pub(crate) fn verify_planned_route_binding(
    state: &RunState,
    step_id: &str,
    route: &RouteRequest,
) -> Result<(), AdmissionError> {
    if route.required_features.execution_style != ExecutionStyle::Sync {
        return Err(AdmissionError::UnsupportedExecutionStyle);
    }
    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| AdmissionError::StepNotFound(step_id.to_owned()))?;
    let Some(planned) = &step.planned_invocation else {
        if !route.required_features.idempotency_key {
            return Err(AdmissionError::IdempotencyKeyFeatureRequired);
        }
        return Ok(());
    };
    let idempotency_feature_matches = match planned.execution_profile() {
        PlannedExecutionProfile::LocalSyncOnceV1 => route.required_features.idempotency_key,
        PlannedExecutionProfile::LocalSyncReadOnlyV1 => !route.required_features.idempotency_key,
    };
    let checks = [
        (
            "capability_id",
            planned.capability_id() == route.capability.capability_id,
        ),
        (
            "contract_version",
            planned.contract_version() == route.capability.contract_version,
        ),
        ("execution_profile", idempotency_feature_matches),
        (
            "target_os",
            planned.target_os() == operating_system_name(route.target_platform.os),
        ),
        (
            "target_arch",
            planned.target_arch() == architecture_name(route.target_platform.arch),
        ),
    ];
    if let Some((field, _)) = checks.into_iter().find(|(_, matches)| !matches) {
        return Err(AdmissionError::PlannedInvocationMismatch {
            step_id: step_id.to_owned(),
            field,
        });
    }
    Ok(())
}

pub(crate) fn verify_planned_definition_binding(
    state: &RunState,
    step_id: &str,
    registry: &CapabilityRegistry,
) -> Result<(), AdmissionError> {
    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| AdmissionError::StepNotFound(step_id.to_owned()))?;
    let Some(planned) = &step.planned_invocation else {
        return Ok(());
    };
    let capability = CapabilityRef {
        capability_id: planned.capability_id().to_owned(),
        contract_version: planned.contract_version().to_owned(),
    };
    let definition =
        registry
            .definition(&capability)
            .ok_or_else(|| AdmissionError::DefinitionNotFound {
                capability_id: capability.capability_id,
                contract_version: capability.contract_version,
            })?;
    if definition_contract_digest(definition)? != planned.definition_digest() {
        return Err(AdmissionError::DefinitionChanged);
    }
    let profile_matches = matches!(
        (planned.execution_profile(), definition.spec.effect.class),
        (
            PlannedExecutionProfile::LocalSyncReadOnlyV1,
            DomainEffectClass::ReadOnly
        ) | (
            PlannedExecutionProfile::LocalSyncOnceV1,
            DomainEffectClass::Idempotent | DomainEffectClass::NonIdempotent
        )
    );
    if !profile_matches {
        return Err(AdmissionError::PlannedInvocationMismatch {
            step_id: step_id.to_owned(),
            field: "execution_profile",
        });
    }
    Ok(())
}

const fn operating_system_name(os: OperatingSystem) -> &'static str {
    match os {
        OperatingSystem::Linux => "linux",
        OperatingSystem::Macos => "macos",
        OperatingSystem::Windows => "windows",
        OperatingSystem::Any => "any",
    }
}

const fn architecture_name(architecture: Architecture) -> &'static str {
    match architecture {
        Architecture::X86_64 => "x86_64",
        Architecture::Aarch64 => "aarch64",
        Architecture::Any => "any",
    }
}

pub(crate) fn validate_arguments(
    definition: &CapabilityDefinitionBody,
    arguments: &Value,
) -> Result<(), AdmissionError> {
    validate_argument_size(arguments)?;
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .offline()
        .should_validate_formats(true)
        .build(&definition.spec.input_schema)
        .map_err(|_| AdmissionError::DefinitionInputSchemaInvalid)?;
    if !validator.is_valid(arguments) {
        return Err(AdmissionError::ArgumentsDoNotConform);
    }
    Ok(())
}

fn validate_argument_size(arguments: &Value) -> Result<(), AdmissionError> {
    let size = serde_jcs::to_vec(arguments)
        .map_err(|error| AdmissionError::Canonicalization(error.to_string()))?
        .len();
    if size > MAX_ARGUMENTS_SIZE_BYTES {
        return Err(AdmissionError::ArgumentsTooLarge {
            actual: size,
            maximum: MAX_ARGUMENTS_SIZE_BYTES,
        });
    }
    Ok(())
}

const fn map_effect_class(
    effect_class: DomainEffectClass,
) -> Result<WorkGraphEffectClass, AdmissionError> {
    match effect_class {
        DomainEffectClass::ReadOnly => Ok(WorkGraphEffectClass::ReadOnly),
        DomainEffectClass::Idempotent => Ok(WorkGraphEffectClass::Idempotent),
        DomainEffectClass::NonIdempotent => Ok(WorkGraphEffectClass::NonIdempotent),
        DomainEffectClass::Compensatable | DomainEffectClass::Unknown => {
            Err(AdmissionError::UnsupportedEffectClass { effect_class })
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestIdentityDigestInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    step_id: &'a str,
    capability: &'a CapabilityRef,
    arguments: &'a Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefinitionContractDigestInput<'a> {
    domain: &'static str,
    capability_id: &'a str,
    contract_version: &'a str,
    spec: &'a xgeny_domain::CapabilitySpec,
    required_extensions: &'a [String],
}

pub(crate) fn definition_contract_digest(
    definition: &CapabilityDefinitionBody,
) -> Result<String, AdmissionError> {
    digest_serializable(&DefinitionContractDigestInput {
        domain: "xgeny.capability-contract/v1",
        capability_id: &definition.metadata.id,
        contract_version: &definition.metadata.contract_version,
        spec: &definition.spec,
        required_extensions: &definition.required_extensions,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalResourceDigestInput<'a> {
    scope: &'a str,
    resource: &'a str,
}

fn resource_digest_inputs(
    request: &ResolvedPermissionRequest,
) -> Vec<CanonicalResourceDigestInput<'_>> {
    request
        .resources()
        .iter()
        .map(|resource| CanonicalResourceDigestInput {
            scope: resource.scope(),
            resource: resource.canonical_resource(),
        })
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SemanticActionDigestInput<'a> {
    domain: &'static str,
    capability: &'a CapabilityRef,
    definition_digest: &'a str,
    effect_class: DomainEffectClass,
    arguments: &'a Value,
    resources: Vec<CanonicalResourceDigestInput<'a>>,
}

pub(crate) fn semantic_action_digest(
    capability: &CapabilityRef,
    definition_digest: &str,
    effect_class: DomainEffectClass,
    arguments: &Value,
    request: &ResolvedPermissionRequest,
) -> Result<String, AdmissionError> {
    digest_serializable(&SemanticActionDigestInput {
        domain: "xgeny.semantic-action/v1",
        capability,
        definition_digest,
        effect_class,
        arguments,
        resources: resource_digest_inputs(request),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutableBindingDigestInput<'a> {
    domain: &'static str,
    instance_id: &'a str,
    definition: &'a CapabilityRef,
    source: xgeny_domain::CapabilitySource,
    placement: xgeny_domain::Placement,
    platform: &'a xgeny_domain::Platform,
    trust: xgeny_domain::TrustLevel,
    data_boundary: xgeny_domain::DataBoundary,
    features: &'a xgeny_domain::InstanceFeatures,
    binding: &'a xgeny_domain::InstanceBinding,
    auth_ref: Option<&'a str>,
    required_extensions: &'a [String],
}

pub(crate) fn executable_binding_digest(
    instance: &CapabilityInstanceBody,
) -> Result<String, AdmissionError> {
    digest_serializable(&ExecutableBindingDigestInput {
        domain: "xgeny.executable-binding/v2",
        instance_id: &instance.instance_id,
        definition: &instance.definition,
        source: instance.source,
        placement: instance.placement,
        platform: &instance.platform,
        trust: instance.trust,
        data_boundary: instance.data_boundary,
        features: &instance.features,
        binding: &instance.binding,
        auth_ref: instance.auth.auth_ref.as_deref(),
        required_extensions: &instance.required_extensions,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PolicyEvidenceDigestInput<'a> {
    domain: &'static str,
    api_version: &'a str,
    request_id: &'a str,
    run_id: &'a str,
    step_id: &'a str,
    capability: &'a CapabilityRef,
    effect_class: DomainEffectClass,
    scopes: &'a [String],
    resources: Vec<CanonicalResourceDigestInput<'a>>,
    critical_actions: &'a [xgeny_domain::CriticalAction],
    lifetime: GrantLifetime,
    sources: &'a [PolicySource],
}

fn policy_evidence_digest(
    request: &ResolvedPermissionRequest,
    provisional: &xgeny_policy::ProvisionalAuthorization,
    sources: &[PolicySource],
) -> Result<String, AdmissionError> {
    if provisional.scopes() != request.requested_scopes()
        || provisional.resources() != request.resources()
        || provisional.critical_actions() != request.critical_actions()
    {
        return Err(AdmissionError::PolicyAllowanceDetached);
    }
    digest_serializable(&PolicyEvidenceDigestInput {
        domain: "xgeny.policy-evidence/v1",
        api_version: request.api_version(),
        request_id: request.request_id(),
        run_id: request.run_id(),
        step_id: request.step_id(),
        capability: request.capability(),
        effect_class: request.effect_class(),
        scopes: request.requested_scopes(),
        resources: resource_digest_inputs(request),
        critical_actions: request.critical_actions(),
        lifetime: provisional.lifetime(),
        sources,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EffectIdentityDigestInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    action_digest: &'a str,
}

fn digest_serializable(value: &impl Serialize) -> Result<String, AdmissionError> {
    let canonical = serde_jcs::to_vec(value)
        .map_err(|error| AdmissionError::Canonicalization(error.to_string()))?;
    let digest = Sha256::digest(canonical);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(format!("sha256:{encoded}"))
}

fn content_id(prefix: &str, digest: &str) -> String {
    let encoded = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("{prefix}-{encoded}")
}

#[derive(Debug, Error)]
pub enum AdmissionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    EventFactory(#[from] EventFactoryError),
    #[error(transparent)]
    EventMetadata(#[from] crate::EventMetadataError),
    #[error(transparent)]
    Resolution(#[from] InvocationResolutionError),
    #[error(transparent)]
    Broker(#[from] BrokerError),
    #[error(transparent)]
    Route(#[from] RouteInputError),
    #[error(transparent)]
    AuthorizationDigest(#[from] AuthorizationDigestError),
    #[error(transparent)]
    InvocationMaterial(#[from] InvocationMaterialError),
    #[error(transparent)]
    PlanningContract(#[from] xgeny_workgraph::PlanningContractError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("durable Run is not initialized")]
    RunNotInitialized,
    #[error("lease is for Run `{lease_run_id}`, but durable state is `{state_run_id}`")]
    LeaseRunMismatch {
        lease_run_id: String,
        state_run_id: String,
    },
    #[error("step `{0}` does not exist")]
    StepNotFound(String),
    #[error("step `{step_id}` must be Planned before admission, got {actual:?}")]
    StepNotPlanned { step_id: String, actual: StepStatus },
    #[error("step `{step_id}` refers to unknown dependency `{dependency_id}`")]
    StepDependencyUnknown {
        step_id: String,
        dependency_id: String,
    },
    #[error("step `{step_id}` dependency `{dependency_id}` is not released ({reason:?})")]
    StepDependencyNotReleased {
        step_id: String,
        dependency_id: String,
        reason: xgeny_workgraph::DependencyBlockReason,
    },
    #[error("step `{0}` has no durable planned-invocation input")]
    PlannedInputMissing(String),
    #[error("step `{step_id}` planned invocation differs at `{field}`")]
    PlannedInvocationMismatch {
        step_id: String,
        field: &'static str,
    },
    #[error("Run head or authority changed while the invocation awaited policy evaluation")]
    RunHeadChanged,
    #[error(
        "Capability Definition `{capability_id}` contract version `{contract_version}` was not found"
    )]
    DefinitionNotFound {
        capability_id: String,
        contract_version: String,
    },
    #[error("Capability Definition changed after permission preparation")]
    DefinitionChanged,
    #[error("effect admission currently supports only synchronous execution")]
    UnsupportedExecutionStyle,
    #[error("ReadOnly admission requires an accepted planned-invocation binding")]
    UnplannedReadOnlyUnsupported,
    #[error("effect admission requires the Router to enforce idempotency-key support")]
    IdempotencyKeyFeatureRequired,
    #[error("Capability Definition does not support an idempotency key")]
    DefinitionDoesNotSupportIdempotencyKey,
    #[error("effect class {effect_class:?} is not supported by the effect-intent admission path")]
    UnsupportedEffectClass { effect_class: DomainEffectClass },
    #[error("Capability input schema cannot be compiled offline")]
    DefinitionInputSchemaInvalid,
    #[error("invocation arguments do not conform to the Capability input schema")]
    ArgumentsDoNotConform,
    #[error("canonical arguments contain {actual} bytes, exceeding the {maximum}-byte limit")]
    ArgumentsTooLarge { actual: usize, maximum: usize },
    #[error("canonical JSON encoding failed: {0}")]
    Canonicalization(String),
    #[error("managed policy requires an expiry and revocation witness not implemented here")]
    ManagedPolicyUnsupported,
    #[error("Router selected missing Instance `{instance_id}`")]
    SelectedInstanceMissing { instance_id: String },
    #[error("Core Receipt execution currently supports only local placement, got {placement:?}")]
    UnsupportedExecutorPlacement { placement: Placement },
    #[error("Router selected an Instance without a bound policy Allow")]
    SelectedRouteWithoutPolicyAllow,
    #[error("only once-lifetime authorization is supported")]
    NonOnceAuthorization,
    #[error("critical authorization issuance is not implemented")]
    CriticalAuthorizationUnsupported,
    #[error("policy Allow is detached from the exact resolved request")]
    PolicyAllowanceDetached,
    #[error("the Core-generated PolicyDecision is invalid")]
    PolicyDecisionInvalid,
}
