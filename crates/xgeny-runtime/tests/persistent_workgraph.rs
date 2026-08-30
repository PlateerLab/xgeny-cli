use std::cell::Cell;

use tempfile::tempdir;
use xgeny_local_store::{
    Commit, ExpectedHead, MemoryRunStore, RunSnapshot, RunStore, SqliteRunStore, StoreError,
};
use xgeny_runtime::WorkGraphCoordinator;
use xgeny_workgraph::{ContinuationAction, FrontierAction, RunEvent, RunEventBody, RunState};

const RUN_ID: &str = "run-persistent-frontier";
const AUTHORITY: &str = "local:test";
const AUTHORITY_EPOCH: u64 = 11;

fn event(event_id: &str, body: RunEventBody) -> RunEvent {
    RunEvent {
        event_id: event_id.to_owned(),
        run_id: RUN_ID.to_owned(),
        authority: AUTHORITY.to_owned(),
        authority_epoch: AUTHORITY_EPOCH,
        recorded_at: "2026-08-30T00:00:00Z".to_owned(),
        body,
    }
}

fn seed_graph<S: RunStore>(store: &mut S) -> RunState {
    let mut state = store
        .append(
            ExpectedHead::Empty,
            event(
                "event-1",
                RunEventBody::RunCreated {
                    goal: "execute a durable diamond graph".to_owned(),
                },
            ),
        )
        .expect("Run should create")
        .state;
    for (event_id, step_id, dependencies) in [
        ("event-2", "a", Vec::new()),
        ("event-3", "b", vec!["a"]),
        ("event-4", "c", vec!["a"]),
        ("event-5", "d", vec!["b", "c"]),
        ("event-6", "independent", Vec::new()),
    ] {
        state = store
            .append(
                ExpectedHead::from_state(&state),
                event(
                    event_id,
                    RunEventBody::StepPlanned {
                        step_id: step_id.to_owned(),
                        objective: format!("complete {step_id}"),
                        depends_on: dependencies.into_iter().map(str::to_owned).collect(),
                    },
                ),
            )
            .expect("topological Step should plan")
            .state;
    }
    state
}

fn expected_initial_actions() -> Vec<FrontierAction> {
    vec![
        FrontierAction {
            step_id: "a".to_owned(),
            action: ContinuationAction::Admit,
        },
        FrontierAction {
            step_id: "independent".to_owned(),
            action: ContinuationAction::Admit,
        },
    ]
}

#[test]
fn memory_and_sqlite_reopen_derive_the_same_frontier() {
    let coordinator = WorkGraphCoordinator::new();
    let mut memory = MemoryRunStore::new();
    let memory_state = seed_graph(&mut memory);
    let memory_frontier = coordinator
        .inspect(&memory)
        .expect("Memory frontier should derive")
        .expect("Run should exist");

    let directory = tempdir().expect("temporary Run directory should exist");
    let path = directory.path().join("run.db");
    let mut sqlite = SqliteRunStore::open(&path).expect("SQLite should open");
    let sqlite_state = seed_graph(&mut sqlite);
    let sqlite_frontier = coordinator
        .inspect(&sqlite)
        .expect("SQLite frontier should derive")
        .expect("Run should exist");

    assert_eq!(memory_state, sqlite_state);
    assert_eq!(memory_frontier, sqlite_frontier);
    assert_eq!(sqlite_frontier.actionable, expected_initial_actions());
    assert_eq!(sqlite_frontier.waiting.len(), 3);
    assert_eq!(sqlite_frontier.journal_sequence, 6);
    drop(sqlite);

    let reopened = SqliteRunStore::open(&path).expect("SQLite should reopen");
    let reopened_frontier = coordinator
        .inspect(&reopened)
        .expect("reopened frontier should derive")
        .expect("Run should exist");
    assert_eq!(reopened_frontier, memory_frontier);
    let snapshot = reopened
        .load()
        .expect("reopened store should audit")
        .expect("Run should exist");
    assert_eq!(snapshot.state.steps["d"].depends_on, ["b", "c"]);
}

struct MinimalViewStore {
    state: RunState,
    full_load_calls: Cell<usize>,
    current_load_calls: Cell<usize>,
}

impl RunStore for MinimalViewStore {
    fn append(&mut self, _expected: ExpectedHead, _event: RunEvent) -> Result<Commit, StoreError> {
        Err(StoreError::InjectedFault("append must not be called"))
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        self.full_load_calls.set(self.full_load_calls.get() + 1);
        Err(StoreError::InjectedFault("full load must not be called"))
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        self.current_load_calls
            .set(self.current_load_calls.get() + 1);
        Ok(Some(self.state.clone()))
    }
}

#[test]
fn coordinator_uses_only_the_generation_verified_current_view() {
    let mut source = MemoryRunStore::new();
    let state = seed_graph(&mut source);
    let store = MinimalViewStore {
        state,
        full_load_calls: Cell::new(0),
        current_load_calls: Cell::new(0),
    };

    let frontier = WorkGraphCoordinator::new()
        .inspect(&store)
        .expect("minimal view should coordinate")
        .expect("Run should exist");

    assert_eq!(frontier.actionable, expected_initial_actions());
    assert_eq!(store.full_load_calls.get(), 0);
    assert_eq!(store.current_load_calls.get(), 1);
}
