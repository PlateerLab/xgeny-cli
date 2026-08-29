#![doc = "I/O-free state transitions and hash-chained events for a durable `XGENy` run."]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunEvent {
    pub event_id: String,
    pub run_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub recorded_at: String,
    pub body: RunEventBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RunEventBody {
    RunCreated {
        goal: String,
    },
    StepPlanned {
        step_id: String,
        objective: String,
    },
    EffectIntentCommitted {
        step_id: String,
        intent: Box<EffectIntent>,
    },
    EffectExecutionStarted {
        step_id: String,
        effect_id: String,
    },
    EffectSucceeded {
        step_id: String,
        effect_id: String,
        receipt_digest: String,
    },
    EffectFailed {
        step_id: String,
        effect_id: String,
        receipt_digest: String,
    },
    EffectBecameUnknown {
        step_id: String,
        effect_id: String,
        reason: String,
    },
    ReconciliationStarted {
        step_id: String,
        effect_id: String,
    },
    ReconciliationResolved {
        step_id: String,
        effect_id: String,
        resolution: ReconciliationResolution,
        evidence_digest: String,
    },
    ManualInterventionRequired {
        step_id: String,
        effect_id: String,
        reason: String,
    },
    VerificationPassed {
        step_id: String,
    },
    VerificationFailed {
        step_id: String,
        reason: String,
    },
}

impl RunEventBody {
    fn kind(&self) -> &'static str {
        match self {
            Self::RunCreated { .. } => "run_created",
            Self::StepPlanned { .. } => "step_planned",
            Self::EffectIntentCommitted { .. } => "effect_intent_committed",
            Self::EffectExecutionStarted { .. } => "effect_execution_started",
            Self::EffectSucceeded { .. } => "effect_succeeded",
            Self::EffectFailed { .. } => "effect_failed",
            Self::EffectBecameUnknown { .. } => "effect_became_unknown",
            Self::ReconciliationStarted { .. } => "reconciliation_started",
            Self::ReconciliationResolved { .. } => "reconciliation_resolved",
            Self::ManualInterventionRequired { .. } => "manual_intervention_required",
            Self::VerificationPassed { .. } => "verification_passed",
            Self::VerificationFailed { .. } => "verification_failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectIntent {
    pub effect_id: String,
    pub action_digest: String,
    pub invocation: InvocationBinding,
    pub effect_class: EffectClass,
    pub idempotency_key: Option<String>,
    pub sink_guarantee: SinkGuarantee,
    pub authorization: AuthorizationUse,
}

/// Immutable executable binding retained with a durable effect intent.
///
/// Dynamic health, authentication state, and cost hints are deliberately excluded. The binding
/// digest identifies the adapter endpoint/operation selected by the trusted admission path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationBinding {
    pub capability_id: String,
    pub contract_version: String,
    pub definition_digest: String,
    pub instance_id: String,
    pub instance_binding_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Reversible,
    Idempotent,
    NonIdempotent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkGuarantee {
    None,
    DeduplicateByKey,
    QueryByKey,
    DeduplicateAndQuery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationUse {
    pub grant_id: String,
    pub grant_digest: String,
    pub max_uses: u32,
    pub binding: AuthorizationBinding,
}

/// Run-local facts covered by one issued authorization digest.
///
/// The journal head is the state against which policy and routing were evaluated. Persisting the
/// binding lets replay reject copying an intent to another Run, Step, authority epoch, action, or
/// executable Instance even though the low-level journal types remain serializable primitives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationBinding {
    pub run_id: String,
    pub step_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub issued_at_sequence: u64,
    pub issued_at_head_digest: String,
    pub capability_id: String,
    pub contract_version: String,
    pub definition_digest: String,
    pub instance_id: String,
    pub instance_binding_digest: String,
    pub action_digest: String,
    pub policy_evidence_digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizationDigestInput<'a> {
    domain: &'static str,
    binding: &'a AuthorizationBinding,
    max_uses: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OnceAuthorizationIdInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    action_digest: &'a str,
}

/// Derive the stable budget identity for one semantic action within a Run.
///
/// Keeping this derivation in the reducer's crate prevents a caller from changing only the grant
/// ID to mint a fresh one-shot budget for the same Run/action pair.
///
/// # Errors
///
/// Returns an error if RFC 8785 canonical JSON encoding fails.
pub fn once_authorization_id(
    run_id: &str,
    action_digest: &str,
) -> Result<String, AuthorizationDigestError> {
    let canonical = serde_jcs::to_vec(&OnceAuthorizationIdInput {
        domain: "xgeny.authorization-budget.once/v1",
        run_id,
        action_digest,
    })
    .map_err(|error| AuthorizationDigestError::Canonicalization(error.to_string()))?;
    let digest = sha256_digest(&canonical);
    let encoded = digest.strip_prefix("sha256:").unwrap_or(&digest);
    Ok(format!("authorization-{encoded}"))
}

/// Calculate the content digest the reducer expects for a durable authorization binding.
///
/// # Errors
///
/// Returns an error if RFC 8785 canonical JSON encoding fails.
pub fn authorization_digest(
    binding: &AuthorizationBinding,
    max_uses: u32,
) -> Result<String, AuthorizationDigestError> {
    let canonical = serde_jcs::to_vec(&AuthorizationDigestInput {
        domain: "xgeny.authorization.once/v1",
        binding,
        max_uses,
    })
    .map_err(|error| AuthorizationDigestError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationResolution {
    ProvedApplied,
    ProvedNotApplied,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventRecord {
    pub sequence: u64,
    pub previous_digest: Option<String>,
    pub event: RunEvent,
    pub digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DigestInput<'a> {
    sequence: u64,
    previous_digest: Option<&'a str>,
    event: &'a RunEvent,
}

impl EventRecord {
    /// Build the next immutable record in a hash chain.
    ///
    /// # Errors
    ///
    /// Returns an error if RFC 8785 canonical JSON encoding fails.
    pub fn next(previous: Option<&Self>, event: RunEvent) -> Result<Self, RecordError> {
        let sequence = previous.map_or(Ok(1), |record| {
            record
                .sequence
                .checked_add(1)
                .ok_or(RecordError::SequenceOverflow)
        })?;
        let previous_digest = previous.map(|record| record.digest.clone());
        let digest = record_digest(sequence, previous_digest.as_deref(), &event)?;
        Ok(Self {
            sequence,
            previous_digest,
            event,
            digest,
        })
    }

    /// Verify this record's derived digest.
    ///
    /// # Errors
    ///
    /// Returns an error if canonicalization fails or the stored digest differs.
    pub fn verify_digest(&self) -> Result<(), RecordError> {
        let actual = record_digest(self.sequence, self.previous_digest.as_deref(), &self.event)?;
        if actual != self.digest {
            return Err(RecordError::DigestMismatch {
                sequence: self.sequence,
                expected: self.digest.clone(),
                actual,
            });
        }
        Ok(())
    }
}

fn record_digest(
    sequence: u64,
    previous_digest: Option<&str>,
    event: &RunEvent,
) -> Result<String, RecordError> {
    let canonical = serde_jcs::to_vec(&DigestInput {
        sequence,
        previous_digest,
        event,
    })
    .map_err(|error| RecordError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn sha256_digest(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunState {
    pub run_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub goal: String,
    pub revision: u64,
    pub journal_sequence: u64,
    pub journal_head_digest: String,
    pub steps: BTreeMap<String, StepState>,
    pub authorization_consumption: BTreeMap<String, AuthorizationConsumption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepState {
    pub step_id: String,
    pub objective: String,
    pub status: StepStatus,
    pub attempts: u32,
    pub intent: Option<EffectIntent>,
    pub receipt_digest: Option<String>,
    pub uncertainty_reason: Option<String>,
    pub reconciliation_evidence_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Planned,
    IntentCommitted,
    Executing,
    EffectUnknown,
    Reconciling,
    Validating,
    Completed,
    Failed,
    ManualRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationConsumption {
    pub grant_digest: String,
    pub max_uses: u32,
    pub uses: u32,
    pub effect_ids: BTreeSet<String>,
}

/// Apply one committed event without performing I/O or invoking an effect.
///
/// # Errors
///
/// Returns an error when chain metadata, authority, or lifecycle invariants fail.
pub fn apply_record(
    current: Option<&RunState>,
    record: &EventRecord,
) -> Result<RunState, TransitionError> {
    verify_record_against_state(current, record)?;
    let mut state = match (current, &record.event.body) {
        (None, RunEventBody::RunCreated { goal }) => RunState {
            run_id: record.event.run_id.clone(),
            authority: record.event.authority.clone(),
            authority_epoch: record.event.authority_epoch,
            goal: goal.clone(),
            revision: 0,
            journal_sequence: 0,
            journal_head_digest: String::new(),
            steps: BTreeMap::new(),
            authorization_consumption: BTreeMap::new(),
        },
        (None, _) => return Err(TransitionError::FirstEventMustCreateRun),
        (Some(_), RunEventBody::RunCreated { .. }) => {
            return Err(TransitionError::RunAlreadyCreated);
        }
        (Some(state), _) => state.clone(),
    };

    if current.is_some() {
        apply_body(&mut state, &record.event.body)?;
    }
    state.revision = record.sequence;
    state.journal_sequence = record.sequence;
    state.journal_head_digest.clone_from(&record.digest);
    Ok(state)
}

fn verify_record_against_state(
    current: Option<&RunState>,
    record: &EventRecord,
) -> Result<(), TransitionError> {
    let expected_sequence = current.map_or(Ok(1), |state| {
        state
            .journal_sequence
            .checked_add(1)
            .ok_or(RecordError::SequenceOverflow)
    })?;
    if record.sequence != expected_sequence {
        return Err(TransitionError::UnexpectedSequence {
            expected: expected_sequence,
            actual: record.sequence,
        });
    }
    let expected_previous = current.map(|state| state.journal_head_digest.clone());
    if record.previous_digest != expected_previous {
        return Err(TransitionError::PreviousDigestMismatch {
            sequence: record.sequence,
            expected: expected_previous,
            actual: record.previous_digest.clone(),
        });
    }
    record.verify_digest()?;
    if let Some(state) = current {
        if record.event.run_id != state.run_id {
            return Err(TransitionError::RunIdMismatch);
        }
        if record.event.authority != state.authority {
            return Err(TransitionError::AuthorityMismatch);
        }
        if record.event.authority_epoch != state.authority_epoch {
            return Err(TransitionError::AuthorityEpochMismatch);
        }
    }
    Ok(())
}

fn apply_body(state: &mut RunState, body: &RunEventBody) -> Result<(), TransitionError> {
    match body {
        RunEventBody::RunCreated { .. } => unreachable!("handled by apply_record"),
        RunEventBody::StepPlanned { step_id, objective } => {
            if state.steps.contains_key(step_id) {
                return Err(TransitionError::DuplicateStep(step_id.clone()));
            }
            state.steps.insert(
                step_id.clone(),
                StepState {
                    step_id: step_id.clone(),
                    objective: objective.clone(),
                    status: StepStatus::Planned,
                    attempts: 0,
                    intent: None,
                    receipt_digest: None,
                    uncertainty_reason: None,
                    reconciliation_evidence_digest: None,
                },
            );
        }
        RunEventBody::EffectIntentCommitted { step_id, intent } => {
            commit_effect_intent(state, step_id, intent)?;
        }
        _ => apply_effect_lifecycle(state, body)?,
    }
    Ok(())
}

fn apply_effect_lifecycle(
    state: &mut RunState,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    match body {
        RunEventBody::RunCreated { .. }
        | RunEventBody::StepPlanned { .. }
        | RunEventBody::EffectIntentCommitted { .. } => {
            unreachable!("handled by apply_body")
        }
        RunEventBody::EffectExecutionStarted { step_id, effect_id } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            require_status(step, StepStatus::IntentCommitted, body)?;
            step.status = StepStatus::Executing;
            step.attempts =
                step.attempts
                    .checked_add(1)
                    .ok_or_else(|| TransitionError::AttemptOverflow {
                        step_id: step_id.clone(),
                    })?;
        }
        RunEventBody::EffectSucceeded {
            step_id,
            effect_id,
            receipt_digest,
        } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            require_status(step, StepStatus::Executing, body)?;
            step.status = StepStatus::Validating;
            step.receipt_digest = Some(receipt_digest.clone());
        }
        RunEventBody::EffectFailed {
            step_id,
            effect_id,
            receipt_digest,
        } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            require_status(step, StepStatus::Executing, body)?;
            step.status = StepStatus::Failed;
            step.receipt_digest = Some(receipt_digest.clone());
        }
        RunEventBody::EffectBecameUnknown {
            step_id,
            effect_id,
            reason,
        } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            require_status(step, StepStatus::Executing, body)?;
            step.status = StepStatus::EffectUnknown;
            step.uncertainty_reason = Some(reason.clone());
        }
        RunEventBody::ReconciliationStarted { step_id, effect_id } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            require_status(step, StepStatus::EffectUnknown, body)?;
            step.status = StepStatus::Reconciling;
        }
        RunEventBody::ReconciliationResolved {
            step_id,
            effect_id,
            resolution,
            evidence_digest,
        } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            require_status(step, StepStatus::Reconciling, body)?;
            step.reconciliation_evidence_digest = Some(evidence_digest.clone());
            step.status = match resolution {
                ReconciliationResolution::ProvedApplied => StepStatus::Validating,
                ReconciliationResolution::ProvedNotApplied => StepStatus::IntentCommitted,
                ReconciliationResolution::Failed => StepStatus::Failed,
            };
        }
        RunEventBody::ManualInterventionRequired {
            step_id,
            effect_id,
            reason,
        } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            if !matches!(
                step.status,
                StepStatus::EffectUnknown | StepStatus::Reconciling
            ) {
                return invalid_transition(step, body);
            }
            step.status = StepStatus::ManualRequired;
            step.uncertainty_reason = Some(reason.clone());
        }
        RunEventBody::VerificationPassed { step_id } => {
            let step = step_mut(state, step_id)?;
            require_status(step, StepStatus::Validating, body)?;
            step.status = StepStatus::Completed;
        }
        RunEventBody::VerificationFailed { step_id, .. } => {
            let step = step_mut(state, step_id)?;
            require_status(step, StepStatus::Validating, body)?;
            step.status = StepStatus::Failed;
        }
    }
    Ok(())
}

fn commit_effect_intent(
    state: &mut RunState,
    step_id: &str,
    intent: &EffectIntent,
) -> Result<(), TransitionError> {
    if intent.authorization.max_uses != 1 {
        return Err(TransitionError::InvalidAuthorizationBudget {
            grant_id: intent.authorization.grant_id.clone(),
        });
    }
    if intent.sink_guarantee != SinkGuarantee::None
        && intent.idempotency_key.as_ref().is_none_or(String::is_empty)
    {
        return Err(TransitionError::SinkGuaranteeRequiresIdempotencyKey {
            effect_id: intent.effect_id.clone(),
        });
    }
    if state.steps.values().any(|step| {
        step.intent
            .as_ref()
            .is_some_and(|existing| existing.effect_id == intent.effect_id)
    }) {
        return Err(TransitionError::DuplicateEffect(intent.effect_id.clone()));
    }

    validate_authorization_binding(state, step_id, intent)?;

    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| TransitionError::UnknownStep(step_id.to_owned()))?;
    require_status(
        step,
        StepStatus::Planned,
        &RunEventBody::EffectIntentCommitted {
            step_id: step_id.to_owned(),
            intent: Box::new(intent.clone()),
        },
    )?;

    let consumption = state
        .authorization_consumption
        .entry(intent.authorization.grant_id.clone())
        .or_insert_with(|| AuthorizationConsumption {
            grant_digest: intent.authorization.grant_digest.clone(),
            max_uses: intent.authorization.max_uses,
            uses: 0,
            effect_ids: BTreeSet::new(),
        });
    if consumption.grant_digest != intent.authorization.grant_digest
        || consumption.max_uses != intent.authorization.max_uses
    {
        return Err(TransitionError::AuthorizationGrantChanged {
            grant_id: intent.authorization.grant_id.clone(),
        });
    }
    if consumption.uses >= consumption.max_uses {
        return Err(TransitionError::AuthorizationBudgetExceeded {
            grant_id: intent.authorization.grant_id.clone(),
            max_uses: consumption.max_uses,
        });
    }
    consumption.uses += 1;
    consumption.effect_ids.insert(intent.effect_id.clone());

    let step = state
        .steps
        .get_mut(step_id)
        .expect("step was checked before authorization mutation");
    step.intent = Some(intent.clone());
    step.status = StepStatus::IntentCommitted;
    Ok(())
}

fn validate_authorization_binding(
    state: &RunState,
    step_id: &str,
    intent: &EffectIntent,
) -> Result<(), TransitionError> {
    let authorization = &intent.authorization;
    let binding = &authorization.binding;
    let invocation = &intent.invocation;

    if binding.run_id != state.run_id {
        return Err(TransitionError::AuthorizationRunMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.step_id != step_id {
        return Err(TransitionError::AuthorizationStepMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.authority != state.authority {
        return Err(TransitionError::AuthorizationAuthorityMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.authority_epoch != state.authority_epoch {
        return Err(TransitionError::AuthorizationEpochMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.issued_at_sequence != state.journal_sequence
        || binding.issued_at_head_digest != state.journal_head_digest
    {
        return Err(TransitionError::AuthorizationHeadMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.action_digest != intent.action_digest {
        return Err(TransitionError::AuthorizationActionMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.capability_id != invocation.capability_id
        || binding.contract_version != invocation.contract_version
        || binding.definition_digest != invocation.definition_digest
        || binding.instance_id != invocation.instance_id
        || binding.instance_binding_digest != invocation.instance_binding_digest
    {
        return Err(TransitionError::AuthorizationInvocationMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    let expected_id = once_authorization_id(&binding.run_id, &binding.action_digest)?;
    if authorization.grant_id != expected_id {
        return Err(TransitionError::AuthorizationIdMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    let expected = authorization_digest(binding, authorization.max_uses)?;
    if authorization.grant_digest != expected {
        return Err(TransitionError::AuthorizationDigestMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    Ok(())
}

fn step_mut<'a>(
    state: &'a mut RunState,
    step_id: &str,
) -> Result<&'a mut StepState, TransitionError> {
    state
        .steps
        .get_mut(step_id)
        .ok_or_else(|| TransitionError::UnknownStep(step_id.to_owned()))
}

fn matching_step_mut<'a>(
    state: &'a mut RunState,
    step_id: &str,
    effect_id: &str,
) -> Result<&'a mut StepState, TransitionError> {
    let step = step_mut(state, step_id)?;
    let actual = step.intent.as_ref().map(|intent| intent.effect_id.as_str());
    if actual != Some(effect_id) {
        return Err(TransitionError::EffectMismatch {
            step_id: step_id.to_owned(),
            expected: actual.map(str::to_owned),
            actual: effect_id.to_owned(),
        });
    }
    Ok(step)
}

fn require_status(
    step: &StepState,
    required: StepStatus,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    if step.status != required {
        return invalid_transition(step, body);
    }
    Ok(())
}

fn invalid_transition<T>(step: &StepState, body: &RunEventBody) -> Result<T, TransitionError> {
    Err(TransitionError::InvalidStepTransition {
        step_id: step.step_id.clone(),
        from: step.status,
        event: body.kind(),
    })
}

/// Rebuild a projection from committed records only.
///
/// This function intentionally has no executor or tool port, so replay cannot emit effects.
///
/// # Errors
///
/// Returns an error when the chain or any state transition is invalid.
pub fn replay(records: &[EventRecord]) -> Result<RunState, ReplayError> {
    let mut state = None;
    for record in records {
        state = Some(apply_record(state.as_ref(), record).map_err(replay_transition_error)?);
    }
    state.ok_or(ReplayError::EmptyJournal)
}

fn replay_transition_error(error: TransitionError) -> ReplayError {
    match error {
        TransitionError::UnexpectedSequence { expected, actual } => {
            ReplayError::UnexpectedSequence { expected, actual }
        }
        TransitionError::PreviousDigestMismatch {
            sequence,
            expected,
            actual,
        } => ReplayError::PreviousDigestMismatch {
            sequence,
            expected,
            actual,
        },
        TransitionError::Record(error) => ReplayError::Record(error),
        error => ReplayError::Transition(error),
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RecordError {
    #[error("event canonicalization failed: {0}")]
    Canonicalization(String),
    #[error("event record {sequence} digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        sequence: u64,
        expected: String,
        actual: String,
    },
    #[error("event sequence overflowed u64")]
    SequenceOverflow,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthorizationDigestError {
    #[error("authorization canonicalization failed: {0}")]
    Canonicalization(String),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransitionError {
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error("first event must create the run")]
    FirstEventMustCreateRun,
    #[error("run is already created")]
    RunAlreadyCreated,
    #[error("event sequence mismatch: expected {expected}, got {actual}")]
    UnexpectedSequence { expected: u64, actual: u64 },
    #[error("event {sequence} previous digest mismatch")]
    PreviousDigestMismatch {
        sequence: u64,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error("event run id differs from the run projection")]
    RunIdMismatch,
    #[error("event authority differs from the run projection")]
    AuthorityMismatch,
    #[error("event authority epoch differs from the run projection")]
    AuthorityEpochMismatch,
    #[error("step `{0}` already exists")]
    DuplicateStep(String),
    #[error("unknown step `{0}`")]
    UnknownStep(String),
    #[error("effect `{0}` already has a committed intent")]
    DuplicateEffect(String),
    #[error("step `{step_id}` expected effect {expected:?}, got `{actual}`")]
    EffectMismatch {
        step_id: String,
        expected: Option<String>,
        actual: String,
    },
    #[error("step `{step_id}` cannot apply `{event}` from {from:?}")]
    InvalidStepTransition {
        step_id: String,
        from: StepStatus,
        event: &'static str,
    },
    #[error("authorization `{grant_id}` must allow exactly one use")]
    InvalidAuthorizationBudget { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another Run")]
    AuthorizationRunMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another Step")]
    AuthorizationStepMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another authority")]
    AuthorizationAuthorityMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another authority epoch")]
    AuthorizationEpochMismatch { grant_id: String },
    #[error("authorization `{grant_id}` was issued against another journal head")]
    AuthorizationHeadMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another semantic action")]
    AuthorizationActionMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another executable invocation")]
    AuthorizationInvocationMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is not the stable ID for its Run/action budget")]
    AuthorizationIdMismatch { grant_id: String },
    #[error("authorization `{grant_id}` digest does not cover its durable binding")]
    AuthorizationDigestMismatch { grant_id: String },
    #[error(transparent)]
    AuthorizationDigest(#[from] AuthorizationDigestError),
    #[error("authorization `{grant_id}` changed after first consumption")]
    AuthorizationGrantChanged { grant_id: String },
    #[error("authorization `{grant_id}` exceeded its {max_uses}-use budget")]
    AuthorizationBudgetExceeded { grant_id: String, max_uses: u32 },
    #[error("effect `{effect_id}` claims a keyed sink guarantee without an idempotency key")]
    SinkGuaranteeRequiresIdempotencyKey { effect_id: String },
    #[error("step `{step_id}` attempt counter overflowed")]
    AttemptOverflow { step_id: String },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReplayError {
    #[error("journal has no run-created event")]
    EmptyJournal,
    #[error("event sequence mismatch: expected {expected}, got {actual}")]
    UnexpectedSequence { expected: u64, actual: u64 },
    #[error("event {sequence} previous digest mismatch")]
    PreviousDigestMismatch {
        sequence: u64,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const RUN_ID: &str = "run-1";
    const AUTHORITY: &str = "local:test";
    const AUTHORITY_EPOCH: u64 = 7;

    type IntentMutation = Box<dyn Fn(&mut EffectIntent)>;

    fn event(event_id: &str, body: RunEventBody) -> RunEvent {
        RunEvent {
            event_id: event_id.to_owned(),
            run_id: RUN_ID.to_owned(),
            authority: AUTHORITY.to_owned(),
            authority_epoch: AUTHORITY_EPOCH,
            recorded_at: "2026-08-28T00:00:00Z".to_owned(),
            body,
        }
    }

    fn append(
        records: &mut Vec<EventRecord>,
        state: Option<&RunState>,
        event: RunEvent,
    ) -> RunState {
        let previous = records.last();
        let record = EventRecord::next(previous, event).expect("record should be canonicalizable");
        let state = apply_record(state, &record).expect("transition should be valid");
        records.push(record);
        state
    }

    fn intent(state: &RunState, step_id: &str, effect_id: &str, max_uses: u32) -> RunEventBody {
        let action_digest = format!("sha256:action-{effect_id}");
        let invocation = InvocationBinding {
            capability_id: "test.effect".to_owned(),
            contract_version: "1.0.0".to_owned(),
            definition_digest: "sha256:definition-1".to_owned(),
            instance_id: "test.instance".to_owned(),
            instance_binding_digest: "sha256:instance-1".to_owned(),
        };
        let binding = AuthorizationBinding {
            run_id: state.run_id.clone(),
            step_id: step_id.to_owned(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            issued_at_sequence: state.journal_sequence,
            issued_at_head_digest: state.journal_head_digest.clone(),
            capability_id: invocation.capability_id.clone(),
            contract_version: invocation.contract_version.clone(),
            definition_digest: invocation.definition_digest.clone(),
            instance_id: invocation.instance_id.clone(),
            instance_binding_digest: invocation.instance_binding_digest.clone(),
            action_digest: action_digest.clone(),
            policy_evidence_digest: "sha256:policy-1".to_owned(),
        };
        let grant_digest =
            authorization_digest(&binding, max_uses).expect("authorization should canonicalize");
        let grant_id = once_authorization_id(&binding.run_id, &binding.action_digest)
            .expect("authorization ID should canonicalize");
        RunEventBody::EffectIntentCommitted {
            step_id: step_id.to_owned(),
            intent: Box::new(EffectIntent {
                effect_id: effect_id.to_owned(),
                action_digest,
                invocation,
                effect_class: EffectClass::NonIdempotent,
                idempotency_key: None,
                sink_guarantee: SinkGuarantee::None,
                authorization: AuthorizationUse {
                    grant_id,
                    grant_digest,
                    max_uses,
                    binding,
                },
            }),
        }
    }

    #[test]
    fn durable_effect_happy_path_is_replayable() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event(
                "event-1",
                RunEventBody::RunCreated {
                    goal: "change one file safely".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".to_owned(),
                    objective: "write file".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-4",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".to_owned(),
                    effect_id: "effect-1".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-5",
                RunEventBody::EffectSucceeded {
                    step_id: "step-1".to_owned(),
                    effect_id: "effect-1".to_owned(),
                    receipt_digest: "sha256:receipt-1".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-6",
                RunEventBody::VerificationPassed {
                    step_id: "step-1".to_owned(),
                },
            ),
        );

        assert_eq!(state.steps["step-1"].status, StepStatus::Completed);
        assert_eq!(state.authorization_consumption.len(), 1);
        assert_eq!(
            state
                .authorization_consumption
                .values()
                .next()
                .expect("one authorization should exist")
                .uses,
            1
        );
        assert_eq!(replay(&records).expect("replay should pass"), state);
        assert_eq!(replay(&records).expect("second replay should pass"), state);
    }

    #[test]
    fn unknown_non_idempotent_effect_cannot_be_blindly_retried() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        for (event_id, body) in [
            (
                "event-4",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                },
            ),
            (
                "event-5",
                RunEventBody::EffectBecameUnknown {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                    reason: "ack lost".into(),
                },
            ),
        ] {
            state = append(&mut records, Some(&state), event(event_id, body));
        }

        let retry = EventRecord::next(
            records.last(),
            event(
                "event-6",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &retry),
            Err(TransitionError::InvalidStepTransition { .. })
        ));
    }

    #[test]
    fn proved_not_applied_allows_same_intent_to_resume() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        let bodies = [
            RunEventBody::EffectExecutionStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
            RunEventBody::EffectBecameUnknown {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
                reason: "timeout".into(),
            },
            RunEventBody::ReconciliationStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
            RunEventBody::ReconciliationResolved {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
                resolution: ReconciliationResolution::ProvedNotApplied,
                evidence_digest: "sha256:evidence-1".into(),
            },
            RunEventBody::EffectExecutionStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
        ];
        for (index, body) in bodies.into_iter().enumerate() {
            state = append(
                &mut records,
                Some(&state),
                event(&format!("event-{}", index + 4), body),
            );
        }

        assert_eq!(state.steps["step-1"].status, StepStatus::Executing);
        assert_eq!(state.steps["step-1"].attempts, 2);
        assert_eq!(state.authorization_consumption.len(), 1);
        assert_eq!(
            state
                .authorization_consumption
                .values()
                .next()
                .expect("one authorization should exist")
                .uses,
            1
        );
    }

    #[test]
    fn one_shot_authorization_cannot_be_rebound_to_another_step() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        for (event_id, step_id) in [("event-2", "step-1"), ("event-3", "step-2")] {
            state = append(
                &mut records,
                Some(&state),
                event(
                    event_id,
                    RunEventBody::StepPlanned {
                        step_id: step_id.into(),
                        objective: step_id.into(),
                    },
                ),
            );
        }
        state = append(
            &mut records,
            Some(&state),
            event("event-4", intent(&state, "step-1", "effect-1", 1)),
        );
        let RunEventBody::EffectIntentCommitted {
            step_id,
            mut intent,
        } = intent(&state, "step-2", "effect-2", 1)
        else {
            unreachable!("intent helper always creates an effect intent")
        };
        intent.action_digest = "sha256:action-effect-1".to_owned();
        intent.authorization.binding.action_digest = intent.action_digest.clone();
        intent.authorization.grant_id = once_authorization_id(
            &intent.authorization.binding.run_id,
            &intent.authorization.binding.action_digest,
        )
        .expect("authorization ID should canonicalize");
        intent.authorization.grant_digest =
            authorization_digest(&intent.authorization.binding, intent.authorization.max_uses)
                .expect("authorization should canonicalize");
        let rebound = EventRecord::next(
            records.last(),
            event(
                "event-5",
                RunEventBody::EffectIntentCommitted { step_id, intent },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &rebound),
            Err(TransitionError::AuthorizationGrantChanged { .. })
        ));
    }

    #[test]
    fn durable_authorization_binding_mismatches_fail_without_mutating_state() {
        let mut records = Vec::new();
        let created = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let state = append(
            &mut records,
            Some(&created),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        let original = state.clone();

        let mutations: Vec<IntentMutation> = vec![
            Box::new(|intent| intent.authorization.binding.run_id = "other-run".into()),
            Box::new(|intent| intent.authorization.binding.step_id = "other-step".into()),
            Box::new(|intent| intent.authorization.binding.authority = "other:authority".into()),
            Box::new(|intent| intent.authorization.binding.authority_epoch += 1),
            Box::new(|intent| intent.authorization.binding.issued_at_sequence += 1),
            Box::new(|intent| {
                intent.authorization.binding.issued_at_head_digest = "sha256:other-head".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.action_digest = "sha256:other-action".into();
            }),
            Box::new(|intent| intent.invocation.capability_id = "other.capability".into()),
            Box::new(|intent| intent.invocation.contract_version = "2.0.0".into()),
            Box::new(|intent| {
                intent.invocation.definition_digest = "sha256:other-definition".into();
            }),
            Box::new(|intent| intent.invocation.instance_id = "other.instance".into()),
            Box::new(|intent| {
                intent.invocation.instance_binding_digest = "sha256:other-binding".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.policy_evidence_digest = "sha256:other-policy".into();
            }),
            Box::new(|intent| intent.authorization.grant_id = "authorization-forged".into()),
            Box::new(|intent| {
                intent.authorization.max_uses = 2;
                intent.authorization.grant_digest = authorization_digest(
                    &intent.authorization.binding,
                    intent.authorization.max_uses,
                )
                .expect("authorization should canonicalize");
            }),
            Box::new(|intent| intent.authorization.grant_digest = "sha256:forged".into()),
        ];

        for (index, mutate) in mutations.into_iter().enumerate() {
            let RunEventBody::EffectIntentCommitted {
                step_id,
                mut intent,
            } = intent(&state, "step-1", "effect-1", 1)
            else {
                unreachable!("intent helper always creates an effect intent")
            };
            mutate(&mut intent);
            let record = EventRecord::next(
                records.last(),
                event(
                    &format!("invalid-event-{index}"),
                    RunEventBody::EffectIntentCommitted { step_id, intent },
                ),
            )
            .expect("record should build");

            assert!(apply_record(Some(&state), &record).is_err());
            assert_eq!(state, original);
        }
    }

    #[test]
    fn replay_rejects_a_broken_hash_chain() {
        let mut records = Vec::new();
        let state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let _state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        records[1].previous_digest = Some("sha256:tampered".into());

        assert!(matches!(
            replay(&records),
            Err(ReplayError::PreviousDigestMismatch { sequence: 2, .. })
        ));
    }

    #[test]
    fn keyed_sink_guarantee_requires_an_actual_key() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        let RunEventBody::EffectIntentCommitted {
            step_id,
            mut intent,
        } = intent(&state, "step-1", "effect-1", 1)
        else {
            unreachable!("intent helper always returns an intent event");
        };
        intent.sink_guarantee = SinkGuarantee::QueryByKey;
        let invalid = EventRecord::next(
            records.last(),
            event(
                "event-3",
                RunEventBody::EffectIntentCommitted { step_id, intent },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &invalid),
            Err(TransitionError::SinkGuaranteeRequiresIdempotencyKey { .. })
        ));
    }

    #[test]
    fn authority_epoch_change_is_fenced() {
        let mut records = Vec::new();
        let current_projection = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let mut stale_event = event(
            "event-2",
            RunEventBody::StepPlanned {
                step_id: "step-1".into(),
                objective: "o".into(),
            },
        );
        stale_event.authority_epoch -= 1;
        let stale_record =
            EventRecord::next(records.last(), stale_event).expect("record should build");

        assert_eq!(
            apply_record(Some(&current_projection), &stale_record),
            Err(TransitionError::AuthorityEpochMismatch)
        );
    }

    #[test]
    fn event_sequence_overflow_is_rejected() {
        let previous = EventRecord {
            sequence: u64::MAX,
            previous_digest: None,
            event: event("event-max", RunEventBody::RunCreated { goal: "g".into() }),
            digest: "sha256:max".into(),
        };

        assert_eq!(
            EventRecord::next(
                Some(&previous),
                event(
                    "event-overflow",
                    RunEventBody::StepPlanned {
                        step_id: "step-1".into(),
                        objective: "o".into(),
                    },
                ),
            ),
            Err(RecordError::SequenceOverflow)
        );
    }

    fn planned_journal(step_count: u8) -> (Vec<EventRecord>, RunState) {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        for index in 0..step_count {
            state = append(
                &mut records,
                Some(&state),
                event(
                    &format!("event-{}", u16::from(index) + 2),
                    RunEventBody::StepPlanned {
                        step_id: format!("step-{index}"),
                        objective: format!("objective-{index}"),
                    },
                ),
            );
        }
        (records, state)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn replay_matches_incremental_reduction_for_unique_plans(step_count in 0_u8..32) {
            let (records, incremental) = planned_journal(step_count);
            prop_assert_eq!(replay(&records), Ok(incremental));
        }

        #[test]
        fn mutation_of_any_committed_event_is_detected(
            step_count in 0_u8..32,
            selected in any::<usize>(),
        ) {
            let (mut records, _) = planned_journal(step_count);
            let index = selected % records.len();
            records[index].event.event_id.push_str("-tampered");

            let detected = matches!(
                replay(&records),
                Err(ReplayError::Record(RecordError::DigestMismatch { .. }))
            );
            prop_assert!(detected);
        }
    }
}
