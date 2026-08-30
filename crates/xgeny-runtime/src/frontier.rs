use thiserror::Error;
use xgeny_local_store::{RunStore, StoreError};
use xgeny_workgraph::{FrontierError, WorkFrontier, derive_frontier};

/// Read-only entry point for single-orchestrator `WorkGraph` coordination.
///
/// It never executes an effect and never stores a second frontier projection. Every call derives
/// the coordination view from the current generation-verified Run projection.
#[derive(Debug, Default, Clone, Copy)]
pub struct WorkGraphCoordinator;

impl WorkGraphCoordinator {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Load and derive the current frontier through the store's minimal current-state API.
    ///
    /// Built-in stores avoid history materialization on a warm verified generation. A cold open,
    /// generation change, or a third-party store's default `load_current` can perform a full
    /// audit. Execution-authoritative use requires the `RunStore` implementation to validate the
    /// projected Receipt identities against complete Receipt sidecars as documented by that API.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot provide a verified current state or the state does
    /// not form a valid dependency DAG.
    pub fn inspect<S: RunStore>(
        &self,
        store: &S,
    ) -> Result<Option<WorkFrontier>, WorkGraphCoordinationError> {
        store
            .load_current()?
            .as_ref()
            .map(derive_frontier)
            .transpose()
            .map_err(WorkGraphCoordinationError::from)
    }
}

#[derive(Debug, Error)]
pub enum WorkGraphCoordinationError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Frontier(#[from] FrontierError),
}
