use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use xgeny_workgraph::{EffectIntent, EventRecord, RunEvent, RunEventBody, RunState};

use crate::{
    Commit, ExpectedHead, RunSnapshot, RunStore, StoreError, prepare_commit, verified_snapshot,
};

const STORE_SCHEMA_VERSION: i64 = 1;

const CREATE_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS run_events (
    sequence INTEGER PRIMARY KEY CHECK (sequence >= 1),
    event_id TEXT NOT NULL UNIQUE,
    previous_digest TEXT,
    digest TEXT NOT NULL UNIQUE,
    event_json BLOB NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS run_projection (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    state_json BLOB NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS effect_intents (
    effect_id TEXT PRIMARY KEY,
    event_sequence INTEGER NOT NULL UNIQUE,
    step_id TEXT NOT NULL,
    action_digest TEXT NOT NULL,
    intent_json BLOB NOT NULL,
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence)
) STRICT;

CREATE TABLE IF NOT EXISTS authorization_consumption (
    grant_id TEXT NOT NULL,
    effect_id TEXT NOT NULL UNIQUE,
    action_digest TEXT NOT NULL,
    grant_digest TEXT NOT NULL,
    max_uses INTEGER NOT NULL CHECK (max_uses >= 1),
    PRIMARY KEY (grant_id, effect_id),
    FOREIGN KEY (effect_id) REFERENCES effect_intents(effect_id)
) STRICT;
";

#[derive(Debug)]
pub struct SqliteRunStore {
    connection: Connection,
}

impl SqliteRunStore {
    /// Open or initialize one embedded SQLite database for a run.
    ///
    /// The `bundled` rusqlite feature links SQLite into the Rust build; no database server,
    /// daemon, or separately installed SQLite executable is required.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened, configured, migrated, or verified.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                connection.execute_batch(CREATE_SCHEMA)?;
                connection.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            }
            STORE_SCHEMA_VERSION => {}
            unsupported => return Err(StoreError::UnsupportedSchemaVersion(unsupported)),
        }

        let store = Self { connection };
        store.load()?;
        Ok(store)
    }

    fn append_internal(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        mut checkpoint: impl FnMut(CommitStage) -> Result<(), StoreError>,
    ) -> Result<Commit, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let records = load_records(&transaction)?;
        let persisted = load_projection(&transaction)?;
        let snapshot = verified_snapshot(records, persisted)?;
        let current_records = snapshot
            .as_ref()
            .map_or_else(Vec::new, |snapshot| snapshot.records.clone());
        let current_state = snapshot.as_ref().map(|snapshot| &snapshot.state);
        let commit = prepare_commit(&current_records, current_state, expected, event)?;

        insert_event(&transaction, &commit.record)?;
        checkpoint(CommitStage::Event)?;
        insert_intent_indexes(&transaction, &commit.record)?;
        checkpoint(CommitStage::IntentIndexes)?;
        write_projection(&transaction, &commit.state)?;
        checkpoint(CommitStage::Projection)?;
        transaction.commit()?;
        Ok(commit)
    }

    #[cfg(test)]
    pub(crate) fn append_with_fault(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, |stage| {
            if stage == fault {
                Err(StoreError::InjectedFault(stage.label()))
            } else {
                Ok(())
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn append_and_exit_at(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, |stage| {
            if stage == fault {
                std::process::exit(86);
            }
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn effect_intent_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "effect_intents")
    }

    #[cfg(test)]
    pub(crate) fn authorization_consumption_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "authorization_consumption")
    }

    #[cfg(test)]
    pub(crate) fn corrupt_effect_index(&self) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE effect_intents SET action_digest = 'sha256:corrupted'",
            [],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn durability_settings(&self) -> Result<(String, i64, i64), StoreError> {
        let journal_mode = self
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous = self
            .connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))?;
        let foreign_keys = self
            .connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        Ok((journal_mode, synchronous, foreign_keys))
    }

    fn validate_indexes(&self, records: &[EventRecord]) -> Result<(), StoreError> {
        let expected: Vec<_> = records
            .iter()
            .filter_map(|record| {
                let RunEventBody::EffectIntentCommitted { step_id, intent } = &record.event.body
                else {
                    return None;
                };
                Some((record.sequence, step_id, intent))
            })
            .collect();
        let mut statement = self.connection.prepare(
            "SELECT i.event_sequence, i.step_id, i.action_digest, i.intent_json, a.grant_id, a.action_digest, a.grant_digest, a.max_uses FROM effect_intents i JOIN authorization_consumption a ON a.effect_id = i.effect_id ORDER BY i.event_sequence",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        let actual: Vec<_> = rows.collect::<Result<_, _>>()?;
        if actual.len() != expected.len() {
            return Err(StoreError::Corrupt(format!(
                "effect index count differs from events: expected {}, actual {}",
                expected.len(),
                actual.len()
            )));
        }
        for (expected, actual) in expected.into_iter().zip(actual) {
            let (sequence, step_id, intent) = expected;
            let (
                stored_sequence,
                stored_step_id,
                stored_action_digest,
                intent_json,
                stored_grant_id,
                stored_authorization_action_digest,
                stored_grant_digest,
                stored_max_uses,
            ) = actual;
            let stored_sequence =
                u64::try_from(stored_sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
            let stored_max_uses =
                u32::try_from(stored_max_uses).map_err(|_| StoreError::SequenceOutOfRange)?;
            let stored_intent: EffectIntent = serde_json::from_slice(&intent_json)?;
            if stored_sequence != sequence
                || stored_step_id != *step_id
                || stored_action_digest != intent.action_digest
                || stored_intent != *intent
                || stored_grant_id != intent.authorization.grant_id
                || stored_authorization_action_digest != intent.action_digest
                || stored_grant_digest != intent.authorization.grant_digest
                || stored_max_uses != intent.authorization.max_uses
            {
                return Err(StoreError::Corrupt(format!(
                    "effect index at event {sequence} differs from committed intent"
                )));
            }
        }
        Ok(())
    }
}

impl RunStore for SqliteRunStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, |_| Ok(()))
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        let records = load_records(&self.connection)?;
        let projection = load_projection(&self.connection)?;
        let snapshot = verified_snapshot(records, projection)?;
        if let Some(snapshot) = &snapshot {
            self.validate_indexes(&snapshot.records)?;
        }
        Ok(snapshot)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitStage {
    Event,
    IntentIndexes,
    Projection,
}

impl CommitStage {
    #[cfg(test)]
    const fn label(self) -> &'static str {
        match self {
            Self::Event => "event insert",
            Self::IntentIndexes => "intent indexes",
            Self::Projection => "projection write",
        }
    }
}

fn insert_event(transaction: &Transaction<'_>, record: &EventRecord) -> Result<(), StoreError> {
    let sequence = i64::try_from(record.sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
    let event_json = serde_json::to_vec(&record.event)?;
    transaction.execute(
        "INSERT INTO run_events (sequence, event_id, previous_digest, digest, event_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            sequence,
            record.event.event_id,
            record.previous_digest,
            record.digest,
            event_json
        ],
    )?;
    Ok(())
}

fn insert_intent_indexes(
    transaction: &Transaction<'_>,
    record: &EventRecord,
) -> Result<(), StoreError> {
    let RunEventBody::EffectIntentCommitted { step_id, intent } = &record.event.body else {
        return Ok(());
    };
    let sequence = i64::try_from(record.sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
    let max_uses = i64::from(intent.authorization.max_uses);
    let intent_json = serde_json::to_vec(intent)?;
    transaction.execute(
        "INSERT INTO effect_intents (effect_id, event_sequence, step_id, action_digest, intent_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![intent.effect_id, sequence, step_id, intent.action_digest, intent_json],
    )?;
    transaction.execute(
        "INSERT INTO authorization_consumption (grant_id, effect_id, action_digest, grant_digest, max_uses) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            intent.authorization.grant_id,
            intent.effect_id,
            intent.action_digest,
            intent.authorization.grant_digest,
            max_uses
        ],
    )?;
    Ok(())
}

fn write_projection(transaction: &Transaction<'_>, state: &RunState) -> Result<(), StoreError> {
    let state_json = serde_json::to_vec(state)?;
    transaction.execute(
        "INSERT INTO run_projection (singleton, state_json) VALUES (1, ?1) ON CONFLICT(singleton) DO UPDATE SET state_json = excluded.state_json",
        [state_json],
    )?;
    Ok(())
}

fn load_records(connection: &Connection) -> Result<Vec<EventRecord>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT sequence, previous_digest, digest, event_json FROM run_events ORDER BY sequence",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    let mut records = Vec::new();
    for row in rows {
        let (sequence, previous_digest, digest, event_json) = row?;
        let sequence = u64::try_from(sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
        let event = serde_json::from_slice(&event_json)?;
        records.push(EventRecord {
            sequence,
            previous_digest,
            event,
            digest,
        });
    }
    Ok(records)
}

fn load_projection(connection: &Connection) -> Result<Option<RunState>, StoreError> {
    let state_json: Option<Vec<u8>> = connection
        .query_row(
            "SELECT state_json FROM run_projection WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    state_json
        .map(|json| serde_json::from_slice(&json).map_err(StoreError::from))
        .transpose()
}

#[cfg(test)]
fn table_count(connection: &Connection, table: &str) -> Result<u64, StoreError> {
    let query = match table {
        "effect_intents" => "SELECT COUNT(*) FROM effect_intents",
        "authorization_consumption" => "SELECT COUNT(*) FROM authorization_consumption",
        _ => {
            return Err(StoreError::Corrupt(format!(
                "unsupported internal count table `{table}`"
            )));
        }
    };
    let count: i64 = connection.query_row(query, [], |row| row.get(0))?;
    u64::try_from(count).map_err(|_| StoreError::SequenceOutOfRange)
}
