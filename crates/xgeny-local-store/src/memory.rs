use std::collections::BTreeMap;

use xgeny_workgraph::{EventRecord, InvocationMaterialRecord, RunEvent, RunState};

use crate::{
    Commit, ExpectedHead, RunSnapshot, RunStore, StoreError, prepare_commit, verified_snapshot,
    verify_material_bundle, verify_material_records,
};

#[derive(Debug, Default)]
pub struct MemoryRunStore {
    records: Vec<EventRecord>,
    projection: Option<RunState>,
    materials: BTreeMap<String, InvocationMaterialRecord>,
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

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        let snapshot = verified_snapshot(self.records.clone(), self.projection.clone())?;
        if let Some(snapshot) = &snapshot {
            verify_material_records(&snapshot.records, &self.materials)?;
        } else if !self.materials.is_empty() {
            return Err(StoreError::Corrupt(
                "invocation material exists without a Run".to_owned(),
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
}
