#![doc = "Local storage candidates for durable `XGENy` run events and projections."]

mod memory;
mod sqlite;

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use xgeny_domain::{
    EffectClass as ProtocolEffectClass, ExecutionReceiptBody, Placement, ProtocolDocument,
    VerificationResult, VerificationStrategy,
};
use xgeny_protocol::{
    CORE_RECEIPT_INPUT_SUMMARY_V1, CORE_RECEIPT_PROFILE_V1, CORE_RECEIPT_REDACTIONS_V1,
    CoreVerificationOutcome, ProtocolError, core_receipt_id_v1, core_receipt_status_v1,
    core_verification_summary_v1, evaluate_core_verification_v1, validate_execution_receipt,
};
use xgeny_workgraph::{
    EffectClass, EventRecord, InvocationMaterialError, InvocationMaterialRecord, ReceiptPlacement,
    ReceiptVerificationStrategy, RecordError, ReplayError, RunEvent, RunEventBody, RunState,
    TransitionError, VerificationDisposition, apply_record, replay,
};

pub use memory::MemoryRunStore;
pub use sqlite::SqliteRunStore;

#[cfg(test)]
use sqlite::CommitStage as AppendFault;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedHead {
    Empty,
    Exact { sequence: u64, digest: String },
}

impl ExpectedHead {
    #[must_use]
    pub fn from_state(state: &RunState) -> Self {
        Self::Exact {
            sequence: state.journal_sequence,
            digest: state.journal_head_digest.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub record: EventRecord,
    pub state: RunState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSnapshot {
    pub records: Vec<EventRecord>,
    pub state: RunState,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredExecutionReceipt {
    pub event_sequence: u64,
    pub effect_id: String,
    pub receipt: ExecutionReceiptBody,
}

pub trait RunStore {
    /// Compare-and-append one event and its derived projection atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for stale heads, invalid transitions, serialization, or storage faults.
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError>;

    /// Atomically append one effect intent and its secret-free invocation material descriptor.
    ///
    /// Stores that do not implement the sidecar contract fail closed. New intents also require a
    /// supported durable Receipt provenance profile; its absence is accepted only while loading
    /// legacy journals. Invocation admission must never fall back to plain `append` because that
    /// can consume authorization without retaining a recovery decision.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported stores, mismatched material bindings, stale heads, invalid
    /// transitions, serialization, or storage faults.
    fn append_with_invocation_material(
        &mut self,
        _expected: ExpectedHead,
        _event: RunEvent,
        _material: InvocationMaterialRecord,
    ) -> Result<Commit, StoreError> {
        Err(StoreError::InvocationMaterialStoreUnsupported)
    }

    /// Atomically append one receipt-bound verification event and its complete protocol Receipt.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported stores, detached or invalid Receipts, stale heads,
    /// invalid transitions, serialization, or storage faults.
    fn append_with_execution_receipt(
        &mut self,
        _expected: ExpectedHead,
        _event: RunEvent,
        _receipt: ExecutionReceiptBody,
    ) -> Result<Commit, StoreError> {
        Err(StoreError::ExecutionReceiptStoreUnsupported)
    }

    /// Load and replay-verify all committed data.
    ///
    /// # Errors
    ///
    /// Returns an error if storage cannot be read or its projection differs from replay.
    fn load(&self) -> Result<Option<RunSnapshot>, StoreError>;

    /// Load one verified secret-free descriptor by effect ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the store does not support material records or committed data is
    /// missing, corrupt, or inconsistent with its effect intent.
    fn load_invocation_material(
        &self,
        _effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        Err(StoreError::InvocationMaterialStoreUnsupported)
    }

    /// Load every verified Receipt in journal order.
    ///
    /// # Errors
    ///
    /// Returns an error when the store does not support Receipts or committed data is corrupt.
    fn load_execution_receipts(&self) -> Result<Vec<ExecutionReceiptBody>, StoreError> {
        Err(StoreError::ExecutionReceiptStoreUnsupported)
    }

    /// Load one replay-verified Run and its complete Receipt chain from one logical snapshot.
    ///
    /// Built-in stores override this method to avoid cross-generation reads. The default is for
    /// simple single-writer implementations and still fails closed through both verified loads.
    ///
    /// # Errors
    ///
    /// Returns an error when either the Run or Receipt chain cannot be verified.
    fn load_with_execution_receipts(
        &self,
    ) -> Result<(Option<RunSnapshot>, Vec<ExecutionReceiptBody>), StoreError> {
        Ok((self.load()?, self.load_execution_receipts()?))
    }

    /// Export committed journal records as RFC 8785 canonical JSON Lines in sequence order.
    ///
    /// This backward-compatible stream is journal-only; it is not a complete Run archive.
    ///
    /// # Errors
    ///
    /// Returns an error if loading, verification, or canonical encoding fails.
    fn export_jsonl(&self) -> Result<Vec<u8>, StoreError> {
        let records = self
            .load()?
            .map_or_else(Vec::new, |snapshot| snapshot.records);
        canonical_jsonl(&records)
    }

    /// Export complete protocol `ExecutionReceipt` documents as canonical JSON Lines in journal
    /// order.
    ///
    /// # Errors
    ///
    /// Returns an error if loading, verification, serialization, or canonical encoding fails.
    fn export_execution_receipts_jsonl(&self) -> Result<Vec<u8>, StoreError> {
        let documents: Vec<_> = self
            .load_execution_receipts()?
            .into_iter()
            .map(|receipt| ProtocolDocument::ExecutionReceipt(Box::new(receipt)))
            .collect();
        canonical_jsonl(&documents)
    }
}

fn verify_material_bundle(
    event: &RunEvent,
    material: Option<&InvocationMaterialRecord>,
) -> Result<(), StoreError> {
    if let RunEventBody::EffectIntentCommitted { intent, .. } = &event.body {
        let provenance = intent
            .receipt_provenance
            .as_ref()
            .ok_or(StoreError::ReceiptProvenanceRequired)?;
        if provenance.profile_version != CORE_RECEIPT_PROFILE_V1 {
            return Err(StoreError::UnsupportedReceiptProfile);
        }
    }
    match (&event.body, material) {
        (RunEventBody::EffectIntentCommitted { step_id, intent }, Some(material)) => material
            .verify_for(&event.run_id, step_id, intent)
            .map_err(StoreError::from),
        (RunEventBody::EffectIntentCommitted { .. }, None) => {
            Err(StoreError::InvocationMaterialRequired)
        }
        (_, Some(_)) => Err(StoreError::UnexpectedInvocationMaterial),
        (_, None) => Ok(()),
    }
}

fn verify_material_records(
    records: &[EventRecord],
    materials: &BTreeMap<String, InvocationMaterialRecord>,
) -> Result<(), StoreError> {
    let mut expected_count = 0_usize;
    for record in records {
        let RunEventBody::EffectIntentCommitted { step_id, intent } = &record.event.body else {
            continue;
        };
        expected_count += 1;
        let material = materials.get(&intent.effect_id).ok_or_else(|| {
            StoreError::Corrupt(format!(
                "effect `{}` has no invocation material descriptor",
                intent.effect_id
            ))
        })?;
        material
            .verify_for(&record.event.run_id, step_id, intent)
            .map_err(|error| {
                StoreError::Corrupt(format!(
                    "invocation material for effect `{}` is invalid: {error}",
                    intent.effect_id
                ))
            })?;
    }
    if materials.len() != expected_count {
        return Err(StoreError::Corrupt(format!(
            "invocation material count differs from effect intents: expected {expected_count}, actual {}",
            materials.len()
        )));
    }
    Ok(())
}

fn verify_receipt_bundle(
    event: &RunEvent,
    receipt: Option<&ExecutionReceiptBody>,
) -> Result<(), StoreError> {
    match (&event.body, receipt) {
        (
            RunEventBody::VerificationRecorded {
                step_id,
                receipt_id,
                receipt_digest,
                ..
            },
            Some(receipt),
        ) => {
            validate_execution_receipt(receipt).map_err(|_| StoreError::ExecutionReceiptInvalid)?;
            if receipt.run_id != event.run_id
                || receipt.step_id != *step_id
                || receipt.receipt_id != *receipt_id
                || receipt.receipt_digest != *receipt_digest
            {
                return Err(StoreError::ExecutionReceiptBindingMismatch);
            }
            Ok(())
        }
        (RunEventBody::VerificationRecorded { .. }, None) => {
            Err(StoreError::ExecutionReceiptRequired)
        }
        (
            RunEventBody::VerificationPassed { .. } | RunEventBody::VerificationFailed { .. },
            None,
        ) => Err(StoreError::LegacyVerificationAppendRejected),
        (_, Some(_)) => Err(StoreError::UnexpectedExecutionReceipt),
        (_, None) => Ok(()),
    }
}

fn verify_receipt_records(
    records: &[EventRecord],
    receipts: &[StoredExecutionReceipt],
) -> Result<(), StoreError> {
    let expected: Vec<_> = records
        .iter()
        .filter(|record| matches!(record.event.body, RunEventBody::VerificationRecorded { .. }))
        .collect();
    if expected.len() != receipts.len() {
        return Err(StoreError::Corrupt(format!(
            "execution receipt count differs from finalization events: expected {}, actual {}",
            expected.len(),
            receipts.len()
        )));
    }

    let mut previous_digest: Option<&str> = None;
    for (record, stored) in expected.into_iter().zip(receipts) {
        let RunEventBody::VerificationRecorded {
            step_id,
            effect_id,
            disposition,
            receipt_id,
            receipt_digest,
        } = &record.event.body
        else {
            unreachable!("filtered above")
        };
        let receipt = &stored.receipt;
        validate_execution_receipt(receipt)
            .map_err(|_| StoreError::Corrupt("execution receipt document is invalid".to_owned()))?;
        if stored.event_sequence != record.sequence
            || stored.effect_id != *effect_id
            || receipt.receipt_id != *receipt_id
            || receipt.receipt_digest != *receipt_digest
            || receipt.run_id != record.event.run_id
            || receipt.step_id != *step_id
            || receipt.previous_receipt_digest.as_deref() != previous_digest
        {
            return Err(StoreError::Corrupt(
                "execution receipt differs from its journal binding".to_owned(),
            ));
        }

        let (intent_record, intent) = records
            .iter()
            .find_map(|candidate| {
                let RunEventBody::EffectIntentCommitted {
                    step_id: intent_step_id,
                    intent,
                } = &candidate.event.body
                else {
                    return None;
                };
                (intent.effect_id == *effect_id && intent_step_id == step_id)
                    .then_some((candidate, intent.as_ref()))
            })
            .ok_or_else(|| {
                StoreError::Corrupt("execution receipt has no committed effect intent".to_owned())
            })?;
        if intent_record.sequence >= record.sequence {
            return Err(StoreError::Corrupt(
                "execution receipt precedes its effect intent".to_owned(),
            ));
        }
        let provenance = intent.receipt_provenance.as_ref().ok_or_else(|| {
            StoreError::Corrupt("execution receipt has no durable provenance".to_owned())
        })?;
        verify_receipt_intent_binding(receipt, intent, provenance, *disposition)?;
        verify_receipt_timestamps(records, record.sequence, step_id, effect_id, receipt)?;
        previous_digest = Some(&receipt.receipt_digest);
    }
    Ok(())
}

fn verify_receipt_intent_binding(
    receipt: &ExecutionReceiptBody,
    intent: &xgeny_workgraph::EffectIntent,
    provenance: &xgeny_workgraph::ReceiptProvenance,
    disposition: VerificationDisposition,
) -> Result<(), StoreError> {
    if receipt.receipt_id != core_receipt_id_v1(&intent.effect_id)
        || !receipt.extensions.is_empty()
        || !receipt.required_extensions.is_empty()
        || !receipt.artifacts.is_empty()
        || !has_core_redactions(receipt)
        || provenance.profile_version != CORE_RECEIPT_PROFILE_V1
        || provenance.input_summary != CORE_RECEIPT_INPUT_SUMMARY_V1
        || receipt.invocation_id != provenance.invocation_id
        || receipt.plan_id != provenance.plan_id
        || receipt.capability.capability_id != intent.invocation.capability_id
        || receipt.capability.contract_version != intent.invocation.contract_version
        || receipt.instance_id != intent.invocation.instance_id
        || receipt.input_digest != intent.authorization.binding.material_digest
        || receipt.input_summary != provenance.input_summary
        || receipt.policy.decision_id != provenance.policy_decision_id
        || receipt.policy.decision_digest != provenance.policy_decision_digest
        || receipt.policy.lease_id.is_some()
        || receipt.policy.lease_digest.is_some()
        || receipt.executor.id != provenance.executor_id
        || receipt.executor.placement != protocol_placement(provenance.executor_placement)
        || receipt.executor.platform != provenance.executor_platform
        || receipt.effect.class != protocol_effect_class(intent.effect_class)
        || receipt.effect.idempotency_key != intent.idempotency_key
        || !receipt.effect.started
    {
        return Err(StoreError::Corrupt(
            "execution receipt differs from its durable intent provenance".to_owned(),
        ));
    }
    if receipt.verification.len() != provenance.verification_plan.len() {
        return Err(StoreError::Corrupt(
            "execution receipt verification disposition differs".to_owned(),
        ));
    }
    for (evidence, rule) in receipt
        .verification
        .iter()
        .zip(&provenance.verification_plan)
    {
        if evidence.strategy != protocol_verification_strategy(rule.strategy)
            || evidence.required != rule.required
            || evidence.summary != core_verification_summary_v1(evidence.result)
            || evidence.artifact.is_some()
            || (evidence.result == VerificationResult::Passed && evidence.evidence_digest.is_none())
        {
            return Err(StoreError::Corrupt(
                "execution receipt verification plan differs".to_owned(),
            ));
        }
    }
    let outcome = evaluate_core_verification_v1(&receipt.verification);
    let expected_disposition = match outcome {
        CoreVerificationOutcome::Passed => VerificationDisposition::Passed,
        CoreVerificationOutcome::Failed => VerificationDisposition::Failed,
        CoreVerificationOutcome::Inconclusive => VerificationDisposition::Inconclusive,
    };
    if disposition != expected_disposition || receipt.status != core_receipt_status_v1(outcome) {
        return Err(StoreError::Corrupt(
            "execution receipt result does not justify its disposition".to_owned(),
        ));
    }
    Ok(())
}

fn verify_receipt_timestamps(
    records: &[EventRecord],
    final_sequence: u64,
    step_id: &str,
    effect_id: &str,
    receipt: &ExecutionReceiptBody,
) -> Result<(), StoreError> {
    let started_at = records
        .iter()
        .rev()
        .filter(|record| record.sequence < final_sequence)
        .find_map(|record| match &record.event.body {
            RunEventBody::EffectExecutionStarted {
                step_id: candidate_step,
                effect_id: candidate_effect,
            } if candidate_step == step_id && candidate_effect == effect_id => {
                Some(record.event.recorded_at.as_str())
            }
            _ => None,
        })
        .ok_or_else(|| StoreError::Corrupt("execution receipt has no start event".to_owned()))?;
    let final_record = records
        .iter()
        .find(|record| record.sequence == final_sequence)
        .expect("final sequence comes from records");
    if receipt.started_at != started_at
        || receipt.effect.started_at.as_deref() != Some(started_at)
        || receipt.ended_at != final_record.event.recorded_at
    {
        return Err(StoreError::Corrupt(
            "execution receipt timestamps differ from the journal".to_owned(),
        ));
    }
    let started = OffsetDateTime::parse(started_at, &Rfc3339).map_err(|_| {
        StoreError::Corrupt("execution receipt start timestamp is invalid".to_owned())
    })?;
    let ended = OffsetDateTime::parse(&receipt.ended_at, &Rfc3339).map_err(|_| {
        StoreError::Corrupt("execution receipt end timestamp is invalid".to_owned())
    })?;
    if ended < started {
        return Err(StoreError::Corrupt(
            "execution receipt ends before its effect starts".to_owned(),
        ));
    }
    Ok(())
}

fn has_core_redactions(receipt: &ExecutionReceiptBody) -> bool {
    receipt
        .redactions_applied
        .iter()
        .map(String::as_str)
        .eq(CORE_RECEIPT_REDACTIONS_V1)
}

const fn protocol_effect_class(effect_class: EffectClass) -> ProtocolEffectClass {
    match effect_class {
        EffectClass::Reversible => ProtocolEffectClass::Compensatable,
        EffectClass::Idempotent => ProtocolEffectClass::Idempotent,
        EffectClass::NonIdempotent => ProtocolEffectClass::NonIdempotent,
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

fn actual_head(records: &[EventRecord]) -> ExpectedHead {
    records
        .last()
        .map_or(ExpectedHead::Empty, |record| ExpectedHead::Exact {
            sequence: record.sequence,
            digest: record.digest.clone(),
        })
}

fn prepare_commit(
    records: &[EventRecord],
    state: Option<&RunState>,
    expected: ExpectedHead,
    event: RunEvent,
) -> Result<Commit, StoreError> {
    let actual = actual_head(records);
    if expected != actual {
        return Err(StoreError::HeadConflict { expected, actual });
    }
    if records
        .iter()
        .any(|record| record.event.event_id == event.event_id)
    {
        return Err(StoreError::DuplicateEventId(event.event_id));
    }
    let record = EventRecord::next(records.last(), event)?;
    let state = apply_record(state, &record)?;
    Ok(Commit { record, state })
}

fn verified_snapshot(
    records: Vec<EventRecord>,
    persisted: Option<RunState>,
) -> Result<Option<RunSnapshot>, StoreError> {
    match (records.is_empty(), persisted) {
        (true, None) => Ok(None),
        (true, Some(_)) => Err(StoreError::Corrupt(
            "projection exists without committed events".to_owned(),
        )),
        (false, None) => Err(StoreError::Corrupt(
            "committed events exist without a projection".to_owned(),
        )),
        (false, Some(persisted)) => {
            let replayed = replay(&records)?;
            if persisted != replayed {
                return Err(StoreError::Corrupt(
                    "persisted projection differs from event replay".to_owned(),
                ));
            }
            Ok(Some(RunSnapshot {
                records,
                state: replayed,
            }))
        }
    }
}

fn canonical_jsonl<T: Serialize>(records: &[T]) -> Result<Vec<u8>, StoreError> {
    let mut output = Vec::new();
    for record in records {
        let value: Value = serde_json::to_value(record)?;
        let mut line = serde_jcs::to_vec(&value)
            .map_err(|error| StoreError::Canonicalization(error.to_string()))?;
        output.append(&mut line);
        output.push(b'\n');
    }
    Ok(output)
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("run head conflict: expected {expected:?}, actual {actual:?}")]
    HeadConflict {
        expected: ExpectedHead,
        actual: ExpectedHead,
    },
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error(transparent)]
    InvocationMaterial(#[from] InvocationMaterialError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("event id `{0}` is already committed")]
    DuplicateEventId(String),
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("canonical JSON encoding failed: {0}")]
    Canonicalization(String),
    #[error("unsupported local store schema version {0}")]
    UnsupportedSchemaVersion(i64),
    #[error("stored sequence is outside the supported integer range")]
    SequenceOutOfRange,
    #[error("local run store is inconsistent: {0}")]
    Corrupt(String),
    #[error("an effect intent requires an atomic invocation material descriptor")]
    InvocationMaterialRequired,
    #[error("invocation material was supplied for an event that is not an effect intent")]
    UnexpectedInvocationMaterial,
    #[error("this Run store does not support durable invocation material descriptors")]
    InvocationMaterialStoreUnsupported,
    #[error("a receipt-bound verification event requires an atomic ExecutionReceipt")]
    ExecutionReceiptRequired,
    #[error("an ExecutionReceipt was supplied for an unrelated event")]
    UnexpectedExecutionReceipt,
    #[error("the ExecutionReceipt differs from its verification event")]
    ExecutionReceiptBindingMismatch,
    #[error("the ExecutionReceipt is invalid")]
    ExecutionReceiptInvalid,
    #[error("the durable Receipt profile is unsupported")]
    UnsupportedReceiptProfile,
    #[error("a new effect intent requires durable Receipt provenance")]
    ReceiptProvenanceRequired,
    #[error("legacy receipt-free verification events cannot be appended")]
    LegacyVerificationAppendRejected,
    #[error("this Run store does not support durable ExecutionReceipts")]
    ExecutionReceiptStoreUnsupported,
    #[error("injected append fault after {0}")]
    InjectedFault(&'static str),
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::tempdir;
    use xgeny_domain::{
        API_VERSION_V1ALPHA1, CapabilityRef, Executor, ProtocolDocument, ReceiptEffect,
        ReceiptPolicy, ReceiptStatus, VerificationEvidence,
    };
    use xgeny_protocol::canonical_digest_without_field;
    use xgeny_workgraph::{
        AuthorizationBinding, AuthorizationUse, EffectClass, EffectIntent, InvocationBinding,
        InvocationMaterialRecord, InvocationMaterialRetention, ReceiptPlacement, ReceiptProvenance,
        ReceiptVerificationRule, ReceiptVerificationStrategy, ReconciliationResolution, RunEvent,
        RunEventBody, RunState, SinkGuarantee, StepStatus, VerificationDisposition,
        authorization_digest, invocation_material_digest, invocation_material_retention_digest,
        once_authorization_id, receipt_provenance_digest,
    };

    use super::*;

    fn event(event_id: &str, body: RunEventBody) -> RunEvent {
        RunEvent {
            event_id: event_id.to_owned(),
            run_id: "run-1".to_owned(),
            authority: "local:test".to_owned(),
            authority_epoch: 3,
            recorded_at: "2026-08-28T00:00:00Z".to_owned(),
            body,
        }
    }

    fn intent(state: &RunState) -> EffectIntent {
        let material_digest = invocation_material_digest(&serde_json::json!({"operation": "test"}))
            .expect("material should canonicalize");
        let material_retention_digest =
            invocation_material_retention_digest(&InvocationMaterialRetention::Ephemeral)
                .expect("retention should canonicalize");
        let invocation = InvocationBinding {
            capability_id: "test.effect".to_owned(),
            contract_version: "1.0.0".to_owned(),
            definition_digest: "sha256:definition-1".to_owned(),
            instance_id: "test.instance".to_owned(),
            instance_binding_digest: "sha256:instance-1".to_owned(),
        };
        let mut binding = AuthorizationBinding {
            run_id: state.run_id.clone(),
            step_id: "step-1".to_owned(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            issued_at_sequence: state.journal_sequence,
            issued_at_head_digest: state.journal_head_digest.clone(),
            capability_id: invocation.capability_id.clone(),
            contract_version: invocation.contract_version.clone(),
            definition_digest: invocation.definition_digest.clone(),
            instance_id: invocation.instance_id.clone(),
            instance_binding_digest: invocation.instance_binding_digest.clone(),
            action_digest: "sha256:action-1".to_owned(),
            material_digest,
            material_retention_digest,
            policy_evidence_digest: "sha256:policy-1".to_owned(),
            receipt_provenance_digest: None,
        };
        let provenance = ReceiptProvenance {
            profile_version: CORE_RECEIPT_PROFILE_V1.to_owned(),
            invocation_id: "invocation-base".to_owned(),
            plan_id: "plan-base".to_owned(),
            policy_decision_id: "decision-base".to_owned(),
            policy_decision_digest: format!("sha256:{}", "c".repeat(64)),
            executor_id: "xgeny-local".to_owned(),
            executor_placement: ReceiptPlacement::Local,
            executor_platform: "linux-x86_64".to_owned(),
            input_summary: CORE_RECEIPT_INPUT_SUMMARY_V1.to_owned(),
            verification_plan: vec![ReceiptVerificationRule {
                strategy: ReceiptVerificationStrategy::Postcondition,
                required: true,
            }],
        };
        binding.receipt_provenance_digest =
            Some(receipt_provenance_digest(&provenance).expect("provenance should canonicalize"));
        EffectIntent {
            effect_id: "effect-1".to_owned(),
            action_digest: "sha256:action-1".to_owned(),
            invocation,
            effect_class: EffectClass::NonIdempotent,
            idempotency_key: None,
            sink_guarantee: SinkGuarantee::None,
            authorization: AuthorizationUse {
                grant_id: once_authorization_id(&binding.run_id, &binding.action_digest)
                    .expect("authorization ID should canonicalize"),
                grant_digest: authorization_digest(&binding, 1)
                    .expect("authorization should canonicalize"),
                max_uses: 1,
                binding,
            },
            receipt_provenance: Some(provenance),
        }
    }

    fn seed<S: RunStore>(store: &mut S) -> Commit {
        let created = store
            .append(
                ExpectedHead::Empty,
                event(
                    "event-1",
                    RunEventBody::RunCreated {
                        goal: "durable effect".to_owned(),
                    },
                ),
            )
            .expect("run creation should commit");
        store
            .append(
                ExpectedHead::from_state(&created.state),
                event(
                    "event-2",
                    RunEventBody::StepPlanned {
                        step_id: "step-1".to_owned(),
                        objective: "perform effect".to_owned(),
                    },
                ),
            )
            .expect("step plan should commit")
    }

    fn material(state: &RunState, effect: &EffectIntent) -> InvocationMaterialRecord {
        material_for(state, "step-1", effect)
    }

    fn material_for(
        state: &RunState,
        step_id: &str,
        effect: &EffectIntent,
    ) -> InvocationMaterialRecord {
        let digest = invocation_material_digest(&serde_json::json!({"operation": "test"}))
            .expect("material should canonicalize");
        InvocationMaterialRecord::new(
            &state.run_id,
            step_id,
            effect,
            digest,
            InvocationMaterialRetention::Ephemeral,
        )
        .expect("material record should bind")
    }

    fn append_intent<S: RunStore>(store: &mut S, previous: &Commit) -> Commit {
        let effect = intent(&previous.state);
        let material = material(&previous.state, &effect);
        store
            .append_with_invocation_material(
                ExpectedHead::from_state(&previous.state),
                event(
                    "event-3",
                    RunEventBody::EffectIntentCommitted {
                        step_id: "step-1".to_owned(),
                        intent: Box::new(effect),
                    },
                ),
                material,
            )
            .expect("effect intent should commit")
    }

    fn receipt_intent(state: &RunState) -> EffectIntent {
        let mut effect = intent(state);
        effect.invocation.capability_id = "test/effect".to_owned();
        effect.invocation.definition_digest = format!("sha256:{}", "a".repeat(64));
        effect.invocation.instance_binding_digest = format!("sha256:{}", "b".repeat(64));
        effect.idempotency_key = Some("stable-key-1".to_owned());
        effect.authorization.binding.capability_id = effect.invocation.capability_id.clone();
        effect.authorization.binding.definition_digest =
            effect.invocation.definition_digest.clone();
        effect.authorization.binding.instance_binding_digest =
            effect.invocation.instance_binding_digest.clone();
        let provenance = ReceiptProvenance {
            profile_version: CORE_RECEIPT_PROFILE_V1.to_owned(),
            invocation_id: "invocation-1".to_owned(),
            plan_id: "plan-1".to_owned(),
            policy_decision_id: "decision-1".to_owned(),
            policy_decision_digest: format!("sha256:{}", "c".repeat(64)),
            executor_id: "xgeny-local".to_owned(),
            executor_placement: ReceiptPlacement::Local,
            executor_platform: "linux-x86_64".to_owned(),
            input_summary: "Invocation input retained by digest only.".to_owned(),
            verification_plan: vec![ReceiptVerificationRule {
                strategy: ReceiptVerificationStrategy::Postcondition,
                required: true,
            }],
        };
        effect.authorization.binding.receipt_provenance_digest =
            Some(receipt_provenance_digest(&provenance).expect("provenance should canonicalize"));
        effect.authorization.grant_digest = authorization_digest(&effect.authorization.binding, 1)
            .expect("authorization should canonicalize");
        effect.receipt_provenance = Some(provenance);
        effect
    }

    fn second_receipt_intent(state: &RunState) -> EffectIntent {
        let mut effect = receipt_intent(state);
        effect.effect_id = "effect-2".to_owned();
        effect.action_digest = "sha256:action-2".to_owned();
        effect.idempotency_key = Some("stable-key-2".to_owned());
        effect.authorization.binding.step_id = "step-2".to_owned();
        effect.authorization.binding.action_digest = effect.action_digest.clone();
        effect.authorization.grant_id = once_authorization_id(&state.run_id, &effect.action_digest)
            .expect("second authorization ID should canonicalize");
        let provenance = effect
            .receipt_provenance
            .as_mut()
            .expect("Receipt provenance should exist");
        provenance.invocation_id = "invocation-2".to_owned();
        provenance.plan_id = "plan-2".to_owned();
        provenance.policy_decision_id = "decision-2".to_owned();
        provenance.policy_decision_digest = format!("sha256:{}", "2".repeat(64));
        effect.authorization.binding.receipt_provenance_digest =
            Some(receipt_provenance_digest(provenance).expect("provenance should canonicalize"));
        effect.authorization.grant_digest = authorization_digest(&effect.authorization.binding, 1)
            .expect("second authorization should canonicalize");
        effect
    }

    fn seed_validating<S: RunStore>(store: &mut S) -> (Commit, EffectIntent) {
        let planned = seed(store);
        let effect = receipt_intent(&planned.state);
        let material = material(&planned.state, &effect);
        let committed = store
            .append_with_invocation_material(
                ExpectedHead::from_state(&planned.state),
                event(
                    "receipt-event-3",
                    RunEventBody::EffectIntentCommitted {
                        step_id: "step-1".to_owned(),
                        intent: Box::new(effect.clone()),
                    },
                ),
                material,
            )
            .expect("Receipt-bearing intent should commit");
        let started = store
            .append(
                ExpectedHead::from_state(&committed.state),
                event(
                    "receipt-event-4",
                    RunEventBody::EffectExecutionStarted {
                        step_id: "step-1".to_owned(),
                        effect_id: effect.effect_id.clone(),
                    },
                ),
            )
            .expect("effect start should commit");
        let succeeded = store
            .append(
                ExpectedHead::from_state(&started.state),
                event(
                    "receipt-event-5",
                    RunEventBody::EffectSucceeded {
                        step_id: "step-1".to_owned(),
                        effect_id: effect.effect_id.clone(),
                        evidence_digest: format!("sha256:{}", "d".repeat(64)),
                    },
                ),
            )
            .expect("effect evidence should commit");
        (succeeded, effect)
    }

    fn append_receipt_effect_to_validating<S: RunStore>(
        store: &mut S,
        state: &RunState,
        step_id: &str,
        effect: &EffectIntent,
        event_prefix: &str,
    ) -> Commit {
        let committed = store
            .append_with_invocation_material(
                ExpectedHead::from_state(state),
                event(
                    &format!("{event_prefix}-intent"),
                    RunEventBody::EffectIntentCommitted {
                        step_id: step_id.to_owned(),
                        intent: Box::new(effect.clone()),
                    },
                ),
                material_for(state, step_id, effect),
            )
            .expect("Receipt-bearing intent should commit");
        let started = store
            .append(
                ExpectedHead::from_state(&committed.state),
                event(
                    &format!("{event_prefix}-started"),
                    RunEventBody::EffectExecutionStarted {
                        step_id: step_id.to_owned(),
                        effect_id: effect.effect_id.clone(),
                    },
                ),
            )
            .expect("effect start should commit");
        store
            .append(
                ExpectedHead::from_state(&started.state),
                event(
                    &format!("{event_prefix}-succeeded"),
                    RunEventBody::EffectSucceeded {
                        step_id: step_id.to_owned(),
                        effect_id: effect.effect_id.clone(),
                        evidence_digest: format!("sha256:{}", "d".repeat(64)),
                    },
                ),
            )
            .expect("effect evidence should commit")
    }

    fn successful_receipt(effect: &EffectIntent) -> ExecutionReceiptBody {
        let provenance = effect
            .receipt_provenance
            .as_ref()
            .expect("test intent should have provenance");
        let mut receipt = ExecutionReceiptBody {
            api_version: API_VERSION_V1ALPHA1.to_owned(),
            extensions: BTreeMap::new(),
            required_extensions: Vec::new(),
            receipt_id: "receipt-1".to_owned(),
            run_id: "run-1".to_owned(),
            step_id: "step-1".to_owned(),
            invocation_id: provenance.invocation_id.clone(),
            plan_id: provenance.plan_id.clone(),
            capability: CapabilityRef {
                capability_id: effect.invocation.capability_id.clone(),
                contract_version: effect.invocation.contract_version.clone(),
            },
            instance_id: effect.invocation.instance_id.clone(),
            input_digest: effect.authorization.binding.material_digest.clone(),
            input_summary: provenance.input_summary.clone(),
            policy: ReceiptPolicy {
                decision_id: provenance.policy_decision_id.clone(),
                decision_digest: provenance.policy_decision_digest.clone(),
                lease_id: None,
                lease_digest: None,
            },
            executor: Executor {
                id: provenance.executor_id.clone(),
                placement: Placement::Local,
                platform: provenance.executor_platform.clone(),
            },
            effect: ReceiptEffect {
                class: ProtocolEffectClass::NonIdempotent,
                started: true,
                started_at: Some("2026-08-28T00:00:00Z".to_owned()),
                idempotency_key: effect.idempotency_key.clone(),
            },
            status: ReceiptStatus::Succeeded,
            started_at: "2026-08-28T00:00:00Z".to_owned(),
            ended_at: "2026-08-28T00:00:00Z".to_owned(),
            output_digest: format!("sha256:{}", "e".repeat(64)),
            artifacts: Vec::new(),
            verification: vec![VerificationEvidence {
                strategy: VerificationStrategy::Postcondition,
                required: true,
                result: VerificationResult::Passed,
                summary: "Core-selected verification rule passed.".to_owned(),
                evidence_digest: Some(format!("sha256:{}", "d".repeat(64))),
                artifact: None,
            }],
            redactions_applied: vec![
                "raw invocation arguments omitted".to_owned(),
                "raw tool output omitted".to_owned(),
            ],
            previous_receipt_digest: None,
            receipt_digest: format!("sha256:{}", "0".repeat(64)),
        };
        seal_receipt(&mut receipt);
        receipt
    }

    fn seal_receipt(receipt: &mut ExecutionReceiptBody) {
        let value = serde_json::to_value(ProtocolDocument::ExecutionReceipt(Box::new(
            receipt.clone(),
        )))
        .expect("Receipt should serialize");
        receipt.receipt_digest = canonical_digest_without_field(&value, "receiptDigest")
            .expect("Receipt should canonicalize");
    }

    fn assert_receipt_mutation_rejected(
        mutate: impl FnOnce(&mut ExecutionReceiptBody, &mut RunEvent),
    ) {
        let mut store = MemoryRunStore::new();
        let (validating, effect) = seed_validating(&mut store);
        let mut receipt = successful_receipt(&effect);
        let mut candidate = receipt_event(&effect, &receipt);
        mutate(&mut receipt, &mut candidate);
        seal_receipt(&mut receipt);
        let RunEventBody::VerificationRecorded { receipt_digest, .. } = &mut candidate.body else {
            panic!("test candidate must be a verification event")
        };
        receipt.receipt_digest.clone_into(receipt_digest);

        let result = store.append_with_execution_receipt(
            ExpectedHead::from_state(&validating.state),
            candidate,
            receipt,
        );
        let error = result.expect_err("forged Receipt must be rejected");
        assert!(matches!(&error, StoreError::Corrupt(_)));
        let rendered = format!("{error}\n{error:?}");
        assert!(
            !rendered.contains("RAW-RECEIPT-SENTINEL"),
            "Receipt validation errors must not echo untrusted Receipt fields"
        );
        assert_eq!(
            store.load().expect("store should load").expect("Run").state,
            validating.state
        );
        assert!(
            store
                .load_execution_receipts()
                .expect("Receipts")
                .is_empty()
        );
    }

    fn receipt_event(effect: &EffectIntent, receipt: &ExecutionReceiptBody) -> RunEvent {
        receipt_event_for("receipt-event-6", "step-1", effect, receipt)
    }

    fn receipt_event_for(
        event_id: &str,
        step_id: &str,
        effect: &EffectIntent,
        receipt: &ExecutionReceiptBody,
    ) -> RunEvent {
        event(
            event_id,
            RunEventBody::VerificationRecorded {
                step_id: step_id.to_owned(),
                effect_id: effect.effect_id.clone(),
                disposition: VerificationDisposition::Passed,
                receipt_id: receipt.receipt_id.clone(),
                receipt_digest: receipt.receipt_digest.clone(),
            },
        )
    }

    fn commit_two_receipt_chain<S: RunStore>(store: &mut S) {
        let first_step = seed(store);
        let two_steps = store
            .append(
                ExpectedHead::from_state(&first_step.state),
                event(
                    "export-step-2",
                    RunEventBody::StepPlanned {
                        step_id: "step-2".to_owned(),
                        objective: "perform another effect".to_owned(),
                    },
                ),
            )
            .expect("second Step should plan");
        let first_effect = receipt_intent(&two_steps.state);
        let first_validating = append_receipt_effect_to_validating(
            store,
            &two_steps.state,
            "step-1",
            &first_effect,
            "export-first",
        );
        let first_receipt = successful_receipt(&first_effect);
        let first_terminal = store
            .append_with_execution_receipt(
                ExpectedHead::from_state(&first_validating.state),
                receipt_event_for(
                    "export-first-receipt",
                    "step-1",
                    &first_effect,
                    &first_receipt,
                ),
                first_receipt.clone(),
            )
            .expect("first Receipt should commit");
        let second_effect = second_receipt_intent(&first_terminal.state);
        let second_validating = append_receipt_effect_to_validating(
            store,
            &first_terminal.state,
            "step-2",
            &second_effect,
            "export-second",
        );
        let mut second_receipt = successful_receipt(&second_effect);
        second_receipt.receipt_id = "receipt-2".to_owned();
        second_receipt.step_id = "step-2".to_owned();
        second_receipt.previous_receipt_digest = Some(first_receipt.receipt_digest);
        seal_receipt(&mut second_receipt);
        store
            .append_with_execution_receipt(
                ExpectedHead::from_state(&second_validating.state),
                receipt_event_for(
                    "export-second-receipt",
                    "step-2",
                    &second_effect,
                    &second_receipt,
                ),
                second_receipt,
            )
            .expect("second Receipt should commit");
    }

    fn legacy_schema_three_data(
        validating: bool,
    ) -> (Vec<EventRecord>, RunState, InvocationMaterialRecord) {
        fn push(records: &mut Vec<EventRecord>, state: &mut Option<RunState>, event: RunEvent) {
            let record = EventRecord::next(records.last(), event).expect("event should record");
            *state = Some(apply_record(state.as_ref(), &record).expect("event should replay"));
            records.push(record);
        }

        let mut records = Vec::new();
        let mut state = None;
        push(
            &mut records,
            &mut state,
            event(
                "legacy-event-1",
                RunEventBody::RunCreated {
                    goal: "legacy durable effect".to_owned(),
                },
            ),
        );
        push(
            &mut records,
            &mut state,
            event(
                "legacy-event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".to_owned(),
                    objective: "perform legacy effect".to_owned(),
                },
            ),
        );
        let planned = state.as_ref().expect("planned state should exist");
        let mut effect = intent(planned);
        effect.receipt_provenance = None;
        effect.authorization.binding.receipt_provenance_digest = None;
        effect.authorization.grant_digest =
            authorization_digest(&effect.authorization.binding, effect.authorization.max_uses)
                .expect("legacy authorization should canonicalize");
        let material = material(planned, &effect);
        push(
            &mut records,
            &mut state,
            event(
                "legacy-event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect.clone()),
                },
            ),
        );
        if validating {
            push(
                &mut records,
                &mut state,
                event(
                    "legacy-event-4",
                    RunEventBody::EffectExecutionStarted {
                        step_id: "step-1".to_owned(),
                        effect_id: effect.effect_id.clone(),
                    },
                ),
            );
            push(
                &mut records,
                &mut state,
                event(
                    "legacy-event-5",
                    RunEventBody::EffectSucceeded {
                        step_id: "step-1".to_owned(),
                        effect_id: effect.effect_id,
                        evidence_digest: format!("sha256:{}", "d".repeat(64)),
                    },
                ),
            );
        }
        (records, state.expect("legacy state should exist"), material)
    }

    #[test]
    fn memory_and_sqlite_obey_the_same_store_contract() {
        let directory = tempdir().expect("temp directory should exist");
        let mut memory = MemoryRunStore::new();
        let mut sqlite = SqliteRunStore::open(directory.path().join("run.db"))
            .expect("embedded sqlite should open");

        let memory_seed = seed(&mut memory);
        let sqlite_seed = seed(&mut sqlite);
        let memory_commit = append_intent(&mut memory, &memory_seed);
        let sqlite_commit = append_intent(&mut sqlite, &sqlite_seed);

        assert_eq!(memory_commit, sqlite_commit);
        assert_eq!(
            memory.load().expect("memory load"),
            sqlite.load().expect("sqlite load")
        );
        assert_eq!(
            memory.export_jsonl().expect("memory export"),
            sqlite.export_jsonl().expect("sqlite export")
        );
        let effect_id = memory_commit.state.steps["step-1"]
            .intent
            .as_ref()
            .expect("intent should exist")
            .effect_id
            .clone();
        assert_eq!(
            memory
                .load_invocation_material(&effect_id)
                .expect("memory material should load"),
            sqlite
                .load_invocation_material(&effect_id)
                .expect("SQLite material should load")
        );
    }

    #[test]
    fn memory_and_sqlite_atomically_commit_the_same_execution_receipt() {
        let directory = tempdir().expect("temp directory should exist");
        let mut memory = MemoryRunStore::new();
        let mut sqlite =
            SqliteRunStore::open(directory.path().join("run.db")).expect("SQLite should open");
        let (memory_validating, memory_effect) = seed_validating(&mut memory);
        let (sqlite_validating, sqlite_effect) = seed_validating(&mut sqlite);
        let memory_receipt = successful_receipt(&memory_effect);
        let sqlite_receipt = successful_receipt(&sqlite_effect);

        let memory_commit = memory
            .append_with_execution_receipt(
                ExpectedHead::from_state(&memory_validating.state),
                receipt_event(&memory_effect, &memory_receipt),
                memory_receipt,
            )
            .expect("memory Receipt should commit");
        let sqlite_commit = sqlite
            .append_with_execution_receipt(
                ExpectedHead::from_state(&sqlite_validating.state),
                receipt_event(&sqlite_effect, &sqlite_receipt),
                sqlite_receipt,
            )
            .expect("SQLite Receipt should commit");

        assert_eq!(memory_commit, sqlite_commit);
        assert_eq!(
            memory_commit.state.steps["step-1"].status,
            StepStatus::Completed
        );
        assert_eq!(
            memory.load_execution_receipts().expect("memory Receipts"),
            sqlite.load_execution_receipts().expect("SQLite Receipts")
        );
        assert_eq!(sqlite.execution_receipt_count().expect("receipt count"), 1);
    }

    #[test]
    fn memory_and_sqlite_export_the_same_complete_receipt_chain_jsonl() {
        let directory = tempdir().expect("temp directory should exist");
        let mut memory = MemoryRunStore::new();
        let mut sqlite =
            SqliteRunStore::open(directory.path().join("run.db")).expect("SQLite should open");
        commit_two_receipt_chain(&mut memory);
        commit_two_receipt_chain(&mut sqlite);

        let memory_jsonl = memory
            .export_execution_receipts_jsonl()
            .expect("Memory Receipt export should succeed");
        let sqlite_jsonl = sqlite
            .export_execution_receipts_jsonl()
            .expect("SQLite Receipt export should succeed");
        assert_eq!(memory_jsonl, sqlite_jsonl);
        assert!(memory_jsonl.ends_with(b"\n"));
        let lines: Vec<_> = memory_jsonl
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);
        let receipts: Vec<_> = lines
            .into_iter()
            .map(|line| {
                let document: ProtocolDocument = serde_json::from_slice(line)
                    .expect("export line should be a protocol document");
                let ProtocolDocument::ExecutionReceipt(receipt) = document else {
                    panic!("export line must retain kind: ExecutionReceipt")
                };
                *receipt
            })
            .collect();
        assert_eq!(receipts[0].receipt_id, "receipt-1");
        assert_eq!(receipts[1].receipt_id, "receipt-2");
        assert_eq!(
            receipts[1].previous_receipt_digest.as_deref(),
            Some(receipts[0].receipt_digest.as_str())
        );
    }

    #[test]
    fn receipt_chain_follows_journal_order_and_rejects_a_wrong_second_link() {
        let mut store = MemoryRunStore::new();
        let first_step = seed(&mut store);
        let two_steps = store
            .append(
                ExpectedHead::from_state(&first_step.state),
                event(
                    "chain-step-2",
                    RunEventBody::StepPlanned {
                        step_id: "step-2".to_owned(),
                        objective: "perform another effect".to_owned(),
                    },
                ),
            )
            .expect("second Step should plan");

        let first_effect = receipt_intent(&two_steps.state);
        let first_validating = append_receipt_effect_to_validating(
            &mut store,
            &two_steps.state,
            "step-1",
            &first_effect,
            "chain-first",
        );
        let first_receipt = successful_receipt(&first_effect);
        let first_terminal = store
            .append_with_execution_receipt(
                ExpectedHead::from_state(&first_validating.state),
                receipt_event_for(
                    "chain-first-receipt",
                    "step-1",
                    &first_effect,
                    &first_receipt,
                ),
                first_receipt.clone(),
            )
            .expect("first Receipt should commit");

        let second_effect = second_receipt_intent(&first_terminal.state);
        let second_validating = append_receipt_effect_to_validating(
            &mut store,
            &first_terminal.state,
            "step-2",
            &second_effect,
            "chain-second",
        );
        let mut second_receipt = successful_receipt(&second_effect);
        second_receipt.receipt_id = "receipt-2".to_owned();
        second_receipt.step_id = "step-2".to_owned();
        second_receipt.previous_receipt_digest = Some(first_receipt.receipt_digest.clone());
        seal_receipt(&mut second_receipt);

        let mut wrong_link = second_receipt.clone();
        wrong_link.previous_receipt_digest = None;
        seal_receipt(&mut wrong_link);
        let wrong_result = store.append_with_execution_receipt(
            ExpectedHead::from_state(&second_validating.state),
            receipt_event_for("chain-second-wrong", "step-2", &second_effect, &wrong_link),
            wrong_link,
        );
        assert!(matches!(wrong_result, Err(StoreError::Corrupt(_))));

        store
            .append_with_execution_receipt(
                ExpectedHead::from_state(&second_validating.state),
                receipt_event_for(
                    "chain-second-receipt",
                    "step-2",
                    &second_effect,
                    &second_receipt,
                ),
                second_receipt,
            )
            .expect("correct second Receipt link should commit");
        let receipts = store.load_execution_receipts().expect("Receipt chain");
        assert_eq!(receipts.len(), 2);
        assert_eq!(
            receipts[1].previous_receipt_digest.as_deref(),
            Some(receipts[0].receipt_digest.as_str())
        );
    }

    #[test]
    fn receipt_bound_and_legacy_verification_events_cannot_use_plain_append() {
        let mut store = MemoryRunStore::new();
        let (validating, effect) = seed_validating(&mut store);
        let receipt = successful_receipt(&effect);
        let result = store.append(
            ExpectedHead::from_state(&validating.state),
            receipt_event(&effect, &receipt),
        );
        assert!(matches!(result, Err(StoreError::ExecutionReceiptRequired)));

        let result = store.append(
            ExpectedHead::from_state(&validating.state),
            event(
                "legacy-verification",
                RunEventBody::VerificationPassed {
                    step_id: "step-1".to_owned(),
                },
            ),
        );
        assert!(matches!(
            result,
            Err(StoreError::LegacyVerificationAppendRejected)
        ));
        assert_eq!(
            store.load().expect("store should load").expect("Run").state,
            validating.state
        );
    }

    #[test]
    fn unsupported_receipt_profile_is_rejected_before_intent_commit() {
        let mut store = MemoryRunStore::new();
        let planned = seed(&mut store);
        let mut effect = receipt_intent(&planned.state);
        let provenance = effect
            .receipt_provenance
            .as_mut()
            .expect("Receipt provenance should exist");
        provenance.profile_version = "unsupported-receipt-profile".to_owned();
        effect.authorization.binding.receipt_provenance_digest =
            Some(receipt_provenance_digest(provenance).expect("provenance should canonicalize"));
        effect.authorization.grant_digest = authorization_digest(&effect.authorization.binding, 1)
            .expect("authorization should canonicalize");
        let material = material(&planned.state, &effect);

        let result = store.append_with_invocation_material(
            ExpectedHead::from_state(&planned.state),
            event(
                "unsupported-profile-intent",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect),
                },
            ),
            material,
        );

        assert!(matches!(result, Err(StoreError::UnsupportedReceiptProfile)));
        assert_eq!(
            store.load().expect("store should load").expect("Run").state,
            planned.state
        );
    }

    #[test]
    fn new_effect_intent_without_receipt_provenance_is_rejected() {
        let mut store = MemoryRunStore::new();
        let planned = seed(&mut store);
        let mut effect = intent(&planned.state);
        effect.receipt_provenance = None;
        effect.authorization.binding.receipt_provenance_digest = None;
        effect.authorization.grant_digest =
            authorization_digest(&effect.authorization.binding, effect.authorization.max_uses)
                .expect("legacy-shaped authorization should canonicalize");
        let material = material(&planned.state, &effect);

        let result = store.append_with_invocation_material(
            ExpectedHead::from_state(&planned.state),
            event(
                "missing-provenance-intent",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect),
                },
            ),
            material,
        );

        assert!(matches!(result, Err(StoreError::ReceiptProvenanceRequired)));
        assert_eq!(
            store.load().expect("store should load").expect("Run").state,
            planned.state
        );
    }

    #[test]
    fn store_rejects_forged_core_owned_receipt_fields_and_evidence_free_success() {
        assert_receipt_mutation_rejected(|receipt, event| {
            receipt.receipt_id = "RAW-RECEIPT-SENTINEL".to_owned();
            let RunEventBody::VerificationRecorded { receipt_id, .. } = &mut event.body else {
                panic!("test candidate must be a verification event")
            };
            receipt.receipt_id.clone_into(receipt_id);
        });
        assert_receipt_mutation_rejected(|receipt, _| {
            receipt.extensions.insert(
                "https://example.test/raw".to_owned(),
                serde_json::json!("RAW-RECEIPT-SENTINEL"),
            );
        });
        assert_receipt_mutation_rejected(|receipt, _| {
            receipt.verification[0].summary = "RAW-RECEIPT-SENTINEL".to_owned();
        });
        assert_receipt_mutation_rejected(|receipt, _| {
            receipt.redactions_applied[0] = "RAW-RECEIPT-SENTINEL".to_owned();
        });
        assert_receipt_mutation_rejected(|receipt, _| {
            receipt.verification[0].evidence_digest = None;
        });
        assert_receipt_mutation_rejected(|receipt, _| {
            receipt.previous_receipt_digest = Some(format!("sha256:{}", "f".repeat(64)));
        });
    }

    #[test]
    fn store_rejects_a_receipt_that_ends_before_the_effect_started() {
        assert_receipt_mutation_rejected(|receipt, event| {
            "2026-08-27T23:59:59Z".clone_into(&mut receipt.ended_at);
            "2026-08-27T23:59:59Z".clone_into(&mut event.recorded_at);
        });
    }

    #[test]
    fn sqlite_rolls_back_partial_execution_receipt_commits() {
        for fault in [
            AppendFault::Event,
            AppendFault::ExecutionReceipt,
            AppendFault::Projection,
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let mut store =
                SqliteRunStore::open(directory.path().join("run.db")).expect("SQLite should open");
            let (validating, effect) = seed_validating(&mut store);
            let receipt = successful_receipt(&effect);
            let candidate = receipt_event(&effect, &receipt);
            let result = store.append_receipt_with_fault(
                ExpectedHead::from_state(&validating.state),
                candidate.clone(),
                &receipt,
                fault,
            );
            assert!(matches!(result, Err(StoreError::InjectedFault(_))));
            let recovered = store
                .load()
                .expect("store should remain valid")
                .expect("Run should remain");
            assert_eq!(recovered.state, validating.state);
            assert_eq!(store.execution_receipt_count().expect("receipt count"), 0);

            store
                .append_with_execution_receipt(
                    ExpectedHead::from_state(&validating.state),
                    candidate,
                    receipt,
                )
                .expect("retry should atomically commit");
        }
    }

    #[test]
    fn sqlite_process_exit_after_receipt_insert_rolls_back_the_whole_finalization() {
        const CHILD_MARKER: &str = "XGENY_SQLITE_RECEIPT_CRASH_CHILD";
        const DATABASE_PATH: &str = "XGENY_SQLITE_RECEIPT_CRASH_DB";

        if std::env::var_os(CHILD_MARKER).is_some() {
            let path = std::env::var_os(DATABASE_PATH).expect("child database path is required");
            let mut store = SqliteRunStore::open(path).expect("child SQLite should open");
            let snapshot = store
                .load()
                .expect("child load should pass")
                .expect("validating Run should exist");
            let effect = snapshot.state.steps["step-1"]
                .intent
                .as_ref()
                .expect("validating Step should retain intent")
                .clone();
            let receipt = successful_receipt(&effect);
            let candidate = receipt_event(&effect, &receipt);
            let _never_returns = store.append_receipt_and_exit_at(
                ExpectedHead::from_state(&snapshot.state),
                candidate,
                &receipt,
                AppendFault::ExecutionReceipt,
            );
            panic!("child should exit after the Receipt row insert");
        }

        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let validating = {
            let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
            seed_validating(&mut store).0
        };
        let status = Command::new(std::env::current_exe().expect("test executable should exist"))
            .args([
                "--exact",
                "tests::sqlite_process_exit_after_receipt_insert_rolls_back_the_whole_finalization",
                "--test-threads=1",
            ])
            .env(CHILD_MARKER, "1")
            .env(DATABASE_PATH, &path)
            .status()
            .expect("Receipt crash child should start");
        assert_eq!(status.code(), Some(86));

        let mut reopened = SqliteRunStore::open(&path).expect("SQLite should recover");
        let recovered = reopened
            .load()
            .expect("recovered store should verify")
            .expect("Run should remain");
        assert_eq!(recovered.state, validating.state);
        assert_eq!(
            recovered.state.steps["step-1"].status,
            StepStatus::Validating
        );
        assert_eq!(
            reopened.execution_receipt_count().expect("receipt count"),
            0
        );
        let effect = recovered.state.steps["step-1"]
            .intent
            .as_ref()
            .expect("intent should remain")
            .clone();
        let receipt = successful_receipt(&effect);
        reopened
            .append_with_execution_receipt(
                ExpectedHead::from_state(&recovered.state),
                receipt_event(&effect, &receipt),
                receipt,
            )
            .expect("finalization retry should commit atomically");
        assert_eq!(
            reopened.execution_receipt_count().expect("receipt count"),
            1
        );
    }

    #[test]
    fn sqlite_detects_missing_and_tampered_execution_receipts() {
        let commit_receipt = |path: &std::path::Path| {
            let mut store = SqliteRunStore::open(path).expect("SQLite should open");
            let (validating, effect) = seed_validating(&mut store);
            let receipt = successful_receipt(&effect);
            store
                .append_with_execution_receipt(
                    ExpectedHead::from_state(&validating.state),
                    receipt_event(&effect, &receipt),
                    receipt,
                )
                .expect("Receipt should commit");
            store
        };

        let missing_directory = tempdir().expect("temp directory should exist");
        let missing_path = missing_directory.path().join("run.db");
        let missing = commit_receipt(&missing_path);
        missing
            .delete_execution_receipts()
            .expect("test Receipt should delete");
        assert!(matches!(missing.load(), Err(StoreError::Corrupt(_))));

        let tampered_directory = tempdir().expect("temp directory should exist");
        let tampered_path = tampered_directory.path().join("run.db");
        let tampered = commit_receipt(&tampered_path);
        tampered
            .corrupt_execution_receipt_document()
            .expect("test Receipt should corrupt");
        assert!(matches!(tampered.load(), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn sqlite_reopens_and_replays_the_committed_projection() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let expected = {
            let mut store = SqliteRunStore::open(&path).expect("sqlite should open");
            let seed = seed(&mut store);
            append_intent(&mut store, &seed).state
        };

        let reopened = SqliteRunStore::open(&path).expect("sqlite should reopen");
        let snapshot = reopened
            .load()
            .expect("replay should pass")
            .expect("run should exist");

        assert_eq!(snapshot.state, expected);
        assert_eq!(snapshot.records.len(), 3);
        assert_eq!(
            reopened
                .invocation_material_count()
                .expect("material count should work"),
            1
        );
    }

    #[test]
    fn sqlite_candidate_uses_wal_full_sync_and_foreign_keys() {
        let directory = tempdir().expect("temp directory should exist");
        let store =
            SqliteRunStore::open(directory.path().join("run.db")).expect("sqlite should open");

        let (journal_mode, synchronous, foreign_keys) = store
            .durability_settings()
            .expect("durability pragmas should be readable");
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(synchronous, 2, "SQLite FULL is numeric level 2");
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn unsupported_store_versions_are_rejected_without_mutation() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let connection = rusqlite::Connection::open(&path).expect("SQLite file should open");
        drop(connection);

        for version in [1_i64, 2_i64, 5_i64] {
            let connection = rusqlite::Connection::open(&path).expect("SQLite file should open");
            connection
                .pragma_update(None, "user_version", version)
                .expect("unsupported version should be written");
            drop(connection);
            let before = fs::read(&path).expect("legacy store should be readable");
            assert!(matches!(
                SqliteRunStore::open(&path),
                Err(StoreError::UnsupportedSchemaVersion(actual)) if actual == version
            ));
            let after = fs::read(&path).expect("rejected store should remain readable");
            assert_eq!(
                after, before,
                "opening a rejected version must not mutate it"
            );
        }
    }

    #[test]
    fn fresh_sqlite_store_uses_schema_version_four() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        drop(SqliteRunStore::open(&path).expect("fresh SQLite should open"));
        let connection = rusqlite::Connection::open(&path).expect("SQLite file should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should be readable");
        assert_eq!(version, 4);
    }

    #[test]
    fn sqlite_migrates_schema_three_without_changing_committed_run_bytes() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let (expected_state, expected_jsonl, expected_events) = {
            let mut store = SqliteRunStore::open(&path).expect("schema four store should open");
            let seed = seed(&mut store);
            let committed = append_intent(&mut store, &seed);
            let jsonl = store.export_jsonl().expect("journal should export");
            let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
            let mut statement = connection
                .prepare("SELECT event_json FROM run_events ORDER BY sequence")
                .expect("event query should prepare");
            let events = statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .expect("event query should run")
                .collect::<Result<Vec<_>, _>>()
                .expect("event bytes should load");
            (committed.state, jsonl, events)
        };
        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        connection
            .execute("DROP TABLE execution_receipts", [])
            .expect("empty schema-four table should drop");
        connection
            .pragma_update(None, "user_version", 3_i64)
            .expect("schema version should downgrade for the fixture");
        drop(connection);

        let migrated = SqliteRunStore::open(&path).expect("schema three should migrate");
        let snapshot = migrated
            .load()
            .expect("migrated store should verify")
            .expect("Run should remain");
        assert_eq!(snapshot.state, expected_state);
        assert_eq!(
            migrated.export_jsonl().expect("journal export"),
            expected_jsonl
        );
        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 4);
        let mut statement = connection
            .prepare("SELECT event_json FROM run_events ORDER BY sequence")
            .expect("event query should prepare");
        let actual_events = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .expect("event query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("event bytes should load");
        assert_eq!(actual_events, expected_events);
    }

    #[test]
    fn sqlite_migrates_real_legacy_pending_and_validating_runs_without_backfill() {
        for (label, validating, expected_status) in [
            ("pending", false, StepStatus::IntentCommitted),
            ("validating", true, StepStatus::Validating),
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let path = directory.path().join(format!("legacy-{label}.db"));
            let (records, state, material) = legacy_schema_three_data(validating);
            let expected_jsonl = canonical_jsonl(&records).expect("legacy journal should export");
            sqlite::write_schema_three_fixture(&path, &records, &state, &[material])
                .expect("schema-three fixture should write");

            let migrated = SqliteRunStore::open(&path).expect("legacy store should migrate");
            let snapshot = migrated
                .load()
                .expect("migrated store should verify")
                .expect("legacy Run should remain");
            assert_eq!(snapshot.state.steps["step-1"].status, expected_status);
            assert!(
                snapshot.state.steps["step-1"]
                    .intent
                    .as_ref()
                    .expect("legacy intent should remain")
                    .receipt_provenance
                    .is_none()
            );
            assert_eq!(
                migrated.export_jsonl().expect("journal should export"),
                expected_jsonl
            );
            assert!(
                migrated
                    .load_execution_receipts()
                    .expect("Receipt table should load")
                    .is_empty()
            );
        }
    }

    #[test]
    fn sqlite_detects_a_corrupted_derived_effect_index() {
        let directory = tempdir().expect("temp directory should exist");
        let mut store =
            SqliteRunStore::open(directory.path().join("run.db")).expect("sqlite should open");
        let seed = seed(&mut store);
        append_intent(&mut store, &seed);
        store
            .corrupt_effect_index()
            .expect("test corruption should be injected");

        assert!(matches!(store.load(), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn sqlite_detects_a_corrupted_invocation_material_index() {
        let directory = tempdir().expect("temp directory should exist");
        let mut store =
            SqliteRunStore::open(directory.path().join("run.db")).expect("sqlite should open");
        let seed = seed(&mut store);
        append_intent(&mut store, &seed);
        store
            .corrupt_invocation_material_index()
            .expect("test corruption should be injected");

        assert!(matches!(store.load(), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn sqlite_refuses_to_append_to_an_open_store_after_index_corruption() {
        let directory = tempdir().expect("temp directory should exist");
        let mut store =
            SqliteRunStore::open(directory.path().join("run.db")).expect("sqlite should open");
        let seed = seed(&mut store);
        let committed = append_intent(&mut store, &seed);
        store
            .corrupt_invocation_material_index()
            .expect("test corruption should be injected");
        let effect_id = committed.state.steps["step-1"]
            .intent
            .as_ref()
            .expect("intent should exist")
            .effect_id
            .clone();

        let result = store.append(
            ExpectedHead::from_state(&committed.state),
            event(
                "event-4",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".to_owned(),
                    effect_id,
                },
            ),
        );

        assert!(matches!(result, Err(StoreError::Corrupt(_))));
        assert_eq!(store.run_event_count().expect("event count should work"), 3);
    }

    #[test]
    fn sqlite_detects_a_missing_invocation_material_record() {
        let directory = tempdir().expect("temp directory should exist");
        let mut store =
            SqliteRunStore::open(directory.path().join("run.db")).expect("sqlite should open");
        let seed = seed(&mut store);
        append_intent(&mut store, &seed);
        store
            .delete_invocation_material()
            .expect("test deletion should be injected");

        assert!(matches!(store.load(), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn sqlite_detects_an_orphan_invocation_material_record() {
        let directory = tempdir().expect("temp directory should exist");
        let mut store =
            SqliteRunStore::open(directory.path().join("run.db")).expect("sqlite should open");
        let seed = seed(&mut store);
        append_intent(&mut store, &seed);
        store
            .insert_orphan_invocation_material()
            .expect("test orphan should be injected");

        assert!(matches!(store.load(), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn effect_intent_without_material_is_rejected_without_consuming_authorization() {
        let mut store = MemoryRunStore::new();
        let seed = seed(&mut store);
        let result = store.append(
            ExpectedHead::from_state(&seed.state),
            event(
                "event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(intent(&seed.state)),
                },
            ),
        );

        assert!(matches!(
            result,
            Err(StoreError::InvocationMaterialRequired)
        ));
        let snapshot = store
            .load()
            .expect("store should load")
            .expect("Run should exist");
        assert_eq!(snapshot.records.len(), 2);
        assert!(snapshot.state.authorization_consumption.is_empty());
    }

    #[test]
    fn concurrent_sqlite_writer_observes_head_compare_and_swap() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut first = SqliteRunStore::open(&path).expect("first writer should open");
        let seed = seed(&mut first);
        let mut stale = SqliteRunStore::open(&path).expect("second writer should open");
        append_intent(&mut first, &seed);

        let effect = intent(&seed.state);
        let material = material(&seed.state, &effect);
        let result = stale.append_with_invocation_material(
            ExpectedHead::from_state(&seed.state),
            event(
                "event-stale",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect),
                },
            ),
            material,
        );

        assert!(matches!(result, Err(StoreError::HeadConflict { .. })));
        assert_eq!(
            stale
                .load()
                .expect("second writer should read committed state")
                .expect("run should exist")
                .records
                .len(),
            3
        );
    }

    #[test]
    fn stale_writer_is_rejected_without_mutation() {
        let mut store = MemoryRunStore::new();
        let seed = seed(&mut store);
        let stale = ExpectedHead::Exact {
            sequence: 1,
            digest: "sha256:stale".to_owned(),
        };

        let effect = intent(&seed.state);
        let material = material(&seed.state, &effect);
        let result = store.append_with_invocation_material(
            stale,
            event(
                "event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect),
                },
            ),
            material,
        );

        assert!(matches!(result, Err(StoreError::HeadConflict { .. })));
        assert_eq!(
            store
                .load()
                .expect("load should pass")
                .expect("run should exist")
                .state,
            seed.state
        );
    }

    #[test]
    fn duplicate_event_id_is_rejected_by_the_store_contract() {
        let mut store = MemoryRunStore::new();
        let seed = seed(&mut store);

        let effect = intent(&seed.state);
        let material = material(&seed.state, &effect);
        let result = store.append_with_invocation_material(
            ExpectedHead::from_state(&seed.state),
            event(
                "event-1",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect),
                },
            ),
            material,
        );

        assert!(matches!(result, Err(StoreError::DuplicateEventId(_))));
    }

    #[test]
    fn sqlite_rolls_back_every_partial_commit_stage() {
        for fault in [
            AppendFault::Event,
            AppendFault::EffectIntentIndex,
            AppendFault::AuthorizationConsumption,
            AppendFault::InvocationMaterial,
            AppendFault::Projection,
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let mut store =
                SqliteRunStore::open(directory.path().join("run.db")).expect("sqlite should open");
            let seed = seed(&mut store);
            let effect = intent(&seed.state);
            let material = material(&seed.state, &effect);
            let candidate = event(
                "event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect),
                },
            );

            let result = store.append_with_fault(
                ExpectedHead::from_state(&seed.state),
                candidate.clone(),
                &material,
                fault,
            );
            assert!(matches!(result, Err(StoreError::InjectedFault(_))));

            let after_fault = store
                .load()
                .expect("store should remain readable")
                .expect("seed should remain");
            assert_eq!(after_fault.records.len(), 2);
            assert_eq!(after_fault.state, seed.state);
            assert_eq!(store.effect_intent_count().expect("count should work"), 0);
            assert_eq!(
                store
                    .authorization_consumption_count()
                    .expect("count should work"),
                0
            );
            assert_eq!(
                store
                    .invocation_material_count()
                    .expect("material count should work"),
                0
            );

            store
                .append_with_invocation_material(
                    ExpectedHead::from_state(&seed.state),
                    candidate,
                    material,
                )
                .expect("retry after rollback should commit");
        }
    }

    #[test]
    fn sqlite_process_exit_rolls_back_transaction() {
        const CHILD_MARKER: &str = "XGENY_SQLITE_CRASH_CHILD";
        const DATABASE_PATH: &str = "XGENY_SQLITE_CRASH_DB";
        const FAULT_STAGE: &str = "XGENY_SQLITE_CRASH_STAGE";

        if std::env::var_os(CHILD_MARKER).is_some() {
            let path = std::env::var_os(DATABASE_PATH).expect("child database path is required");
            let fault = match std::env::var(FAULT_STAGE)
                .expect("child fault stage is required")
                .as_str()
            {
                "event" => AppendFault::Event,
                "intent" => AppendFault::EffectIntentIndex,
                "authorization" => AppendFault::AuthorizationConsumption,
                "material" => AppendFault::InvocationMaterial,
                "projection" => AppendFault::Projection,
                stage => panic!("unknown child fault stage: {stage}"),
            };
            let mut store = SqliteRunStore::open(path).expect("child sqlite should open");
            let snapshot = store
                .load()
                .expect("child load should pass")
                .expect("child seed should exist");
            let effect = intent(&snapshot.state);
            let material = material(&snapshot.state, &effect);
            let candidate = event(
                "event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect),
                },
            );
            let _never_returns = store.append_and_exit_at(
                ExpectedHead::from_state(&snapshot.state),
                candidate,
                &material,
                fault,
            );
            panic!("child should have exited at the selected commit stage");
        }

        for (label, fault) in [
            ("event", AppendFault::Event),
            ("intent", AppendFault::EffectIntentIndex),
            ("authorization", AppendFault::AuthorizationConsumption),
            ("material", AppendFault::InvocationMaterial),
            ("projection", AppendFault::Projection),
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let path = directory.path().join("run.db");
            let seed_state = {
                let mut store = SqliteRunStore::open(&path).expect("sqlite should open");
                seed(&mut store).state
            };

            let status =
                Command::new(std::env::current_exe().expect("test executable should exist"))
                    .args([
                        "--exact",
                        "tests::sqlite_process_exit_rolls_back_transaction",
                        "--test-threads=1",
                    ])
                    .env(CHILD_MARKER, "1")
                    .env(DATABASE_PATH, &path)
                    .env(FAULT_STAGE, label)
                    .status()
                    .expect("crash child should start");
            assert_eq!(
                status.code(),
                Some(86),
                "unexpected child result at {fault:?}"
            );

            let reopened = SqliteRunStore::open(&path).expect("sqlite should recover");
            let recovered = reopened
                .load()
                .expect("recovered store should verify")
                .expect("seed should remain");
            assert_eq!(recovered.records.len(), 2);
            assert_eq!(recovered.state, seed_state);
            assert_eq!(reopened.effect_intent_count().expect("intent count"), 0);
            assert_eq!(
                reopened
                    .authorization_consumption_count()
                    .expect("authorization count"),
                0
            );
            assert_eq!(
                reopened
                    .invocation_material_count()
                    .expect("material count"),
                0
            );
        }
    }

    #[test]
    fn lost_ack_never_causes_a_non_idempotent_blind_retry_after_restart() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut physical_effect_count = 0;
        {
            let mut store = SqliteRunStore::open(&path).expect("sqlite should open");
            let seed = seed(&mut store);
            let intent_commit = append_intent(&mut store, &seed);
            let started = store
                .append(
                    ExpectedHead::from_state(&intent_commit.state),
                    event(
                        "event-4",
                        RunEventBody::EffectExecutionStarted {
                            step_id: "step-1".to_owned(),
                            effect_id: "effect-1".to_owned(),
                        },
                    ),
                )
                .expect("start marker should commit before effect");

            // The simulated sink applies the effect, but its acknowledgement is lost.
            physical_effect_count += 1;
            store
                .append(
                    ExpectedHead::from_state(&started.state),
                    event(
                        "event-5",
                        RunEventBody::EffectBecameUnknown {
                            step_id: "step-1".to_owned(),
                            effect_id: "effect-1".to_owned(),
                            reason: "ack lost".to_owned(),
                        },
                    ),
                )
                .expect("uncertainty should be durable");
        }

        let mut reopened = SqliteRunStore::open(&path).expect("sqlite should reopen");
        let unknown = reopened
            .load()
            .expect("load should pass")
            .expect("run should exist");
        let retry = reopened.append(
            ExpectedHead::from_state(&unknown.state),
            event(
                "event-6",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".to_owned(),
                    effect_id: "effect-1".to_owned(),
                },
            ),
        );
        assert!(matches!(
            result_transition(&retry),
            Some(StepStatus::EffectUnknown)
        ));
        assert_eq!(physical_effect_count, 1);

        let reconciling = reopened
            .append(
                ExpectedHead::from_state(&unknown.state),
                event(
                    "event-6",
                    RunEventBody::ReconciliationStarted {
                        step_id: "step-1".to_owned(),
                        effect_id: "effect-1".to_owned(),
                    },
                ),
            )
            .expect("reconciliation should start");
        let applied = reopened
            .append(
                ExpectedHead::from_state(&reconciling.state),
                event(
                    "event-7",
                    RunEventBody::ReconciliationResolved {
                        step_id: "step-1".to_owned(),
                        effect_id: "effect-1".to_owned(),
                        resolution: ReconciliationResolution::ProvedApplied,
                        evidence_digest: "sha256:sink-query-1".to_owned(),
                    },
                ),
            )
            .expect("applied evidence should resolve uncertainty");
        assert_eq!(applied.state.steps["step-1"].status, StepStatus::Validating);
        assert_eq!(physical_effect_count, 1);
    }

    fn result_transition(result: &Result<Commit, StoreError>) -> Option<StepStatus> {
        match result {
            Err(StoreError::Transition(
                xgeny_workgraph::TransitionError::InvalidStepTransition { from, .. },
            )) => Some(*from),
            _ => None,
        }
    }
}
