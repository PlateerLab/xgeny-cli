use std::num::NonZeroU32;

use thiserror::Error;
use xgeny_local_store::{Commit, ExpectedHead, RunStore, StoreError};
use xgeny_workgraph::{
    EffectIntent, ReconciliationResolution, RunEvent, RunEventBody, RunState, SinkGuarantee,
    StepState, StepStatus,
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
pub trait EffectSink {
    type Prepared: PreparedEffect;

    fn execute(&mut self, intent: &EffectIntent, prepared: &Self::Prepared)
    -> ExecutionObservation;
    fn reconcile(&mut self, intent: &EffectIntent) -> ReconciliationObservation;
}

/// Ephemeral, adapter-owned executable material.
///
/// Raw arguments and resolved credentials remain outside the journal. The digest must identify
/// the same canonical semantic action committed in [`EffectIntent::action_digest`].
pub trait PreparedEffect {
    fn action_digest(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionObservation {
    Succeeded { receipt_digest: String },
    Failed { receipt_digest: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationObservation {
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
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self::new(NonZeroU32::new(3).expect("three is non-zero"))
    }
}

pub struct DurableEffectRuntime<'a, S, E, F, L> {
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
    pub fn new(store: &'a mut S, sink: &'a mut E, events: &'a mut F, lease: &'a L) -> Self {
        Self {
            store,
            sink,
            events,
            lease,
            policy: RuntimePolicy::default(),
        }
    }

    #[must_use]
    pub const fn with_policy(mut self, policy: RuntimePolicy) -> Self {
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
    pub fn drive_step(
        &mut self,
        step_id: &str,
        prepared: Option<&E::Prepared>,
    ) -> Result<DriveReport, RuntimeError> {
        let snapshot = self.store.load()?.ok_or(RuntimeError::RunNotInitialized)?;
        self.verify_lease(&snapshot.state)?;
        let step = snapshot
            .state
            .steps
            .get(step_id)
            .cloned()
            .ok_or_else(|| RuntimeError::StepNotFound(step_id.to_owned()))?;

        match step.status {
            StepStatus::IntentCommitted => self.execute_intent(&snapshot.state, &step, prepared),
            StepStatus::Executing => self.mark_recovered_execution_unknown(&snapshot.state, &step),
            StepStatus::EffectUnknown => self.begin_reconciliation(&snapshot.state, &step),
            StepStatus::Reconciling => self.finish_reconciliation(&snapshot.state, &step),
            StepStatus::Planned
            | StepStatus::Validating
            | StepStatus::Completed
            | StepStatus::Failed
            | StepStatus::ManualRequired => Ok(DriveReport {
                action: DriveAction::NoAction,
                state: snapshot.state,
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
        prepared: Option<&E::Prepared>,
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
        if prepared.action_digest() != intent.action_digest {
            return Err(RuntimeError::PreparedEffectDigestMismatch {
                step_id: step.step_id.clone(),
                expected: intent.action_digest,
                actual: prepared.action_digest().to_owned(),
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
        let (body, action) = match observation {
            ExecutionObservation::Succeeded { receipt_digest } => {
                let receipt_digest = require_observation_value(
                    "execution_succeeded",
                    "receipt_digest",
                    receipt_digest,
                )?;
                (
                    RunEventBody::EffectSucceeded {
                        step_id: step.step_id.clone(),
                        effect_id: intent.effect_id,
                        receipt_digest,
                    },
                    DriveAction::EffectSucceeded,
                )
            }
            ExecutionObservation::Failed { receipt_digest } => {
                let receipt_digest = require_observation_value(
                    "execution_failed",
                    "receipt_digest",
                    receipt_digest,
                )?;
                (
                    RunEventBody::EffectFailed {
                        step_id: step.step_id.clone(),
                        effect_id: intent.effect_id,
                        receipt_digest,
                    },
                    DriveAction::EffectFailed,
                )
            }
            ExecutionObservation::Unknown { reason } => {
                let reason = require_observation_value("execution_unknown", "reason", reason)?;
                (
                    RunEventBody::EffectBecameUnknown {
                        step_id: step.step_id.clone(),
                        effect_id: intent.effect_id,
                        reason,
                    },
                    DriveAction::EffectUnknown,
                )
            }
        };
        self.commit_report(&started.state, body, action)
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
    #[error("durable Run is not initialized")]
    RunNotInitialized,
    #[error("step `{0}` does not exist")]
    StepNotFound(String),
    #[error("step `{0}` has an effect status but no committed intent")]
    IntentMissing(String),
    #[error("step `{step_id}` needs prepared effect material before execution")]
    PreparedEffectRequired { step_id: String },
    #[error(
        "step `{step_id}` prepared effect digest mismatch: expected `{expected}`, got `{actual}`"
    )]
    PreparedEffectDigestMismatch {
        step_id: String,
        expected: String,
        actual: String,
    },
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
