#![doc = "Local storage candidates for durable `XGENy` run events and projections."]

mod memory;
mod sqlite;

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use xgeny_domain::{
    EffectClass as ProtocolEffectClass, ExecutionReceiptBody, Placement, ProtocolDocument,
    VerificationResult, VerificationStrategy,
};
use xgeny_protocol::{
    CORE_RECEIPT_INPUT_SUMMARY_V1, CORE_RECEIPT_MAX_ARTIFACT_TOTAL_BYTES_V2,
    CORE_RECEIPT_MAX_ARTIFACTS_V2, CORE_RECEIPT_PROFILE_V1, CORE_RECEIPT_PROFILE_V2,
    CORE_RECEIPT_REDACTIONS_V1, CoreVerificationOutcome, ProtocolError,
    core_artifact_descriptor_v2_is_valid, core_receipt_id_v1, core_receipt_status_v1,
    core_verification_summary_v1, evaluate_core_verification_v1, validate_execution_receipt,
};
use xgeny_workgraph::{
    CompletionOutputError, CompletionOutputRecord, EffectClass, EffectIntent, EventRecord,
    ExpectedPlanningTurn, InvocationMaterialError, InvocationMaterialRecord,
    InvocationMaterialRetention, PlannedInvocationBinding, PlannedInvocationMaterialRecord,
    PlanningContractError, ReceiptPlacement, ReceiptVerificationStrategy, RecordError, ReplayError,
    RunEvent, RunEventBody, RunState, StepStatus, TOOL_OUTPUT_PROFILE_V1, ToolOutputError,
    ToolOutputRecord, TransitionError, VerificationDisposition, apply_record, replay,
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

/// Minimal verified view used by the runtime hot path when finalizing one Receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunVerificationSnapshot {
    pub state: RunState,
    pub effect_started_at: Option<String>,
    pub previous_receipt_digest: Option<String>,
    pub tool_output: Option<ToolOutputRecord>,
}

/// One generation-checked view used to construct the next provider planning context.
///
/// Only output-bound Steps that reached `Completed` through a passed, fully verified Core
/// Receipt are present. Raw output values remain local sidecars and are deliberately redacted
/// from `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct RunPlanningSnapshot {
    pub state: RunState,
    completed_tool_outputs: BTreeMap<String, ToolOutputRecord>,
}

impl RunPlanningSnapshot {
    /// Construct a snapshot for a trusted [`RunStore`] implementation.
    ///
    /// Runtime consumers still validate every output against `state`; built-in stores additionally
    /// guarantee one-generation reads and durable sidecar verification before construction.
    #[must_use]
    pub fn new(
        state: RunState,
        completed_tool_outputs: BTreeMap<String, ToolOutputRecord>,
    ) -> Self {
        Self {
            state,
            completed_tool_outputs,
        }
    }

    #[must_use]
    pub const fn completed_tool_outputs(&self) -> &BTreeMap<String, ToolOutputRecord> {
        &self.completed_tool_outputs
    }
}

impl std::fmt::Debug for RunPlanningSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunPlanningSnapshot")
            .field("state", &self.state)
            .field(
                "completed_tool_output_bindings",
                &PlanningOutputBindingsDebug(&self.completed_tool_outputs),
            )
            .finish()
    }
}

struct PlanningOutputBindingsDebug<'a>(&'a BTreeMap<String, ToolOutputRecord>);

impl std::fmt::Debug for PlanningOutputBindingsDebug<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = formatter.debug_map();
        for (step_id, output) in self.0 {
            map.entry(
                step_id,
                &(
                    output.output_id(),
                    output.output_digest(),
                    output.record_digest(),
                ),
            );
        }
        map.finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredExecutionReceipt {
    pub event_sequence: u64,
    pub effect_id: String,
    pub receipt: ExecutionReceiptBody,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StoredToolOutput {
    pub event_sequence: u64,
    pub record: ToolOutputRecord,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StoredCompletionOutput {
    pub event_sequence: u64,
    pub record: CompletionOutputRecord,
}

impl std::fmt::Debug for StoredCompletionOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredCompletionOutput")
            .field("event_sequence", &self.event_sequence)
            .field("record", &self.record)
            .finish()
    }
}

impl std::fmt::Debug for StoredToolOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredToolOutput")
            .field("event_sequence", &self.event_sequence)
            .field("record", &self.record)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectIntentAnchor {
    event_sequence: u64,
    step_id: String,
    intent: EffectIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectStartAnchor {
    event_sequence: u64,
    step_id: String,
    recorded_at: String,
    execution_attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedInvocationAnchor {
    event_sequence: u64,
    binding: PlannedInvocationBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiptEventAnchor {
    event_sequence: u64,
    run_id: String,
    step_id: String,
    effect_id: String,
    disposition: VerificationDisposition,
    receipt_id: String,
    receipt_digest: String,
    started_at: String,
    ended_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolOutputEventAnchor {
    event_sequence: u64,
    run_id: String,
    step_id: String,
    effect_id: String,
    evidence_digest: String,
    record_digest: String,
    execution_attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionOutputEventAnchor {
    event_sequence: u64,
    run_id: String,
    candidate_id: String,
    turn_index: u32,
    model_call_id: String,
    context_digest: String,
    proposal_digest: String,
    summary_digest: String,
    record_digest: String,
}

#[derive(Clone, Copy, Default)]
struct CommitSidecars<'a> {
    plan_inputs: Option<&'a [PlannedInvocationMaterialRecord]>,
    material: Option<&'a InvocationMaterialRecord>,
    output: Option<&'a ToolOutputRecord>,
    completion_output: Option<&'a CompletionOutputRecord>,
    receipt: Option<&'a ExecutionReceiptBody>,
}

impl<'a> CommitSidecars<'a> {
    fn plan_inputs(inputs: &'a [PlannedInvocationMaterialRecord]) -> Self {
        Self {
            plan_inputs: Some(inputs),
            ..Self::default()
        }
    }

    fn material(material: &'a InvocationMaterialRecord) -> Self {
        Self {
            material: Some(material),
            ..Self::default()
        }
    }

    fn output(output: &'a ToolOutputRecord) -> Self {
        Self {
            output: Some(output),
            ..Self::default()
        }
    }

    fn completion_output(output: &'a CompletionOutputRecord) -> Self {
        Self {
            completion_output: Some(output),
            ..Self::default()
        }
    }

    fn receipt(receipt: &'a ExecutionReceiptBody) -> Self {
        Self {
            receipt: Some(receipt),
            ..Self::default()
        }
    }
}

#[derive(Default)]
struct CommitAnchors {
    output: Option<ToolOutputEventAnchor>,
    completion_output: Option<CompletionOutputEventAnchor>,
    receipt: Option<ReceiptEventAnchor>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct VerifiedRunIndex {
    state: Option<RunState>,
    last_record: Option<EventRecord>,
    event_ids: BTreeSet<String>,
    planned_invocations: BTreeMap<String, PlannedInvocationAnchor>,
    plan_input_step_ids: BTreeSet<String>,
    intents: BTreeMap<String, EffectIntentAnchor>,
    effect_starts: BTreeMap<String, EffectStartAnchor>,
    receipt_events: Vec<ReceiptEventAnchor>,
    receipt_event_positions: BTreeMap<String, usize>,
    tool_output_events: BTreeMap<String, ToolOutputEventAnchor>,
    material_effect_ids: BTreeSet<String>,
    receipt_ids: BTreeSet<String>,
    receipt_digests: BTreeSet<String>,
    receipt_effect_ids: BTreeSet<String>,
    receipt_head_digest: Option<String>,
    tool_output_effect_ids: BTreeSet<String>,
    tool_output_ids: BTreeSet<String>,
    tool_output_record_digests: BTreeSet<String>,
    tool_output_digests: BTreeMap<String, String>,
    tool_output_sizes: BTreeMap<String, u64>,
    completion_output_event: Option<CompletionOutputEventAnchor>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AuditMetrics {
    #[cfg(test)]
    full_audits: u64,
    #[cfg(test)]
    historical_events: u64,
    #[cfg(test)]
    historical_materials: u64,
    #[cfg(test)]
    historical_plan_inputs: u64,
    #[cfg(test)]
    historical_receipts: u64,
    #[cfg(test)]
    historical_tool_outputs: u64,
    #[cfg(test)]
    candidate_events: u64,
    #[cfg(test)]
    candidate_materials: u64,
    #[cfg(test)]
    candidate_plan_inputs: u64,
    #[cfg(test)]
    candidate_receipts: u64,
    #[cfg(test)]
    candidate_tool_outputs: u64,
    #[cfg(test)]
    receipt_anchor_intent_lookups: u64,
    #[cfg(test)]
    receipt_anchor_start_lookups: u64,
    #[cfg(test)]
    receipt_binding_intent_lookups: u64,
}

impl AuditMetrics {
    fn record_candidate_sidecars(&mut self, sidecars: CommitSidecars<'_>) {
        self.record_candidate_plan_inputs(
            sidecars
                .plan_inputs
                .map_or(0, <[PlannedInvocationMaterialRecord]>::len),
        );
        self.record_candidate(
            sidecars.material.is_some(),
            sidecars.output.is_some(),
            sidecars.receipt.is_some(),
        );
    }

    fn record_full_audit(&mut self) {
        #[cfg(test)]
        {
            self.full_audits = self.full_audits.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_historical_event(&mut self) {
        #[cfg(test)]
        {
            self.historical_events = self.historical_events.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_historical_materials(&mut self, count: usize) {
        #[cfg(test)]
        {
            self.historical_materials = self
                .historical_materials
                .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        }
        #[cfg(not(test))]
        let _ = (self, count);
    }

    fn record_historical_plan_inputs(&mut self, count: usize) {
        #[cfg(test)]
        {
            self.historical_plan_inputs = self
                .historical_plan_inputs
                .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        }
        #[cfg(not(test))]
        let _ = (self, count);
    }

    fn record_candidate_plan_inputs(&mut self, count: usize) {
        #[cfg(test)]
        {
            self.candidate_plan_inputs = self
                .candidate_plan_inputs
                .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        }
        #[cfg(not(test))]
        let _ = (self, count);
    }

    fn record_historical_receipt(&mut self) {
        #[cfg(test)]
        {
            self.historical_receipts = self.historical_receipts.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_candidate(&mut self, has_material: bool, has_tool_output: bool, has_receipt: bool) {
        #[cfg(test)]
        {
            self.candidate_events = self.candidate_events.saturating_add(1);
            if has_material {
                self.candidate_materials = self.candidate_materials.saturating_add(1);
            }
            if has_receipt {
                self.candidate_receipts = self.candidate_receipts.saturating_add(1);
            }
            if has_tool_output {
                self.candidate_tool_outputs = self.candidate_tool_outputs.saturating_add(1);
            }
        }
        #[cfg(not(test))]
        let _ = (self, has_material, has_tool_output, has_receipt);
    }

    fn record_historical_tool_output(&mut self) {
        #[cfg(test)]
        {
            self.historical_tool_outputs = self.historical_tool_outputs.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_receipt_anchor_intent_lookup(&mut self) {
        #[cfg(test)]
        {
            self.receipt_anchor_intent_lookups =
                self.receipt_anchor_intent_lookups.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_receipt_anchor_start_lookup(&mut self) {
        #[cfg(test)]
        {
            self.receipt_anchor_start_lookups = self.receipt_anchor_start_lookups.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }

    fn record_receipt_binding_intent_lookup(&mut self) {
        #[cfg(test)]
        {
            self.receipt_binding_intent_lookups =
                self.receipt_binding_intent_lookups.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = self;
    }
}

fn verify_commit_sidecars(
    event: &RunEvent,
    sidecars: CommitSidecars<'_>,
) -> Result<(), StoreError> {
    verify_plan_input_bundle(event, sidecars.plan_inputs)?;
    verify_material_bundle(event, sidecars.material)?;
    verify_receipt_bundle(event, sidecars.receipt)?;
    verify_tool_output_bundle(event, sidecars.output)?;
    verify_completion_output_bundle(event, sidecars.completion_output)
}

pub trait RunStore {
    /// Compare-and-append one event and its derived projection atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for stale heads, invalid transitions, serialization, or storage faults.
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError>;

    /// Atomically append one accepted plan and every secret-free reconstructable input sidecar.
    ///
    /// Stores that do not implement this bundle fail closed. A `PlanAccepted` event must never be
    /// sent through plain [`RunStore::append`], because that could publish runnable Steps without
    /// restart material.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported stores, missing/orphan/mismatched inputs, stale heads,
    /// invalid graph transitions, serialization, or storage faults.
    fn append_with_plan_inputs(
        &mut self,
        _expected: ExpectedHead,
        _event: RunEvent,
        _inputs: Vec<PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        Err(StoreError::PlannedInvocationStoreUnsupported)
    }

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

    /// Atomically append one output-bound successful effect event and its typed JSON sidecar.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported stores, detached or invalid output, stale heads,
    /// transition failures, serialization, or storage faults.
    fn append_with_tool_output(
        &mut self,
        _expected: ExpectedHead,
        _event: RunEvent,
        _output: ToolOutputRecord,
    ) -> Result<Commit, StoreError> {
        Err(StoreError::ToolOutputStoreUnsupported)
    }

    /// Atomically append one completion candidate and its exact local summary sidecar.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported stores, legacy or detached events, stale heads,
    /// mismatched bindings, transition failures, serialization, or storage faults.
    fn append_with_completion_output(
        &mut self,
        _expected: ExpectedHead,
        _event: RunEvent,
        _output: CompletionOutputRecord,
    ) -> Result<Commit, StoreError> {
        Err(StoreError::CompletionOutputStoreUnsupported)
    }

    /// Load and replay-verify all committed data.
    ///
    /// # Errors
    ///
    /// Returns an error if storage cannot be read or its projection differs from replay.
    fn load(&self) -> Result<Option<RunSnapshot>, StoreError>;

    /// Load only the current verified projection for runtime coordination.
    ///
    /// Implementations used for execution-authoritative dependency release must verify that the
    /// projection is replay-equivalent and that every projected Receipt identity is backed by the
    /// complete, valid Receipt sidecar/chain. The built-in stores provide that contract. The
    /// default preserves compatibility by using `load`; built-in stores override it with a
    /// generation-checked index, whose warm path avoids historical materialization. Cold open,
    /// generation change, or the default implementation can still perform a full audit.
    ///
    /// # Errors
    ///
    /// Returns an error when the current projection cannot be verified.
    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        Ok(self.load()?.map(|snapshot| snapshot.state))
    }

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

    /// Load one verified accepted-plan input by Step ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the store does not support plan inputs or committed data is missing,
    /// corrupt, or inconsistent with its journal binding.
    fn load_planned_invocation(
        &self,
        _step_id: &str,
    ) -> Result<Option<PlannedInvocationMaterialRecord>, StoreError> {
        Err(StoreError::PlannedInvocationStoreUnsupported)
    }

    /// Load every verified Receipt in journal order.
    ///
    /// # Errors
    ///
    /// Returns an error when the store does not support Receipts or committed data is corrupt.
    fn load_execution_receipts(&self) -> Result<Vec<ExecutionReceiptBody>, StoreError> {
        Err(StoreError::ExecutionReceiptStoreUnsupported)
    }

    /// Load one verified typed tool output by exact effect ID.
    ///
    /// # Errors
    ///
    /// Returns an error when output storage is unsupported or committed data is inconsistent.
    fn load_tool_output(&self, _effect_id: &str) -> Result<Option<ToolOutputRecord>, StoreError> {
        Err(StoreError::ToolOutputStoreUnsupported)
    }

    /// Load the exact final summary bound to one candidate at the expected Run head.
    ///
    /// A `None` result is valid only for an absent Run/candidate or a replayed legacy candidate
    /// whose event has no completion-output digest. A digest-bound missing record is corruption.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported stores, stale heads, or inconsistent committed data.
    fn load_completion_output(
        &self,
        _expected: ExpectedHead,
        _candidate_id: &str,
    ) -> Result<Option<CompletionOutputRecord>, StoreError> {
        Err(StoreError::CompletionOutputStoreUnsupported)
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

    /// Load the minimal verified state needed to finalize a Step Receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when the Run, journal, or Receipt chain cannot be verified.
    fn load_verification_snapshot(
        &self,
        step_id: &str,
    ) -> Result<Option<RunVerificationSnapshot>, StoreError> {
        let (snapshot, receipts) = self.load_with_execution_receipts()?;
        let Some(snapshot) = snapshot else {
            return Ok(None);
        };
        let effect_id = snapshot
            .state
            .steps
            .get(step_id)
            .and_then(|step| step.intent.as_ref())
            .map(|intent| intent.effect_id.clone());
        let output_required = snapshot
            .state
            .steps
            .get(step_id)
            .is_some_and(|step| step.output_record_digest.is_some());
        let effect_started_at = effect_id.as_deref().and_then(|effect_id| {
            snapshot
                .records
                .iter()
                .rev()
                .find_map(|record| match &record.event.body {
                    RunEventBody::EffectExecutionStarted {
                        step_id: candidate_step,
                        effect_id: candidate_effect,
                    } if candidate_step == step_id && candidate_effect == effect_id => {
                        Some(record.event.recorded_at.clone())
                    }
                    _ => None,
                })
        });
        let tool_output = if output_required {
            effect_id
                .as_deref()
                .map(|effect_id| self.load_tool_output(effect_id))
                .transpose()?
                .flatten()
        } else {
            None
        };
        Ok(Some(RunVerificationSnapshot {
            state: snapshot.state,
            effect_started_at,
            previous_receipt_digest: receipts
                .last()
                .map(|receipt| receipt.receipt_digest.clone()),
            tool_output,
        }))
    }

    /// Load one exact Run head and every verified completed tool output from the same logical
    /// store generation.
    ///
    /// Implementations must not compose this view from independent point reads. The default
    /// fails closed because a cross-generation context could expose an output under the wrong
    /// journal head.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported stores, a stale expected head, or any missing, corrupt,
    /// or receipt-detached output sidecar.
    fn load_planning_snapshot(
        &self,
        _expected: ExpectedHead,
        _max_output_bytes: u64,
    ) -> Result<Option<RunPlanningSnapshot>, StoreError> {
        Err(StoreError::PlanningSnapshotStoreUnsupported)
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

fn verify_plan_input_bundle(
    event: &RunEvent,
    inputs: Option<&[PlannedInvocationMaterialRecord]>,
) -> Result<(), StoreError> {
    match (&event.body, inputs) {
        (RunEventBody::PlanAccepted { steps, .. }, Some(inputs)) => {
            if inputs.len() != steps.len() {
                return Err(StoreError::PlannedInvocationInputCountMismatch {
                    expected: steps.len(),
                    actual: inputs.len(),
                });
            }
            let mut by_step = BTreeMap::new();
            for input in inputs {
                if by_step.insert(input.step_id(), input).is_some() {
                    return Err(StoreError::DuplicatePlannedInvocationInput(
                        input.step_id().to_owned(),
                    ));
                }
            }
            for step in steps {
                let input = by_step.get(step.step_id.as_str()).ok_or_else(|| {
                    StoreError::PlannedInvocationInputMissing(step.step_id.clone())
                })?;
                input.verify_for(&event.run_id, &step.step_id, &step.invocation)?;
            }
            Ok(())
        }
        (RunEventBody::PlanAccepted { .. }, None) => {
            Err(StoreError::PlannedInvocationInputsRequired)
        }
        (_, Some(inputs)) if !inputs.is_empty() => {
            Err(StoreError::UnexpectedPlannedInvocationInputs)
        }
        (_, Some(_) | None) => Ok(()),
    }
}

fn verify_plan_input_records(
    index: &mut VerifiedRunIndex,
    inputs: &BTreeMap<String, PlannedInvocationMaterialRecord>,
    metrics: &mut AuditMetrics,
) -> Result<(), StoreError> {
    metrics.record_historical_plan_inputs(inputs.len());
    for (step_id, anchor) in &index.planned_invocations {
        let input = inputs.get(step_id).ok_or_else(|| {
            StoreError::Corrupt(format!(
                "planned Step `{step_id}` has no invocation input sidecar"
            ))
        })?;
        let run_id = index
            .state
            .as_ref()
            .map(|state| state.run_id.as_str())
            .ok_or_else(|| StoreError::Corrupt("planned input exists without a Run".to_owned()))?;
        input
            .verify_for(run_id, step_id, &anchor.binding)
            .map_err(|error| {
                StoreError::Corrupt(format!(
                    "planned invocation input for Step `{step_id}` is invalid: {error}"
                ))
            })?;
    }
    if inputs.len() != index.planned_invocations.len() {
        return Err(StoreError::Corrupt(format!(
            "planned invocation input count differs from accepted Steps: expected {}, actual {}",
            index.planned_invocations.len(),
            inputs.len()
        )));
    }
    index.plan_input_step_ids = inputs.keys().cloned().collect();
    Ok(())
}

fn verify_plan_input_point(
    index: &VerifiedRunIndex,
    step_id: &str,
    input: Option<&PlannedInvocationMaterialRecord>,
) -> Result<(), StoreError> {
    match (index.planned_invocations.get(step_id), input) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(StoreError::Corrupt(
            "planned invocation input has no accepted Step".to_owned(),
        )),
        (Some(_), None) => Err(StoreError::Corrupt(
            "accepted Step has no planned invocation input".to_owned(),
        )),
        (Some(anchor), Some(input)) => {
            let run_id = index
                .state
                .as_ref()
                .map(|state| state.run_id.as_str())
                .ok_or_else(|| {
                    StoreError::Corrupt("planned input exists without a Run".to_owned())
                })?;
            input
                .verify_for(run_id, step_id, &anchor.binding)
                .map_err(|error| {
                    StoreError::Corrupt(format!(
                        "planned invocation input for Step `{step_id}` is invalid: {error}"
                    ))
                })
        }
    }
}

fn verify_planned_material_retention(
    index: &VerifiedRunIndex,
    event: &RunEvent,
    material: &InvocationMaterialRecord,
    input: Option<&PlannedInvocationMaterialRecord>,
) -> Result<(), StoreError> {
    let RunEventBody::EffectIntentCommitted { step_id, .. } = &event.body else {
        return Ok(());
    };
    match (index.planned_invocations.get(step_id), input) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(StoreError::Corrupt(
            "planned input exists for a legacy effect Step".to_owned(),
        )),
        (Some(_), None) => Err(StoreError::PlannedInvocationInputMissing(step_id.clone())),
        (Some(anchor), Some(input)) => {
            let run_id = index
                .state
                .as_ref()
                .map(|state| state.run_id.as_str())
                .ok_or_else(|| {
                    StoreError::Corrupt("planned input exists without a Run".to_owned())
                })?;
            input.verify_for(run_id, step_id, &anchor.binding)?;
            let expected =
                InvocationMaterialRetention::ReconstructableReference(input.reference().clone());
            if material.retention() != &expected {
                return Err(StoreError::PlannedInvocationRetentionMismatch(
                    step_id.clone(),
                ));
            }
            Ok(())
        }
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
        let profile_matches_effect = matches!(
            (provenance.profile_version.as_str(), intent.effect_class),
            (CORE_RECEIPT_PROFILE_V2, EffectClass::ReadOnly)
                | (
                    CORE_RECEIPT_PROFILE_V1,
                    EffectClass::Reversible | EffectClass::Idempotent | EffectClass::NonIdempotent
                )
        );
        if !profile_matches_effect {
            return Err(StoreError::UnsupportedReceiptProfile);
        }
        let output_profile_matches_effect = match intent.effect_class {
            EffectClass::ReadOnly => {
                provenance.tool_output_profile.as_deref() == Some(TOOL_OUTPUT_PROFILE_V1)
            }
            EffectClass::Reversible | EffectClass::Idempotent | EffectClass::NonIdempotent => {
                provenance.tool_output_profile.is_none()
            }
        };
        if !output_profile_matches_effect {
            return Err(StoreError::ToolOutputProfileRequired);
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
    index: &mut VerifiedRunIndex,
    materials: &BTreeMap<String, InvocationMaterialRecord>,
    metrics: &mut AuditMetrics,
) -> Result<(), StoreError> {
    metrics.record_historical_materials(materials.len());
    for (effect_id, anchor) in &index.intents {
        let material = materials.get(effect_id).ok_or_else(|| {
            StoreError::Corrupt(format!(
                "effect `{effect_id}` has no invocation material descriptor"
            ))
        })?;
        material
            .verify_for(
                index
                    .state
                    .as_ref()
                    .map(|state| state.run_id.as_str())
                    .ok_or_else(|| {
                        StoreError::Corrupt("invocation material exists without a Run".to_owned())
                    })?,
                &anchor.step_id,
                &anchor.intent,
            )
            .map_err(|error| {
                StoreError::Corrupt(format!(
                    "invocation material for effect `{effect_id}` is invalid: {error}"
                ))
            })?;
    }
    if materials.len() != index.intents.len() {
        return Err(StoreError::Corrupt(format!(
            "invocation material count differs from effect intents: expected {}, actual {}",
            index.intents.len(),
            materials.len()
        )));
    }
    index.material_effect_ids = materials.keys().cloned().collect();
    Ok(())
}

fn verify_material_point(
    index: &VerifiedRunIndex,
    effect_id: &str,
    material: Option<&InvocationMaterialRecord>,
) -> Result<(), StoreError> {
    match (index.intents.get(effect_id), material) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(StoreError::Corrupt(
            "invocation material has no committed effect intent".to_owned(),
        )),
        (Some(_), None) => Err(StoreError::Corrupt(
            "committed effect intent has no invocation material descriptor".to_owned(),
        )),
        (Some(anchor), Some(material)) => {
            let run_id = index
                .state
                .as_ref()
                .map(|state| state.run_id.as_str())
                .ok_or_else(|| {
                    StoreError::Corrupt("invocation material exists without a Run".to_owned())
                })?;
            material
                .verify_for(run_id, &anchor.step_id, &anchor.intent)
                .map_err(|error| {
                    StoreError::Corrupt(format!(
                        "invocation material for effect `{effect_id}` is invalid: {error}"
                    ))
                })
        }
    }
}

fn verify_tool_output_bundle(
    event: &RunEvent,
    output: Option<&ToolOutputRecord>,
) -> Result<(), StoreError> {
    match (&event.body, output) {
        (
            RunEventBody::EffectSucceeded {
                output_record_digest: Some(expected),
                ..
            },
            Some(output),
        ) if expected == output.record_digest() => Ok(()),
        (RunEventBody::EffectSucceeded { .. }, Some(_)) => {
            Err(StoreError::ToolOutputBindingMismatch)
        }
        (
            RunEventBody::EffectSucceeded {
                output_record_digest: Some(_),
                ..
            },
            None,
        ) => Err(StoreError::ToolOutputRequired),
        (_, Some(_)) => Err(StoreError::UnexpectedToolOutput),
        (_, None) => Ok(()),
    }
}

fn verify_tool_output_candidate(
    index: &VerifiedRunIndex,
    record: &EventRecord,
    output: &ToolOutputRecord,
) -> Result<ToolOutputEventAnchor, StoreError> {
    let anchor = index.tool_output_anchor_for(record)?;
    if index.tool_output_effect_ids.contains(&anchor.effect_id)
        || index.tool_output_ids.contains(output.output_id())
        || index
            .tool_output_record_digests
            .contains(output.record_digest())
    {
        return Err(StoreError::Corrupt(
            "duplicate tool-output identity".to_owned(),
        ));
    }
    let intent = &index
        .intents
        .get(&anchor.effect_id)
        .expect("output anchor verifies its effect intent")
        .intent;
    output
        .verify_for(
            &anchor.run_id,
            &anchor.step_id,
            intent,
            anchor.execution_attempt,
            &anchor.evidence_digest,
        )
        .map_err(|_| StoreError::ToolOutputBindingMismatch)?;
    if output.record_digest() != anchor.record_digest {
        return Err(StoreError::ToolOutputBindingMismatch);
    }
    Ok(anchor)
}

fn verify_tool_output_records(
    index: &mut VerifiedRunIndex,
    outputs: &BTreeMap<String, StoredToolOutput>,
    metrics: &mut AuditMetrics,
) -> Result<(), StoreError> {
    if index.tool_output_events.len() != outputs.len() {
        return Err(StoreError::Corrupt(format!(
            "tool-output count differs from output-bound success events: expected {}, actual {}",
            index.tool_output_events.len(),
            outputs.len()
        )));
    }
    let mut anchors: Vec<_> = index.tool_output_events.values().cloned().collect();
    anchors.sort_by_key(|anchor| anchor.event_sequence);
    for anchor in &anchors {
        metrics.record_historical_tool_output();
        let stored = outputs.get(&anchor.effect_id).ok_or_else(|| {
            StoreError::Corrupt(format!(
                "effect `{}` has no durable tool output",
                anchor.effect_id
            ))
        })?;
        verify_stored_tool_output(index, anchor, stored)?;
        index_tool_output(index, anchor, &stored.record)?;
    }
    Ok(())
}

fn verify_stored_tool_output(
    index: &VerifiedRunIndex,
    anchor: &ToolOutputEventAnchor,
    stored: &StoredToolOutput,
) -> Result<(), StoreError> {
    if stored.event_sequence != anchor.event_sequence
        || stored.record.effect_id() != anchor.effect_id
        || stored.record.record_digest() != anchor.record_digest
    {
        return Err(StoreError::Corrupt(
            "tool output differs from its journal binding".to_owned(),
        ));
    }
    let intent = &index
        .intents
        .get(&anchor.effect_id)
        .ok_or_else(|| StoreError::Corrupt("tool output has no effect intent".to_owned()))?
        .intent;
    stored
        .record
        .verify_for(
            &anchor.run_id,
            &anchor.step_id,
            intent,
            anchor.execution_attempt,
            &anchor.evidence_digest,
        )
        .map_err(|_| StoreError::Corrupt("tool output record is invalid".to_owned()))
}

fn index_tool_output(
    index: &mut VerifiedRunIndex,
    anchor: &ToolOutputEventAnchor,
    output: &ToolOutputRecord,
) -> Result<(), StoreError> {
    if !index
        .tool_output_effect_ids
        .insert(anchor.effect_id.clone())
        || !index.tool_output_ids.insert(output.output_id().to_owned())
        || !index
            .tool_output_record_digests
            .insert(output.record_digest().to_owned())
        || index
            .tool_output_digests
            .insert(anchor.effect_id.clone(), output.output_digest().to_owned())
            .is_some()
        || index
            .tool_output_sizes
            .insert(anchor.effect_id.clone(), output.canonical_size_bytes())
            .is_some()
    {
        return Err(StoreError::Corrupt(
            "duplicate tool-output identity".to_owned(),
        ));
    }
    Ok(())
}

fn verify_tool_output_point(
    index: &VerifiedRunIndex,
    effect_id: &str,
    stored: Option<&StoredToolOutput>,
) -> Result<(), StoreError> {
    let anchor = index.tool_output_events.get(effect_id);
    match (anchor, stored) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(StoreError::Corrupt(
            "tool output has no output-bound success event".to_owned(),
        )),
        (Some(_), None) => Err(StoreError::Corrupt(
            "output-bound success has no durable tool output".to_owned(),
        )),
        (Some(anchor), Some(stored)) => verify_stored_tool_output(index, anchor, stored),
    }
}

fn verify_completion_output_bundle(
    event: &RunEvent,
    output: Option<&CompletionOutputRecord>,
) -> Result<(), StoreError> {
    match (&event.body, output) {
        (
            RunEventBody::CompletionCandidateRecorded {
                completion_output_record_digest: Some(expected),
                ..
            },
            Some(output),
        ) if expected == output.record_digest() => Ok(()),
        (RunEventBody::CompletionCandidateRecorded { .. }, Some(_)) => {
            Err(StoreError::CompletionOutputBindingMismatch)
        }
        (RunEventBody::CompletionCandidateRecorded { .. }, None) => {
            Err(StoreError::CompletionOutputRequired)
        }
        (_, Some(_)) => Err(StoreError::UnexpectedCompletionOutput),
        (_, None) => Ok(()),
    }
}

fn completion_anchor_decision(
    anchor: &CompletionOutputEventAnchor,
) -> Result<ExpectedPlanningTurn, StoreError> {
    Ok(ExpectedPlanningTurn::for_model_call(
        anchor.turn_index,
        &anchor.model_call_id,
        &anchor.context_digest,
        &anchor.proposal_digest,
    )?)
}

fn verify_completion_output_candidate(
    index: &VerifiedRunIndex,
    record: &EventRecord,
    output: &CompletionOutputRecord,
) -> Result<CompletionOutputEventAnchor, StoreError> {
    if index.completion_output_event.is_some() {
        return Err(StoreError::Corrupt(
            "duplicate completion-output identity".to_owned(),
        ));
    }
    let anchor = VerifiedRunIndex::completion_output_anchor_for(record)?;
    let decision = completion_anchor_decision(&anchor)?;
    output
        .verify_for(
            &anchor.run_id,
            &decision,
            &anchor.candidate_id,
            &anchor.summary_digest,
        )
        .map_err(|_| StoreError::CompletionOutputBindingMismatch)?;
    if output.record_digest() != anchor.record_digest {
        return Err(StoreError::CompletionOutputBindingMismatch);
    }
    Ok(anchor)
}

fn verify_stored_completion_output(
    anchor: &CompletionOutputEventAnchor,
    stored: &StoredCompletionOutput,
) -> Result<(), StoreError> {
    if stored.event_sequence != anchor.event_sequence
        || stored.record.candidate_id() != anchor.candidate_id
        || stored.record.record_digest() != anchor.record_digest
    {
        return Err(StoreError::Corrupt(
            "completion output differs from its journal binding".to_owned(),
        ));
    }
    let decision = completion_anchor_decision(anchor)?;
    stored
        .record
        .verify_for(
            &anchor.run_id,
            &decision,
            &anchor.candidate_id,
            &anchor.summary_digest,
        )
        .map_err(|_| StoreError::Corrupt("completion output record is invalid".to_owned()))
}

fn verify_completion_output_record(
    index: &VerifiedRunIndex,
    stored: Option<&StoredCompletionOutput>,
) -> Result<(), StoreError> {
    match (&index.completion_output_event, stored) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(StoreError::Corrupt(
            "completion output has no digest-bound completion event".to_owned(),
        )),
        (Some(_), None) => Err(StoreError::Corrupt(
            "digest-bound completion event has no durable output".to_owned(),
        )),
        (Some(anchor), Some(stored)) => verify_stored_completion_output(anchor, stored),
    }
}

fn build_completion_output(
    index: &VerifiedRunIndex,
    expected: ExpectedHead,
    candidate_id: &str,
    stored: Option<StoredCompletionOutput>,
) -> Result<Option<CompletionOutputRecord>, StoreError> {
    let actual = index.head();
    if expected != actual {
        return Err(StoreError::HeadConflict { expected, actual });
    }
    let candidate = index
        .state
        .as_ref()
        .and_then(|state| state.agent_loop.as_ref())
        .and_then(|loop_state| loop_state.completion_candidate.as_ref());
    let Some(candidate) = candidate else {
        if stored.is_some() {
            return Err(StoreError::Corrupt(
                "completion output exists without a projected candidate".to_owned(),
            ));
        }
        return Ok(None);
    };
    if candidate.candidate_id != candidate_id {
        return Ok(None);
    }
    verify_completion_output_record(index, stored.as_ref())?;
    match (candidate.completion_output_record_digest.as_deref(), stored) {
        (None, None) => Ok(None),
        (Some(expected_digest), Some(stored))
            if expected_digest == stored.record.record_digest() =>
        {
            Ok(Some(stored.record))
        }
        _ => Err(StoreError::Corrupt(
            "completion output differs from its projected candidate".to_owned(),
        )),
    }
}

fn build_planning_snapshot<F>(
    index: &VerifiedRunIndex,
    expected: ExpectedHead,
    max_output_bytes: u64,
    mut load_output: F,
) -> Result<Option<RunPlanningSnapshot>, StoreError>
where
    F: FnMut(&str) -> Result<Option<StoredToolOutput>, StoreError>,
{
    let actual = index.head();
    if expected != actual {
        return Err(StoreError::HeadConflict { expected, actual });
    }
    let Some(state) = index.state.clone() else {
        return Ok(None);
    };
    let mut selections = Vec::new();
    let mut selected_output_bytes = 0_u64;
    for (step_id, step) in &state.steps {
        if step.status != StepStatus::Completed {
            continue;
        }
        let Some(expected_record_digest) = step.output_record_digest.as_deref() else {
            // Receipt-completed journals created before durable output sidecars remain readable,
            // but no output is invented for them.
            continue;
        };
        let intent = step.intent.as_ref().ok_or_else(|| {
            StoreError::Corrupt(
                "completed output-bound Step has no committed effect intent".to_owned(),
            )
        })?;
        let receipt_id = step.execution_receipt_id.as_deref().ok_or_else(|| {
            StoreError::Corrupt("completed output-bound Step has no Core Receipt ID".to_owned())
        })?;
        let receipt_digest = step.execution_receipt_digest.as_deref().ok_or_else(|| {
            StoreError::Corrupt("completed output-bound Step has no Core Receipt digest".to_owned())
        })?;
        let receipt_anchor = index
            .receipt_event_positions
            .get(&intent.effect_id)
            .and_then(|position| index.receipt_events.get(*position))
            .ok_or_else(|| {
                StoreError::Corrupt(
                    "completed output-bound Step has no verified Core Receipt".to_owned(),
                )
            })?;
        if receipt_anchor.step_id != *step_id
            || receipt_anchor.disposition != VerificationDisposition::Passed
            || receipt_anchor.receipt_id != receipt_id
            || receipt_anchor.receipt_digest != receipt_digest
            || !index.receipt_effect_ids.contains(&intent.effect_id)
        {
            return Err(StoreError::Corrupt(
                "completed tool output differs from its passed Core Receipt".to_owned(),
            ));
        }
        let output_size = index
            .tool_output_sizes
            .get(&intent.effect_id)
            .copied()
            .ok_or_else(|| {
                StoreError::Corrupt(
                    "completed tool output has no verified size commitment".to_owned(),
                )
            })?;
        selected_output_bytes = selected_output_bytes
            .checked_add(output_size)
            .ok_or(StoreError::PlanningSnapshotBudgetExceeded)?;
        if selected_output_bytes > max_output_bytes {
            return Err(StoreError::PlanningSnapshotBudgetExceeded);
        }
        selections.push((
            step_id.clone(),
            intent.effect_id.clone(),
            expected_record_digest.to_owned(),
        ));
    }
    let mut completed_tool_outputs = BTreeMap::new();
    for (step_id, effect_id, expected_record_digest) in selections {
        let stored = load_output(&effect_id)?;
        verify_tool_output_point(index, &effect_id, stored.as_ref())?;
        let stored = stored.ok_or_else(|| {
            StoreError::Corrupt("completed output-bound Step has no durable tool output".to_owned())
        })?;
        if stored.record.step_id() != step_id
            || stored.record.record_digest() != expected_record_digest
            || index
                .tool_output_digests
                .get(&effect_id)
                .map(String::as_str)
                != Some(stored.record.output_digest())
        {
            return Err(StoreError::Corrupt(
                "completed tool output differs from its verified projection".to_owned(),
            ));
        }
        if completed_tool_outputs
            .insert(step_id, stored.record)
            .is_some()
        {
            return Err(StoreError::Corrupt(
                "completed tool output Step identity is duplicated".to_owned(),
            ));
        }
    }
    Ok(Some(RunPlanningSnapshot::new(
        state,
        completed_tool_outputs,
    )))
}

fn verify_receipt_tool_output_binding(
    index: &VerifiedRunIndex,
    effect_id: &str,
    receipt: &ExecutionReceiptBody,
) -> Result<(), StoreError> {
    let intent = &index
        .intents
        .get(effect_id)
        .ok_or_else(|| StoreError::Corrupt("Receipt has no effect intent".to_owned()))?
        .intent;
    let profile = intent
        .receipt_provenance
        .as_ref()
        .and_then(|provenance| provenance.tool_output_profile.as_deref());
    match (profile, index.tool_output_digests.get(effect_id)) {
        (Some(TOOL_OUTPUT_PROFILE_V1), Some(output_digest))
            if receipt.output_digest == *output_digest =>
        {
            Ok(())
        }
        (Some(TOOL_OUTPUT_PROFILE_V1), _) => Err(StoreError::Corrupt(
            "Receipt output digest differs from the durable tool output".to_owned(),
        )),
        (None, None) => Ok(()),
        _ => Err(StoreError::Corrupt(
            "tool-output profile and durable output differ".to_owned(),
        )),
    }
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

impl VerifiedRunIndex {
    #[allow(clippy::too_many_lines)] // Keep every journal-to-index anchor in one audited pass.
    fn from_snapshot(
        snapshot: Option<&RunSnapshot>,
        metrics: &mut AuditMetrics,
    ) -> Result<Self, StoreError> {
        let Some(snapshot) = snapshot else {
            return Ok(Self::default());
        };
        let mut index = Self {
            state: Some(snapshot.state.clone()),
            ..Self::default()
        };
        for record in &snapshot.records {
            metrics.record_historical_event();
            if !index.event_ids.insert(record.event.event_id.clone()) {
                return Err(StoreError::Corrupt(
                    "journal contains a duplicate event identifier".to_owned(),
                ));
            }
            match &record.event.body {
                RunEventBody::PlanAccepted { steps, .. } => {
                    for step in steps {
                        if index
                            .planned_invocations
                            .insert(
                                step.step_id.clone(),
                                PlannedInvocationAnchor {
                                    event_sequence: record.sequence,
                                    binding: step.invocation.clone(),
                                },
                            )
                            .is_some()
                        {
                            return Err(StoreError::Corrupt(
                                "journal contains a duplicate planned invocation".to_owned(),
                            ));
                        }
                    }
                }
                RunEventBody::EffectIntentCommitted { step_id, intent } => {
                    if index
                        .intents
                        .insert(
                            intent.effect_id.clone(),
                            EffectIntentAnchor {
                                event_sequence: record.sequence,
                                step_id: step_id.clone(),
                                intent: intent.as_ref().clone(),
                            },
                        )
                        .is_some()
                    {
                        return Err(StoreError::Corrupt(
                            "journal contains a duplicate effect intent".to_owned(),
                        ));
                    }
                }
                RunEventBody::EffectExecutionStarted { step_id, effect_id } => {
                    let execution_attempt = index
                        .effect_starts
                        .get(effect_id)
                        .map_or(1, |anchor| anchor.execution_attempt.saturating_add(1));
                    index.effect_starts.insert(
                        effect_id.clone(),
                        EffectStartAnchor {
                            event_sequence: record.sequence,
                            step_id: step_id.clone(),
                            recorded_at: record.event.recorded_at.clone(),
                            execution_attempt,
                        },
                    );
                }
                RunEventBody::EffectSucceeded {
                    output_record_digest: Some(_),
                    ..
                } => {
                    let anchor = index.tool_output_anchor_for(record)?;
                    if index
                        .tool_output_events
                        .insert(anchor.effect_id.clone(), anchor)
                        .is_some()
                    {
                        return Err(StoreError::Corrupt(
                            "journal contains duplicate tool-output events".to_owned(),
                        ));
                    }
                }
                RunEventBody::CompletionCandidateRecorded {
                    completion_output_record_digest: Some(_),
                    ..
                } => {
                    if index.completion_output_event.is_some() {
                        return Err(StoreError::Corrupt(
                            "journal contains duplicate completion-output events".to_owned(),
                        ));
                    }
                    index.completion_output_event =
                        Some(Self::completion_output_anchor_for(record)?);
                }
                RunEventBody::VerificationRecorded { .. } => {
                    index.index_receipt_event(record, metrics)?;
                }
                _ => {}
            }
            index.last_record = Some(record.clone());
        }
        Ok(index)
    }

    fn head(&self) -> ExpectedHead {
        self.last_record
            .as_ref()
            .map_or(ExpectedHead::Empty, |record| ExpectedHead::Exact {
                sequence: record.sequence,
                digest: record.digest.clone(),
            })
    }

    fn index_receipt_event(
        &mut self,
        record: &EventRecord,
        metrics: &mut AuditMetrics,
    ) -> Result<(), StoreError> {
        let anchor = self.receipt_anchor_for(record, Some(metrics))?;
        let position = self.receipt_events.len();
        if self
            .receipt_event_positions
            .insert(anchor.effect_id.clone(), position)
            .is_some()
        {
            return Err(StoreError::Corrupt(
                "journal contains duplicate Receipt events for one effect".to_owned(),
            ));
        }
        self.receipt_events.push(anchor);
        Ok(())
    }

    fn receipt_anchor_for(
        &self,
        record: &EventRecord,
        mut metrics: Option<&mut AuditMetrics>,
    ) -> Result<ReceiptEventAnchor, StoreError> {
        let RunEventBody::VerificationRecorded {
            step_id,
            effect_id,
            disposition,
            receipt_id,
            receipt_digest,
        } = &record.event.body
        else {
            return Err(StoreError::UnexpectedExecutionReceipt);
        };
        if let Some(metrics) = metrics.as_mut() {
            metrics.record_receipt_anchor_intent_lookup();
        }
        let intent = self.intents.get(effect_id).ok_or_else(|| {
            StoreError::Corrupt("execution receipt has no committed effect intent".to_owned())
        })?;
        if intent.step_id != *step_id || intent.event_sequence >= record.sequence {
            return Err(StoreError::Corrupt(
                "execution receipt precedes or differs from its effect intent".to_owned(),
            ));
        }
        if let Some(metrics) = metrics.as_mut() {
            metrics.record_receipt_anchor_start_lookup();
        }
        let started = self.effect_starts.get(effect_id).ok_or_else(|| {
            StoreError::Corrupt("execution receipt has no start event".to_owned())
        })?;
        if started.step_id != *step_id || started.event_sequence >= record.sequence {
            return Err(StoreError::Corrupt(
                "execution receipt precedes or differs from its start event".to_owned(),
            ));
        }
        Ok(ReceiptEventAnchor {
            event_sequence: record.sequence,
            run_id: record.event.run_id.clone(),
            step_id: step_id.clone(),
            effect_id: effect_id.clone(),
            disposition: *disposition,
            receipt_id: receipt_id.clone(),
            receipt_digest: receipt_digest.clone(),
            started_at: started.recorded_at.clone(),
            ended_at: record.event.recorded_at.clone(),
        })
    }

    fn tool_output_anchor_for(
        &self,
        record: &EventRecord,
    ) -> Result<ToolOutputEventAnchor, StoreError> {
        let RunEventBody::EffectSucceeded {
            step_id,
            effect_id,
            evidence_digest,
            output_record_digest: Some(record_digest),
        } = &record.event.body
        else {
            return Err(StoreError::UnexpectedToolOutput);
        };
        let intent = self.intents.get(effect_id).ok_or_else(|| {
            StoreError::Corrupt("tool output has no committed effect intent".to_owned())
        })?;
        if intent.step_id != *step_id || intent.event_sequence >= record.sequence {
            return Err(StoreError::Corrupt(
                "tool output precedes or differs from its effect intent".to_owned(),
            ));
        }
        let profile = intent
            .intent
            .receipt_provenance
            .as_ref()
            .and_then(|provenance| provenance.tool_output_profile.as_deref());
        if profile != Some(TOOL_OUTPUT_PROFILE_V1) {
            return Err(StoreError::Corrupt(
                "tool output has no supported durable profile".to_owned(),
            ));
        }
        let started = self.effect_starts.get(effect_id).ok_or_else(|| {
            StoreError::Corrupt("tool output has no effect start event".to_owned())
        })?;
        if started.step_id != *step_id || started.event_sequence >= record.sequence {
            return Err(StoreError::Corrupt(
                "tool output precedes or differs from its start event".to_owned(),
            ));
        }
        Ok(ToolOutputEventAnchor {
            event_sequence: record.sequence,
            run_id: record.event.run_id.clone(),
            step_id: step_id.clone(),
            effect_id: effect_id.clone(),
            evidence_digest: evidence_digest.clone(),
            record_digest: record_digest.clone(),
            execution_attempt: started.execution_attempt,
        })
    }

    fn completion_output_anchor_for(
        record: &EventRecord,
    ) -> Result<CompletionOutputEventAnchor, StoreError> {
        let RunEventBody::CompletionCandidateRecorded {
            decision,
            candidate_id,
            summary_digest,
            completion_output_record_digest: Some(record_digest),
        } = &record.event.body
        else {
            return Err(StoreError::UnexpectedCompletionOutput);
        };
        let model_call_id = decision.model_call_id().ok_or_else(|| {
            StoreError::Corrupt(
                "completion output event has no durable model-call binding".to_owned(),
            )
        })?;
        Ok(CompletionOutputEventAnchor {
            event_sequence: record.sequence,
            run_id: record.event.run_id.clone(),
            candidate_id: candidate_id.clone(),
            turn_index: decision.turn_index(),
            model_call_id: model_call_id.to_owned(),
            context_digest: decision.context_digest().to_owned(),
            proposal_digest: decision.proposal_digest().to_owned(),
            summary_digest: summary_digest.clone(),
            record_digest: record_digest.clone(),
        })
    }

    fn verification_snapshot(&self, step_id: &str) -> Option<RunVerificationSnapshot> {
        let state = self.state.clone()?;
        let effect_started_at = state
            .steps
            .get(step_id)
            .and_then(|step| step.intent.as_ref())
            .and_then(|intent| self.effect_starts.get(&intent.effect_id))
            .filter(|started| started.step_id == step_id)
            .map(|started| started.recorded_at.clone());
        Some(RunVerificationSnapshot {
            state,
            effect_started_at,
            previous_receipt_digest: self.receipt_head_digest.clone(),
            tool_output: None,
        })
    }

    #[allow(clippy::too_many_lines)] // Mirror the cold index pass for every atomic sidecar kind.
    fn apply_committed(
        &mut self,
        commit: &Commit,
        sidecars: CommitSidecars<'_>,
        anchors: CommitAnchors,
    ) {
        let CommitSidecars {
            plan_inputs,
            material,
            output,
            completion_output: _,
            receipt,
        } = sidecars;
        let CommitAnchors {
            output: output_anchor,
            completion_output: completion_output_anchor,
            receipt: receipt_anchor,
        } = anchors;
        self.event_ids.insert(commit.record.event.event_id.clone());
        match &commit.record.event.body {
            RunEventBody::PlanAccepted { steps, .. } => {
                for step in steps {
                    self.planned_invocations.insert(
                        step.step_id.clone(),
                        PlannedInvocationAnchor {
                            event_sequence: commit.record.sequence,
                            binding: step.invocation.clone(),
                        },
                    );
                }
            }
            RunEventBody::EffectIntentCommitted { step_id, intent } => {
                self.intents.insert(
                    intent.effect_id.clone(),
                    EffectIntentAnchor {
                        event_sequence: commit.record.sequence,
                        step_id: step_id.clone(),
                        intent: intent.as_ref().clone(),
                    },
                );
            }
            RunEventBody::EffectExecutionStarted { step_id, effect_id } => {
                let execution_attempt = self
                    .effect_starts
                    .get(effect_id)
                    .map_or(1, |anchor| anchor.execution_attempt.saturating_add(1));
                self.effect_starts.insert(
                    effect_id.clone(),
                    EffectStartAnchor {
                        event_sequence: commit.record.sequence,
                        step_id: step_id.clone(),
                        recorded_at: commit.record.event.recorded_at.clone(),
                        execution_attempt,
                    },
                );
            }
            RunEventBody::EffectSucceeded {
                output_record_digest: Some(_),
                ..
            } => {
                let anchor = output_anchor
                    .expect("verified tool-output commit must retain its event anchor");
                self.tool_output_events
                    .insert(anchor.effect_id.clone(), anchor);
            }
            RunEventBody::CompletionCandidateRecorded {
                completion_output_record_digest: Some(_),
                ..
            } => {
                self.completion_output_event = Some(
                    completion_output_anchor
                        .expect("verified completion-output commit must retain its event anchor"),
                );
            }
            RunEventBody::VerificationRecorded { .. } => {
                let anchor =
                    receipt_anchor.expect("verified Receipt commit must retain its event anchor");
                self.receipt_event_positions
                    .insert(anchor.effect_id.clone(), self.receipt_events.len());
                self.receipt_events.push(anchor);
            }
            _ => {}
        }
        if let Some(inputs) = plan_inputs {
            self.plan_input_step_ids
                .extend(inputs.iter().map(|input| input.step_id().to_owned()));
        }
        if let Some(material) = material {
            self.material_effect_ids
                .insert(material.effect_id().to_owned());
        }
        if let Some(output) = output {
            let effect_id = output.effect_id().to_owned();
            self.tool_output_effect_ids.insert(effect_id.clone());
            self.tool_output_ids.insert(output.output_id().to_owned());
            self.tool_output_record_digests
                .insert(output.record_digest().to_owned());
            self.tool_output_digests
                .insert(effect_id.clone(), output.output_digest().to_owned());
            self.tool_output_sizes
                .insert(effect_id, output.canonical_size_bytes());
        }
        if let Some(receipt) = receipt {
            self.receipt_ids.insert(receipt.receipt_id.clone());
            self.receipt_digests.insert(receipt.receipt_digest.clone());
            if let RunEventBody::VerificationRecorded { effect_id, .. } = &commit.record.event.body
            {
                self.receipt_effect_ids.insert(effect_id.clone());
            }
            self.receipt_head_digest = Some(receipt.receipt_digest.clone());
        }
        self.state = Some(commit.state.clone());
        self.last_record = Some(commit.record.clone());
    }
}

fn verify_receipt_records(
    index: &mut VerifiedRunIndex,
    receipts: &[StoredExecutionReceipt],
    metrics: &mut AuditMetrics,
) -> Result<(), StoreError> {
    if index.receipt_events.len() != receipts.len() {
        return Err(StoreError::Corrupt(format!(
            "execution receipt count differs from finalization events: expected {}, actual {}",
            index.receipt_events.len(),
            receipts.len()
        )));
    }

    let mut previous_digest: Option<&str> = None;
    for (anchor, stored) in index.receipt_events.iter().zip(receipts) {
        metrics.record_historical_receipt();
        let receipt = &stored.receipt;
        validate_execution_receipt(receipt)
            .map_err(|_| StoreError::Corrupt("execution receipt document is invalid".to_owned()))?;
        if stored.event_sequence != anchor.event_sequence
            || stored.effect_id != anchor.effect_id
            || receipt.receipt_id != anchor.receipt_id
            || receipt.receipt_digest != anchor.receipt_digest
            || receipt.run_id != anchor.run_id
            || receipt.step_id != anchor.step_id
            || receipt.previous_receipt_digest.as_deref() != previous_digest
        {
            return Err(StoreError::Corrupt(
                "execution receipt differs from its journal binding".to_owned(),
            ));
        }
        metrics.record_receipt_binding_intent_lookup();
        let intent = &index
            .intents
            .get(&anchor.effect_id)
            .expect("Receipt anchor creation verifies the effect intent")
            .intent;
        let provenance = intent.receipt_provenance.as_ref().ok_or_else(|| {
            StoreError::Corrupt("execution receipt has no durable provenance".to_owned())
        })?;
        verify_receipt_intent_binding(receipt, intent, provenance, anchor.disposition)?;
        verify_receipt_tool_output_binding(index, &anchor.effect_id, receipt)?;
        verify_receipt_timestamps(anchor, receipt)?;
        if !index.receipt_ids.insert(receipt.receipt_id.clone())
            || !index.receipt_digests.insert(receipt.receipt_digest.clone())
            || !index.receipt_effect_ids.insert(anchor.effect_id.clone())
        {
            return Err(StoreError::Corrupt(
                "execution receipt identity is duplicated".to_owned(),
            ));
        }
        previous_digest = Some(&receipt.receipt_digest);
    }
    index.receipt_head_digest = previous_digest.map(ToOwned::to_owned);
    Ok(())
}

fn verify_receipt_candidate(
    index: &VerifiedRunIndex,
    record: &EventRecord,
    receipt: &ExecutionReceiptBody,
) -> Result<ReceiptEventAnchor, StoreError> {
    let anchor = index.receipt_anchor_for(record, None)?;
    if index.receipt_ids.contains(&receipt.receipt_id)
        || index.receipt_digests.contains(&receipt.receipt_digest)
        || index.receipt_effect_ids.contains(&anchor.effect_id)
    {
        return Err(StoreError::Corrupt(
            "duplicate execution receipt identity".to_owned(),
        ));
    }
    if receipt.receipt_id != anchor.receipt_id
        || receipt.receipt_digest != anchor.receipt_digest
        || receipt.run_id != anchor.run_id
        || receipt.step_id != anchor.step_id
        || receipt.previous_receipt_digest != index.receipt_head_digest
    {
        return Err(StoreError::Corrupt(
            "execution receipt differs from its verified journal or chain anchor".to_owned(),
        ));
    }
    let intent = &index
        .intents
        .get(&anchor.effect_id)
        .expect("Receipt anchor creation verifies the effect intent")
        .intent;
    let provenance = intent
        .receipt_provenance
        .as_ref()
        .ok_or(StoreError::ReceiptProvenanceRequired)?;
    verify_receipt_intent_binding(receipt, intent, provenance, anchor.disposition)?;
    verify_receipt_tool_output_binding(index, &anchor.effect_id, receipt)?;
    verify_receipt_timestamps(&anchor, receipt)?;
    Ok(anchor)
}

fn verify_receipt_intent_binding(
    receipt: &ExecutionReceiptBody,
    intent: &xgeny_workgraph::EffectIntent,
    provenance: &xgeny_workgraph::ReceiptProvenance,
    disposition: VerificationDisposition,
) -> Result<(), StoreError> {
    verify_core_receipt_artifacts(receipt, intent, provenance)?;
    if receipt.receipt_id != core_receipt_id_v1(&intent.effect_id)
        || !receipt.extensions.is_empty()
        || !receipt.required_extensions.is_empty()
        || !has_core_redactions(receipt)
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

fn verify_core_receipt_artifacts(
    receipt: &ExecutionReceiptBody,
    intent: &xgeny_workgraph::EffectIntent,
    provenance: &xgeny_workgraph::ReceiptProvenance,
) -> Result<(), StoreError> {
    match provenance.profile_version.as_str() {
        CORE_RECEIPT_PROFILE_V1 => {
            if intent.effect_class == EffectClass::ReadOnly || !receipt.artifacts.is_empty() {
                return Err(StoreError::Corrupt(
                    "core Receipt v1 artifact semantics differ".to_owned(),
                ));
            }
        }
        CORE_RECEIPT_PROFILE_V2 => {
            if intent.effect_class != EffectClass::ReadOnly
                || receipt.artifacts.is_empty()
                || receipt.artifacts.len() > CORE_RECEIPT_MAX_ARTIFACTS_V2
            {
                return Err(StoreError::Corrupt(
                    "core Receipt v2 artifact semantics differ".to_owned(),
                ));
            }
            let mut identifiers = BTreeSet::new();
            let mut total_size = 0_u64;
            for artifact in &receipt.artifacts {
                let expected_provenance = artifact.provenance.as_ref().is_some_and(|artifact| {
                    artifact.run_id == receipt.run_id
                        && artifact.step_id == receipt.step_id
                        && artifact.receipt_id.as_deref() == Some(receipt.receipt_id.as_str())
                });
                total_size = total_size.checked_add(artifact.size).ok_or_else(|| {
                    StoreError::Corrupt("core Receipt artifact size overflows".to_owned())
                })?;
                if !core_artifact_descriptor_v2_is_valid(
                    &artifact.artifact_id,
                    artifact.name.as_deref(),
                    &artifact.media_type,
                    artifact.size,
                    &artifact.digest,
                ) || total_size > CORE_RECEIPT_MAX_ARTIFACT_TOTAL_BYTES_V2
                    || !identifiers.insert(artifact.artifact_id.as_str())
                    || !expected_provenance
                    || !artifact.extensions.is_empty()
                    || !artifact.required_extensions.is_empty()
                {
                    return Err(StoreError::Corrupt(
                        "core Receipt artifact binding differs".to_owned(),
                    ));
                }
            }
        }
        _ => return Err(StoreError::UnsupportedReceiptProfile),
    }
    Ok(())
}

fn verify_receipt_timestamps(
    anchor: &ReceiptEventAnchor,
    receipt: &ExecutionReceiptBody,
) -> Result<(), StoreError> {
    if receipt.started_at != anchor.started_at
        || receipt.effect.started_at.as_deref() != Some(anchor.started_at.as_str())
        || receipt.ended_at != anchor.ended_at
    {
        return Err(StoreError::Corrupt(
            "execution receipt timestamps differ from the journal".to_owned(),
        ));
    }
    let started = OffsetDateTime::parse(&anchor.started_at, &Rfc3339).map_err(|_| {
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
        EffectClass::ReadOnly => ProtocolEffectClass::ReadOnly,
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

fn prepare_commit(
    index: &VerifiedRunIndex,
    expected: ExpectedHead,
    event: RunEvent,
) -> Result<Commit, StoreError> {
    let actual = index.head();
    if expected != actual {
        return Err(StoreError::HeadConflict { expected, actual });
    }
    if index.event_ids.contains(&event.event_id) {
        return Err(StoreError::DuplicateEventId(event.event_id));
    }
    let record = EventRecord::next(index.last_record.as_ref(), event)?;
    let state = apply_record(index.state.as_ref(), &record)?;
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

fn audit_snapshot(
    records: Vec<EventRecord>,
    persisted: Option<RunState>,
    metrics: &mut AuditMetrics,
) -> Result<(Option<RunSnapshot>, VerifiedRunIndex), StoreError> {
    let snapshot = verified_snapshot(records, persisted)?;
    let index = VerifiedRunIndex::from_snapshot(snapshot.as_ref(), metrics)?;
    Ok((snapshot, index))
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
    ToolOutput(#[from] ToolOutputError),
    #[error(transparent)]
    CompletionOutput(#[from] CompletionOutputError),
    #[error(transparent)]
    PlanningContract(#[from] PlanningContractError),
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
    #[error("an accepted plan requires all planned invocation input sidecars atomically")]
    PlannedInvocationInputsRequired,
    #[error("planned invocation inputs were supplied for an event that is not an accepted plan")]
    UnexpectedPlannedInvocationInputs,
    #[error("this Run store does not support durable planned invocation inputs")]
    PlannedInvocationStoreUnsupported,
    #[error("accepted plan expected {expected} input sidecars, got {actual}")]
    PlannedInvocationInputCountMismatch { expected: usize, actual: usize },
    #[error("accepted Step `{0}` has no planned invocation input sidecar")]
    PlannedInvocationInputMissing(String),
    #[error("planned invocation input for Step `{0}` is duplicated")]
    DuplicatePlannedInvocationInput(String),
    #[error("effect material for planned Step `{0}` does not retain its accepted recipe reference")]
    PlannedInvocationRetentionMismatch(String),
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
    #[error("a tool-output-bound success event requires an atomic ToolOutputRecord")]
    ToolOutputRequired,
    #[error("a ToolOutputRecord was supplied for an unrelated event")]
    UnexpectedToolOutput,
    #[error("the ToolOutputRecord differs from its success event or effect intent")]
    ToolOutputBindingMismatch,
    #[error("a new read-only effect intent requires the supported tool-output profile")]
    ToolOutputProfileRequired,
    #[error("this Run store does not support durable tool outputs")]
    ToolOutputStoreUnsupported,
    #[error("a completion candidate requires an atomic CompletionOutputRecord")]
    CompletionOutputRequired,
    #[error("a CompletionOutputRecord was supplied for an unrelated event")]
    UnexpectedCompletionOutput,
    #[error("the CompletionOutputRecord differs from its completion event")]
    CompletionOutputBindingMismatch,
    #[error("this Run store does not support durable completion outputs")]
    CompletionOutputStoreUnsupported,
    #[error("this Run store does not support generation-checked planning snapshots")]
    PlanningSnapshotStoreUnsupported,
    #[error("verified tool outputs exceed the planning snapshot byte budget")]
    PlanningSnapshotBudgetExceeded,
    #[error("injected append fault after {0}")]
    InjectedFault(&'static str),
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::tempdir;
    use xgeny_domain::{
        API_VERSION_V1ALPHA1, ArtifactProvenance, ArtifactRef, CapabilityRef, Executor,
        ProtocolDocument, ReceiptEffect, ReceiptPolicy, ReceiptStatus, VerificationEvidence,
    };
    use xgeny_protocol::{
        CORE_RECEIPT_MAX_ARTIFACT_SIZE_BYTES_V2, CORE_RECEIPT_PROFILE_V2,
        canonical_digest_without_field,
    };
    use xgeny_workgraph::{
        AcceptedPlanStep, AgentLoopBudget, AuthorizationBinding, AuthorizationUse,
        ContinuationAction, EffectClass, EffectIntent, ExpectedPlanningTurn, InvocationBinding,
        InvocationMaterialRecord, InvocationMaterialRetention, ModelCallAbandonmentReason,
        ModelCallBudget, ModelCallReservation, ModelCallSettlement, ModelCallStatus,
        ModelCallUnknownReason, PlannedExecutionProfile, PlannedInvocationMaterialRecord,
        PlannedInvocationSpec, ReceiptPlacement, ReceiptProvenance, ReceiptVerificationRule,
        ReceiptVerificationStrategy, ReconciliationResolution, ReconstructableMaterialReference,
        RunEvent, RunEventBody, RunState, SinkGuarantee, StepStatus, VerificationDisposition,
        authorization_digest, derive_frontier, invocation_material_digest,
        invocation_material_retention_digest, once_authorization_id, receipt_provenance_digest,
    };

    use super::*;

    fn sqlite_blob_rows(connection: &rusqlite::Connection, query: &str) -> Vec<Vec<u8>> {
        let mut statement = connection.prepare(query).expect("query should prepare");
        statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .expect("query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("blob rows should load")
    }

    fn sqlite_table_count(connection: &rusqlite::Connection, table_name: &str) -> i64 {
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table_name],
                |row| row.get(0),
            )
            .expect("schema should remain inspectable")
    }

    type SqliteEventRow = (i64, String, Option<String>, String, Vec<u8>);
    type SqliteAuthorizationRow = (String, String, String, String, i64);
    type SqliteStoreMutation = fn(&SqliteRunStore) -> Result<(), StoreError>;
    type ReceiptMutation = fn(&mut ExecutionReceiptBody);

    #[derive(Debug, PartialEq, Eq)]
    struct SqliteDurableRows {
        events: Vec<SqliteEventRow>,
        projection: Vec<Vec<u8>>,
        intents: Vec<Vec<u8>>,
        authorizations: Vec<SqliteAuthorizationRow>,
        materials: Vec<Vec<u8>>,
        tool_outputs: Vec<Vec<u8>>,
        plans: Vec<Vec<u8>>,
        receipts: Vec<Vec<u8>>,
        completion_outputs: Vec<Vec<u8>>,
    }

    fn sqlite_durable_rows(connection: &rusqlite::Connection) -> SqliteDurableRows {
        SqliteDurableRows {
            events: sqlite_event_rows(connection),
            projection: sqlite_blob_rows(
                connection,
                "SELECT state_json FROM run_projection ORDER BY singleton",
            ),
            intents: sqlite_blob_rows(
                connection,
                "SELECT intent_json FROM effect_intents ORDER BY effect_id",
            ),
            authorizations: sqlite_authorization_rows(connection),
            materials: sqlite_blob_rows(
                connection,
                "SELECT record_json FROM invocation_materials ORDER BY effect_id",
            ),
            tool_outputs: sqlite_blob_rows(
                connection,
                "SELECT record_json FROM tool_outputs ORDER BY event_sequence, effect_id",
            ),
            plans: sqlite_blob_rows(
                connection,
                "SELECT record_json FROM planned_invocations ORDER BY event_sequence, step_id",
            ),
            receipts: sqlite_blob_rows(
                connection,
                "SELECT receipt_json FROM execution_receipts ORDER BY event_sequence",
            ),
            completion_outputs: sqlite_blob_rows(
                connection,
                "SELECT record_json FROM completion_outputs ORDER BY event_sequence",
            ),
        }
    }

    fn sqlite_event_rows(connection: &rusqlite::Connection) -> Vec<SqliteEventRow> {
        let mut statement = connection
            .prepare(
                "SELECT sequence, event_id, previous_digest, digest, event_json \
                 FROM run_events ORDER BY sequence",
            )
            .expect("event query should prepare");
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("event query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("event rows should load")
    }

    fn sqlite_authorization_rows(connection: &rusqlite::Connection) -> Vec<SqliteAuthorizationRow> {
        let mut statement = connection
            .prepare(
                "SELECT grant_id, effect_id, action_digest, grant_digest, max_uses \
                 FROM authorization_consumption ORDER BY grant_id, effect_id",
            )
            .expect("authorization query should prepare");
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("authorization query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("authorization rows should load")
    }

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
            tool_output_profile: None,
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
                        depends_on: Vec::new(),
                    },
                ),
            )
            .expect("step plan should commit")
    }

    fn plan_input(
        step_id: &str,
        proposal_digest: &str,
        marker: char,
    ) -> (AcceptedPlanStep, PlannedInvocationMaterialRecord) {
        let hex = |value: char| format!("sha256:{}", value.to_string().repeat(64));
        let spec = PlannedInvocationSpec::new(
            "test.read",
            "1.0.0",
            hex('a'),
            hex(marker),
            hex('e'),
            PlannedExecutionProfile::LocalSyncOnceV1,
            "linux",
            "x86_64",
        )
        .expect("plan spec should be valid");
        let reference = ReconstructableMaterialReference::new(
            "test-recipes",
            format!("recipe-{marker}"),
            "rev-1",
        )
        .expect("recipe reference should be valid");
        let (binding, record) = PlannedInvocationMaterialRecord::bind(
            "run-1",
            step_id,
            proposal_digest,
            spec,
            reference,
        )
        .expect("plan input should bind");
        (
            AcceptedPlanStep {
                step_id: step_id.to_owned(),
                objective: format!("perform {step_id}"),
                depends_on: Vec::new(),
                invocation: binding,
            },
            record,
        )
    }

    fn seed_planning_context<S: RunStore>(store: &mut S) -> Commit {
        let created = store
            .append(
                ExpectedHead::Empty,
                event(
                    "plan-test-run-created",
                    RunEventBody::RunCreated {
                        goal: "durable plan failure tests".to_owned(),
                    },
                ),
            )
            .expect("Run should be created");
        store
            .append(
                ExpectedHead::from_state(&created.state),
                event(
                    "plan-test-loop-configured",
                    RunEventBody::AgentLoopConfigured {
                        budget: AgentLoopBudget::new(4, 8, 8, 16_384)
                            .expect("budget should be valid"),
                    },
                ),
            )
            .expect("loop should configure")
    }

    fn seed_model_call_lifecycle<S: RunStore>(store: &mut S) -> Commit {
        let configured = seed_planning_context(store);
        store
            .append(
                ExpectedHead::from_state(&configured.state),
                event(
                    "model-call-lifecycle-configured",
                    RunEventBody::ModelCallLifecycleConfigured {
                        budget: ModelCallBudget::new(3).expect("model-call budget should validate"),
                    },
                ),
            )
            .expect("model-call lifecycle should configure")
    }

    fn model_call_reservation(state: &RunState) -> ModelCallReservation {
        ModelCallReservation::new(
            &state.run_id,
            state.authority_epoch,
            "xgeny.test.store-planner",
            1,
            1,
            state.journal_sequence,
            &state.journal_head_digest,
            format!("sha256:{}", "c".repeat(64)),
            format!("sha256:{}", "d".repeat(64)),
        )
        .expect("model-call reservation should validate")
    }

    fn accepted_plan_event(
        event_id: &str,
        proposal_digest: &str,
        steps: Vec<AcceptedPlanStep>,
    ) -> RunEvent {
        event(
            event_id,
            RunEventBody::PlanAccepted {
                decision: ExpectedPlanningTurn::new(
                    1,
                    format!("sha256:{}", "d".repeat(64)),
                    proposal_digest,
                )
                .expect("turn should bind"),
                steps,
            },
        )
    }

    fn append_plan_bundle<S: RunStore>(store: &mut S) -> Commit {
        let created = store
            .append(
                ExpectedHead::Empty,
                event(
                    "plan-run-created",
                    RunEventBody::RunCreated {
                        goal: "durable plan".to_owned(),
                    },
                ),
            )
            .expect("Run should be created");
        let configured = store
            .append(
                ExpectedHead::from_state(&created.state),
                event(
                    "plan-loop-configured",
                    RunEventBody::AgentLoopConfigured {
                        budget: AgentLoopBudget::new(4, 8, 8, 16_384)
                            .expect("budget should be valid"),
                    },
                ),
            )
            .expect("loop should configure");
        let proposal_digest = format!("sha256:{}", "f".repeat(64));
        let (step_a, input_a) = plan_input("step-a", &proposal_digest, 'b');
        let (mut step_b, input_b) = plan_input("step-b", &proposal_digest, 'c');
        step_b.depends_on.push("step-a".to_owned());
        let event = event(
            "plan-accepted",
            RunEventBody::PlanAccepted {
                decision: ExpectedPlanningTurn::new(
                    1,
                    format!("sha256:{}", "d".repeat(64)),
                    proposal_digest,
                )
                .expect("turn should bind"),
                steps: vec![step_b, step_a],
            },
        );
        store
            .append_with_plan_inputs(
                ExpectedHead::from_state(&configured.state),
                event,
                vec![input_b, input_a],
            )
            .expect("plan and inputs should commit atomically")
    }

    fn planned_effect_bundle(
        state: &RunState,
        step_id: &str,
        retention: InvocationMaterialRetention,
    ) -> (RunEvent, InvocationMaterialRecord) {
        let planned = state.steps[step_id]
            .planned_invocation
            .as_ref()
            .expect("accepted Step should have a planned binding");
        let invocation = InvocationBinding {
            capability_id: planned.capability_id().to_owned(),
            contract_version: planned.contract_version().to_owned(),
            definition_digest: planned.definition_digest().to_owned(),
            instance_id: "test.planned.instance".to_owned(),
            instance_binding_digest: format!("sha256:{}", "8".repeat(64)),
        };
        let provenance = ReceiptProvenance {
            profile_version: CORE_RECEIPT_PROFILE_V1.to_owned(),
            tool_output_profile: None,
            invocation_id: "invocation-planned-step".to_owned(),
            plan_id: planned.plan_id().to_owned(),
            policy_decision_id: "decision-planned-step".to_owned(),
            policy_decision_digest: format!("sha256:{}", "7".repeat(64)),
            executor_id: "xgeny-local".to_owned(),
            executor_placement: ReceiptPlacement::Local,
            executor_platform: format!("{}-{}", planned.target_os(), planned.target_arch()),
            input_summary: CORE_RECEIPT_INPUT_SUMMARY_V1.to_owned(),
            verification_plan: Vec::new(),
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
            action_digest: planned.action_digest().to_owned(),
            material_digest: planned.plan_input_digest().to_owned(),
            material_retention_digest: invocation_material_retention_digest(&retention)
                .expect("retention should digest"),
            policy_evidence_digest: format!("sha256:{}", "6".repeat(64)),
            receipt_provenance_digest: Some(
                receipt_provenance_digest(&provenance).expect("provenance should digest"),
            ),
        };
        let intent = EffectIntent {
            effect_id: format!("effect-{step_id}"),
            action_digest: planned.action_digest().to_owned(),
            invocation,
            effect_class: EffectClass::Idempotent,
            idempotency_key: Some(format!("key-{step_id}")),
            sink_guarantee: SinkGuarantee::None,
            authorization: AuthorizationUse {
                grant_id: once_authorization_id(&state.run_id, planned.action_digest())
                    .expect("authorization ID should derive"),
                grant_digest: authorization_digest(&binding, 1)
                    .expect("authorization should digest"),
                max_uses: 1,
                binding,
            },
            receipt_provenance: Some(provenance),
        };
        let material = InvocationMaterialRecord::new(
            &state.run_id,
            step_id,
            &intent,
            planned.plan_input_digest(),
            retention,
        )
        .expect("material should bind");
        (
            event(
                "planned-effect-intent",
                RunEventBody::EffectIntentCommitted {
                    step_id: step_id.to_owned(),
                    intent: Box::new(intent),
                },
            ),
            material,
        )
    }

    #[test]
    fn memory_and_sqlite_atomically_retain_all_plan_inputs() {
        let mut memory = MemoryRunStore::new();
        let memory_commit = append_plan_bundle(&mut memory);
        assert_eq!(memory_commit.state.steps.len(), 2);
        assert_eq!(
            memory
                .load_planned_invocation("step-a")
                .expect("memory plan input should load")
                .expect("step-a input should exist")
                .step_id(),
            "step-a"
        );

        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("planned.db");
        let sqlite_commit = {
            let mut sqlite = SqliteRunStore::open(&path).expect("SQLite should open");
            append_plan_bundle(&mut sqlite)
        };
        let reopened = SqliteRunStore::open(&path).expect("SQLite should reopen");
        assert_eq!(
            reopened
                .load_current()
                .expect("projection should load")
                .expect("Run should exist"),
            sqlite_commit.state
        );
        assert_eq!(
            reopened
                .load_planned_invocation("step-b")
                .expect("SQLite plan input should load")
                .expect("step-b input should exist")
                .step_id(),
            "step-b"
        );
    }

    fn assert_planned_recipe_replacement_is_rejected<S: RunStore>(store: &mut S) {
        let planned = append_plan_bundle(store);
        let before = store
            .load()
            .expect("planned store should verify")
            .expect("Run should exist");
        let replacement = InvocationMaterialRetention::ReconstructableReference(
            ReconstructableMaterialReference::new("other-recipes", "replacement", "rev-9")
                .expect("replacement reference should validate"),
        );
        let (event, material) = planned_effect_bundle(&planned.state, "step-a", replacement);

        let result = store.append_with_invocation_material(
            ExpectedHead::from_state(&planned.state),
            event,
            material,
        );

        assert!(matches!(
            result,
            Err(StoreError::PlannedInvocationRetentionMismatch(step_id)) if step_id == "step-a"
        ));
        assert_eq!(
            store.load().expect("rejected store should still verify"),
            Some(before)
        );
    }

    #[test]
    fn memory_and_sqlite_pin_final_effect_material_to_the_accepted_recipe() {
        let mut memory = MemoryRunStore::new();
        assert_planned_recipe_replacement_is_rejected(&mut memory);

        let directory = tempdir().expect("temp directory should exist");
        let mut sqlite = SqliteRunStore::open(directory.path().join("planned-retention.db"))
            .expect("SQLite should open");
        assert_planned_recipe_replacement_is_rejected(&mut sqlite);
    }

    fn assert_plan_input_bundle_rejections_leave_store_unchanged<S: RunStore>(store: &mut S) {
        let configured = seed_planning_context(store);
        let before = store
            .load()
            .expect("seeded store should verify")
            .expect("seeded Run should exist");
        let expected = ExpectedHead::from_state(&configured.state);
        let proposal_digest = format!("sha256:{}", "f".repeat(64));
        let (step_a, input_a) = plan_input("step-a", &proposal_digest, 'a');

        let plain = store.append(
            expected.clone(),
            accepted_plan_event(
                "plain-plan-accepted",
                &proposal_digest,
                vec![step_a.clone()],
            ),
        );
        assert!(matches!(
            plain,
            Err(StoreError::PlannedInvocationInputsRequired)
        ));
        assert_eq!(
            store.load().expect("plain rejection should verify"),
            Some(before.clone())
        );

        let (step_b, _input_b) = plan_input("step-b", &proposal_digest, 'b');
        let (_step_c, orphan_input) = plan_input("step-c", &proposal_digest, 'c');
        let missing = store.append_with_plan_inputs(
            expected.clone(),
            accepted_plan_event(
                "missing-plan-input",
                &proposal_digest,
                vec![step_a.clone(), step_b],
            ),
            vec![input_a.clone(), orphan_input],
        );
        assert!(matches!(
            missing,
            Err(StoreError::PlannedInvocationInputMissing(step_id)) if step_id == "step-b"
        ));
        assert_eq!(
            store.load().expect("missing rejection should verify"),
            Some(before.clone())
        );

        let orphan = store.append_with_plan_inputs(
            expected.clone(),
            event(
                "orphan-plan-input",
                RunEventBody::StepPlanned {
                    step_id: "legacy-step".to_owned(),
                    objective: "legacy manual planning".to_owned(),
                    depends_on: Vec::new(),
                },
            ),
            vec![input_a.clone()],
        );
        assert!(matches!(
            orphan,
            Err(StoreError::UnexpectedPlannedInvocationInputs)
        ));
        assert_eq!(
            store.load().expect("orphan rejection should verify"),
            Some(before.clone())
        );

        let (_same_step, mismatched_input) = plan_input("step-a", &proposal_digest, '9');
        let mismatched = store.append_with_plan_inputs(
            expected,
            accepted_plan_event("mismatched-plan-input", &proposal_digest, vec![step_a]),
            vec![mismatched_input],
        );
        assert!(matches!(mismatched, Err(StoreError::PlanningContract(_))));
        assert_eq!(
            store.load().expect("mismatch rejection should verify"),
            Some(before)
        );
        assert!(
            store
                .load_planned_invocation("step-a")
                .expect("uncommitted input lookup should verify")
                .is_none()
        );
    }

    #[test]
    fn memory_and_sqlite_reject_incomplete_or_mismatched_plan_bundles_without_mutation() {
        let mut memory = MemoryRunStore::new();
        assert_plan_input_bundle_rejections_leave_store_unchanged(&mut memory);

        let directory = tempdir().expect("temp directory should exist");
        let mut sqlite =
            SqliteRunStore::open(directory.path().join("planned.db")).expect("SQLite should open");
        assert_plan_input_bundle_rejections_leave_store_unchanged(&mut sqlite);
        assert_eq!(
            sqlite
                .planned_invocation_count()
                .expect("planned invocation count"),
            0
        );
    }

    #[test]
    fn sqlite_rolls_back_plan_event_sidecars_and_projection_at_each_plan_write_stage() {
        for fault in [
            AppendFault::Event,
            AppendFault::PlannedInvocation,
            AppendFault::Projection,
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let mut store = SqliteRunStore::open(directory.path().join("planned.db"))
                .expect("SQLite should open");
            let configured = seed_planning_context(&mut store);
            let before = store
                .load()
                .expect("seeded store should verify")
                .expect("seeded Run should exist");
            let proposal_digest = format!("sha256:{}", "f".repeat(64));
            let (step_a, input_a) = plan_input("step-a", &proposal_digest, 'a');
            let (step_b, input_b) = plan_input("step-b", &proposal_digest, 'b');
            let candidate =
                accepted_plan_event("faulted-plan", &proposal_digest, vec![step_a, step_b]);
            let inputs = vec![input_a, input_b];

            let result = store.append_plan_with_fault(
                ExpectedHead::from_state(&configured.state),
                candidate.clone(),
                &inputs,
                fault,
            );
            assert!(matches!(result, Err(StoreError::InjectedFault(_))));
            assert_eq!(
                store.load().expect("rolled-back store should verify"),
                Some(before)
            );
            assert_eq!(store.run_event_count().expect("event count"), 2);
            assert_eq!(
                store
                    .planned_invocation_count()
                    .expect("planned invocation count"),
                0
            );

            let committed = store
                .append_with_plan_inputs(
                    ExpectedHead::from_state(&configured.state),
                    candidate,
                    inputs,
                )
                .expect("retry should atomically commit the plan bundle");
            assert_eq!(committed.record.sequence, 3);
            assert_eq!(store.run_event_count().expect("event count"), 3);
            assert_eq!(
                store
                    .planned_invocation_count()
                    .expect("planned invocation count"),
                2
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Keeps the complete Memory/SQLite reopen parity chain visible.
    fn memory_and_sqlite_reopen_preserve_unknown_model_call_without_retry_state() {
        let directory = tempdir().expect("temp directory should exist");
        let database = directory.path().join("model-call.db");
        let mut memory = MemoryRunStore::new();
        let memory_lifecycle = seed_model_call_lifecycle(&mut memory);
        let mut sqlite = SqliteRunStore::open(&database).expect("SQLite should open");
        let sqlite_lifecycle = seed_model_call_lifecycle(&mut sqlite);
        assert_eq!(memory_lifecycle.state, sqlite_lifecycle.state);

        let reservation = model_call_reservation(&memory_lifecycle.state);
        let reserved_event = event(
            "model-call-reserved",
            RunEventBody::ModelCallReserved {
                reservation: reservation.clone(),
            },
        );
        let memory_reserved = memory
            .append(
                ExpectedHead::from_state(&memory_lifecycle.state),
                reserved_event.clone(),
            )
            .expect("memory reservation should commit");
        let sqlite_reserved = sqlite
            .append(
                ExpectedHead::from_state(&sqlite_lifecycle.state),
                reserved_event,
            )
            .expect("SQLite reservation should commit");
        assert_eq!(memory_reserved.state, sqlite_reserved.state);

        let unknown_event = event(
            "model-call-unknown",
            RunEventBody::ModelCallBecameUnknown {
                call_id: reservation.call_id().to_owned(),
                reason: ModelCallUnknownReason::Timeout,
            },
        );
        let memory_unknown = memory
            .append(
                ExpectedHead::from_state(&memory_reserved.state),
                unknown_event.clone(),
            )
            .expect("memory Unknown transition should commit");
        let sqlite_unknown = sqlite
            .append(
                ExpectedHead::from_state(&sqlite_reserved.state),
                unknown_event,
            )
            .expect("SQLite Unknown transition should commit");
        assert_eq!(memory_unknown.state, sqlite_unknown.state);
        drop(sqlite);

        let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
        let reopened_unknown = reopened
            .load_current()
            .expect("reopened state should verify")
            .expect("Run should exist");
        assert_eq!(reopened_unknown, memory_unknown.state);
        let calls = reopened_unknown
            .agent_loop
            .as_ref()
            .and_then(|loop_state| loop_state.model_calls.as_ref())
            .expect("model-call lifecycle should project");
        assert_eq!(
            (
                calls.reserved_calls,
                calls.settled_calls,
                calls.unknown_calls
            ),
            (1, 0, 1)
        );
        assert!(matches!(
            calls.active_call.as_ref().map(|call| call.status),
            Some(ModelCallStatus::Unknown {
                reason: ModelCallUnknownReason::Timeout
            })
        ));

        let abandoned_event = event(
            "model-call-abandoned",
            RunEventBody::ModelCallSettled {
                call_id: reservation.call_id().to_owned(),
                settlement: ModelCallSettlement::Abandoned {
                    reason: ModelCallAbandonmentReason::RecoveryDiscarded,
                },
            },
        );
        let memory_abandoned = memory
            .append(
                ExpectedHead::from_state(&memory_unknown.state),
                abandoned_event.clone(),
            )
            .expect("memory abandonment should commit");
        let sqlite_abandoned = reopened
            .append(ExpectedHead::from_state(&reopened_unknown), abandoned_event)
            .expect("SQLite abandonment should commit");
        assert_eq!(memory_abandoned.state, sqlite_abandoned.state);
        let calls = sqlite_abandoned
            .state
            .agent_loop
            .as_ref()
            .and_then(|loop_state| loop_state.model_calls.as_ref())
            .expect("settled model-call lifecycle should project");
        assert_eq!(
            (
                calls.reserved_calls,
                calls.settled_calls,
                calls.unknown_calls
            ),
            (1, 1, 1)
        );
        assert!(calls.active_call.is_none());
    }

    #[test]
    fn sqlite_rolls_back_model_call_reservation_event_and_projection_together() {
        for fault in [AppendFault::Event, AppendFault::Projection] {
            let directory = tempdir().expect("temp directory should exist");
            let database = directory.path().join("model-call-fault.db");
            let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
            let lifecycle = seed_model_call_lifecycle(&mut store);
            let before = store
                .load()
                .expect("seeded store should verify")
                .expect("seeded Run should exist");
            let reservation = model_call_reservation(&lifecycle.state);
            let candidate = event(
                "faulted-model-call-reservation",
                RunEventBody::ModelCallReserved {
                    reservation: reservation.clone(),
                },
            );

            let result = store.append_plain_with_fault(
                ExpectedHead::from_state(&lifecycle.state),
                candidate.clone(),
                fault,
            );
            assert!(matches!(result, Err(StoreError::InjectedFault(_))));
            assert_eq!(
                store.load().expect("rolled-back store should verify"),
                Some(before.clone())
            );
            assert_eq!(store.run_event_count().expect("event count"), 3);
            drop(store);

            let mut reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
            assert_eq!(
                reopened.load().expect("reopened store should verify"),
                Some(before)
            );
            let committed = reopened
                .append(ExpectedHead::from_state(&lifecycle.state), candidate)
                .expect("reservation should commit after rollback");
            assert_eq!(committed.record.sequence, 4);
            let calls = committed
                .state
                .agent_loop
                .as_ref()
                .and_then(|loop_state| loop_state.model_calls.as_ref())
                .expect("reservation should project");
            assert_eq!(
                (
                    calls.reserved_calls,
                    calls.settled_calls,
                    calls.unknown_calls
                ),
                (1, 0, 0)
            );
            assert!(matches!(
                calls.active_call.as_ref().map(|call| call.status),
                Some(ModelCallStatus::Reserved)
            ));
        }
    }

    #[test]
    fn sqlite_rolls_back_model_call_success_plan_sidecars_and_settlement_together() {
        for fault in [
            AppendFault::Event,
            AppendFault::PlannedInvocation,
            AppendFault::Projection,
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let database = directory.path().join("model-call-plan-fault.db");
            let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
            let lifecycle = seed_model_call_lifecycle(&mut store);
            let reservation = model_call_reservation(&lifecycle.state);
            let reserved = store
                .append(
                    ExpectedHead::from_state(&lifecycle.state),
                    event(
                        "model-call-plan-reserved",
                        RunEventBody::ModelCallReserved {
                            reservation: reservation.clone(),
                        },
                    ),
                )
                .expect("reservation should commit");
            let before = store
                .load()
                .expect("reserved store should verify")
                .expect("reserved Run should exist");
            let proposal_digest = format!("sha256:{}", "f".repeat(64));
            let (step, input) = plan_input("model-call-step", &proposal_digest, 'a');
            let candidate = event(
                "faulted-model-call-plan",
                RunEventBody::PlanAccepted {
                    decision: ExpectedPlanningTurn::for_model_call(
                        reservation.turn_index(),
                        reservation.call_id(),
                        reservation.context_digest(),
                        &proposal_digest,
                    )
                    .expect("model-call decision should bind"),
                    steps: vec![step],
                },
            );

            let result = store.append_plan_with_fault(
                ExpectedHead::from_state(&reserved.state),
                candidate,
                &[input],
                fault,
            );
            assert!(matches!(result, Err(StoreError::InjectedFault(_))));
            assert_eq!(
                store.load().expect("rolled-back store should verify"),
                Some(before.clone())
            );
            assert_eq!(store.run_event_count().expect("event count"), 4);
            assert_eq!(
                store
                    .planned_invocation_count()
                    .expect("planned invocation count"),
                0
            );
            drop(store);

            let reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
            let reopened_state = reopened
                .load_current()
                .expect("reopened state should verify")
                .expect("Run should exist");
            assert_eq!(reopened_state, before.state);
            assert!(reopened_state.steps.is_empty());
            let calls = reopened_state
                .agent_loop
                .as_ref()
                .and_then(|loop_state| loop_state.model_calls.as_ref())
                .expect("reservation should remain projected");
            assert_eq!((calls.reserved_calls, calls.settled_calls), (1, 0));
            assert!(matches!(
                calls.active_call.as_ref().map(|call| call.status),
                Some(ModelCallStatus::Reserved)
            ));
        }
    }

    #[test]
    fn sqlite_rolls_back_model_call_unknown_settlement_and_projection_together() {
        for fault in [AppendFault::Event, AppendFault::Projection] {
            let directory = tempdir().expect("temp directory should exist");
            let database = directory.path().join("model-call-unknown-fault.db");
            let mut store = SqliteRunStore::open(&database).expect("SQLite should open");
            let lifecycle = seed_model_call_lifecycle(&mut store);
            let reservation = model_call_reservation(&lifecycle.state);
            let reserved = store
                .append(
                    ExpectedHead::from_state(&lifecycle.state),
                    event(
                        "model-call-unknown-reserved",
                        RunEventBody::ModelCallReserved {
                            reservation: reservation.clone(),
                        },
                    ),
                )
                .expect("reservation should commit");
            let before = store
                .load()
                .expect("reserved store should verify")
                .expect("reserved Run should exist");
            let candidate = event(
                "faulted-model-call-unknown",
                RunEventBody::ModelCallBecameUnknown {
                    call_id: reservation.call_id().to_owned(),
                    reason: ModelCallUnknownReason::TransportUnavailable,
                },
            );

            let result = store.append_plain_with_fault(
                ExpectedHead::from_state(&reserved.state),
                candidate,
                fault,
            );
            assert!(matches!(result, Err(StoreError::InjectedFault(_))));
            assert_eq!(
                store.load().expect("rolled-back store should verify"),
                Some(before.clone())
            );
            drop(store);

            let reopened = SqliteRunStore::open(&database).expect("SQLite should reopen");
            let reopened_state = reopened
                .load_current()
                .expect("reopened state should verify")
                .expect("Run should exist");
            assert_eq!(reopened_state, before.state);
            let calls = reopened_state
                .agent_loop
                .as_ref()
                .and_then(|loop_state| loop_state.model_calls.as_ref())
                .expect("reservation should remain projected");
            assert_eq!(
                (
                    calls.reserved_calls,
                    calls.settled_calls,
                    calls.unknown_calls
                ),
                (1, 0, 0)
            );
            assert!(matches!(
                calls.active_call.as_ref().map(|call| call.status),
                Some(ModelCallStatus::Reserved)
            ));
        }
    }

    #[test]
    fn sqlite_detects_missing_tampered_and_orphan_planned_invocation_sidecars() {
        let populate = |path: &std::path::Path| {
            let mut store = SqliteRunStore::open(path).expect("SQLite should open");
            append_plan_bundle(&mut store);
            assert_eq!(
                store
                    .planned_invocation_count()
                    .expect("planned invocation count"),
                2
            );
        };

        let missing_directory = tempdir().expect("temp directory should exist");
        let missing_path = missing_directory.path().join("missing.db");
        populate(&missing_path);
        let connection = rusqlite::Connection::open(&missing_path).expect("raw SQLite should open");
        connection
            .execute(
                "DELETE FROM planned_invocations WHERE step_id = 'step-a'",
                [],
            )
            .expect("sidecar deletion should commit");
        drop(connection);
        assert!(matches!(
            SqliteRunStore::open(&missing_path),
            Err(StoreError::Corrupt(_))
        ));

        let tampered_directory = tempdir().expect("temp directory should exist");
        let tampered_path = tampered_directory.path().join("tampered.db");
        populate(&tampered_path);
        let connection =
            rusqlite::Connection::open(&tampered_path).expect("raw SQLite should open");
        let record_json: Vec<u8> = connection
            .query_row(
                "SELECT record_json FROM planned_invocations WHERE step_id = 'step-a'",
                [],
                |row| row.get(0),
            )
            .expect("planned invocation should load");
        let mut record: serde_json::Value =
            serde_json::from_slice(&record_json).expect("record should decode");
        assert_eq!(record["reference"]["revision"], "rev-1");
        record["reference"]["revision"] = serde_json::Value::String("rev-2".to_owned());
        connection
            .execute(
                "UPDATE planned_invocations SET record_json = ?1 WHERE step_id = 'step-a'",
                [serde_json::to_vec(&record).expect("tampered record should encode")],
            )
            .expect("sidecar tampering should commit");
        drop(connection);
        assert!(matches!(
            SqliteRunStore::open(&tampered_path),
            Err(StoreError::Corrupt(_))
        ));

        let orphan_directory = tempdir().expect("temp directory should exist");
        let orphan_path = orphan_directory.path().join("orphan.db");
        populate(&orphan_path);
        let connection = rusqlite::Connection::open(&orphan_path).expect("raw SQLite should open");
        connection
            .execute(
                "INSERT INTO planned_invocations \
                 (step_id, event_sequence, plan_id, record_digest, record_json) \
                 SELECT 'orphan-step', event_sequence, plan_id || '-orphan', \
                        record_digest || '-orphan', record_json \
                 FROM planned_invocations WHERE step_id = 'step-a'",
                [],
            )
            .expect("orphan sidecar insertion should commit");
        drop(connection);
        assert!(matches!(
            SqliteRunStore::open(&orphan_path),
            Err(StoreError::Corrupt(_))
        ));
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
            tool_output_profile: None,
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

    fn read_only_receipt_intent(state: &RunState) -> EffectIntent {
        let mut effect = receipt_intent(state);
        effect.effect_class = EffectClass::ReadOnly;
        effect.idempotency_key = None;
        let provenance = effect
            .receipt_provenance
            .as_mut()
            .expect("Receipt provenance should exist");
        provenance.profile_version = CORE_RECEIPT_PROFILE_V2.to_owned();
        provenance.tool_output_profile = Some(TOOL_OUTPUT_PROFILE_V1.to_owned());
        effect.authorization.binding.receipt_provenance_digest =
            Some(receipt_provenance_digest(provenance).expect("provenance should canonicalize"));
        effect.authorization.grant_digest = authorization_digest(&effect.authorization.binding, 1)
            .expect("read-only authorization should canonicalize");
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

    fn numbered_receipt_intent(state: &RunState, step_id: &str, ordinal: u16) -> EffectIntent {
        let mut effect = receipt_intent(state);
        effect.effect_id = format!("receipt-scale-effect-{ordinal}");
        effect.action_digest = format!("sha256:receipt-scale-action-{ordinal}");
        effect.idempotency_key = Some(format!("receipt-scale-key-{ordinal}"));
        effect.authorization.binding.step_id = step_id.to_owned();
        effect.authorization.binding.action_digest = effect.action_digest.clone();
        effect.authorization.grant_id = once_authorization_id(&state.run_id, &effect.action_digest)
            .expect("scaled authorization ID should canonicalize");
        let provenance = effect
            .receipt_provenance
            .as_mut()
            .expect("Receipt provenance should exist");
        provenance.invocation_id = format!("receipt-scale-invocation-{ordinal}");
        provenance.plan_id = format!("receipt-scale-plan-{ordinal}");
        provenance.policy_decision_id = format!("receipt-scale-decision-{ordinal}");
        provenance.policy_decision_digest = format!("sha256:{}", "c".repeat(64));
        effect.authorization.binding.receipt_provenance_digest =
            Some(receipt_provenance_digest(provenance).expect("provenance should canonicalize"));
        effect.authorization.grant_digest = authorization_digest(&effect.authorization.binding, 1)
            .expect("scaled authorization should canonicalize");
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
                        output_record_digest: None,
                    },
                ),
            )
            .expect("effect evidence should commit");
        (succeeded, effect)
    }

    fn seed_read_only_validating<S: RunStore>(store: &mut S) -> (Commit, EffectIntent) {
        let planned = seed(store);
        let effect = read_only_receipt_intent(&planned.state);
        let validating = append_receipt_effect_to_validating(
            store,
            &planned.state,
            "step-1",
            &effect,
            "read-only-receipt",
        );
        (validating, effect)
    }

    fn seed_read_only_executing<S: RunStore>(store: &mut S) -> (Commit, EffectIntent) {
        let planned = seed(store);
        let effect = read_only_receipt_intent(&planned.state);
        let committed = store
            .append_with_invocation_material(
                ExpectedHead::from_state(&planned.state),
                event(
                    "read-only-output-intent",
                    RunEventBody::EffectIntentCommitted {
                        step_id: "step-1".to_owned(),
                        intent: Box::new(effect.clone()),
                    },
                ),
                material_for(&planned.state, "step-1", &effect),
            )
            .expect("read-only intent should commit");
        let started = store
            .append(
                ExpectedHead::from_state(&committed.state),
                event(
                    "read-only-output-started",
                    RunEventBody::EffectExecutionStarted {
                        step_id: "step-1".to_owned(),
                        effect_id: effect.effect_id.clone(),
                    },
                ),
            )
            .expect("read-only effect should start");
        (started, effect)
    }

    fn tool_output_success(
        started: &RunState,
        effect: &EffectIntent,
        event_id: &str,
        output: serde_json::Value,
    ) -> (RunEvent, ToolOutputRecord) {
        let evidence_digest = format!("sha256:{}", "d".repeat(64));
        let record = ToolOutputRecord::new(
            &started.run_id,
            "step-1",
            effect,
            started.steps["step-1"].attempts,
            &evidence_digest,
            output,
        )
        .expect("tool output should bind");
        let candidate = event(
            event_id,
            RunEventBody::EffectSucceeded {
                step_id: "step-1".to_owned(),
                effect_id: effect.effect_id.clone(),
                evidence_digest,
                output_record_digest: Some(record.record_digest().to_owned()),
            },
        );
        (candidate, record)
    }

    fn assert_secret_absent(secret: &str, exposures: impl IntoIterator<Item = String>) {
        for exposure in exposures {
            assert!(
                !exposure.contains(secret),
                "raw output must stay outside observable diagnostics"
            );
        }
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
        let evidence_digest = format!("sha256:{}", "d".repeat(64));
        if effect
            .receipt_provenance
            .as_ref()
            .and_then(|provenance| provenance.tool_output_profile.as_deref())
            == Some(TOOL_OUTPUT_PROFILE_V1)
        {
            let output = ToolOutputRecord::new(
                &started.state.run_id,
                step_id,
                effect,
                started.state.steps[step_id].attempts,
                &evidence_digest,
                serde_json::json!({}),
            )
            .expect("test tool output should bind");
            let output_record_digest = output.record_digest().to_owned();
            store
                .append_with_tool_output(
                    ExpectedHead::from_state(&started.state),
                    event(
                        &format!("{event_prefix}-succeeded"),
                        RunEventBody::EffectSucceeded {
                            step_id: step_id.to_owned(),
                            effect_id: effect.effect_id.clone(),
                            evidence_digest,
                            output_record_digest: Some(output_record_digest),
                        },
                    ),
                    output,
                )
                .expect("effect output should commit")
        } else {
            store
                .append(
                    ExpectedHead::from_state(&started.state),
                    event(
                        &format!("{event_prefix}-succeeded"),
                        RunEventBody::EffectSucceeded {
                            step_id: step_id.to_owned(),
                            effect_id: effect.effect_id.clone(),
                            evidence_digest,
                            output_record_digest: None,
                        },
                    ),
                )
                .expect("effect evidence should commit")
        }
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

    fn successful_read_only_receipt(effect: &EffectIntent) -> ExecutionReceiptBody {
        let mut receipt = successful_receipt(effect);
        receipt.effect.class = ProtocolEffectClass::ReadOnly;
        receipt.effect.idempotency_key = None;
        receipt.output_digest =
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a".to_owned();
        receipt.artifacts = vec![ArtifactRef {
            artifact_id: "artifact-read-output".to_owned(),
            name: Some("read-output.json".to_owned()),
            media_type: "application/json".to_owned(),
            size: 128,
            digest: format!("sha256:{}", "a".repeat(64)),
            provenance: Some(ArtifactProvenance {
                run_id: receipt.run_id.clone(),
                step_id: receipt.step_id.clone(),
                receipt_id: Some(receipt.receipt_id.clone()),
            }),
            extensions: BTreeMap::new(),
            required_extensions: Vec::new(),
        }];
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

    fn assert_read_only_receipt_mutation_rejected<S: RunStore>(
        store: &mut S,
        validating: &RunState,
        effect: &EffectIntent,
        case: &str,
        mutate: fn(&mut ExecutionReceiptBody),
    ) {
        let mut receipt = successful_read_only_receipt(effect);
        mutate(&mut receipt);
        seal_receipt(&mut receipt);
        let candidate = receipt_event(effect, &receipt);

        let error = store
            .append_with_execution_receipt(ExpectedHead::from_state(validating), candidate, receipt)
            .expect_err(case);
        assert!(
            matches!(
                error,
                StoreError::Corrupt(_) | StoreError::ExecutionReceiptInvalid
            ),
            "unexpected {case} error: {error:?}"
        );
        assert_eq!(
            store.load_current().expect("store should load"),
            Some(validating.clone()),
            "{case} must not mutate durable state"
        );
        assert!(
            store
                .load_execution_receipts()
                .expect("Receipts should load")
                .is_empty(),
            "{case} must not persist a Receipt"
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

    fn prepare_completion_output<S: RunStore>(
        store: &mut S,
        summary: &str,
    ) -> (Commit, RunEvent, CompletionOutputRecord) {
        let (validating, effect) = seed_validating(store);
        let receipt = successful_receipt(&effect);
        let completed = store
            .append_with_execution_receipt(
                ExpectedHead::from_state(&validating.state),
                receipt_event(&effect, &receipt),
                receipt,
            )
            .expect("Receipt should complete the Step");
        let configured = store
            .append(
                ExpectedHead::from_state(&completed.state),
                event(
                    "completion-loop-configured",
                    RunEventBody::AgentLoopConfigured {
                        budget: AgentLoopBudget::new(4, 8, 8, 16_384)
                            .expect("loop budget should validate"),
                    },
                ),
            )
            .expect("loop should configure");
        let lifecycle = store
            .append(
                ExpectedHead::from_state(&configured.state),
                event(
                    "completion-model-call-lifecycle",
                    RunEventBody::ModelCallLifecycleConfigured {
                        budget: ModelCallBudget::new(3).expect("model-call budget should validate"),
                    },
                ),
            )
            .expect("model-call lifecycle should configure");
        let context_digest = format!("sha256:{}", "c".repeat(64));
        let reservation = ModelCallReservation::new(
            &lifecycle.state.run_id,
            lifecycle.state.authority_epoch,
            "xgeny.test.completion-planner",
            1,
            1,
            lifecycle.state.journal_sequence,
            &lifecycle.state.journal_head_digest,
            &context_digest,
            format!("sha256:{}", "d".repeat(64)),
        )
        .expect("completion model call should reserve");
        let reserved = store
            .append(
                ExpectedHead::from_state(&lifecycle.state),
                event(
                    "completion-model-call-reserved",
                    RunEventBody::ModelCallReserved {
                        reservation: reservation.clone(),
                    },
                ),
            )
            .expect("completion model call should commit");
        let output = CompletionOutputRecord::bind(
            &reserved.state.run_id,
            1,
            reservation.call_id(),
            context_digest,
            summary,
        )
        .expect("completion output should bind");
        let event = event(
            "completion-candidate-recorded",
            RunEventBody::CompletionCandidateRecorded {
                decision: output.decision().expect("decision should reconstruct"),
                candidate_id: output.candidate_id().to_owned(),
                summary_digest: output.summary_digest().to_owned(),
                completion_output_record_digest: Some(output.record_digest().to_owned()),
            },
        );
        (reserved, event, output)
    }

    #[test]
    fn memory_and_sqlite_atomically_restore_exact_completion_output() {
        const SUMMARY: &str = "FINAL-SUMMARY-SENTINEL \"quote\" \\ slash\n\t- 한글과 UTF-8 결과";
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut memory = MemoryRunStore::new();
        let mut sqlite = SqliteRunStore::open(&path).expect("SQLite should open");
        let (memory_reserved, memory_event, memory_output) =
            prepare_completion_output(&mut memory, SUMMARY);
        let (sqlite_reserved, sqlite_event, sqlite_output) =
            prepare_completion_output(&mut sqlite, SUMMARY);

        for (case, error) in [
            (
                "memory",
                memory
                    .append(
                        ExpectedHead::from_state(&memory_reserved.state),
                        memory_event.clone(),
                    )
                    .expect_err("plain memory append must fail"),
            ),
            (
                "sqlite",
                sqlite
                    .append(
                        ExpectedHead::from_state(&sqlite_reserved.state),
                        sqlite_event.clone(),
                    )
                    .expect_err("plain SQLite append must fail"),
            ),
        ] {
            assert!(
                matches!(error, StoreError::CompletionOutputRequired),
                "unexpected {case} error: {error:?}"
            );
        }

        let memory_commit = memory
            .append_with_completion_output(
                ExpectedHead::from_state(&memory_reserved.state),
                memory_event,
                memory_output,
            )
            .expect("memory completion should commit");
        let sqlite_commit = sqlite
            .append_with_completion_output(
                ExpectedHead::from_state(&sqlite_reserved.state),
                sqlite_event,
                sqlite_output,
            )
            .expect("SQLite completion should commit");
        assert_eq!(memory_commit, sqlite_commit);
        let expected_head = ExpectedHead::from_state(&sqlite_commit.state);
        let candidate_id = &sqlite_commit
            .state
            .agent_loop
            .as_ref()
            .unwrap()
            .completion_candidate
            .as_ref()
            .unwrap()
            .candidate_id;
        let memory_loaded = memory
            .load_completion_output(expected_head.clone(), candidate_id)
            .expect("memory completion should load")
            .expect("memory completion should exist");
        let sqlite_loaded = sqlite
            .load_completion_output(expected_head.clone(), candidate_id)
            .expect("SQLite completion should load")
            .expect("SQLite completion should exist");
        assert_eq!(memory_loaded, sqlite_loaded);
        assert_eq!(sqlite_loaded.summary().as_bytes(), SUMMARY.as_bytes());
        assert_eq!(
            sqlite.completion_output_count().expect("completion count"),
            1
        );
        assert!(!format!("{sqlite_loaded:?}").contains(SUMMARY));
        assert!(
            !String::from_utf8(sqlite.export_jsonl().expect("journal export"))
                .expect("journal should be UTF-8")
                .contains(SUMMARY)
        );

        drop(sqlite);
        let reopened = SqliteRunStore::open(&path).expect("SQLite should reopen");
        let reopened_output = reopened
            .load_completion_output(expected_head, candidate_id)
            .expect("reopened completion should load")
            .expect("reopened completion should exist");
        assert_eq!(reopened_output.summary().as_bytes(), SUMMARY.as_bytes());
    }

    #[test]
    fn completion_output_binding_failures_leave_the_run_unchanged() {
        let mut memory = MemoryRunStore::new();
        let (reserved, candidate_event, output) =
            prepare_completion_output(&mut memory, "BOUND-COMPLETION-SUMMARY");
        let expected_state = reserved.state.clone();

        let unrelated = event(
            "unrelated-completion-sidecar",
            RunEventBody::StepPlanned {
                step_id: "unrelated-step".to_owned(),
                objective: "must not commit".to_owned(),
                depends_on: Vec::new(),
            },
        );
        assert!(matches!(
            memory.append_with_completion_output(
                ExpectedHead::from_state(&expected_state),
                unrelated,
                output.clone(),
            ),
            Err(StoreError::UnexpectedCompletionOutput)
        ));

        let mut tampered_value =
            serde_json::to_value(&output).expect("completion output should serialize");
        tampered_value["summary"] = serde_json::json!("TAMPERED-COMPLETION-SUMMARY");
        let tampered: CompletionOutputRecord = serde_json::from_value(tampered_value)
            .expect("structurally valid tampered record should deserialize");
        assert!(matches!(
            memory.append_with_completion_output(
                ExpectedHead::from_state(&expected_state),
                candidate_event,
                tampered,
            ),
            Err(StoreError::CompletionOutputBindingMismatch)
        ));
        assert_eq!(
            memory.load_current().expect("state should load"),
            Some(expected_state)
        );
    }

    #[test]
    fn sqlite_rolls_back_every_partial_completion_output_commit() {
        for stage in [
            AppendFault::Event,
            AppendFault::CompletionOutput,
            AppendFault::Projection,
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let path = directory.path().join("run.db");
            let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
            let (reserved, event, output) =
                prepare_completion_output(&mut store, "FAULT-COMPLETION-SUMMARY");
            let event_count = store.run_event_count().expect("event count should load");

            let error = store
                .append_completion_output_with_fault(
                    ExpectedHead::from_state(&reserved.state),
                    event,
                    &output,
                    stage,
                )
                .expect_err("injected completion fault should fail");
            assert!(matches!(error, StoreError::InjectedFault(_)));
            assert_eq!(
                store.run_event_count().expect("event count should load"),
                event_count
            );
            assert_eq!(
                store
                    .completion_output_count()
                    .expect("completion count should load"),
                0
            );
            assert_eq!(
                store.load_current().expect("state should load"),
                Some(reserved.state)
            );
        }
    }

    #[test]
    fn sqlite_process_exit_after_completion_output_insert_rolls_back_candidate_and_row() {
        const CHILD_MARKER: &str = "XGENY_SQLITE_COMPLETION_CRASH_CHILD";
        const DATABASE_PATH: &str = "XGENY_SQLITE_COMPLETION_CRASH_PATH";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let path = std::env::var_os(DATABASE_PATH).expect("child database path should exist");
            let mut store = SqliteRunStore::open(path).expect("child SQLite fixture should open");
            let (reserved, event, output) =
                prepare_completion_output(&mut store, "CRASH-COMPLETION-SUMMARY");
            let _never_returns = store.append_completion_output_and_exit_at(
                ExpectedHead::from_state(&reserved.state),
                event,
                &output,
                AppendFault::CompletionOutput,
            );
            unreachable!("completion crash injection must terminate the child process");
        }

        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("completion-crash.db");
        let status = Command::new(std::env::current_exe().expect("test binary should resolve"))
            .arg("--exact")
            .arg(
                "tests::sqlite_process_exit_after_completion_output_insert_rolls_back_candidate_and_row",
            )
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .env(DATABASE_PATH, &path)
            .status()
            .expect("completion crash child should start");
        assert_eq!(status.code(), Some(86));

        let store = SqliteRunStore::open(&path).expect("SQLite should reopen after child exit");
        assert_eq!(
            store
                .completion_output_count()
                .expect("completion count should load"),
            0
        );
        let state = store
            .load_current()
            .expect("state should verify")
            .expect("Run should remain");
        let loop_state = state.agent_loop.expect("loop should remain configured");
        assert!(loop_state.completion_candidate.is_none());
        assert!(
            loop_state
                .model_calls
                .expect("model-call lifecycle should remain")
                .active_call
                .is_some()
        );
    }

    #[test]
    fn sqlite_cold_audit_rejects_missing_and_tampered_completion_outputs() {
        let mutations: [(&str, SqliteStoreMutation); 3] = [
            ("missing", SqliteRunStore::delete_completion_output),
            (
                "tampered",
                SqliteRunStore::corrupt_completion_output_document,
            ),
            ("orphan", SqliteRunStore::insert_orphan_completion_output),
        ];
        for (case, mutate) in mutations {
            let directory = tempdir().expect("temp directory should exist");
            let path = directory.path().join(format!("{case}.db"));
            let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
            let (reserved, event, output) =
                prepare_completion_output(&mut store, "AUDIT-COMPLETION-SUMMARY");
            store
                .append_with_completion_output(
                    ExpectedHead::from_state(&reserved.state),
                    event,
                    output,
                )
                .expect("completion should commit");
            mutate(&store).expect("fixture mutation should apply");
            drop(store);

            let error = SqliteRunStore::open(&path)
                .expect_err("corrupt completion output must fail cold audit");
            assert!(
                matches!(error, StoreError::Corrupt(_)),
                "unexpected {case} error: {error:?}"
            );
            assert!(!format!("{error:?}").contains("AUDIT-COMPLETION-SUMMARY"));
        }
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
                        depends_on: Vec::new(),
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
                    depends_on: Vec::new(),
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
                        output_record_digest: None,
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
    fn memory_and_sqlite_atomically_commit_and_reopen_the_exact_tool_output() {
        const SECRET_SENTINEL: &str = "tool-output-secret-must-not-reach-journal-or-debug";

        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut memory = MemoryRunStore::new();
        let mut sqlite = SqliteRunStore::open(&path).expect("SQLite should open");
        let (memory_started, memory_effect) = seed_read_only_executing(&mut memory);
        let (sqlite_started, sqlite_effect) = seed_read_only_executing(&mut sqlite);
        let value = serde_json::json!({
            "content": SECRET_SENTINEL,
            "metadata": {"encoding": "utf-8"}
        });
        let (memory_event, memory_output) = tool_output_success(
            &memory_started.state,
            &memory_effect,
            "read-only-output-succeeded",
            value.clone(),
        );
        let (sqlite_event, sqlite_output) = tool_output_success(
            &sqlite_started.state,
            &sqlite_effect,
            "read-only-output-succeeded",
            value.clone(),
        );
        assert_eq!(memory_output, sqlite_output);
        assert!(!format!("{memory_output:?}").contains(SECRET_SENTINEL));

        let memory_commit = memory
            .append_with_tool_output(
                ExpectedHead::from_state(&memory_started.state),
                memory_event,
                memory_output.clone(),
            )
            .expect("memory output should commit");
        let sqlite_commit = sqlite
            .append_with_tool_output(
                ExpectedHead::from_state(&sqlite_started.state),
                sqlite_event,
                sqlite_output.clone(),
            )
            .expect("SQLite output should commit");

        assert_eq!(memory_commit, sqlite_commit);
        assert_eq!(
            memory_commit.state.steps["step-1"].status,
            StepStatus::Validating
        );
        assert_eq!(
            memory
                .load_tool_output(memory_effect.effect_id.as_str())
                .expect("memory output should load"),
            Some(memory_output.clone())
        );
        assert_eq!(
            sqlite
                .load_tool_output(sqlite_effect.effect_id.as_str())
                .expect("SQLite output should load"),
            Some(sqlite_output.clone())
        );
        assert_eq!(sqlite.tool_output_count().expect("output count"), 1);
        sqlite.reset_test_metrics();
        assert_eq!(
            sqlite
                .load_tool_output(sqlite_effect.effect_id.as_str())
                .expect("warm output should load"),
            Some(sqlite_output.clone())
        );
        let warm_metrics = sqlite.test_metrics();
        assert_eq!(warm_metrics.full_audits, 0);
        assert_eq!(warm_metrics.historical_tool_outputs, 0);
        assert_eq!(
            sqlite
                .load_verification_snapshot("step-1")
                .expect("verification snapshot should load")
                .expect("verification snapshot should exist")
                .tool_output,
            Some(sqlite_output.clone())
        );
        assert_secret_absent(
            SECRET_SENTINEL,
            [
                String::from_utf8(memory.export_jsonl().expect("journal should export"))
                    .expect("journal should be UTF-8"),
                serde_json::to_string(&memory_commit.state).expect("state should serialize"),
            ],
        );

        drop(sqlite);
        let reopened = SqliteRunStore::open(&path).expect("SQLite should cold-open");
        let cold_metrics = reopened.test_metrics();
        assert_eq!(cold_metrics.full_audits, 1);
        assert_eq!(cold_metrics.historical_tool_outputs, 1);
        assert_eq!(
            reopened
                .load_tool_output(sqlite_effect.effect_id.as_str())
                .expect("cold output should verify and load"),
            Some(sqlite_output)
        );
        assert_eq!(reopened.test_metrics(), cold_metrics);
        reopened
            .load()
            .expect("cold full audit should pass")
            .expect("Run should remain");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One vertical snapshot/restart contract with shared fixtures.
    fn planning_snapshot_exposes_only_receipt_completed_outputs_from_the_expected_head() {
        const OUTPUT_SENTINEL: &str = "planning-output-visible-only-in-local-context";

        fn append_output<S: RunStore>(store: &mut S) -> (Commit, EffectIntent, ToolOutputRecord) {
            let (started, effect) = seed_read_only_executing(store);
            let (candidate, output) = tool_output_success(
                &started.state,
                &effect,
                "planning-output-succeeded",
                serde_json::json!({"content": OUTPUT_SENTINEL}),
            );
            let validating = store
                .append_with_tool_output(
                    ExpectedHead::from_state(&started.state),
                    candidate,
                    output.clone(),
                )
                .expect("tool output should commit");
            (validating, effect, output)
        }

        fn complete_output<S: RunStore>(
            store: &mut S,
            validating: &Commit,
            effect: &EffectIntent,
            output: &ToolOutputRecord,
        ) -> Commit {
            let mut receipt = successful_read_only_receipt(effect);
            receipt.output_digest = output.output_digest().to_owned();
            seal_receipt(&mut receipt);
            store
                .append_with_execution_receipt(
                    ExpectedHead::from_state(&validating.state),
                    receipt_event(effect, &receipt),
                    receipt,
                )
                .expect("passed Receipt should complete the Step")
        }

        let directory = tempdir().expect("temporary directory should exist");
        let path = directory.path().join("planning-snapshot.db");
        let mut memory = MemoryRunStore::new();
        let mut sqlite = SqliteRunStore::open(&path).expect("SQLite should open");
        let (memory_validating, memory_effect, memory_output) = append_output(&mut memory);
        let (sqlite_validating, sqlite_effect, sqlite_output) = append_output(&mut sqlite);

        for snapshot in [
            memory
                .load_planning_snapshot(
                    ExpectedHead::from_state(&memory_validating.state),
                    u64::MAX,
                )
                .expect("memory planning snapshot should load")
                .expect("memory Run should exist"),
            sqlite
                .load_planning_snapshot(
                    ExpectedHead::from_state(&sqlite_validating.state),
                    u64::MAX,
                )
                .expect("SQLite planning snapshot should load")
                .expect("SQLite Run should exist"),
        ] {
            assert!(snapshot.completed_tool_outputs().is_empty());
        }

        let memory_completed = complete_output(
            &mut memory,
            &memory_validating,
            &memory_effect,
            &memory_output,
        );
        let sqlite_completed = complete_output(
            &mut sqlite,
            &sqlite_validating,
            &sqlite_effect,
            &sqlite_output,
        );
        let expected = ExpectedHead::from_state(&memory_completed.state);
        let memory_snapshot = memory
            .load_planning_snapshot(expected.clone(), u64::MAX)
            .expect("memory planning snapshot should load")
            .expect("memory Run should exist");
        let sqlite_snapshot = sqlite
            .load_planning_snapshot(ExpectedHead::from_state(&sqlite_completed.state), u64::MAX)
            .expect("SQLite planning snapshot should load")
            .expect("SQLite Run should exist");
        assert_eq!(memory_snapshot, sqlite_snapshot);
        assert_eq!(
            memory_snapshot.completed_tool_outputs().get("step-1"),
            Some(&memory_output)
        );
        assert!(!format!("{memory_snapshot:?}").contains(OUTPUT_SENTINEL));
        sqlite.reset_test_metrics();
        sqlite
            .load_planning_snapshot(
                ExpectedHead::from_state(&sqlite_completed.state),
                sqlite_output.canonical_size_bytes(),
            )
            .expect("warm planning snapshot should fit the exact raw-output budget")
            .expect("SQLite Run should exist");
        let warm_metrics = sqlite.test_metrics();
        assert_eq!(warm_metrics.full_audits, 0);
        assert_eq!(warm_metrics.historical_tool_outputs, 0);
        assert!(matches!(
            sqlite.load_planning_snapshot(
                ExpectedHead::from_state(&sqlite_completed.state),
                sqlite_output
                    .canonical_size_bytes()
                    .checked_sub(1)
                    .expect("non-empty output should have a positive canonical size"),
            ),
            Err(StoreError::PlanningSnapshotBudgetExceeded)
        ));
        assert!(matches!(
            memory.load_planning_snapshot(ExpectedHead::Empty, u64::MAX),
            Err(StoreError::HeadConflict { .. })
        ));

        drop(sqlite);
        let reopened = SqliteRunStore::open(&path).expect("SQLite should reopen");
        let reopened_snapshot = reopened
            .load_planning_snapshot(ExpectedHead::from_state(&sqlite_completed.state), u64::MAX)
            .expect("reopened planning snapshot should verify")
            .expect("reopened Run should exist");
        assert_eq!(reopened_snapshot, memory_snapshot);
    }

    #[test]
    fn planning_snapshot_fails_closed_when_a_completed_output_sidecar_disappears() {
        let directory = tempdir().expect("temporary directory should exist");
        let path = directory.path().join("missing-output.db");
        let mut sqlite = SqliteRunStore::open(&path).expect("SQLite should open");
        let (started, effect) = seed_read_only_executing(&mut sqlite);
        let (candidate, output) = tool_output_success(
            &started.state,
            &effect,
            "missing-planning-output-succeeded",
            serde_json::json!({"content": "must-not-be-silently-omitted"}),
        );
        let validating = sqlite
            .append_with_tool_output(
                ExpectedHead::from_state(&started.state),
                candidate,
                output.clone(),
            )
            .expect("output should commit");
        let mut receipt = successful_read_only_receipt(&effect);
        receipt.output_digest = output.output_digest().to_owned();
        seal_receipt(&mut receipt);
        let completed = sqlite
            .append_with_execution_receipt(
                ExpectedHead::from_state(&validating.state),
                receipt_event(&effect, &receipt),
                receipt,
            )
            .expect("Receipt should complete the Step");
        sqlite
            .load_planning_snapshot(ExpectedHead::from_state(&completed.state), u64::MAX)
            .expect("planning snapshot should warm the verified cache")
            .expect("Run should exist");
        sqlite.reset_test_metrics();
        let external = rusqlite::Connection::open(&path).expect("external SQLite should open");
        external
            .execute(
                "UPDATE tool_outputs SET output_digest = 'sha256:corrupted'",
                [],
            )
            .expect("external corruption should commit");
        drop(external);
        assert!(matches!(
            sqlite.load_planning_snapshot(ExpectedHead::from_state(&completed.state), u64::MAX,),
            Err(StoreError::Corrupt(_))
        ));
        assert_eq!(sqlite.test_metrics().full_audits, 1);

        let external = rusqlite::Connection::open(&path).expect("external SQLite should reopen");
        external
            .execute(
                "UPDATE tool_outputs SET output_digest = ?1",
                [output.output_digest()],
            )
            .expect("test repair should commit");
        drop(external);
        sqlite
            .load_planning_snapshot(ExpectedHead::from_state(&completed.state), u64::MAX)
            .expect("repaired planning snapshot should verify")
            .expect("Run should exist");
        sqlite
            .delete_tool_outputs()
            .expect("test corruption should delete output rows");

        assert!(matches!(
            sqlite.load_planning_snapshot(ExpectedHead::from_state(&completed.state), u64::MAX,),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn planning_snapshot_does_not_invent_outputs_for_legacy_receipt_completions() {
        let mut memory = MemoryRunStore::new();
        commit_two_receipt_chain(&mut memory);
        let state = memory
            .load_current()
            .expect("legacy completion should load")
            .expect("Run should exist");
        assert!(
            state
                .steps
                .values()
                .all(|step| step.status == StepStatus::Completed)
        );
        let snapshot = memory
            .load_planning_snapshot(ExpectedHead::from_state(&state), u64::MAX)
            .expect("legacy planning snapshot should load")
            .expect("Run should exist");
        assert!(snapshot.completed_tool_outputs().is_empty());
    }

    #[test]
    fn output_bound_success_requires_the_atomic_sidecar_api() {
        let mut memory = MemoryRunStore::new();
        let (memory_started, memory_effect) = seed_read_only_executing(&mut memory);
        let (candidate, output) = tool_output_success(
            &memory_started.state,
            &memory_effect,
            "plain-output-success",
            serde_json::json!({"content": "must-be-atomic"}),
        );
        assert!(matches!(
            memory.append(
                ExpectedHead::from_state(&memory_started.state),
                candidate.clone()
            ),
            Err(StoreError::ToolOutputRequired)
        ));
        assert_eq!(
            memory
                .load_tool_output(memory_effect.effect_id.as_str())
                .expect("memory output lookup should work"),
            None
        );

        let directory = tempdir().expect("temp directory should exist");
        let mut sqlite =
            SqliteRunStore::open(directory.path().join("run.db")).expect("SQLite should open");
        let (sqlite_started, sqlite_effect) = seed_read_only_executing(&mut sqlite);
        let (candidate, _) = tool_output_success(
            &sqlite_started.state,
            &sqlite_effect,
            "plain-output-success",
            output.output().clone(),
        );
        assert!(matches!(
            sqlite.append(ExpectedHead::from_state(&sqlite_started.state), candidate),
            Err(StoreError::ToolOutputRequired)
        ));
        assert_eq!(sqlite.tool_output_count().expect("output count"), 0);
        assert_eq!(
            sqlite
                .load_current()
                .expect("SQLite state should load")
                .expect("Run should remain"),
            sqlite_started.state
        );
    }

    #[test]
    fn distinct_effects_may_commit_the_same_canonical_output_digest() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("same-output.db");
        let (first_effect, second_effect, expected_digest) = {
            let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
            let planned = seed(&mut store);
            let graph = store
                .append(
                    ExpectedHead::from_state(&planned.state),
                    event(
                        "same-output-second-step",
                        RunEventBody::StepPlanned {
                            step_id: "step-2".to_owned(),
                            objective: "observe the same JSON independently".to_owned(),
                            depends_on: Vec::new(),
                        },
                    ),
                )
                .expect("second Step should plan");
            let first_effect = read_only_receipt_intent(&graph.state);
            let first = append_receipt_effect_to_validating(
                &mut store,
                &graph.state,
                "step-1",
                &first_effect,
                "same-output-first",
            );
            let mut second_effect = second_receipt_intent(&first.state);
            second_effect.effect_class = EffectClass::ReadOnly;
            second_effect.idempotency_key = None;
            let provenance = second_effect
                .receipt_provenance
                .as_mut()
                .expect("second provenance should exist");
            provenance.profile_version = CORE_RECEIPT_PROFILE_V2.to_owned();
            provenance.tool_output_profile = Some(TOOL_OUTPUT_PROFILE_V1.to_owned());
            second_effect
                .authorization
                .binding
                .receipt_provenance_digest = Some(
                receipt_provenance_digest(provenance).expect("provenance should canonicalize"),
            );
            second_effect.authorization.grant_digest =
                authorization_digest(&second_effect.authorization.binding, 1)
                    .expect("authorization should canonicalize");
            append_receipt_effect_to_validating(
                &mut store,
                &first.state,
                "step-2",
                &second_effect,
                "same-output-second",
            );
            let first_output = store
                .load_tool_output(&first_effect.effect_id)
                .expect("first output should load")
                .expect("first output should exist");
            let second_output = store
                .load_tool_output(&second_effect.effect_id)
                .expect("second output should load")
                .expect("second output should exist");
            assert_eq!(first_output.output_digest(), second_output.output_digest());
            assert_ne!(first_output.output_id(), second_output.output_id());
            assert_ne!(first_output.record_digest(), second_output.record_digest());
            assert_eq!(store.tool_output_count().expect("output count"), 2);
            (
                first_effect.effect_id,
                second_effect.effect_id,
                first_output.output_digest().to_owned(),
            )
        };

        let reopened = SqliteRunStore::open(&path).expect("SQLite should cold-open");
        for effect_id in [first_effect, second_effect] {
            assert_eq!(
                reopened
                    .load_tool_output(&effect_id)
                    .expect("cold output should load")
                    .expect("cold output should exist")
                    .output_digest(),
                expected_digest
            );
        }
    }

    #[test]
    fn receipt_output_digest_must_equal_the_durable_tool_output_digest() {
        let mut memory = MemoryRunStore::new();
        let (validating, effect) = seed_read_only_validating(&mut memory);
        let mut receipt = successful_read_only_receipt(&effect);
        receipt.output_digest = format!("sha256:{}", "e".repeat(64));
        seal_receipt(&mut receipt);
        assert!(matches!(
            memory.append_with_execution_receipt(
                ExpectedHead::from_state(&validating.state),
                receipt_event(&effect, &receipt),
                receipt,
            ),
            Err(StoreError::Corrupt(message))
                if message == "Receipt output digest differs from the durable tool output"
        ));
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
    fn receipt_finalization_atomically_releases_the_dependent_frontier() {
        fn complete_parent<S: RunStore>(store: &mut S) -> xgeny_workgraph::WorkFrontier {
            let parent = seed(store);
            let graph = store
                .append(
                    ExpectedHead::from_state(&parent.state),
                    event(
                        "frontier-child-plan",
                        RunEventBody::StepPlanned {
                            step_id: "step-2".to_owned(),
                            objective: "run after verified parent".to_owned(),
                            depends_on: vec!["step-1".to_owned()],
                        },
                    ),
                )
                .expect("dependent Step should plan");
            let effect = receipt_intent(&graph.state);
            let validating = append_receipt_effect_to_validating(
                store,
                &graph.state,
                "step-1",
                &effect,
                "frontier-parent",
            );
            assert!(
                derive_frontier(&validating.state)
                    .expect("validating graph should derive")
                    .actionable
                    .iter()
                    .all(|action| action.step_id != "step-2"),
                "effect success without a Receipt must not release the child"
            );
            let receipt = successful_receipt(&effect);
            let completed = store
                .append_with_execution_receipt(
                    ExpectedHead::from_state(&validating.state),
                    receipt_event_for("frontier-parent-receipt", "step-1", &effect, &receipt),
                    receipt,
                )
                .expect("Receipt and completion should commit atomically");
            derive_frontier(&completed.state).expect("completed graph should derive")
        }

        let mut memory = MemoryRunStore::new();
        let memory_frontier = complete_parent(&mut memory);
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut sqlite = SqliteRunStore::open(&path).expect("SQLite should open");
        let sqlite_frontier = complete_parent(&mut sqlite);

        assert_eq!(memory_frontier, sqlite_frontier);
        assert_eq!(sqlite_frontier.actionable.len(), 1);
        assert_eq!(sqlite_frontier.actionable[0].step_id, "step-2");
        assert_eq!(
            sqlite_frontier.actionable[0].action,
            ContinuationAction::Admit
        );
        drop(sqlite);

        let reopened = SqliteRunStore::open(&path).expect("SQLite should reopen");
        let state = reopened
            .load_current()
            .expect("verified state should load")
            .expect("Run should exist");
        assert_eq!(
            derive_frontier(&state)
                .expect("reopened graph should derive")
                .actionable,
            sqlite_frontier.actionable
        );
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
                        depends_on: Vec::new(),
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
        for (event_id, profile) in [
            ("unsupported-profile-intent", "unsupported-receipt-profile"),
            ("wrong-effect-profile-intent", CORE_RECEIPT_PROFILE_V2),
        ] {
            let mut effect = receipt_intent(&planned.state);
            let provenance = effect
                .receipt_provenance
                .as_mut()
                .expect("Receipt provenance should exist");
            provenance.profile_version = profile.to_owned();
            effect.authorization.binding.receipt_provenance_digest = Some(
                receipt_provenance_digest(provenance).expect("provenance should canonicalize"),
            );
            effect.authorization.grant_digest =
                authorization_digest(&effect.authorization.binding, 1)
                    .expect("authorization should canonicalize");
            let material = material(&planned.state, &effect);

            let result = store.append_with_invocation_material(
                ExpectedHead::from_state(&planned.state),
                event(
                    event_id,
                    RunEventBody::EffectIntentCommitted {
                        step_id: "step-1".to_owned(),
                        intent: Box::new(effect),
                    },
                ),
                material,
            );

            assert!(matches!(result, Err(StoreError::UnsupportedReceiptProfile)));
        }
        assert_eq!(
            store.load().expect("store should load").expect("Run").state,
            planned.state
        );
    }

    #[test]
    fn new_read_only_intent_without_the_tool_output_profile_is_rejected() {
        let mut store = MemoryRunStore::new();
        let planned = seed(&mut store);
        let mut effect = read_only_receipt_intent(&planned.state);
        let provenance = effect
            .receipt_provenance
            .as_mut()
            .expect("Receipt provenance should exist");
        provenance.tool_output_profile = None;
        effect.authorization.binding.receipt_provenance_digest =
            Some(receipt_provenance_digest(provenance).expect("provenance should canonicalize"));
        effect.authorization.grant_digest = authorization_digest(&effect.authorization.binding, 1)
            .expect("authorization should canonicalize");
        let material = material(&planned.state, &effect);

        let result = store.append_with_invocation_material(
            ExpectedHead::from_state(&planned.state),
            event(
                "missing-tool-output-profile-intent",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(effect),
                },
            ),
            material,
        );

        assert!(matches!(result, Err(StoreError::ToolOutputProfileRequired)));
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
        assert_receipt_mutation_rejected(|receipt, _| {
            receipt.artifacts.push(ArtifactRef {
                artifact_id: "artifact-forged-v1".to_owned(),
                name: None,
                media_type: "application/json".to_owned(),
                size: 0,
                digest: format!("sha256:{}", "a".repeat(64)),
                provenance: Some(ArtifactProvenance {
                    run_id: receipt.run_id.clone(),
                    step_id: receipt.step_id.clone(),
                    receipt_id: Some(receipt.receipt_id.clone()),
                }),
                extensions: BTreeMap::new(),
                required_extensions: Vec::new(),
            });
        });
    }

    #[test]
    fn core_receipt_v2_artifact_provenance_is_exact_and_core_owned() {
        let mut store = MemoryRunStore::new();
        let planned = seed(&mut store);
        let mut effect = receipt_intent(&planned.state);
        effect.effect_class = EffectClass::ReadOnly;
        effect.idempotency_key = None;
        effect
            .receipt_provenance
            .as_mut()
            .expect("provenance should exist")
            .profile_version = CORE_RECEIPT_PROFILE_V2.to_owned();
        let mut receipt = successful_receipt(&effect);
        receipt.effect.class = ProtocolEffectClass::ReadOnly;
        receipt.artifacts.push(ArtifactRef {
            artifact_id: "artifact-read-output".to_owned(),
            name: Some("read-output.json".to_owned()),
            media_type: "application/json".to_owned(),
            size: 128,
            digest: format!("sha256:{}", "a".repeat(64)),
            provenance: Some(ArtifactProvenance {
                run_id: receipt.run_id.clone(),
                step_id: receipt.step_id.clone(),
                receipt_id: Some(receipt.receipt_id.clone()),
            }),
            extensions: BTreeMap::new(),
            required_extensions: Vec::new(),
        });
        let provenance = effect
            .receipt_provenance
            .as_ref()
            .expect("provenance should exist");
        verify_core_receipt_artifacts(&receipt, &effect, provenance)
            .expect("exact Core provenance should pass");

        receipt.artifacts[0]
            .provenance
            .as_mut()
            .expect("artifact provenance should exist")
            .receipt_id = Some("receipt-forged".to_owned());
        assert!(matches!(
            verify_core_receipt_artifacts(&receipt, &effect, provenance),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn core_receipt_v2_artifact_rules_fail_closed_in_memory_and_sqlite_reopen() {
        let mutations: [(&str, ReceiptMutation); 7] = [
            ("empty artifact set", |receipt| receipt.artifacts.clear()),
            ("artifact count overflow", |receipt| {
                let template = receipt.artifacts[0].clone();
                receipt.artifacts = (0..=CORE_RECEIPT_MAX_ARTIFACTS_V2)
                    .map(|index| ArtifactRef {
                        artifact_id: format!("artifact-count-{index}"),
                        ..template.clone()
                    })
                    .collect();
            }),
            ("individual artifact overflow", |receipt| {
                receipt.artifacts[0].size = CORE_RECEIPT_MAX_ARTIFACT_SIZE_BYTES_V2 + 1;
            }),
            ("aggregate artifact overflow", |receipt| {
                let template = receipt.artifacts[0].clone();
                receipt.artifacts = (0..5)
                    .map(|index| ArtifactRef {
                        artifact_id: format!("artifact-total-{index}"),
                        size: CORE_RECEIPT_MAX_ARTIFACT_SIZE_BYTES_V2,
                        ..template.clone()
                    })
                    .collect();
            }),
            ("duplicate artifact identity", |receipt| {
                receipt.artifacts.push(receipt.artifacts[0].clone());
            }),
            ("missing artifact provenance", |receipt| {
                receipt.artifacts[0].provenance = None;
            }),
            ("artifact extension injection", |receipt| {
                let extension = "https://example.test/artifact-extension".to_owned();
                receipt.artifacts[0]
                    .extensions
                    .insert(extension.clone(), serde_json::json!(true));
                receipt.artifacts[0].required_extensions.push(extension);
            }),
        ];

        let mut memory = MemoryRunStore::new();
        let (memory_validating, memory_effect) = seed_read_only_validating(&mut memory);
        let directory = tempdir().expect("temporary SQLite directory should exist");
        let database = directory.path().join("run.db");
        let mut sqlite = SqliteRunStore::open(&database).expect("SQLite store should open");
        let (sqlite_validating, sqlite_effect) = seed_read_only_validating(&mut sqlite);

        for (case, mutate) in mutations {
            assert_read_only_receipt_mutation_rejected(
                &mut memory,
                &memory_validating.state,
                &memory_effect,
                case,
                mutate,
            );
            assert_read_only_receipt_mutation_rejected(
                &mut sqlite,
                &sqlite_validating.state,
                &sqlite_effect,
                case,
                mutate,
            );
            drop(sqlite);
            sqlite = SqliteRunStore::open(&database).expect("SQLite store should cold-open");
            assert_eq!(
                sqlite.load_current().expect("reopened state should load"),
                Some(sqlite_validating.state.clone()),
                "{case} rejection must survive cold-open"
            );
            assert!(
                sqlite
                    .load_execution_receipts()
                    .expect("reopened Receipts should load")
                    .is_empty(),
                "{case} rejection must not leave a Receipt sidecar"
            );
        }
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
            let (validating_parent, effect) = seed_validating(&mut store);
            let validating = store
                .append(
                    ExpectedHead::from_state(&validating_parent.state),
                    event(
                        "receipt-fault-child-plan",
                        RunEventBody::StepPlanned {
                            step_id: "step-2".to_owned(),
                            objective: "wait for the parent Receipt".to_owned(),
                            depends_on: vec!["step-1".to_owned()],
                        },
                    ),
                )
                .expect("dependent Step should plan");
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
            assert!(
                derive_frontier(&recovered.state)
                    .expect("recovered frontier should derive")
                    .actionable
                    .iter()
                    .all(|action| action.step_id != "step-2")
            );

            let completed = store
                .append_with_execution_receipt(
                    ExpectedHead::from_state(&validating.state),
                    candidate,
                    receipt,
                )
                .expect("retry should atomically commit");
            assert_eq!(
                derive_frontier(&completed.state)
                    .expect("completed frontier should derive")
                    .actionable[0]
                    .step_id,
                "step-2"
            );
        }
    }

    #[test]
    fn sqlite_rolls_back_every_partial_tool_output_commit() {
        for fault in [
            AppendFault::Event,
            AppendFault::ToolOutput,
            AppendFault::Projection,
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let path = directory.path().join("run.db");
            let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
            let (started, effect) = seed_read_only_executing(&mut store);
            let (candidate, output) = tool_output_success(
                &started.state,
                &effect,
                "faulted-tool-output-success",
                serde_json::json!({"content": "transactional-output"}),
            );

            let result = store.append_tool_output_with_fault(
                ExpectedHead::from_state(&started.state),
                candidate.clone(),
                &output,
                fault,
            );
            assert!(matches!(result, Err(StoreError::InjectedFault(_))));
            let recovered = store
                .load()
                .expect("faulted store should remain valid")
                .expect("Run should remain");
            assert_eq!(recovered.state, started.state);
            assert_eq!(store.tool_output_count().expect("output count"), 0);
            drop(store);

            let mut reopened = SqliteRunStore::open(&path).expect("SQLite should cold-open");
            assert_eq!(
                reopened
                    .load_current()
                    .expect("cold state should load")
                    .expect("Run should remain"),
                started.state
            );
            assert_eq!(reopened.tool_output_count().expect("output count"), 0);
            reopened
                .append_with_tool_output(
                    ExpectedHead::from_state(&started.state),
                    candidate,
                    output,
                )
                .expect("retry after rollback should commit exactly once");
            assert_eq!(reopened.tool_output_count().expect("output count"), 1);
        }
    }

    #[test]
    fn sqlite_process_exit_after_tool_output_insert_rolls_back_event_row_and_projection() {
        const CHILD_MARKER: &str = "XGENY_SQLITE_TOOL_OUTPUT_CRASH_CHILD";
        const DATABASE_PATH: &str = "XGENY_SQLITE_TOOL_OUTPUT_CRASH_DB";

        if std::env::var_os(CHILD_MARKER).is_some() {
            let path = std::env::var_os(DATABASE_PATH).expect("child database path is required");
            let mut store = SqliteRunStore::open(path).expect("child SQLite should open");
            let snapshot = store
                .load()
                .expect("child load should pass")
                .expect("executing Run should exist");
            let effect = snapshot.state.steps["step-1"]
                .intent
                .as_ref()
                .expect("executing Step should retain intent")
                .clone();
            let (candidate, output) = tool_output_success(
                &snapshot.state,
                &effect,
                "crashed-tool-output-success",
                serde_json::json!({"content": "crash-output"}),
            );
            let _never_returns = store.append_tool_output_and_exit_at(
                ExpectedHead::from_state(&snapshot.state),
                candidate,
                &output,
                AppendFault::ToolOutput,
            );
            panic!("child should exit after the tool-output row insert");
        }

        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let (started, effect) = {
            let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
            seed_read_only_executing(&mut store)
        };
        let status = Command::new(std::env::current_exe().expect("test executable should exist"))
            .args([
                "--exact",
                "tests::sqlite_process_exit_after_tool_output_insert_rolls_back_event_row_and_projection",
                "--test-threads=1",
            ])
            .env(CHILD_MARKER, "1")
            .env(DATABASE_PATH, &path)
            .status()
            .expect("tool-output crash child should start");
        assert_eq!(status.code(), Some(86));

        let mut reopened = SqliteRunStore::open(&path).expect("SQLite should recover");
        let recovered = reopened
            .load()
            .expect("recovered store should verify")
            .expect("Run should remain");
        assert_eq!(recovered.state, started.state);
        assert_eq!(reopened.tool_output_count().expect("output count"), 0);
        let (candidate, output) = tool_output_success(
            &recovered.state,
            &effect,
            "crashed-tool-output-success",
            serde_json::json!({"content": "crash-output"}),
        );
        reopened
            .append_with_tool_output(
                ExpectedHead::from_state(&recovered.state),
                candidate,
                output,
            )
            .expect("post-crash retry should commit");
        assert_eq!(reopened.tool_output_count().expect("output count"), 1);
    }

    #[test]
    fn sqlite_cold_audit_rejects_missing_tampered_indexed_and_orphan_tool_outputs() {
        type Mutation = fn(&SqliteRunStore) -> Result<(), StoreError>;
        let mutations: [(&str, Mutation); 4] = [
            ("missing", SqliteRunStore::delete_tool_outputs),
            (
                "tampered document",
                SqliteRunStore::corrupt_tool_output_document,
            ),
            ("tampered index", SqliteRunStore::corrupt_tool_output_index),
            ("orphan", SqliteRunStore::insert_orphan_tool_output),
        ];

        for (label, mutate) in mutations {
            let directory = tempdir().expect("temp directory should exist");
            let path = directory.path().join(format!("{label}.db"));
            let store = {
                let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
                seed_read_only_validating(&mut store);
                assert_eq!(store.tool_output_count().expect("output count"), 1);
                store
            };
            mutate(&store).expect("corruption fixture should apply");
            assert!(
                matches!(store.load(), Err(StoreError::Corrupt(_))),
                "warm full audit must reject {label} output corruption"
            );
            drop(store);
            assert!(
                matches!(SqliteRunStore::open(&path), Err(StoreError::Corrupt(_))),
                "cold open must reject {label} output corruption"
            );
        }
    }

    #[test]
    fn corrupted_tool_output_json_errors_never_disclose_raw_bytes() {
        const SECRET_SENTINEL: &str = "ERROR-OUTPUT-SECRET";

        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("secret-corruption.db");
        let store = {
            let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
            seed_read_only_validating(&mut store);
            store
        };
        store
            .corrupt_tool_output_shape_with_secret(SECRET_SENTINEL)
            .expect("corruption fixture should apply");
        let warm_error = store
            .load()
            .expect_err("warm audit must reject invalid JSON");
        assert!(matches!(warm_error, StoreError::Corrupt(_)));
        assert!(!format!("{warm_error:?} {warm_error}").contains(SECRET_SENTINEL));
        drop(store);

        let cold_error =
            SqliteRunStore::open(&path).expect_err("cold audit must reject invalid JSON");
        assert!(matches!(cold_error, StoreError::Corrupt(_)));
        assert!(!format!("{cold_error:?} {cold_error}").contains(SECRET_SENTINEL));
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
            let validating_parent = seed_validating(&mut store).0;
            store
                .append(
                    ExpectedHead::from_state(&validating_parent.state),
                    event(
                        "receipt-crash-child-plan",
                        RunEventBody::StepPlanned {
                            step_id: "step-2".to_owned(),
                            objective: "wait for the parent Receipt".to_owned(),
                            depends_on: vec!["step-1".to_owned()],
                        },
                    ),
                )
                .expect("dependent Step should plan")
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
        assert!(
            derive_frontier(&recovered.state)
                .expect("crash-recovered frontier should derive")
                .actionable
                .iter()
                .all(|action| action.step_id != "step-2")
        );
        let effect = recovered.state.steps["step-1"]
            .intent
            .as_ref()
            .expect("intent should remain")
            .clone();
        let receipt = successful_receipt(&effect);
        let completed = reopened
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
        assert_eq!(
            derive_frontier(&completed.state)
                .expect("completed frontier should derive")
                .actionable[0]
                .step_id,
            "step-2"
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

        for version in [1_i64, 2_i64, 9_i64] {
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
    fn fresh_sqlite_store_uses_schema_version_eight() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        drop(SqliteRunStore::open(&path).expect("fresh SQLite should open"));
        let connection = rusqlite::Connection::open(&path).expect("SQLite file should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should be readable");
        assert_eq!(version, 8);
    }

    #[test]
    fn sqlite_migrates_schema_seven_to_eight_without_rewriting_tool_output_bytes() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let effect_id = {
            let mut store = SqliteRunStore::open(&path).expect("schema eight store should open");
            let (started, effect) = seed_read_only_executing(&mut store);
            let (event, output) = tool_output_success(
                &started.state,
                &effect,
                "schema-seven-tool-output",
                serde_json::json!({"content": "SCHEMA-SEVEN-OUTPUT-SENTINEL"}),
            );
            store
                .append_with_tool_output(ExpectedHead::from_state(&started.state), event, output)
                .expect("tool output should commit");
            effect.effect_id
        };
        let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
        let expected_rows = sqlite_durable_rows(&connection);
        assert!(!expected_rows.tool_outputs.is_empty());
        connection
            .execute("DROP TABLE completion_outputs", [])
            .expect("schema-seven fixture should not have completion outputs");
        connection
            .pragma_update(None, "user_version", 7_i64)
            .expect("fixture should declare schema seven");
        drop(connection);

        let migrated = SqliteRunStore::open(&path).expect("schema seven should migrate");
        assert!(
            migrated
                .load_tool_output(&effect_id)
                .expect("tool output should load")
                .is_some()
        );
        drop(migrated);
        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 8);
        assert_eq!(sqlite_durable_rows(&connection), expected_rows);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fixture proves byte-preserving legacy completion replay.
    fn schema_seven_legacy_completion_migrates_without_inventing_a_summary() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("legacy-completion.db");
        let candidate_id = {
            let mut store = SqliteRunStore::open(&path).expect("SQLite should open");
            let (reserved, event, output) =
                prepare_completion_output(&mut store, "LEGACY-SUMMARY-MUST-NOT-BE-INVENTED");
            let commit = store
                .append_with_completion_output(
                    ExpectedHead::from_state(&reserved.state),
                    event,
                    output,
                )
                .expect("completion should commit");
            commit
                .state
                .agent_loop
                .unwrap()
                .completion_candidate
                .unwrap()
                .candidate_id
        };

        let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
        let last_sequence: i64 = connection
            .query_row("SELECT MAX(sequence) FROM run_events", [], |row| row.get(0))
            .expect("last sequence should load");
        let previous_row: (i64, Option<String>, Vec<u8>, String) = connection
            .query_row(
                "SELECT sequence, previous_digest, event_json, digest FROM run_events WHERE sequence = ?1",
                [last_sequence - 1],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("previous event should load");
        let previous = EventRecord {
            sequence: u64::try_from(previous_row.0).expect("sequence should fit"),
            previous_digest: previous_row.1,
            event: serde_json::from_slice(&previous_row.2).expect("event should decode"),
            digest: previous_row.3,
        };
        let mut last_event: RunEvent = connection
            .query_row(
                "SELECT event_json FROM run_events WHERE sequence = ?1",
                [last_sequence],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .map(|bytes| serde_json::from_slice(&bytes).expect("last event should decode"))
            .expect("last event should load");
        let RunEventBody::CompletionCandidateRecorded {
            completion_output_record_digest,
            ..
        } = &mut last_event.body
        else {
            panic!("last event should be a completion candidate")
        };
        *completion_output_record_digest = None;
        let legacy_record = EventRecord::next(Some(&previous), last_event)
            .expect("legacy completion record should hash");
        let mut projection: RunState = connection
            .query_row(
                "SELECT state_json FROM run_projection WHERE singleton = 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .map(|bytes| serde_json::from_slice(&bytes).expect("projection should decode"))
            .expect("projection should load");
        projection.journal_head_digest = legacy_record.digest.clone();
        projection
            .agent_loop
            .as_mut()
            .unwrap()
            .completion_candidate
            .as_mut()
            .unwrap()
            .completion_output_record_digest = None;
        connection
            .execute(
                "UPDATE run_events SET digest = ?1, event_json = ?2 WHERE sequence = ?3",
                rusqlite::params![
                    legacy_record.digest,
                    serde_json::to_vec(&legacy_record.event).expect("event should encode"),
                    last_sequence,
                ],
            )
            .expect("legacy event should update");
        connection
            .execute(
                "UPDATE run_projection SET state_json = ?1 WHERE singleton = 1",
                [serde_json::to_vec(&projection).expect("projection should encode")],
            )
            .expect("legacy projection should update");
        connection
            .execute("DELETE FROM completion_outputs", [])
            .expect("completion output should delete");
        connection
            .execute("DROP TABLE completion_outputs", [])
            .expect("schema-seven fixture should drop completion table");
        connection
            .pragma_update(None, "user_version", 7_i64)
            .expect("fixture should declare schema seven");
        drop(connection);

        let migrated = SqliteRunStore::open(&path).expect("legacy completion should migrate");
        let state = migrated
            .load_current()
            .expect("state should load")
            .expect("Run should exist");
        let expected_head = ExpectedHead::from_state(&state);
        assert!(
            migrated
                .load_completion_output(expected_head, &candidate_id)
                .expect("legacy completion lookup should succeed")
                .is_none()
        );
        assert_eq!(
            migrated
                .completion_output_count()
                .expect("completion count should load"),
            0
        );
    }

    #[test]
    fn corrupt_schema_seven_migration_rolls_back_completion_table_and_version() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("corrupt-schema-seven.db");
        {
            let mut store = SqliteRunStore::open(&path).expect("schema eight store should open");
            let seeded = seed(&mut store);
            append_intent(&mut store, &seeded);
        }

        let expected_rows = {
            let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
            connection
                .execute(
                    "UPDATE run_events SET event_id = 'schema-seven-index-tampered' WHERE sequence = 1",
                    [],
                )
                .expect("fixture event index should be corrupted");
            connection
                .execute("DROP TABLE completion_outputs", [])
                .expect("schema-seven fixture should omit completion-output storage");
            connection
                .pragma_update(None, "user_version", 7_i64)
                .expect("fixture should declare schema seven");
            (
                sqlite_event_rows(&connection),
                sqlite_blob_rows(
                    &connection,
                    "SELECT state_json FROM run_projection ORDER BY singleton",
                ),
                sqlite_blob_rows(
                    &connection,
                    "SELECT intent_json FROM effect_intents ORDER BY effect_id",
                ),
                sqlite_authorization_rows(&connection),
                sqlite_blob_rows(
                    &connection,
                    "SELECT record_json FROM invocation_materials ORDER BY effect_id",
                ),
                sqlite_blob_rows(
                    &connection,
                    "SELECT receipt_json FROM execution_receipts ORDER BY event_sequence",
                ),
                sqlite_blob_rows(
                    &connection,
                    "SELECT record_json FROM tool_outputs ORDER BY event_sequence, effect_id",
                ),
                sqlite_blob_rows(
                    &connection,
                    "SELECT record_json FROM planned_invocations ORDER BY event_sequence, step_id",
                ),
            )
        };

        assert!(matches!(
            SqliteRunStore::open(&path),
            Err(StoreError::Corrupt(message))
                if message == "run event ID index differs from the journal event"
        ));

        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 7);
        assert_eq!(sqlite_table_count(&connection, "completion_outputs"), 0);
        let actual_rows = (
            sqlite_event_rows(&connection),
            sqlite_blob_rows(
                &connection,
                "SELECT state_json FROM run_projection ORDER BY singleton",
            ),
            sqlite_blob_rows(
                &connection,
                "SELECT intent_json FROM effect_intents ORDER BY effect_id",
            ),
            sqlite_authorization_rows(&connection),
            sqlite_blob_rows(
                &connection,
                "SELECT record_json FROM invocation_materials ORDER BY effect_id",
            ),
            sqlite_blob_rows(
                &connection,
                "SELECT receipt_json FROM execution_receipts ORDER BY event_sequence",
            ),
            sqlite_blob_rows(
                &connection,
                "SELECT record_json FROM tool_outputs ORDER BY event_sequence, effect_id",
            ),
            sqlite_blob_rows(
                &connection,
                "SELECT record_json FROM planned_invocations ORDER BY event_sequence, step_id",
            ),
        );
        assert_eq!(actual_rows, expected_rows);
    }

    #[test]
    fn sqlite_migrates_schema_six_to_eight_without_rewriting_any_durable_bytes() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        {
            let mut store = SqliteRunStore::open(&path).expect("schema seven store should open");
            let planned = append_plan_bundle(&mut store);
            let retention = InvocationMaterialRetention::ReconstructableReference(
                ReconstructableMaterialReference::new("test-recipes", "recipe-b", "rev-1")
                    .expect("schema-six recipe should validate"),
            );
            let (candidate, material) = planned_effect_bundle(&planned.state, "step-a", retention);
            store
                .append_with_invocation_material(
                    ExpectedHead::from_state(&planned.state),
                    candidate,
                    material,
                )
                .expect("schema-six planned effect should commit");
        }

        let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
        let expected_rows = sqlite_durable_rows(&connection);
        assert!(!expected_rows.intents.is_empty());
        assert!(!expected_rows.materials.is_empty());
        assert!(!expected_rows.plans.is_empty());
        let expected_receipt_exports = {
            let store = SqliteRunStore::open(&path).expect("store should reopen before downgrade");
            store
                .export_execution_receipts_jsonl()
                .expect("Receipts should export")
        };
        connection
            .execute("DROP TABLE tool_outputs", [])
            .expect("schema-six fixture should not have tool-output storage");
        connection
            .execute("DROP TABLE completion_outputs", [])
            .expect("schema-six fixture should not have completion-output storage");
        connection
            .pragma_update(None, "user_version", 6_i64)
            .expect("fixture should declare schema six");
        drop(connection);

        let migrated = SqliteRunStore::open(&path).expect("schema six should migrate");
        assert_eq!(
            migrated
                .export_execution_receipts_jsonl()
                .expect("Receipts should export after migration"),
            expected_receipt_exports
        );
        assert_eq!(migrated.tool_output_count().expect("output count"), 0);
        drop(migrated);

        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 8);
        assert_eq!(sqlite_durable_rows(&connection), expected_rows);
    }

    #[test]
    fn corrupt_schema_six_migration_does_not_publish_schema_seven() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        {
            let mut store = SqliteRunStore::open(&path).expect("schema seven store should open");
            let planned = append_plan_bundle(&mut store);
            let retention = InvocationMaterialRetention::ReconstructableReference(
                ReconstructableMaterialReference::new("test-recipes", "recipe-b", "rev-1")
                    .expect("schema-six recipe should validate"),
            );
            let (candidate, material) = planned_effect_bundle(&planned.state, "step-a", retention);
            store
                .append_with_invocation_material(
                    ExpectedHead::from_state(&planned.state),
                    candidate,
                    material,
                )
                .expect("schema-six planned effect should commit");
        }

        let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
        connection
            .execute("DROP TABLE tool_outputs", [])
            .expect("schema-six fixture should not have tool-output storage");
        connection
            .execute("DROP TABLE completion_outputs", [])
            .expect("schema-six fixture should not have completion-output storage");
        connection
            .execute(
                "UPDATE effect_intents SET action_digest = 'sha256:corrupted-schema-six'",
                [],
            )
            .expect("derived index should be corrupted");
        connection
            .pragma_update(None, "user_version", 6_i64)
            .expect("fixture should declare schema six");
        let expected_events = sqlite_event_rows(&connection);
        let expected_projection = sqlite_blob_rows(
            &connection,
            "SELECT state_json FROM run_projection ORDER BY singleton",
        );
        let expected_plans = sqlite_blob_rows(
            &connection,
            "SELECT record_json FROM planned_invocations ORDER BY event_sequence, step_id",
        );
        assert!(!expected_plans.is_empty());
        drop(connection);

        assert!(matches!(
            SqliteRunStore::open(&path),
            Err(StoreError::Corrupt(_))
        ));

        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        let output_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'tool_outputs'",
                [],
                |row| row.get(0),
            )
            .expect("schema should remain inspectable");
        let action_digest: String = connection
            .query_row(
                "SELECT action_digest FROM effect_intents LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("corrupt fixture should remain intact");
        assert_eq!(version, 6);
        assert_eq!(output_table_count, 0);
        assert_eq!(sqlite_table_count(&connection, "completion_outputs"), 0);
        assert_eq!(action_digest, "sha256:corrupted-schema-six");
        assert_eq!(sqlite_event_rows(&connection), expected_events);
        assert_eq!(
            sqlite_blob_rows(
                &connection,
                "SELECT state_json FROM run_projection ORDER BY singleton"
            ),
            expected_projection
        );
        assert_eq!(
            sqlite_blob_rows(
                &connection,
                "SELECT record_json FROM planned_invocations ORDER BY event_sequence, step_id"
            ),
            expected_plans
        );
    }

    #[test]
    fn sqlite_migrates_schema_five_without_rewriting_existing_durable_data() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        {
            let mut store = SqliteRunStore::open(&path).expect("schema six store should open");
            commit_two_receipt_chain(&mut store);
            assert_eq!(
                store
                    .planned_invocation_count()
                    .expect("planned invocation count"),
                0
            );
        }

        let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
        let expected_events = sqlite_event_rows(&connection);
        let expected_projection = sqlite_blob_rows(
            &connection,
            "SELECT state_json FROM run_projection ORDER BY singleton",
        );
        let expected_intents = sqlite_blob_rows(
            &connection,
            "SELECT intent_json FROM effect_intents ORDER BY effect_id",
        );
        let expected_authorizations = sqlite_authorization_rows(&connection);
        let expected_materials = sqlite_blob_rows(
            &connection,
            "SELECT record_json FROM invocation_materials ORDER BY effect_id",
        );
        let expected_receipts = sqlite_blob_rows(
            &connection,
            "SELECT receipt_json FROM execution_receipts ORDER BY event_sequence",
        );
        connection
            .execute("DROP TABLE planned_invocations", [])
            .expect("schema-five fixture should not have plan input storage");
        connection
            .execute("DROP TABLE tool_outputs", [])
            .expect("schema-five fixture should not have tool-output storage");
        connection
            .execute("DROP TABLE completion_outputs", [])
            .expect("schema-five fixture should not have completion-output storage");
        connection
            .pragma_update(None, "user_version", 5_i64)
            .expect("fixture should declare schema five");
        drop(connection);

        let migrated = SqliteRunStore::open(&path).expect("schema five should migrate");
        assert_eq!(
            migrated
                .planned_invocation_count()
                .expect("planned invocation count"),
            0
        );
        migrated
            .load()
            .expect("migrated store should verify")
            .expect("Run should remain");
        drop(migrated);

        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 8);
        assert_eq!(sqlite_event_rows(&connection), expected_events);
        assert_eq!(
            sqlite_blob_rows(
                &connection,
                "SELECT state_json FROM run_projection ORDER BY singleton"
            ),
            expected_projection
        );
        assert_eq!(
            sqlite_blob_rows(
                &connection,
                "SELECT intent_json FROM effect_intents ORDER BY effect_id"
            ),
            expected_intents
        );
        assert_eq!(
            sqlite_authorization_rows(&connection),
            expected_authorizations
        );
        assert_eq!(
            sqlite_blob_rows(
                &connection,
                "SELECT record_json FROM invocation_materials ORDER BY effect_id"
            ),
            expected_materials
        );
        assert_eq!(
            sqlite_blob_rows(
                &connection,
                "SELECT receipt_json FROM execution_receipts ORDER BY event_sequence"
            ),
            expected_receipts
        );
    }

    #[test]
    fn corrupt_schema_five_migration_rolls_back_schema_and_version() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        {
            let mut store = SqliteRunStore::open(&path).expect("schema six store should open");
            let seeded = seed(&mut store);
            append_intent(&mut store, &seeded);
        }

        let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
        connection
            .execute("DROP TABLE planned_invocations", [])
            .expect("schema-five fixture should not have plan input storage");
        connection
            .execute("DROP TABLE tool_outputs", [])
            .expect("schema-five fixture should not have tool-output storage");
        connection
            .execute("DROP TABLE completion_outputs", [])
            .expect("schema-five fixture should not have completion-output storage");
        connection
            .execute(
                "UPDATE effect_intents SET action_digest = 'sha256:corrupted'",
                [],
            )
            .expect("derived index should be corrupted");
        connection
            .pragma_update(None, "user_version", 5_i64)
            .expect("fixture should declare schema five");
        drop(connection);

        let Err(error) = SqliteRunStore::open(&path) else {
            panic!("corrupt schema five must not migrate");
        };
        assert!(
            matches!(&error, StoreError::Corrupt(_)),
            "unexpected migration error: {error}"
        );

        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        let planned_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'planned_invocations'",
                [],
                |row| row.get(0),
            )
            .expect("schema should remain inspectable");
        let action_digest: String = connection
            .query_row(
                "SELECT action_digest FROM effect_intents LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("corrupt fixture should remain intact");

        assert_eq!(version, 5);
        assert_eq!(planned_table_count, 0);
        assert_eq!(sqlite_table_count(&connection, "completion_outputs"), 0);
        assert_eq!(action_digest, "sha256:corrupted");
    }

    #[test]
    fn sqlite_migrates_schema_three_without_changing_committed_run_bytes() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let (expected_state, expected_jsonl, expected_events) = {
            let mut store = SqliteRunStore::open(&path).expect("schema six store should open");
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
            .execute("DROP TABLE tool_outputs", [])
            .expect("schema-three fixture should not have tool-output storage");
        connection
            .execute("DROP TABLE completion_outputs", [])
            .expect("schema-three fixture should not have completion-output storage");
        connection
            .execute("DROP TABLE planned_invocations", [])
            .expect("schema-three fixture should not have plan input storage");
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
        assert_eq!(version, 8);
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
    fn schema_three_with_receipt_finalization_fails_without_publishing_schema_six() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        {
            let mut store = SqliteRunStore::open(&path).expect("schema six store should open");
            commit_two_receipt_chain(&mut store);
        }
        let expected_events = {
            let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
            connection
                .execute("DROP TABLE execution_receipts", [])
                .expect("hostile schema-three fixture should omit Receipt storage");
            connection
                .execute("DROP TABLE tool_outputs", [])
                .expect("schema-three fixture should omit tool-output storage");
            connection
                .execute("DROP TABLE completion_outputs", [])
                .expect("schema-three fixture should omit completion-output storage");
            connection
                .execute("DROP TABLE planned_invocations", [])
                .expect("schema-three fixture should omit plan input storage");
            connection
                .pragma_update(None, "user_version", 3_i64)
                .expect("fixture should declare schema three");
            sqlite_event_rows(&connection)
        };

        assert!(matches!(
            SqliteRunStore::open(&path),
            Err(StoreError::Corrupt(message))
                if message.contains("execution receipt count differs from finalization events")
        ));

        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 3);
        let receipt_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'execution_receipts'",
                [],
                |row| row.get(0),
            )
            .expect("schema should be inspectable");
        assert_eq!(receipt_table_count, 0);
        assert_eq!(sqlite_table_count(&connection, "completion_outputs"), 0);
        assert_eq!(sqlite_event_rows(&connection), expected_events);
    }

    #[test]
    fn sqlite_migrates_schema_four_by_auditing_without_rewriting_run_data() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let (journal, receipts, expected_rows) = {
            let mut store = SqliteRunStore::open(&path).expect("schema six store should open");
            commit_two_receipt_chain(&mut store);
            let journal = store.export_jsonl().expect("journal should export");
            let receipts = store
                .export_execution_receipts_jsonl()
                .expect("Receipts should export");
            drop(store);
            let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
            let expected_rows = sqlite_durable_rows(&connection);
            connection
                .execute("DROP TABLE tool_outputs", [])
                .expect("schema-four fixture should not have tool-output storage");
            connection
                .execute("DROP TABLE completion_outputs", [])
                .expect("schema-four fixture should not have completion-output storage");
            connection
                .execute("DROP TABLE planned_invocations", [])
                .expect("schema-four fixture should not have plan input storage");
            connection
                .pragma_update(None, "user_version", 4_i64)
                .expect("fixture should declare schema four");
            (journal, receipts, expected_rows)
        };

        let migrated = SqliteRunStore::open(&path).expect("schema four should migrate");
        assert_eq!(migrated.export_jsonl().expect("journal"), journal);
        assert_eq!(
            migrated
                .export_execution_receipts_jsonl()
                .expect("Receipts"),
            receipts
        );
        drop(migrated);

        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 8);
        assert_eq!(sqlite_durable_rows(&connection), expected_rows);
    }

    #[test]
    fn stale_schema_three_migration_observation_converges_when_version_is_already_four() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        {
            let mut store = SqliteRunStore::open(&path).expect("schema six store should open");
            commit_two_receipt_chain(&mut store);
        }
        let mut connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("foreign keys should enable");
        connection
            .execute("DROP TABLE tool_outputs", [])
            .expect("schema-four fixture should not have tool-output storage");
        connection
            .execute("DROP TABLE completion_outputs", [])
            .expect("schema-four fixture should not have completion-output storage");
        connection
            .execute("DROP TABLE planned_invocations", [])
            .expect("schema-four fixture should not have plan input storage");
        connection
            .pragma_update(None, "user_version", 4_i64)
            .expect("concurrent legacy migration should publish schema four");

        sqlite::migrate_schema_three(&mut connection)
            .expect("stale schema-three open should converge from schema four");

        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 8);
        drop(connection);
        SqliteRunStore::open(&path).expect("converged store should pass full open verification");
    }

    #[test]
    fn failed_schema_four_audit_keeps_version_and_all_durable_rows_unchanged() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        {
            let mut store = SqliteRunStore::open(&path).expect("schema six store should open");
            commit_two_receipt_chain(&mut store);
        }
        let before = {
            let connection = rusqlite::Connection::open(&path).expect("raw SQLite should open");
            connection
                .execute(
                    "UPDATE run_events SET event_id = 'event-index-tampered' WHERE sequence = 1",
                    [],
                )
                .expect("fixture event index should be corrupted");
            connection
                .execute("DROP TABLE tool_outputs", [])
                .expect("schema-four fixture should not have tool-output storage");
            connection
                .execute("DROP TABLE completion_outputs", [])
                .expect("schema-four fixture should not have completion-output storage");
            connection
                .execute("DROP TABLE planned_invocations", [])
                .expect("schema-four fixture should not have plan input storage");
            connection
                .pragma_update(None, "user_version", 4_i64)
                .expect("fixture should declare schema four");
            (
                sqlite_event_rows(&connection),
                sqlite_blob_rows(
                    &connection,
                    "SELECT state_json FROM run_projection ORDER BY singleton",
                ),
                sqlite_blob_rows(
                    &connection,
                    "SELECT intent_json FROM effect_intents ORDER BY effect_id",
                ),
                sqlite_authorization_rows(&connection),
                sqlite_blob_rows(
                    &connection,
                    "SELECT record_json FROM invocation_materials ORDER BY effect_id",
                ),
                sqlite_blob_rows(
                    &connection,
                    "SELECT receipt_json FROM execution_receipts ORDER BY event_sequence",
                ),
            )
        };

        assert!(matches!(
            SqliteRunStore::open(&path),
            Err(StoreError::Corrupt(message))
                if message == "run event ID index differs from the journal event"
        ));

        let connection = rusqlite::Connection::open(&path).expect("SQLite should reopen");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version should load");
        assert_eq!(version, 4);
        assert_eq!(sqlite_table_count(&connection, "completion_outputs"), 0);
        let after = (
            sqlite_event_rows(&connection),
            sqlite_blob_rows(
                &connection,
                "SELECT state_json FROM run_projection ORDER BY singleton",
            ),
            sqlite_blob_rows(
                &connection,
                "SELECT intent_json FROM effect_intents ORDER BY effect_id",
            ),
            sqlite_authorization_rows(&connection),
            sqlite_blob_rows(
                &connection,
                "SELECT record_json FROM invocation_materials ORDER BY effect_id",
            ),
            sqlite_blob_rows(
                &connection,
                "SELECT receipt_json FROM execution_receipts ORDER BY event_sequence",
            ),
        );
        assert_eq!(after, before);
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

        assert!(
            matches!(result, Err(StoreError::HeadConflict { .. })),
            "stale writer returned {result:?}"
        );
        assert_eq!(
            stale
                .load()
                .expect("second writer should read committed state")
                .expect("run should exist")
                .records
                .len(),
            3
        );
        let refreshed = stale
            .load_current()
            .expect("verified current state should load")
            .expect("run should exist");
        let retried = stale
            .append(
                ExpectedHead::from_state(&refreshed),
                event(
                    "event-after-refresh",
                    RunEventBody::StepPlanned {
                        step_id: "step-after-refresh".to_owned(),
                        objective: "continue after observing the winning writer".to_owned(),
                        depends_on: Vec::new(),
                    },
                ),
            )
            .expect("refreshed writer should append without reopening");
        assert_eq!(retried.state.journal_sequence, 4);
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

    #[test]
    fn sqlite_cold_audit_indexes_each_event_once_and_warm_append_skips_historical_rows() {
        // In-memory SQLite keeps this structural test independent of filesystem fsync latency.
        let mut store = SqliteRunStore::open(":memory:").expect("sqlite should open");
        let mut head = seed(&mut store);
        store.reset_test_metrics();
        for index in 0..1_000_u16 {
            let event_id = format!("scale-event-{index}");
            let step_id = format!("scale-step-{index}");
            head = store
                .append(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &event_id,
                        RunEventBody::StepPlanned {
                            step_id,
                            objective: "exercise warm append".to_owned(),
                            depends_on: Vec::new(),
                        },
                    ),
                )
                .expect("warm append should commit");
        }
        let metrics = store.test_metrics();
        assert_eq!(metrics.full_audits, 0);
        assert_eq!(metrics.historical_events, 0);
        assert_eq!(metrics.historical_materials, 0);
        assert_eq!(metrics.historical_receipts, 0);
        assert_eq!(metrics.candidate_events, 1_000);
        let historical_event_count = store.run_event_count().expect("event count");

        store.invalidate_cache();
        store.reset_test_metrics();
        store
            .load_current()
            .expect("cold current state should load")
            .expect("Run should exist");
        let cold = store.test_metrics();
        assert_eq!(cold.full_audits, 1);
        assert_eq!(cold.historical_events, historical_event_count);
        assert_eq!(cold.historical_materials, 0);
        assert_eq!(cold.historical_receipts, 0);

        store.reset_test_metrics();
        let state = store
            .load_current()
            .expect("current state should load")
            .expect("Run should exist");
        store
            .append(
                ExpectedHead::from_state(&state),
                event(
                    "scale-event-final",
                    RunEventBody::StepPlanned {
                        step_id: "scale-step-final".to_owned(),
                        objective: "prove cached append".to_owned(),
                        depends_on: Vec::new(),
                    },
                ),
            )
            .expect("cached append should commit");
        let warm = store.test_metrics();
        assert_eq!(warm.full_audits, 0);
        assert_eq!(warm.historical_events, 0);
        assert_eq!(warm.historical_materials, 0);
        assert_eq!(warm.historical_receipts, 0);
        assert_eq!(warm.candidate_events, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Keeps the complete Receipt lifecycle workload auditable.
    fn sqlite_receipt_heavy_run_uses_one_lookup_per_binding_and_no_warm_history_scan() {
        let mut store = SqliteRunStore::open(":memory:").expect("sqlite should open");
        let mut head = store
            .append(
                ExpectedHead::Empty,
                event(
                    "receipt-scale-created",
                    RunEventBody::RunCreated {
                        goal: "exercise a nontrivial Receipt history".to_owned(),
                    },
                ),
            )
            .expect("Run should be created");
        let mut previous_receipt_digest = None;

        for ordinal in 1..=40_u16 {
            let step_id = format!("receipt-scale-step-{ordinal}");
            head = store
                .append(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &format!("receipt-scale-plan-{ordinal}"),
                        RunEventBody::StepPlanned {
                            step_id: step_id.clone(),
                            objective: "exercise Receipt indexing".to_owned(),
                            depends_on: Vec::new(),
                        },
                    ),
                )
                .expect("Step should be planned");
            let effect = numbered_receipt_intent(&head.state, &step_id, ordinal);
            head = store
                .append_with_invocation_material(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &format!("receipt-scale-intent-{ordinal}"),
                        RunEventBody::EffectIntentCommitted {
                            step_id: step_id.clone(),
                            intent: Box::new(effect.clone()),
                        },
                    ),
                    material_for(&head.state, &step_id, &effect),
                )
                .expect("intent and material should commit");

            for retry in 0..5_u8 {
                head = store
                    .append(
                        ExpectedHead::from_state(&head.state),
                        event(
                            &format!("receipt-scale-{ordinal}-{retry}-started"),
                            RunEventBody::EffectExecutionStarted {
                                step_id: step_id.clone(),
                                effect_id: effect.effect_id.clone(),
                            },
                        ),
                    )
                    .expect("effect should start");
                head = store
                    .append(
                        ExpectedHead::from_state(&head.state),
                        event(
                            &format!("receipt-scale-{ordinal}-{retry}-unknown"),
                            RunEventBody::EffectBecameUnknown {
                                step_id: step_id.clone(),
                                effect_id: effect.effect_id.clone(),
                                reason: "simulated_lost_ack".to_owned(),
                            },
                        ),
                    )
                    .expect("effect should become uncertain");
                head = store
                    .append(
                        ExpectedHead::from_state(&head.state),
                        event(
                            &format!("receipt-scale-{ordinal}-{retry}-reconciling"),
                            RunEventBody::ReconciliationStarted {
                                step_id: step_id.clone(),
                                effect_id: effect.effect_id.clone(),
                            },
                        ),
                    )
                    .expect("reconciliation should start");
                head = store
                    .append(
                        ExpectedHead::from_state(&head.state),
                        event(
                            &format!("receipt-scale-{ordinal}-{retry}-retry"),
                            RunEventBody::ReconciliationResolved {
                                step_id: step_id.clone(),
                                effect_id: effect.effect_id.clone(),
                                resolution: ReconciliationResolution::ProvedNotApplied,
                                evidence_digest: format!("sha256:{}", "a".repeat(64)),
                            },
                        ),
                    )
                    .expect("intent should become retryable");
            }

            head = store
                .append(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &format!("receipt-scale-final-start-{ordinal}"),
                        RunEventBody::EffectExecutionStarted {
                            step_id: step_id.clone(),
                            effect_id: effect.effect_id.clone(),
                        },
                    ),
                )
                .expect("final effect attempt should start");
            head = store
                .append(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &format!("receipt-scale-succeeded-{ordinal}"),
                        RunEventBody::EffectSucceeded {
                            step_id: step_id.clone(),
                            effect_id: effect.effect_id.clone(),
                            evidence_digest: format!("sha256:{}", "d".repeat(64)),
                            output_record_digest: None,
                        },
                    ),
                )
                .expect("effect evidence should commit");

            let mut receipt = successful_receipt(&effect);
            receipt.receipt_id = core_receipt_id_v1(&effect.effect_id);
            receipt.step_id.clone_from(&step_id);
            receipt.previous_receipt_digest = previous_receipt_digest;
            seal_receipt(&mut receipt);
            let next_receipt_digest = Some(receipt.receipt_digest.clone());
            if ordinal == 40 {
                store.reset_test_metrics();
            }
            head = store
                .append_with_execution_receipt(
                    ExpectedHead::from_state(&head.state),
                    receipt_event_for(
                        &format!("receipt-scale-verified-{ordinal}"),
                        &step_id,
                        &effect,
                        &receipt,
                    ),
                    receipt,
                )
                .expect("Receipt should commit");
            previous_receipt_digest = next_receipt_digest;
        }

        assert_eq!(head.state.journal_sequence, 1_001);
        assert_eq!(store.execution_receipt_count().expect("Receipt count"), 40);
        let warm = store.test_metrics();
        assert_eq!(warm.full_audits, 0);
        assert_eq!(warm.historical_events, 0);
        assert_eq!(warm.historical_materials, 0);
        assert_eq!(warm.historical_receipts, 0);
        assert_eq!(warm.candidate_events, 1);
        assert_eq!(warm.candidate_materials, 0);
        assert_eq!(warm.candidate_receipts, 1);

        store.invalidate_cache();
        store.reset_test_metrics();
        let audited = store
            .load_current()
            .expect("cold audit should pass")
            .expect("Run should exist");
        assert_eq!(audited, head.state);
        let cold = store.test_metrics();
        assert_eq!(cold.full_audits, 1);
        assert_eq!(cold.historical_events, 1_001);
        assert_eq!(cold.historical_materials, 40);
        assert_eq!(cold.historical_receipts, 40);
        assert_eq!(cold.receipt_anchor_intent_lookups, 40);
        assert_eq!(cold.receipt_anchor_start_lookups, 40);
        assert_eq!(cold.receipt_binding_intent_lookups, 40);
    }

    #[test]
    fn sqlite_ten_thousand_event_run_has_bounded_warm_verification_and_linear_cold_audit() {
        let mut store = SqliteRunStore::open(":memory:").expect("sqlite should open");
        let planned = seed(&mut store);
        let mut head = append_intent(&mut store, &planned);
        store.reset_test_metrics();

        for cycle in 0..2_499_u16 {
            head = store
                .append(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &format!("long-start-{cycle}"),
                        RunEventBody::EffectExecutionStarted {
                            step_id: "step-1".to_owned(),
                            effect_id: "effect-1".to_owned(),
                        },
                    ),
                )
                .expect("effect should start");
            head = store
                .append(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &format!("long-unknown-{cycle}"),
                        RunEventBody::EffectBecameUnknown {
                            step_id: "step-1".to_owned(),
                            effect_id: "effect-1".to_owned(),
                            reason: "simulated_lost_ack".to_owned(),
                        },
                    ),
                )
                .expect("effect should become uncertain");
            head = store
                .append(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &format!("long-reconcile-{cycle}"),
                        RunEventBody::ReconciliationStarted {
                            step_id: "step-1".to_owned(),
                            effect_id: "effect-1".to_owned(),
                        },
                    ),
                )
                .expect("reconciliation should start");
            head = store
                .append(
                    ExpectedHead::from_state(&head.state),
                    event(
                        &format!("long-retry-{cycle}"),
                        RunEventBody::ReconciliationResolved {
                            step_id: "step-1".to_owned(),
                            effect_id: "effect-1".to_owned(),
                            resolution: ReconciliationResolution::ProvedNotApplied,
                            evidence_digest: format!("sha256:{}", "a".repeat(64)),
                        },
                    ),
                )
                .expect("effect should return to its committed intent");
        }
        head = store
            .append(
                ExpectedHead::from_state(&head.state),
                event(
                    "long-final-start",
                    RunEventBody::EffectExecutionStarted {
                        step_id: "step-1".to_owned(),
                        effect_id: "effect-1".to_owned(),
                    },
                ),
            )
            .expect("ten-thousandth event should commit");

        assert_eq!(head.state.journal_sequence, 10_000);
        let warm = store.test_metrics();
        assert_eq!(warm.full_audits, 0);
        assert_eq!(warm.historical_events, 0);
        assert_eq!(warm.historical_materials, 0);
        assert_eq!(warm.historical_receipts, 0);
        assert_eq!(warm.candidate_events, 9_997);

        store.invalidate_cache();
        store.reset_test_metrics();
        let audited = store
            .load_current()
            .expect("cold audit should pass")
            .expect("Run should exist");
        assert_eq!(audited, head.state);
        let cold = store.test_metrics();
        assert_eq!(cold.full_audits, 1);
        assert_eq!(cold.historical_events, 10_000);
        assert_eq!(cold.historical_materials, 1);
        assert_eq!(cold.historical_receipts, 0);
    }

    #[test]
    fn sqlite_external_projection_mutation_invalidates_the_verified_cache() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut store = SqliteRunStore::open(&path).expect("sqlite should open");
        seed(&mut store);
        store.reset_test_metrics();

        let outsider = rusqlite::Connection::open(&path).expect("outside connection should open");
        outsider
            .execute(
                "UPDATE run_projection SET state_json = ?1 WHERE singleton = 1",
                [br#"{"tampered":true}"#.as_slice()],
            )
            .expect("outside mutation should commit");
        drop(outsider);

        assert!(store.load_current().is_err());
        assert_eq!(store.test_metrics().full_audits, 1);
    }

    #[test]
    fn sqlite_external_material_mutation_is_detected_before_point_read() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut store = SqliteRunStore::open(&path).expect("sqlite should open");
        let seeded = seed(&mut store);
        append_intent(&mut store, &seeded);
        store.reset_test_metrics();

        let outsider = rusqlite::Connection::open(&path).expect("outside connection should open");
        outsider
            .execute(
                "UPDATE invocation_materials SET material_digest = 'sha256:externally-corrupted'",
                [],
            )
            .expect("outside mutation should commit");
        drop(outsider);

        assert!(store.load_invocation_material("effect-1").is_err());
        assert_eq!(store.test_metrics().full_audits, 1);
    }

    #[test]
    fn sqlite_external_receipt_mutation_invalidates_the_verified_cache() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut store = SqliteRunStore::open(&path).expect("sqlite should open");
        commit_two_receipt_chain(&mut store);
        let expected = ExpectedHead::from_state(
            &store
                .load_current()
                .expect("current state should load")
                .expect("Run should exist"),
        );
        store
            .load_planning_snapshot(expected.clone(), u64::MAX)
            .expect("planning snapshot should warm the verified cache")
            .expect("Run should exist");
        store.reset_test_metrics();

        let outsider = rusqlite::Connection::open(&path).expect("outside connection should open");
        let receipt_json: Vec<u8> = outsider
            .query_row(
                "SELECT receipt_json FROM execution_receipts ORDER BY event_sequence LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("Receipt should load");
        let mut document: serde_json::Value =
            serde_json::from_slice(&receipt_json).expect("Receipt should decode");
        document["outputDigest"] = serde_json::Value::String(format!("sha256:{}", "9".repeat(64)));
        outsider
            .execute(
                "UPDATE execution_receipts SET receipt_json = ?1 WHERE event_sequence = (SELECT MIN(event_sequence) FROM execution_receipts)",
                [serde_json::to_vec(&document).expect("Receipt should encode")],
            )
            .expect("outside mutation should commit");
        drop(outsider);

        assert!(matches!(
            store.load_planning_snapshot(expected, u64::MAX),
            Err(StoreError::Corrupt(_))
        ));
        assert_eq!(store.test_metrics().full_audits, 1);
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
