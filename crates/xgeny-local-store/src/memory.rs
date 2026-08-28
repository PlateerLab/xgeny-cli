use xgeny_workgraph::{EventRecord, RunEvent, RunState};

use crate::{
    Commit, ExpectedHead, RunSnapshot, RunStore, StoreError, prepare_commit, verified_snapshot,
};

#[derive(Debug, Default)]
pub struct MemoryRunStore {
    records: Vec<EventRecord>,
    projection: Option<RunState>,
}

impl MemoryRunStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RunStore for MemoryRunStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        let commit = prepare_commit(&self.records, self.projection.as_ref(), expected, event)?;
        self.records.push(commit.record.clone());
        self.projection = Some(commit.state.clone());
        Ok(commit)
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        verified_snapshot(self.records.clone(), self.projection.clone())
    }
}
