use std::collections::BTreeMap;

use xgeny_domain::ExecutionReceiptBody;
use xgeny_workgraph::{
    EventRecord, InvocationMaterialRecord, PlannedInvocationMaterialRecord, RunEvent, RunState,
    ToolOutputRecord,
};

use crate::{
    AuditMetrics, Commit, CommitAnchors, CommitSidecars, ExpectedHead, RunPlanningSnapshot,
    RunSnapshot, RunStore, RunVerificationSnapshot, StoreError, StoredExecutionReceipt,
    StoredToolOutput, VerifiedRunIndex, audit_snapshot, build_planning_snapshot, prepare_commit,
    verify_material_bundle, verify_material_point, verify_material_records,
    verify_plan_input_bundle, verify_plan_input_point, verify_plan_input_records,
    verify_planned_material_retention, verify_receipt_bundle, verify_receipt_candidate,
    verify_receipt_records, verify_tool_output_bundle, verify_tool_output_candidate,
    verify_tool_output_point, verify_tool_output_records,
};

#[derive(Debug, Default)]
pub struct MemoryRunStore {
    records: Vec<EventRecord>,
    projection: Option<RunState>,
    plan_inputs: BTreeMap<String, PlannedInvocationMaterialRecord>,
    materials: BTreeMap<String, InvocationMaterialRecord>,
    receipts: Vec<StoredExecutionReceipt>,
    outputs: BTreeMap<String, StoredToolOutput>,
    index: VerifiedRunIndex,
}

impl MemoryRunStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RunStore for MemoryRunStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        verify_plan_input_bundle(&event, None)?;
        verify_material_bundle(&event, None)?;
        verify_receipt_bundle(&event, None)?;
        verify_tool_output_bundle(&event, None)?;
        let commit = prepare_commit(&self.index, expected, event)?;
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        self.index
            .apply_committed(&commit, CommitSidecars::default(), CommitAnchors::default());
        Ok(commit)
    }

    fn append_with_plan_inputs(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: Vec<PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        verify_plan_input_bundle(&event, Some(&inputs))?;
        verify_material_bundle(&event, None)?;
        verify_receipt_bundle(&event, None)?;
        verify_tool_output_bundle(&event, None)?;
        let commit = prepare_commit(&self.index, expected, event)?;
        for input in &inputs {
            if self.index.plan_input_step_ids.contains(input.step_id())
                || self.plan_inputs.contains_key(input.step_id())
            {
                return Err(StoreError::Corrupt(
                    "duplicate planned invocation input Step ID".to_owned(),
                ));
            }
        }
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        for input in &inputs {
            self.plan_inputs
                .insert(input.step_id().to_owned(), input.clone());
        }
        self.index.apply_committed(
            &commit,
            CommitSidecars::plan_inputs(&inputs),
            CommitAnchors::default(),
        );
        Ok(commit)
    }

    fn append_with_invocation_material(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        material: InvocationMaterialRecord,
    ) -> Result<Commit, StoreError> {
        verify_plan_input_bundle(&event, None)?;
        verify_material_bundle(&event, Some(&material))?;
        verify_receipt_bundle(&event, None)?;
        verify_tool_output_bundle(&event, None)?;
        let planned_input = match &event.body {
            xgeny_workgraph::RunEventBody::EffectIntentCommitted { step_id, .. } => {
                self.plan_inputs.get(step_id)
            }
            _ => None,
        };
        verify_planned_material_retention(&self.index, &event, &material, planned_input)?;
        let commit = prepare_commit(&self.index, expected, event)?;
        if self
            .index
            .material_effect_ids
            .contains(material.effect_id())
        {
            return Err(StoreError::Corrupt(
                "duplicate invocation material effect ID".to_owned(),
            ));
        }
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        self.materials
            .insert(material.effect_id().to_owned(), material);
        let material = self
            .materials
            .get(match &commit.record.event.body {
                xgeny_workgraph::RunEventBody::EffectIntentCommitted { intent, .. } => {
                    &intent.effect_id
                }
                _ => unreachable!("material bundle requires an effect intent"),
            })
            .expect("committed material must be indexed");
        self.index.apply_committed(
            &commit,
            CommitSidecars::material(material),
            CommitAnchors::default(),
        );
        Ok(commit)
    }

    fn append_with_execution_receipt(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        receipt: ExecutionReceiptBody,
    ) -> Result<Commit, StoreError> {
        verify_plan_input_bundle(&event, None)?;
        verify_material_bundle(&event, None)?;
        verify_receipt_bundle(&event, Some(&receipt))?;
        verify_tool_output_bundle(&event, None)?;
        let commit = prepare_commit(&self.index, expected, event)?;
        let receipt_anchor = verify_receipt_candidate(&self.index, &commit.record, &receipt)?;
        let effect_id = match &commit.record.event.body {
            xgeny_workgraph::RunEventBody::VerificationRecorded { effect_id, .. } => {
                effect_id.clone()
            }
            _ => unreachable!("receipt bundle validation requires a finalization event"),
        };
        let stored = StoredExecutionReceipt {
            event_sequence: commit.record.sequence,
            effect_id,
            receipt,
        };
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        self.receipts.push(stored);
        let receipt = &self
            .receipts
            .last()
            .expect("committed receipt must be retained")
            .receipt;
        self.index.apply_committed(
            &commit,
            CommitSidecars::receipt(receipt),
            CommitAnchors {
                receipt: Some(receipt_anchor),
                ..CommitAnchors::default()
            },
        );
        Ok(commit)
    }

    fn append_with_tool_output(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        output: ToolOutputRecord,
    ) -> Result<Commit, StoreError> {
        verify_plan_input_bundle(&event, None)?;
        verify_material_bundle(&event, None)?;
        verify_receipt_bundle(&event, None)?;
        verify_tool_output_bundle(&event, Some(&output))?;
        let commit = prepare_commit(&self.index, expected, event)?;
        let output_anchor = verify_tool_output_candidate(&self.index, &commit.record, &output)?;
        if self.outputs.contains_key(output.effect_id()) {
            return Err(StoreError::Corrupt(
                "duplicate tool output effect ID".to_owned(),
            ));
        }
        let effect_id = output.effect_id().to_owned();
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        self.outputs.insert(
            effect_id,
            StoredToolOutput {
                event_sequence: commit.record.sequence,
                record: output,
            },
        );
        let output = &self
            .outputs
            .get(match &commit.record.event.body {
                xgeny_workgraph::RunEventBody::EffectSucceeded { effect_id, .. } => effect_id,
                _ => unreachable!("tool-output bundle requires a success event"),
            })
            .expect("committed tool output must be retained")
            .record;
        self.index.apply_committed(
            &commit,
            CommitSidecars::output(output),
            CommitAnchors {
                output: Some(output_anchor),
                ..CommitAnchors::default()
            },
        );
        Ok(commit)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        let mut metrics = AuditMetrics::default();
        let (snapshot, mut audited_index) =
            audit_snapshot(self.records.clone(), self.projection.clone(), &mut metrics)?;
        verify_plan_input_records(&mut audited_index, &self.plan_inputs, &mut metrics)?;
        verify_material_records(&mut audited_index, &self.materials, &mut metrics)?;
        verify_tool_output_records(&mut audited_index, &self.outputs, &mut metrics)?;
        verify_receipt_records(&mut audited_index, &self.receipts, &mut metrics)?;
        if audited_index != self.index {
            return Err(StoreError::Corrupt(
                "in-memory verified index differs from committed data".to_owned(),
            ));
        }
        Ok(snapshot)
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        Ok(self.projection.clone())
    }

    fn load_invocation_material(
        &self,
        effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        let material = self.materials.get(effect_id);
        verify_material_point(&self.index, effect_id, material)?;
        Ok(material.cloned())
    }

    fn load_planned_invocation(
        &self,
        step_id: &str,
    ) -> Result<Option<PlannedInvocationMaterialRecord>, StoreError> {
        let input = self.plan_inputs.get(step_id);
        verify_plan_input_point(&self.index, step_id, input)?;
        Ok(input.cloned())
    }

    fn load_execution_receipts(&self) -> Result<Vec<ExecutionReceiptBody>, StoreError> {
        Ok(self
            .receipts
            .iter()
            .map(|stored| stored.receipt.clone())
            .collect())
    }

    fn load_tool_output(&self, effect_id: &str) -> Result<Option<ToolOutputRecord>, StoreError> {
        let output = self.outputs.get(effect_id);
        verify_tool_output_point(&self.index, effect_id, output)?;
        Ok(output.map(|stored| stored.record.clone()))
    }

    fn load_with_execution_receipts(
        &self,
    ) -> Result<(Option<RunSnapshot>, Vec<ExecutionReceiptBody>), StoreError> {
        let snapshot = self.load()?;
        let receipts = self
            .receipts
            .iter()
            .map(|stored| stored.receipt.clone())
            .collect();
        Ok((snapshot, receipts))
    }

    fn load_verification_snapshot(
        &self,
        step_id: &str,
    ) -> Result<Option<RunVerificationSnapshot>, StoreError> {
        let Some(mut snapshot) = self.index.verification_snapshot(step_id) else {
            return Ok(None);
        };
        let effect_id = snapshot
            .state
            .steps
            .get(step_id)
            .and_then(|step| step.intent.as_ref())
            .map(|intent| intent.effect_id.as_str());
        snapshot.tool_output = effect_id
            .and_then(|effect_id| self.outputs.get(effect_id))
            .map(|stored| stored.record.clone());
        if let Some(effect_id) = effect_id {
            verify_tool_output_point(&self.index, effect_id, self.outputs.get(effect_id))?;
        }
        Ok(Some(snapshot))
    }

    fn load_planning_snapshot(
        &self,
        expected: ExpectedHead,
        max_output_bytes: u64,
    ) -> Result<Option<RunPlanningSnapshot>, StoreError> {
        build_planning_snapshot(&self.index, expected, max_output_bytes, |effect_id| {
            Ok(self.outputs.get(effect_id).cloned())
        })
    }
}
