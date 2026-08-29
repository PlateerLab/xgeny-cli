#![doc = "Local storage candidates for durable `XGENy` run events and projections."]

mod memory;
mod sqlite;

use serde_json::Value;
use thiserror::Error;
use xgeny_workgraph::{
    EventRecord, RecordError, ReplayError, RunEvent, RunState, TransitionError, apply_record,
    replay,
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

pub trait RunStore {
    /// Compare-and-append one event and its derived projection atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for stale heads, invalid transitions, serialization, or storage faults.
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError>;

    /// Load and replay-verify all committed data.
    ///
    /// # Errors
    ///
    /// Returns an error if storage cannot be read or its projection differs from replay.
    fn load(&self) -> Result<Option<RunSnapshot>, StoreError>;

    /// Export committed records as RFC 8785 canonical JSON Lines in sequence order.
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

fn canonical_jsonl(records: &[EventRecord]) -> Result<Vec<u8>, StoreError> {
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
    Replay(#[from] ReplayError),
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
    #[error("injected append fault after {0}")]
    InjectedFault(&'static str),
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::tempdir;
    use xgeny_workgraph::{
        AuthorizationBinding, AuthorizationUse, EffectClass, EffectIntent, InvocationBinding,
        ReconciliationResolution, RunEvent, RunEventBody, RunState, SinkGuarantee, StepStatus,
        authorization_digest, once_authorization_id,
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
        let invocation = InvocationBinding {
            capability_id: "test.effect".to_owned(),
            contract_version: "1.0.0".to_owned(),
            definition_digest: "sha256:definition-1".to_owned(),
            instance_id: "test.instance".to_owned(),
            instance_binding_digest: "sha256:instance-1".to_owned(),
        };
        let binding = AuthorizationBinding {
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
            policy_evidence_digest: "sha256:policy-1".to_owned(),
        };
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

    fn append_intent<S: RunStore>(store: &mut S, previous: &Commit) -> Commit {
        store
            .append(
                ExpectedHead::from_state(&previous.state),
                event(
                    "event-3",
                    RunEventBody::EffectIntentCommitted {
                        step_id: "step-1".to_owned(),
                        intent: Box::new(intent(&previous.state)),
                    },
                ),
            )
            .expect("effect intent should commit")
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
    fn prior_experimental_store_version_is_rejected_instead_of_silently_misread() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let connection = rusqlite::Connection::open(&path).expect("SQLite file should open");
        connection
            .pragma_update(None, "user_version", 1_i64)
            .expect("old experimental version should be written");
        drop(connection);

        assert!(matches!(
            SqliteRunStore::open(&path),
            Err(StoreError::UnsupportedSchemaVersion(1))
        ));
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
    fn concurrent_sqlite_writer_observes_head_compare_and_swap() {
        let directory = tempdir().expect("temp directory should exist");
        let path = directory.path().join("run.db");
        let mut first = SqliteRunStore::open(&path).expect("first writer should open");
        let seed = seed(&mut first);
        let mut stale = SqliteRunStore::open(&path).expect("second writer should open");
        append_intent(&mut first, &seed);

        let result = stale.append(
            ExpectedHead::from_state(&seed.state),
            event(
                "event-stale",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(intent(&seed.state)),
                },
            ),
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

        let result = store.append(
            stale,
            event(
                "event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(intent(&seed.state)),
                },
            ),
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

        let result = store.append(
            ExpectedHead::from_state(&seed.state),
            event(
                "event-1",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(intent(&seed.state)),
                },
            ),
        );

        assert!(matches!(result, Err(StoreError::DuplicateEventId(_))));
    }

    #[test]
    fn sqlite_rolls_back_every_partial_commit_stage() {
        for fault in [
            AppendFault::Event,
            AppendFault::EffectIntentIndex,
            AppendFault::AuthorizationConsumption,
            AppendFault::Projection,
        ] {
            let directory = tempdir().expect("temp directory should exist");
            let mut store =
                SqliteRunStore::open(directory.path().join("run.db")).expect("sqlite should open");
            let seed = seed(&mut store);
            let candidate = event(
                "event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(intent(&seed.state)),
                },
            );

            let result = store.append_with_fault(
                ExpectedHead::from_state(&seed.state),
                candidate.clone(),
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

            store
                .append(ExpectedHead::from_state(&seed.state), candidate)
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
                "projection" => AppendFault::Projection,
                stage => panic!("unknown child fault stage: {stage}"),
            };
            let mut store = SqliteRunStore::open(path).expect("child sqlite should open");
            let snapshot = store
                .load()
                .expect("child load should pass")
                .expect("child seed should exist");
            let candidate = event(
                "event-3",
                RunEventBody::EffectIntentCommitted {
                    step_id: "step-1".to_owned(),
                    intent: Box::new(intent(&snapshot.state)),
                },
            );
            let _never_returns = store.append_and_exit_at(
                ExpectedHead::from_state(&snapshot.state),
                candidate,
                fault,
            );
            panic!("child should have exited at the selected commit stage");
        }

        for (label, fault) in [
            ("event", AppendFault::Event),
            ("intent", AppendFault::EffectIntentIndex),
            ("authorization", AppendFault::AuthorizationConsumption),
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
