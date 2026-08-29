use std::collections::BTreeMap;
use std::fmt;

use thiserror::Error;
use xgeny_domain::{
    API_VERSION_V1ALPHA1, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef,
    EffectClass, ExecutionReceiptBody, Executor, Placement, ProtocolDocument, ReceiptEffect,
    ReceiptPolicy, VerificationEvidence, VerificationResult, VerificationStrategy,
};
use xgeny_local_store::{ExpectedHead, RunStore, StoreError};
use xgeny_protocol::{
    CORE_RECEIPT_INPUT_SUMMARY_V1, CORE_RECEIPT_PROFILE_V1, CORE_RECEIPT_REDACTIONS_V1,
    CoreVerificationOutcome, ProtocolError, canonical_digest_without_field, core_receipt_id_v1,
    core_receipt_status_v1, core_verification_summary_v1, evaluate_core_verification_v1,
    validate_execution_receipt,
};
use xgeny_workgraph::{
    EffectIntent, ReceiptPlacement, ReceiptProvenance, ReceiptVerificationStrategy, RunEvent,
    RunEventBody, RunState, StepState, StepStatus, VerificationDisposition,
    receipt_provenance_digest,
};

use crate::admission::{AdmissionError, definition_contract_digest, executable_binding_digest};
use crate::executor::{AdapterBindingKey, AdapterEvidenceDigest};
use crate::{
    CapabilityRegistry, DriveAction, DriveReport, EventFactory, EventFactoryError, RunLease,
};

const EMPTY_RECEIPT_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Canonical digest of a verifier-observed, bounded tool output.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifierOutputDigest(String);

impl VerifierOutputDigest {
    /// Validate one canonical lowercase SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Returns a fixed error that never echoes the candidate.
    pub fn new(value: impl Into<String>) -> Result<Self, VerifierOutputDigestError> {
        let value = value.into();
        validate_digest(&value)
            .then_some(Self(value))
            .ok_or(VerifierOutputDigestError)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for VerifierOutputDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("VerifierOutputDigest")
            .field(&self.0)
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("verifier output digest is not canonical SHA-256")]
pub struct VerifierOutputDigestError;

fn validate_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

/// Closed result for one core-selected verification rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleVerificationObservation {
    strategy: VerificationStrategy,
    result: VerificationResult,
    evidence_digest: Option<AdapterEvidenceDigest>,
}

impl RuleVerificationObservation {
    #[must_use]
    pub const fn new(
        strategy: VerificationStrategy,
        result: VerificationResult,
        evidence_digest: Option<AdapterEvidenceDigest>,
    ) -> Self {
        Self {
            strategy,
            result,
            evidence_digest,
        }
    }

    #[must_use]
    pub const fn strategy(&self) -> VerificationStrategy {
        self.strategy
    }

    #[must_use]
    pub const fn result(&self) -> VerificationResult {
        self.result
    }

    #[must_use]
    pub const fn evidence_digest(&self) -> Option<&AdapterEvidenceDigest> {
        self.evidence_digest.as_ref()
    }
}

/// Output commitment and positional observations returned by a trusted verifier port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    output_digest: VerifierOutputDigest,
    rules: Vec<RuleVerificationObservation>,
}

impl VerificationReport {
    #[must_use]
    pub fn new(
        output_digest: VerifierOutputDigest,
        rules: Vec<RuleVerificationObservation>,
    ) -> Self {
        Self {
            output_digest,
            rules,
        }
    }

    #[must_use]
    pub const fn output_digest(&self) -> &VerifierOutputDigest {
        &self.output_digest
    }

    #[must_use]
    pub fn rules(&self) -> &[RuleVerificationObservation] {
        &self.rules
    }
}

/// Read-only verification request assembled from durable Core state.
pub struct VerificationRequest<'a> {
    intent: &'a EffectIntent,
    definition: &'a CapabilityDefinitionBody,
    instance: &'a CapabilityInstanceBody,
    outcome_evidence_digest: &'a AdapterEvidenceDigest,
}

impl VerificationRequest<'_> {
    #[must_use]
    pub const fn intent(&self) -> &EffectIntent {
        self.intent
    }

    #[must_use]
    pub const fn definition(&self) -> &CapabilityDefinitionBody {
        self.definition
    }

    #[must_use]
    pub const fn instance(&self) -> &CapabilityInstanceBody {
        self.instance
    }

    #[must_use]
    pub const fn outcome_evidence_digest(&self) -> &AdapterEvidenceDigest {
        self.outcome_evidence_digest
    }
}

impl fmt::Debug for VerificationRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerificationRequest")
            .field("effect_id", &self.intent.effect_id)
            .field("instance_id", &self.instance.instance_id)
            .field("outcome_evidence_digest", self.outcome_evidence_digest)
            .finish()
    }
}

/// Trusted read-only verifier for one exact Capability Instance binding.
pub trait EffectVerifier {
    /// Verify the observed external effect without applying it again.
    ///
    /// # Errors
    ///
    /// Returns a closed, non-sensitive failure class when no report can be produced.
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure>;
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum VerificationPortFailure {
    #[error("verifier is unavailable")]
    Unavailable,
    #[error("verification strategy is unsupported")]
    UnsupportedStrategy,
    #[error("verification evidence is unavailable")]
    EvidenceUnavailable,
    #[error("verification response cannot be trusted")]
    ResponseUnverifiable,
}

/// Exact, process-local verifier registry. Nearby binding or version fallback is never attempted.
#[derive(Default)]
pub struct EffectVerifierRegistry {
    verifiers: BTreeMap<AdapterBindingKey, Box<dyn EffectVerifier>>,
}

impl EffectVerifierRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            verifiers: BTreeMap::new(),
        }
    }

    /// Register one verifier under a complete Instance binding.
    ///
    /// # Errors
    ///
    /// Rejects invalid or duplicate bindings without replacing an existing verifier.
    pub fn register<V>(
        &mut self,
        binding: &xgeny_domain::InstanceBinding,
        verifier: V,
    ) -> Result<(), VerificationRegistryError>
    where
        V: EffectVerifier + 'static,
    {
        let key = AdapterBindingKey::from_binding(binding)
            .map_err(|_| VerificationRegistryError::InvalidBinding)?;
        if self.verifiers.contains_key(&key) {
            return Err(VerificationRegistryError::DuplicateBinding);
        }
        self.verifiers.insert(key, Box::new(verifier));
        Ok(())
    }

    fn verifier_mut(&mut self, key: &AdapterBindingKey) -> Option<&mut (dyn EffectVerifier + '_)> {
        match self.verifiers.get_mut(key) {
            Some(verifier) => Some(verifier.as_mut()),
            None => None,
        }
    }
}

impl fmt::Debug for EffectVerifierRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectVerifierRegistry")
            .field("verifier_count", &self.verifiers.len())
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum VerificationRegistryError {
    #[error("verifier binding is invalid")]
    InvalidBinding,
    #[error("verifier binding is already registered")]
    DuplicateBinding,
}

/// Core-owned transition from `Validating` to a Receipt-bound terminal state.
#[derive(Debug, Default, Clone, Copy)]
pub struct VerificationRunner;

impl VerificationRunner {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Verify one durable Step and atomically persist its Receipt and finalization event.
    ///
    /// `Validating` recovery never reconstructs material and never prepares, executes, or
    /// reconciles an effect adapter. A failed Receipt commit leaves the Step resumably
    /// `Validating`; the read-only verifier may then be called again.
    ///
    /// # Errors
    ///
    /// Fails closed for stale identity, missing provenance/evidence/verifier, malformed reports,
    /// protocol violations, event creation, or storage faults.
    pub fn drive_step<S, F, L>(
        &self,
        store: &mut S,
        events: &mut F,
        lease: &L,
        capabilities: &CapabilityRegistry,
        verifiers: &mut EffectVerifierRegistry,
        step_id: &str,
    ) -> Result<DriveReport, VerificationRunnerError>
    where
        S: RunStore,
        F: EventFactory,
        L: RunLease,
    {
        let snapshot = store.load_verification_snapshot(step_id)?;
        let snapshot = snapshot.ok_or(VerificationRunnerError::RunNotInitialized)?;
        verify_lease(lease, &snapshot.state)?;
        let step = snapshot
            .state
            .steps
            .get(step_id)
            .ok_or_else(|| VerificationRunnerError::StepNotFound(step_id.to_owned()))?;
        if matches!(
            step.status,
            StepStatus::Completed | StepStatus::Failed | StepStatus::ManualRequired
        ) {
            return Ok(DriveReport {
                action: DriveAction::NoAction,
                state: snapshot.state,
            });
        }
        if step.status != StepStatus::Validating {
            return Err(VerificationRunnerError::StepNotValidating {
                step_id: step_id.to_owned(),
                actual: step.status,
            });
        }
        let verified = verify_step(step, capabilities, verifiers)?;
        let metadata = events.create_metadata(&snapshot.state)?;
        metadata.validate()?;
        let started_at = snapshot.effect_started_at.ok_or_else(|| {
            VerificationRunnerError::EffectStartMissing {
                effect_id: verified.intent.effect_id.clone(),
            }
        })?;
        let receipt = build_execution_receipt(
            &snapshot.state,
            step_id,
            started_at,
            &metadata.recorded_at,
            snapshot.previous_receipt_digest,
            &verified,
        )?;
        let event = RunEvent {
            event_id: metadata.event_id,
            run_id: snapshot.state.run_id.clone(),
            authority: snapshot.state.authority.clone(),
            authority_epoch: snapshot.state.authority_epoch,
            recorded_at: metadata.recorded_at,
            body: RunEventBody::VerificationRecorded {
                step_id: step_id.to_owned(),
                effect_id: verified.intent.effect_id.clone(),
                disposition: verified.disposition,
                receipt_id: receipt.receipt_id.clone(),
                receipt_digest: receipt.receipt_digest.clone(),
            },
        };
        let commit = store.append_with_execution_receipt(
            ExpectedHead::from_state(&snapshot.state),
            event,
            receipt,
        )?;
        Ok(DriveReport {
            action: match verified.disposition {
                VerificationDisposition::Passed => DriveAction::VerificationPassed,
                VerificationDisposition::Failed => DriveAction::VerificationFailed,
                VerificationDisposition::Inconclusive => DriveAction::VerificationInconclusive,
            },
            state: commit.state,
        })
    }
}

struct VerifiedStep {
    intent: EffectIntent,
    provenance: ReceiptProvenance,
    report: VerificationReport,
    outcome: CoreVerificationOutcome,
    disposition: VerificationDisposition,
    verification: Vec<VerificationEvidence>,
}

fn verify_step(
    step: &StepState,
    capabilities: &CapabilityRegistry,
    verifiers: &mut EffectVerifierRegistry,
) -> Result<VerifiedStep, VerificationRunnerError> {
    let intent = step
        .intent
        .as_ref()
        .ok_or_else(|| VerificationRunnerError::IntentMissing(step.step_id.clone()))?;
    let provenance = intent.receipt_provenance.as_ref().ok_or_else(|| {
        VerificationRunnerError::ReceiptProvenanceMissing {
            effect_id: intent.effect_id.clone(),
        }
    })?;
    if provenance.profile_version != CORE_RECEIPT_PROFILE_V1 {
        return Err(VerificationRunnerError::UnsupportedReceiptProfile);
    }
    if provenance.input_summary != CORE_RECEIPT_INPUT_SUMMARY_V1 {
        return Err(VerificationRunnerError::ReceiptProvenanceBindingMismatch);
    }
    let expected_provenance_digest = intent
        .authorization
        .binding
        .receipt_provenance_digest
        .as_ref()
        .ok_or(VerificationRunnerError::ReceiptProvenanceBindingMismatch)?;
    let actual_provenance_digest = receipt_provenance_digest(provenance)
        .map_err(|_| VerificationRunnerError::ReceiptProvenanceBindingMismatch)?;
    if &actual_provenance_digest != expected_provenance_digest {
        return Err(VerificationRunnerError::ReceiptProvenanceBindingMismatch);
    }
    let definition = verify_current_definition(capabilities, intent)?;
    verify_verification_plan(definition, provenance)?;
    let instance = verify_current_instance(capabilities, intent)?;
    let evidence = step
        .effect_evidence_digest
        .as_ref()
        .or(step.reconciliation_evidence_digest.as_ref())
        .ok_or_else(|| VerificationRunnerError::EffectEvidenceMissing {
            effect_id: intent.effect_id.clone(),
        })?;
    let evidence = AdapterEvidenceDigest::new(evidence.clone())?;
    let key = AdapterBindingKey::from_binding(&instance.binding)
        .map_err(|_| VerificationRunnerError::InvalidVerifierBinding)?;
    let verifier = verifiers.verifier_mut(&key).ok_or_else(|| {
        VerificationRunnerError::VerifierNotRegistered {
            instance_id: instance.instance_id.clone(),
        }
    })?;
    let report = verifier.verify(VerificationRequest {
        intent,
        definition,
        instance,
        outcome_evidence_digest: &evidence,
    })?;
    let (outcome, verification) = verify_report(provenance, &report)?;
    let disposition = match outcome {
        CoreVerificationOutcome::Passed => VerificationDisposition::Passed,
        CoreVerificationOutcome::Failed => VerificationDisposition::Failed,
        CoreVerificationOutcome::Inconclusive => VerificationDisposition::Inconclusive,
    };
    Ok(VerifiedStep {
        intent: intent.clone(),
        provenance: provenance.clone(),
        report,
        outcome,
        disposition,
        verification,
    })
}

fn build_execution_receipt(
    state: &RunState,
    step_id: &str,
    started_at: String,
    ended_at: &str,
    previous_receipt_digest: Option<String>,
    verified: &VerifiedStep,
) -> Result<ExecutionReceiptBody, VerificationRunnerError> {
    let intent = &verified.intent;
    let provenance = &verified.provenance;
    let mut receipt = ExecutionReceiptBody {
        api_version: API_VERSION_V1ALPHA1.to_owned(),
        extensions: BTreeMap::new(),
        required_extensions: Vec::new(),
        receipt_id: core_receipt_id_v1(&intent.effect_id),
        run_id: state.run_id.clone(),
        step_id: step_id.to_owned(),
        invocation_id: provenance.invocation_id.clone(),
        plan_id: provenance.plan_id.clone(),
        capability: CapabilityRef {
            capability_id: intent.invocation.capability_id.clone(),
            contract_version: intent.invocation.contract_version.clone(),
        },
        instance_id: intent.invocation.instance_id.clone(),
        input_digest: intent.authorization.binding.material_digest.clone(),
        input_summary: provenance.input_summary.clone(),
        policy: ReceiptPolicy {
            decision_id: provenance.policy_decision_id.clone(),
            decision_digest: provenance.policy_decision_digest.clone(),
            lease_id: None,
            lease_digest: None,
        },
        executor: Executor {
            id: provenance.executor_id.clone(),
            placement: protocol_placement(provenance.executor_placement),
            platform: provenance.executor_platform.clone(),
        },
        effect: ReceiptEffect {
            class: protocol_effect_class(intent.effect_class),
            started: true,
            started_at: Some(started_at.clone()),
            idempotency_key: intent.idempotency_key.clone(),
        },
        status: core_receipt_status_v1(verified.outcome),
        started_at,
        ended_at: ended_at.to_owned(),
        output_digest: verified.report.output_digest().as_str().to_owned(),
        artifacts: Vec::new(),
        verification: verified.verification.clone(),
        redactions_applied: CORE_RECEIPT_REDACTIONS_V1
            .iter()
            .map(ToString::to_string)
            .collect(),
        previous_receipt_digest,
        receipt_digest: EMPTY_RECEIPT_DIGEST.to_owned(),
    };
    receipt.receipt_digest = execution_receipt_digest(&receipt)?;
    validate_execution_receipt(&receipt)?;
    Ok(receipt)
}

fn verify_current_definition<'a>(
    registry: &'a CapabilityRegistry,
    intent: &EffectIntent,
) -> Result<&'a CapabilityDefinitionBody, VerificationRunnerError> {
    let capability = CapabilityRef {
        capability_id: intent.invocation.capability_id.clone(),
        contract_version: intent.invocation.contract_version.clone(),
    };
    let definition = registry.definition(&capability).ok_or_else(|| {
        VerificationRunnerError::DefinitionNotFound {
            capability_id: capability.capability_id.clone(),
            contract_version: capability.contract_version.clone(),
        }
    })?;
    if definition_contract_digest(definition)? != intent.invocation.definition_digest {
        return Err(VerificationRunnerError::DefinitionChanged);
    }
    Ok(definition)
}

fn verify_current_instance<'a>(
    registry: &'a CapabilityRegistry,
    intent: &EffectIntent,
) -> Result<&'a CapabilityInstanceBody, VerificationRunnerError> {
    let instance = registry
        .instance(&intent.invocation.instance_id)
        .ok_or_else(|| VerificationRunnerError::InstanceNotFound {
            instance_id: intent.invocation.instance_id.clone(),
        })?;
    if instance.definition.capability_id != intent.invocation.capability_id
        || instance.definition.contract_version != intent.invocation.contract_version
        || executable_binding_digest(instance)? != intent.invocation.instance_binding_digest
    {
        return Err(VerificationRunnerError::InstanceBindingChanged);
    }
    Ok(instance)
}

fn verify_verification_plan(
    definition: &CapabilityDefinitionBody,
    provenance: &ReceiptProvenance,
) -> Result<(), VerificationRunnerError> {
    if !receipt_verification_plan_matches(definition, provenance) {
        return Err(VerificationRunnerError::VerificationPlanChanged);
    }
    Ok(())
}

pub(crate) fn receipt_verification_plan_matches(
    definition: &CapabilityDefinitionBody,
    provenance: &ReceiptProvenance,
) -> bool {
    definition.spec.verification.len() == provenance.verification_plan.len()
        && definition
            .spec
            .verification
            .iter()
            .zip(&provenance.verification_plan)
            .all(|(definition_rule, durable_rule)| {
                definition_rule.required == durable_rule.required
                    && definition_rule.strategy
                        == protocol_verification_strategy(durable_rule.strategy)
            })
}

fn verify_report(
    provenance: &ReceiptProvenance,
    report: &VerificationReport,
) -> Result<(CoreVerificationOutcome, Vec<VerificationEvidence>), VerificationRunnerError> {
    if report.rules().len() != provenance.verification_plan.len() {
        return Err(VerificationRunnerError::VerificationReportMismatch);
    }
    let mut evidence = Vec::with_capacity(report.rules().len());
    for (observed, rule) in report.rules().iter().zip(&provenance.verification_plan) {
        let strategy = protocol_verification_strategy(rule.strategy);
        if observed.strategy() != strategy
            || (observed.result() == VerificationResult::Passed
                && observed.evidence_digest().is_none())
        {
            return Err(VerificationRunnerError::VerificationReportMismatch);
        }
        evidence.push(VerificationEvidence {
            strategy,
            required: rule.required,
            result: observed.result(),
            summary: core_verification_summary_v1(observed.result()).to_owned(),
            evidence_digest: observed
                .evidence_digest()
                .map(|digest| digest.as_str().to_owned()),
            artifact: None,
        });
    }
    Ok((evaluate_core_verification_v1(&evidence), evidence))
}

fn execution_receipt_digest(
    receipt: &ExecutionReceiptBody,
) -> Result<String, VerificationRunnerError> {
    let value = serde_json::to_value(ProtocolDocument::ExecutionReceipt(Box::new(
        receipt.clone(),
    )))?;
    Ok(canonical_digest_without_field(&value, "receiptDigest")?)
}

const fn protocol_effect_class(effect_class: xgeny_workgraph::EffectClass) -> EffectClass {
    match effect_class {
        xgeny_workgraph::EffectClass::Reversible => EffectClass::Compensatable,
        xgeny_workgraph::EffectClass::Idempotent => EffectClass::Idempotent,
        xgeny_workgraph::EffectClass::NonIdempotent => EffectClass::NonIdempotent,
    }
}

const fn protocol_placement(placement: ReceiptPlacement) -> Placement {
    match placement {
        ReceiptPlacement::Local => Placement::Local,
        ReceiptPlacement::Device => Placement::Device,
        ReceiptPlacement::Remote => Placement::Remote,
    }
}

const fn protocol_verification_strategy(
    strategy: ReceiptVerificationStrategy,
) -> VerificationStrategy {
    match strategy {
        ReceiptVerificationStrategy::OutputSchema => VerificationStrategy::OutputSchema,
        ReceiptVerificationStrategy::Postcondition => VerificationStrategy::Postcondition,
        ReceiptVerificationStrategy::ArtifactDigest => VerificationStrategy::ArtifactDigest,
        ReceiptVerificationStrategy::Receipt => VerificationStrategy::Receipt,
        ReceiptVerificationStrategy::Human => VerificationStrategy::Human,
    }
}

fn verify_lease<L: RunLease>(lease: &L, state: &RunState) -> Result<(), VerificationRunnerError> {
    if lease.run_id() != state.run_id {
        return Err(VerificationRunnerError::LeaseRunMismatch {
            lease_run_id: lease.run_id().to_owned(),
            state_run_id: state.run_id.clone(),
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum VerificationRunnerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    EventFactory(#[from] EventFactoryError),
    #[error(transparent)]
    EventMetadata(#[from] crate::EventMetadataError),
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Verifier(#[from] VerificationPortFailure),
    #[error(transparent)]
    EvidenceDigest(#[from] crate::AdapterEvidenceDigestError),
    #[error("JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("durable Run is not initialized")]
    RunNotInitialized,
    #[error("step `{0}` does not exist")]
    StepNotFound(String),
    #[error("step `{step_id}` must be Validating, got {actual:?}")]
    StepNotValidating { step_id: String, actual: StepStatus },
    #[error("step `{0}` has no committed effect intent")]
    IntentMissing(String),
    #[error("effect `{effect_id}` has no durable Receipt provenance")]
    ReceiptProvenanceMissing { effect_id: String },
    #[error("the durable Receipt profile is unsupported")]
    UnsupportedReceiptProfile,
    #[error("durable Receipt provenance does not match its authorization binding")]
    ReceiptProvenanceBindingMismatch,
    #[error("effect `{effect_id}` has no durable adapter evidence")]
    EffectEvidenceMissing { effect_id: String },
    #[error("Capability Definition `{capability_id}` version `{contract_version}` is missing")]
    DefinitionNotFound {
        capability_id: String,
        contract_version: String,
    },
    #[error("Capability Definition changed after invocation admission")]
    DefinitionChanged,
    #[error("Capability Instance `{instance_id}` is missing")]
    InstanceNotFound { instance_id: String },
    #[error("Capability Instance binding changed after invocation admission")]
    InstanceBindingChanged,
    #[error("the exact verifier binding is invalid")]
    InvalidVerifierBinding,
    #[error("the exact Capability Instance verifier is not registered")]
    VerifierNotRegistered { instance_id: String },
    #[error("the durable verification plan changed")]
    VerificationPlanChanged,
    #[error("the verifier report does not cover the durable plan exactly")]
    VerificationReportMismatch,
    #[error("effect `{effect_id}` has no durable execution start timestamp")]
    EffectStartMissing { effect_id: String },
    #[error("lease is for Run `{lease_run_id}`, but durable state is `{state_run_id}`")]
    LeaseRunMismatch {
        lease_run_id: String,
        state_run_id: String,
    },
}
