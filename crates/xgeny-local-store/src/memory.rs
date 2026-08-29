use std::collections::BTreeMap;

use xgeny_domain::ExecutionReceiptBody;
use xgeny_workgraph::{EventRecord, InvocationMaterialRecord, RunEvent, RunState};

use crate::{
    Commit, ExpectedHead, RunSnapshot, RunStore, StoreError, StoredExecutionReceipt,
    prepare_commit, verified_snapshot, verify_material_bundle, verify_material_records,
    verify_receipt_bundle, verify_receipt_records,
};

#[derive(Debug, Default)]
pub struct MemoryRunStore {
    records: Vec<EventRecord>,
    projection: Option<RunState>,
    materials: BTreeMap<String, InvocationMaterialRecord>,
    receipts: Vec<StoredExecutionReceipt>,
}

impl MemoryRunStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RunStore for MemoryRunStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        verify_material_bundle(&event, None)?;
        verify_receipt_bundle(&event, None)?;
        let commit = prepare_commit(&self.records, self.projection.as_ref(), expected, event)?;
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        Ok(commit)
    }

    fn append_with_invocation_material(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        material: InvocationMaterialRecord,
    ) -> Result<Commit, StoreError> {
        verify_material_bundle(&event, Some(&material))?;
        verify_receipt_bundle(&event, None)?;
        let commit = prepare_commit(&self.records, self.projection.as_ref(), expected, event)?;
        if self.materials.contains_key(material.effect_id()) {
            return Err(StoreError::Corrupt(
                "duplicate invocation material effect ID".to_owned(),
            ));
        }
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        self.materials
            .insert(material.effect_id().to_owned(), material);
        Ok(commit)
    }

    fn append_with_execution_receipt(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        receipt: ExecutionReceiptBody,
    ) -> Result<Commit, StoreError> {
        verify_material_bundle(&event, None)?;
        verify_receipt_bundle(&event, Some(&receipt))?;
        let commit = prepare_commit(&self.records, self.projection.as_ref(), expected, event)?;
        let effect_id = match &commit.record.event.body {
            xgeny_workgraph::RunEventBody::VerificationRecorded { effect_id, .. } => {
                effect_id.clone()
            }
            _ => unreachable!("receipt bundle validation requires a finalization event"),
        };
        if self.receipts.iter().any(|stored| {
            stored.receipt.receipt_id == receipt.receipt_id
                || stored.receipt.receipt_digest == receipt.receipt_digest
                || stored.effect_id == effect_id
        }) {
            return Err(StoreError::Corrupt(
                "duplicate execution receipt identity".to_owned(),
            ));
        }
        let stored = StoredExecutionReceipt {
            event_sequence: commit.record.sequence,
            effect_id,
            receipt,
        };
        let mut candidate_receipts = self.receipts.clone();
        candidate_receipts.push(stored.clone());
        let mut candidate_records = self.records.clone();
        candidate_records.push(commit.record.clone());
        verify_receipt_records(&candidate_records, &candidate_receipts)?;
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        self.receipts.push(stored);
        Ok(commit)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        let snapshot = verified_snapshot(self.records.clone(), self.projection.clone())?;
        if let Some(snapshot) = &snapshot {
            verify_material_records(&snapshot.records, &self.materials)?;
            verify_receipt_records(&snapshot.records, &self.receipts)?;
        } else if !self.materials.is_empty() {
            return Err(StoreError::Corrupt(
                "invocation material exists without a Run".to_owned(),
            ));
        } else if !self.receipts.is_empty() {
            return Err(StoreError::Corrupt(
                "execution receipt exists without a Run".to_owned(),
            ));
        }
        Ok(snapshot)
    }

    fn load_invocation_material(
        &self,
        effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        self.load()?;
        Ok(self.materials.get(effect_id).cloned())
    }

    fn load_execution_receipts(&self) -> Result<Vec<ExecutionReceiptBody>, StoreError> {
        self.load()?;
        Ok(self
            .receipts
            .iter()
            .map(|stored| stored.receipt.clone())
            .collect())
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
}
