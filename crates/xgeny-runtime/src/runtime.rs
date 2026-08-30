use std::num::NonZeroU32;

use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use xgeny_local_store::{Commit, ExpectedHead, RunStore, StoreError};
use xgeny_workgraph::{
    EffectIntent, InvocationMaterialRecord, ReconciliationResolution, RunEvent, RunEventBody,
    RunState, SinkGuarantee, StepState, StepStatus, TOOL_OUTPUT_PROFILE_V1, ToolOutputRecord,
};

use crate::RunLease;

pub trait EventFactory {
    /// Create metadata for one event without changing durable state.
    ///
    /// # Errors
    ///
    /// Returns an error when a unique event identifier or timestamp cannot be produced.
    fn create_metadata(&mut self, state: &RunState) -> Result<EventMetadata, EventFactoryError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventMetadata {
    pub event_id: String,
    pub recorded_at: String,
}

impl EventMetadata {
    pub(crate) fn validate(&self) -> Result<(), EventMetadataError> {
        OffsetDateTime::parse(&self.recorded_at, &Rfc3339)
            .map(|_| ())
            .map_err(|_| EventMetadataError)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("event metadata is invalid")]
pub struct EventMetadataError;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("event metadata creation failed: {message}")]
pub struct EventFactoryError {
    message: String,
}

impl EventFactoryError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Effect adapter boundary.
///
/// Implementations must classify transport errors after a request may have been sent as
/// [`ExecutionObservation::Unknown`], never as a definite failure. `reconcile` must be a
/// read-only query safe to repeat after process restart.
pub(crate) trait EffectSink {
    type Prepared: PreparedEffect;

    fn execute(&mut self, intent: &EffectIntent, prepared: Self::Prepared) -> ExecutionObservation;
    fn reconcile(&mut self, intent: &EffectIntent) -> ReconciliationObservation;
}

/// Core-owned identity for one prepared, consume-once adapter invocation.
///
/// This type and its constructor are crate-private so adapters cannot self-report or forge the
/// durable identity they are about to execute.
pub(crate) struct PreparedEffectBinding {
    run_id: String,
    authority: String,
    authority_epoch: u64,
    journal_sequence: u64,
    journal_head_digest: String,
    step_id: String,
    effect_id: String,
    material_record: InvocationMaterialRecord,
}

impl PreparedEffectBinding {
    pub(crate) fn from_verified(
        state: &RunState,
        step_id: &str,
        intent: &EffectIntent,
        material_record: InvocationMaterialRecord,
    ) -> Self {
        Self {
            run_id: state.run_id.clone(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            journal_sequence: state.journal_sequence,
            journal_head_digest: state.journal_head_digest.clone(),
            step_id: step_id.to_owned(),
            effect_id: intent.effect_id.clone(),
            material_record,
        }
    }

    #[cfg(test)]
    pub(crate) fn corrupt_step_id_for_test(&mut self) {
        self.step_id.push_str("-different");
    }

    #[cfg(test)]
    pub(crate) fn corrupt_effect_id_for_test(&mut self) {
        self.effect_id.push_str("-different");
    }
}

impl std::fmt::Debug for PreparedEffectBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedEffectBinding")
            .field("run_id", &self.run_id)
            .field("authority", &self.authority)
            .field("authority_epoch", &self.authority_epoch)
            .field("journal_sequence", &self.journal_sequence)
            .field("journal_head_digest", &self.journal_head_digest)
            .field("step_id", &self.step_id)
            .field("effect_id", &self.effect_id)
            .field("material_record", &self.material_record)
            .finish()
    }
}

pub(crate) trait PreparedEffect {
    fn binding(&self) -> &PreparedEffectBinding;
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ToolOutputCandidate(serde_json::Value);

impl ToolOutputCandidate {
    pub(crate) const fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    fn into_value(self) -> serde_json::Value {
        self.0
    }
}

impl std::fmt::Debug for ToolOutputCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ToolOutputCandidate(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExecutionObservation {
    Succeeded {
        evidence_digest: String,
    },
    SucceededWithOutput {
        evidence_digest: String,
        output: ToolOutputCandidate,
    },
    Failed {
        evidence_digest: String,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReconciliationObservation {
    Applied { evidence_digest: String },
    NotApplied { evidence_digest: String },
    Failed { evidence_digest: String },
    Inconclusive { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveAction {
    EffectSucceeded,
    EffectFailed,
    EffectUnknown,
    ExecutionRecoveredAsUnknown,
    ReconciliationApplied,
    ReconciliationNotApplied,
    ReconciliationFailed,
    ManualRequired,
    VerificationPassed,
    VerificationFailed,
    VerificationInconclusive,
    NoAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveReport {
    pub action: DriveAction,
    pub state: RunState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimePolicy {
    max_execution_attempts: NonZeroU32,
}

impl RuntimePolicy {
    #[must_use]
    pub const fn new(max_execution_attempts: NonZeroU32) -> Self {
        Self {
            max_execution_attempts,
        }
    }

    #[must_use]
    pub const fn max_execution_attempts(self) -> u32 {
        self.max_execution_attempts.get()
    }
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self::new(NonZeroU32::new(3).expect("three is non-zero"))
    }
}

pub(crate) struct DurableEffectRuntime<'a, S, E, F, L> {
    store: &'a mut S,
    sink: &'a mut E,
    events: &'a mut F,
    lease: &'a L,
    policy: RuntimePolicy,
}

impl<'a, S, E, F, L> DurableEffectRuntime<'a, S, E, F, L>
where
    S: RunStore,
    E: EffectSink,
    F: EventFactory,
    L: RunLease,
{
    #[must_use]
    pub(crate) fn new(store: &'a mut S, sink: &'a mut E, events: &'a mut F, lease: &'a L) -> Self {
        Self {
            store,
            sink,
            events,
            lease,
            policy: RuntimePolicy::default(),
        }
    }

    #[must_use]
    pub(crate) const fn with_policy(mut self, policy: RuntimePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Advance one step from the durable state observed at method entry.
    ///
    /// An `IntentCommitted` step records `EffectExecutionStarted` before invoking the sink. An
    /// `Executing` step observed at entry is always treated as recovery and is never executed.
    ///
    /// # Errors
    ///
    /// Returns an error for missing Run/step/intent, lease mismatch, event creation, or store
    /// failures. A store failure after the sink call intentionally leaves `Executing` for the
    /// next recovery pass.
    pub(crate) fn drive_step(
        &mut self,
        step_id: &str,
        prepared: Option<E::Prepared>,
    ) -> Result<DriveReport, RuntimeError> {
        let state = self
            .store
            .load_current()?
            .ok_or(RuntimeError::RunNotInitialized)?;
        self.verify_lease(&state)?;
        let step = state
            .steps
            .get(step_id)
            .cloned()
            .ok_or_else(|| RuntimeError::StepNotFound(step_id.to_owned()))?;

        match step.status {
            StepStatus::IntentCommitted => self.execute_intent(&state, &step, prepared),
            StepStatus::Executing => self.mark_recovered_execution_unknown(&state, &step),
            StepStatus::EffectUnknown => self.begin_reconciliation(&state, &step),
            StepStatus::Reconciling => self.finish_reconciliation(&state, &step),
            StepStatus::Planned
            | StepStatus::Validating
            | StepStatus::Completed
            | StepStatus::Failed
            | StepStatus::ManualRequired => Ok(DriveReport {
                action: DriveAction::NoAction,
                state,
            }),
        }
    }

    fn verify_lease(&self, state: &RunState) -> Result<(), RuntimeError> {
        if self.lease.run_id() != state.run_id {
            return Err(RuntimeError::LeaseRunMismatch {
                lease_run_id: self.lease.run_id().to_owned(),
                state_run_id: state.run_id.clone(),
            });
        }
        Ok(())
    }

    fn execute_intent(
        &mut self,
        state: &RunState,
        step: &StepState,
        prepared: Option<E::Prepared>,
    ) -> Result<DriveReport, RuntimeError> {
        let intent = require_intent(step)?;
        if step.attempts >= self.policy.max_execution_attempts.get() {
            return Err(RuntimeError::ExecutionAttemptLimitReached {
                step_id: step.step_id.clone(),
                attempts: step.attempts,
                maximum: self.policy.max_execution_attempts.get(),
            });
        }
        let prepared = prepared.ok_or_else(|| RuntimeError::PreparedEffectRequired {
            step_id: step.step_id.clone(),
        })?;
        verify_prepared_invocation(state, step, &intent, prepared.binding())?;
        let current_record = self
            .store
            .load_invocation_material(&intent.effect_id)?
            .ok_or_else(|| RuntimeError::PreparedMaterialRecordMissing {
                effect_id: intent.effect_id.clone(),
            })?;
        if current_record != prepared.binding().material_record {
            return Err(RuntimeError::PreparedMaterialRecordChanged {
                step_id: step.step_id.clone(),
            });
        }
        let started = self.append_event(
            state,
            RunEventBody::EffectExecutionStarted {
                step_id: step.step_id.clone(),
                effect_id: intent.effect_id.clone(),
            },
        )?;
        let observation = self.sink.execute(&intent, prepared);
        let output_required = intent
            .receipt_provenance
            .as_ref()
            .and_then(|provenance| provenance.tool_output_profile.as_deref())
            == Some(TOOL_OUTPUT_PROFILE_V1);
        let pending = classify_execution_observation(
            &started.state,
            step,
            &intent,
            output_required,
            observation,
        )?;
        if let Some(output) = pending.output {
            self.commit_tool_output_report(&started.state, pending.body, pending.action, output)
        } else {
            self.commit_report(&started.state, pending.body, pending.action)
        }
    }

    fn mark_recovered_execution_unknown(
        &mut self,
        state: &RunState,
        step: &StepState,
    ) -> Result<DriveReport, RuntimeError> {
        let intent = require_intent(step)?;
        self.commit_report(
            state,
            RunEventBody::EffectBecameUnknown {
                step_id: step.step_id.clone(),
                effect_id: intent.effect_id,
                reason: "runtime resumed with no committed effect outcome".to_owned(),
            },
            DriveAction::ExecutionRecoveredAsUnknown,
        )
    }

    fn begin_reconciliation(
        &mut self,
        state: &RunState,
        step: &StepState,
    ) -> Result<DriveReport, RuntimeError> {
        let intent = require_intent(step)?;
        if !supports_query(intent.sink_guarantee) {
            return self.commit_report(
                state,
                RunEventBody::ManualInterventionRequired {
                    step_id: step.step_id.clone(),
                    effect_id: intent.effect_id,
                    reason:
                        "sink cannot query the effect by its stable key; blind retry is disabled"
                            .to_owned(),
                },
                DriveAction::ManualRequired,
            );
        }
        let reconciling = self.append_event(
            state,
            RunEventBody::ReconciliationStarted {
                step_id: step.step_id.clone(),
                effect_id: intent.effect_id,
            },
        )?;
        let reconciling_step = reconciling
            .state
            .steps
            .get(&step.step_id)
            .cloned()
            .ok_or_else(|| RuntimeError::StepNotFound(step.step_id.clone()))?;
        self.finish_reconciliation(&reconciling.state, &reconciling_step)
    }

    fn finish_reconciliation(
        &mut self,
        state: &RunState,
        step: &StepState,
    ) -> Result<DriveReport, RuntimeError> {
        let intent = require_intent(step)?;
        if !supports_query(intent.sink_guarantee) {
            return self.commit_report(
                state,
                RunEventBody::ManualInterventionRequired {
                    step_id: step.step_id.clone(),
                    effect_id: intent.effect_id,
                    reason: "reconciliation state has no query-capable sink contract".to_owned(),
                },
                DriveAction::ManualRequired,
            );
        }
        let observation = self.sink.reconcile(&intent);
        let (body, action) = match observation {
            ReconciliationObservation::Applied { evidence_digest } => {
                let evidence_digest = require_observation_value(
                    "reconciliation_applied",
                    "evidence_digest",
                    evidence_digest,
                )?;
                (
                    reconciliation_resolution(
                        step,
                        &intent,
                        ReconciliationResolution::ProvedApplied,
                        evidence_digest,
                    ),
                    DriveAction::ReconciliationApplied,
                )
            }
            ReconciliationObservation::NotApplied { evidence_digest } => {
                let evidence_digest = require_observation_value(
                    "reconciliation_not_applied",
                    "evidence_digest",
                    evidence_digest,
                )?;
                (
                    reconciliation_resolution(
                        step,
                        &intent,
                        ReconciliationResolution::ProvedNotApplied,
                        evidence_digest,
                    ),
                    DriveAction::ReconciliationNotApplied,
                )
            }
            ReconciliationObservation::Failed { evidence_digest } => {
                let evidence_digest = require_observation_value(
                    "reconciliation_failed",
                    "evidence_digest",
                    evidence_digest,
                )?;
                (
                    reconciliation_resolution(
                        step,
                        &intent,
                        ReconciliationResolution::Failed,
                        evidence_digest,
                    ),
                    DriveAction::ReconciliationFailed,
                )
            }
            ReconciliationObservation::Inconclusive { reason } => {
                let reason =
                    require_observation_value("reconciliation_inconclusive", "reason", reason)?;
                (
                    RunEventBody::ManualInterventionRequired {
                        step_id: step.step_id.clone(),
                        effect_id: intent.effect_id,
                        reason,
                    },
                    DriveAction::ManualRequired,
                )
            }
        };
        self.commit_report(state, body, action)
    }

    fn append_event(
        &mut self,
        state: &RunState,
        body: RunEventBody,
    ) -> Result<Commit, RuntimeError> {
        let metadata = self.events.create_metadata(state)?;
        metadata.validate()?;
        let event = RunEvent {
            event_id: metadata.event_id,
            run_id: state.run_id.clone(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            recorded_at: metadata.recorded_at,
            body,
        };
        self.store
            .append(ExpectedHead::from_state(state), event)
            .map_err(RuntimeError::from)
    }

    fn commit_report(
        &mut self,
        state: &RunState,
        body: RunEventBody,
        action: DriveAction,
    ) -> Result<DriveReport, RuntimeError> {
        let commit = self.append_event(state, body)?;
        Ok(DriveReport {
            action,
            state: commit.state,
        })
    }

    fn commit_tool_output_report(
        &mut self,
        state: &RunState,
        body: RunEventBody,
        action: DriveAction,
        output: ToolOutputRecord,
    ) -> Result<DriveReport, RuntimeError> {
        let metadata = self.events.create_metadata(state)?;
        metadata.validate()?;
        let event = RunEvent {
            event_id: metadata.event_id,
            run_id: state.run_id.clone(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            recorded_at: metadata.recorded_at,
            body,
        };
        let commit =
            self.store
                .append_with_tool_output(ExpectedHead::from_state(state), event, output)?;
        Ok(DriveReport {
            action,
            state: commit.state,
        })
    }
}

struct PendingExecutionCommit {
    body: RunEventBody,
    action: DriveAction,
    output: Option<ToolOutputRecord>,
}

impl PendingExecutionCommit {
    fn succeeded(
        step: &StepState,
        intent: &EffectIntent,
        evidence_digest: String,
        output: Option<ToolOutputRecord>,
    ) -> Self {
        let output_record_digest = output
            .as_ref()
            .map(|record| record.record_digest().to_owned());
        Self {
            body: RunEventBody::EffectSucceeded {
                step_id: step.step_id.clone(),
                effect_id: intent.effect_id.clone(),
                evidence_digest,
                output_record_digest,
            },
            action: DriveAction::EffectSucceeded,
            output,
        }
    }

    fn failed(step: &StepState, intent: &EffectIntent, evidence_digest: String) -> Self {
        Self {
            body: RunEventBody::EffectFailed {
                step_id: step.step_id.clone(),
                effect_id: intent.effect_id.clone(),
                evidence_digest,
            },
            action: DriveAction::EffectFailed,
            output: None,
        }
    }

    fn unknown(step: &StepState, intent: &EffectIntent, reason: impl Into<String>) -> Self {
        Self {
            body: RunEventBody::EffectBecameUnknown {
                step_id: step.step_id.clone(),
                effect_id: intent.effect_id.clone(),
                reason: reason.into(),
            },
            action: DriveAction::EffectUnknown,
            output: None,
        }
    }
}

fn classify_execution_observation(
    started: &RunState,
    step: &StepState,
    intent: &EffectIntent,
    output_required: bool,
    observation: ExecutionObservation,
) -> Result<PendingExecutionCommit, RuntimeError> {
    match observation {
        ExecutionObservation::Succeeded { evidence_digest } => {
            let evidence_digest = require_observation_value(
                "execution_succeeded",
                "evidence_digest",
                evidence_digest,
            )?;
            if output_required {
                Ok(PendingExecutionCommit::unknown(
                    step,
                    intent,
                    "adapter returned no durable tool output",
                ))
            } else {
                Ok(PendingExecutionCommit::succeeded(
                    step,
                    intent,
                    evidence_digest,
                    None,
                ))
            }
        }
        ExecutionObservation::SucceededWithOutput {
            evidence_digest,
            output,
        } => classify_output_success(
            started,
            step,
            intent,
            output_required,
            evidence_digest,
            output,
        ),
        ExecutionObservation::Failed { evidence_digest } => {
            let evidence_digest =
                require_observation_value("execution_failed", "evidence_digest", evidence_digest)?;
            Ok(PendingExecutionCommit::failed(
                step,
                intent,
                evidence_digest,
            ))
        }
        ExecutionObservation::Unknown { reason } => {
            let reason = require_observation_value("execution_unknown", "reason", reason)?;
            Ok(PendingExecutionCommit::unknown(step, intent, reason))
        }
    }
}

fn classify_output_success(
    started: &RunState,
    step: &StepState,
    intent: &EffectIntent,
    output_required: bool,
    evidence_digest: String,
    output: ToolOutputCandidate,
) -> Result<PendingExecutionCommit, RuntimeError> {
    let evidence_digest =
        require_observation_value("execution_succeeded", "evidence_digest", evidence_digest)?;
    let execution_attempt = started
        .steps
        .get(&step.step_id)
        .map(|step| step.attempts)
        .ok_or_else(|| RuntimeError::StepNotFound(step.step_id.clone()))?;
    if !output_required {
        return Ok(PendingExecutionCommit::unknown(
            step,
            intent,
            "adapter returned an unexpected tool output",
        ));
    }
    match ToolOutputRecord::new(
        &started.run_id,
        &step.step_id,
        intent,
        execution_attempt,
        &evidence_digest,
        output.into_value(),
    ) {
        Ok(record) => Ok(PendingExecutionCommit::succeeded(
            step,
            intent,
            evidence_digest,
            Some(record),
        )),
        Err(_) => Ok(PendingExecutionCommit::unknown(
            step,
            intent,
            "adapter tool output is invalid",
        )),
    }
}

fn verify_prepared_invocation(
    state: &RunState,
    step: &StepState,
    intent: &EffectIntent,
    binding: &PreparedEffectBinding,
) -> Result<(), RuntimeError> {
    if binding.run_id != state.run_id
        || binding.authority != state.authority
        || binding.authority_epoch != state.authority_epoch
        || binding.journal_sequence != state.journal_sequence
        || binding.journal_head_digest != state.journal_head_digest
    {
        return Err(RuntimeError::PreparedEffectHeadChanged {
            step_id: step.step_id.clone(),
        });
    }
    if binding.step_id != step.step_id || binding.effect_id != intent.effect_id {
        return Err(RuntimeError::PreparedEffectBindingMismatch {
            step_id: step.step_id.clone(),
        });
    }
    binding
        .material_record
        .verify_for(&state.run_id, &step.step_id, intent)
        .map_err(RuntimeError::PreparedMaterialRecordInvalid)?;
    Ok(())
}

fn require_intent(step: &StepState) -> Result<EffectIntent, RuntimeError> {
    step.intent
        .clone()
        .ok_or_else(|| RuntimeError::IntentMissing(step.step_id.clone()))
}

const fn supports_query(guarantee: SinkGuarantee) -> bool {
    matches!(
        guarantee,
        SinkGuarantee::QueryByKey | SinkGuarantee::DeduplicateAndQuery
    )
}

fn reconciliation_resolution(
    step: &StepState,
    intent: &EffectIntent,
    resolution: ReconciliationResolution,
    evidence_digest: String,
) -> RunEventBody {
    RunEventBody::ReconciliationResolved {
        step_id: step.step_id.clone(),
        effect_id: intent.effect_id.clone(),
        resolution,
        evidence_digest,
    }
}

fn require_observation_value(
    observation: &'static str,
    field: &'static str,
    value: String,
) -> Result<String, RuntimeError> {
    if value.trim().is_empty() {
        return Err(RuntimeError::InvalidSinkObservation { observation, field });
    }
    Ok(value)
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    EventFactory(#[from] EventFactoryError),
    #[error(transparent)]
    EventMetadata(#[from] EventMetadataError),
    #[error("durable Run is not initialized")]
    RunNotInitialized,
    #[error("step `{0}` does not exist")]
    StepNotFound(String),
    #[error("step `{0}` has an effect status but no committed intent")]
    IntentMissing(String),
    #[error("step `{step_id}` needs prepared effect material before execution")]
    PreparedEffectRequired { step_id: String },
    #[error("step `{step_id}` prepared effect was created for a different journal head")]
    PreparedEffectHeadChanged { step_id: String },
    #[error("step `{step_id}` prepared effect is bound to another Step or effect")]
    PreparedEffectBindingMismatch { step_id: String },
    #[error("prepared effect material record is invalid")]
    PreparedMaterialRecordInvalid(xgeny_workgraph::InvocationMaterialError),
    #[error("effect `{effect_id}` has no invocation material descriptor")]
    PreparedMaterialRecordMissing { effect_id: String },
    #[error("step `{step_id}` invocation material changed after adapter preparation")]
    PreparedMaterialRecordChanged { step_id: String },
    #[error(
        "step `{step_id}` reached its execution attempt limit: {attempts} of {maximum} attempts"
    )]
    ExecutionAttemptLimitReached {
        step_id: String,
        attempts: u32,
        maximum: u32,
    },
    #[error("sink observation `{observation}` has an empty `{field}`")]
    InvalidSinkObservation {
        observation: &'static str,
        field: &'static str,
    },
    #[error("lease is for Run `{lease_run_id}`, but durable state is `{state_run_id}`")]
    LeaseRunMismatch {
        lease_run_id: String,
        state_run_id: String,
    },
}
