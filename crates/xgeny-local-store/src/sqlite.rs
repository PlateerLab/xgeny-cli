use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use xgeny_workgraph::{
    EffectIntent, EventRecord, InvocationMaterialRecord, RunEvent, RunEventBody, RunState,
};

use crate::{
    Commit, ExpectedHead, RunSnapshot, RunStore, StoreError, prepare_commit, verified_snapshot,
    verify_material_bundle, verify_material_records,
};

const STORE_SCHEMA_VERSION: i64 = 3;

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

CREATE TABLE IF NOT EXISTS invocation_materials (
    effect_id TEXT PRIMARY KEY,
    material_id TEXT NOT NULL UNIQUE,
    material_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL,
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

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                configure_connection(&connection)?;
                connection.execute_batch(CREATE_SCHEMA)?;
                connection.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            }
            STORE_SCHEMA_VERSION => configure_connection(&connection)?,
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
        material: Option<&InvocationMaterialRecord>,
        mut checkpoint: impl FnMut(CommitStage) -> Result<(), StoreError>,
    ) -> Result<Commit, StoreError> {
        verify_material_bundle(&event, material)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let records = load_records(&transaction)?;
        let persisted = load_projection(&transaction)?;
        let snapshot = verified_snapshot(records, persisted)?;
        let current_records = snapshot
            .as_ref()
            .map_or_else(Vec::new, |snapshot| snapshot.records.clone());
        validate_derived_state(&transaction, &current_records)?;
        let current_state = snapshot.as_ref().map(|snapshot| &snapshot.state);
        let commit = prepare_commit(&current_records, current_state, expected, event)?;

        insert_event(&transaction, &commit.record)?;
        checkpoint(CommitStage::Event)?;
        insert_effect_intent_index(&transaction, &commit.record)?;
        checkpoint(CommitStage::EffectIntentIndex)?;
        insert_authorization_consumption(&transaction, &commit.record)?;
        checkpoint(CommitStage::AuthorizationConsumption)?;
        insert_invocation_material(&transaction, material)?;
        checkpoint(CommitStage::InvocationMaterial)?;
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
        material: &InvocationMaterialRecord,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, Some(material), |stage| {
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
        material: &InvocationMaterialRecord,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, Some(material), |stage| {
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
    pub(crate) fn run_event_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "run_events")
    }

    #[cfg(test)]
    pub(crate) fn authorization_consumption_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "authorization_consumption")
    }

    #[cfg(test)]
    pub(crate) fn invocation_material_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "invocation_materials")
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
    pub(crate) fn corrupt_invocation_material_index(&self) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE invocation_materials SET material_digest = 'sha256:corrupted'",
            [],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn delete_invocation_material(&self) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM invocation_materials", [])?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_orphan_invocation_material(&self) -> Result<(), StoreError> {
        self.connection.pragma_update(None, "foreign_keys", false)?;
        let result = self.connection.execute(
            "INSERT INTO invocation_materials (effect_id, material_id, material_digest, record_digest, record_json) SELECT 'orphan-effect', material_id || '-orphan', material_digest, record_digest || '-orphan', record_json FROM invocation_materials LIMIT 1",
            [],
        );
        self.connection.pragma_update(None, "foreign_keys", true)?;
        result?;
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
}

fn expected_intents(records: &[EventRecord]) -> Vec<(u64, &str, &EffectIntent)> {
    records
        .iter()
        .filter_map(|record| {
            let RunEventBody::EffectIntentCommitted { step_id, intent } = &record.event.body else {
                return None;
            };
            Some((record.sequence, step_id.as_str(), intent.as_ref()))
        })
        .collect()
}

fn validate_effect_intent_index(
    connection: &Connection,
    records: &[EventRecord],
) -> Result<(), StoreError> {
    let expected = expected_intents(records);
    let mut statement = connection.prepare(
        "SELECT effect_id, event_sequence, step_id, action_digest, intent_json FROM effect_intents ORDER BY event_sequence, effect_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
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
    for ((sequence, step_id, intent), actual) in expected.into_iter().zip(actual) {
        let (effect_id, stored_sequence, stored_step_id, action_digest, intent_json) = actual;
        let stored_sequence =
            u64::try_from(stored_sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
        let stored_intent: EffectIntent = serde_json::from_slice(&intent_json)?;
        if effect_id != intent.effect_id
            || stored_sequence != sequence
            || stored_step_id != step_id
            || action_digest != intent.action_digest
            || stored_intent != *intent
        {
            return Err(StoreError::Corrupt(format!(
                "effect index at event {sequence} differs from committed intent"
            )));
        }
    }
    Ok(())
}

fn validate_authorization_index(
    connection: &Connection,
    records: &[EventRecord],
) -> Result<(), StoreError> {
    let mut expected: Vec<_> = expected_intents(records)
        .into_iter()
        .map(|(_, _, intent)| intent)
        .collect();
    expected.sort_by(|left, right| {
        (&left.effect_id, &left.authorization.grant_id)
            .cmp(&(&right.effect_id, &right.authorization.grant_id))
    });
    let mut statement = connection.prepare(
        "SELECT effect_id, grant_id, action_digest, grant_digest, max_uses FROM authorization_consumption ORDER BY effect_id, grant_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let actual: Vec<_> = rows.collect::<Result<_, _>>()?;
    if actual.len() != expected.len() {
        return Err(StoreError::Corrupt(format!(
            "authorization index count differs from intents: expected {}, actual {}",
            expected.len(),
            actual.len()
        )));
    }
    for (intent, actual) in expected.into_iter().zip(actual) {
        let (effect_id, grant_id, action_digest, grant_digest, max_uses) = actual;
        let max_uses = u32::try_from(max_uses).map_err(|_| StoreError::SequenceOutOfRange)?;
        if effect_id != intent.effect_id
            || grant_id != intent.authorization.grant_id
            || action_digest != intent.action_digest
            || grant_digest != intent.authorization.grant_digest
            || max_uses != intent.authorization.max_uses
        {
            return Err(StoreError::Corrupt(format!(
                "authorization index for effect `{}` differs from committed intent",
                intent.effect_id
            )));
        }
    }
    Ok(())
}

fn validate_foreign_keys(connection: &Connection) -> Result<(), StoreError> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    if statement.query([])?.next()?.is_some() {
        return Err(StoreError::Corrupt(
            "SQLite foreign-key integrity check failed".to_owned(),
        ));
    }
    Ok(())
}

fn validate_derived_state(
    connection: &Connection,
    records: &[EventRecord],
) -> Result<(), StoreError> {
    validate_foreign_keys(connection)?;
    validate_effect_intent_index(connection, records)?;
    validate_authorization_index(connection, records)?;
    let materials = load_invocation_materials(connection)?;
    verify_material_records(records, &materials)
}

fn configure_connection(connection: &Connection) -> Result<(), StoreError> {
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

impl RunStore for SqliteRunStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, None, |_| Ok(()))
    }

    fn append_with_invocation_material(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        material: InvocationMaterialRecord,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, Some(&material), |_| Ok(()))
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        let records = load_records(&self.connection)?;
        let projection = load_projection(&self.connection)?;
        let snapshot = verified_snapshot(records, projection)?;
        let records = snapshot
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.records.as_slice());
        validate_derived_state(&self.connection, records)?;
        Ok(snapshot)
    }

    fn load_invocation_material(
        &self,
        effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        self.load()?;
        Ok(load_invocation_materials(&self.connection)?.remove(effect_id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitStage {
    Event,
    EffectIntentIndex,
    AuthorizationConsumption,
    InvocationMaterial,
    Projection,
}

impl CommitStage {
    #[cfg(test)]
    const fn label(self) -> &'static str {
        match self {
            Self::Event => "event insert",
            Self::EffectIntentIndex => "effect intent index",
            Self::AuthorizationConsumption => "authorization consumption",
            Self::InvocationMaterial => "invocation material",
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

fn insert_effect_intent_index(
    transaction: &Transaction<'_>,
    record: &EventRecord,
) -> Result<(), StoreError> {
    let RunEventBody::EffectIntentCommitted { step_id, intent } = &record.event.body else {
        return Ok(());
    };
    let sequence = i64::try_from(record.sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
    let intent_json = serde_json::to_vec(intent)?;
    transaction.execute(
        "INSERT INTO effect_intents (effect_id, event_sequence, step_id, action_digest, intent_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![intent.effect_id, sequence, step_id, intent.action_digest, intent_json],
    )?;
    Ok(())
}

fn insert_authorization_consumption(
    transaction: &Transaction<'_>,
    record: &EventRecord,
) -> Result<(), StoreError> {
    let RunEventBody::EffectIntentCommitted { intent, .. } = &record.event.body else {
        return Ok(());
    };
    let max_uses = i64::from(intent.authorization.max_uses);
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

fn insert_invocation_material(
    transaction: &Transaction<'_>,
    material: Option<&InvocationMaterialRecord>,
) -> Result<(), StoreError> {
    let Some(material) = material else {
        return Ok(());
    };
    let record_json = serde_json::to_vec(material)?;
    transaction.execute(
        "INSERT INTO invocation_materials (effect_id, material_id, material_digest, record_digest, record_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            material.effect_id(),
            material.material_id(),
            material.material_digest(),
            material.record_digest(),
            record_json
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

fn load_invocation_materials(
    connection: &Connection,
) -> Result<BTreeMap<String, InvocationMaterialRecord>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT effect_id, material_id, material_digest, record_digest, record_json FROM invocation_materials ORDER BY effect_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    let mut materials = BTreeMap::new();
    for row in rows {
        let (effect_id, material_id, material_digest, record_digest, record_json) = row?;
        let material: InvocationMaterialRecord = serde_json::from_slice(&record_json)?;
        if effect_id != material.effect_id()
            || material_id != material.material_id()
            || material_digest != material.material_digest()
            || record_digest != material.record_digest()
        {
            return Err(StoreError::Corrupt(format!(
                "invocation material index for effect `{effect_id}` differs from its record"
            )));
        }
        if materials.insert(effect_id.clone(), material).is_some() {
            return Err(StoreError::Corrupt(format!(
                "duplicate invocation material for effect `{effect_id}`"
            )));
        }
    }
    Ok(materials)
}

#[cfg(test)]
fn table_count(connection: &Connection, table: &str) -> Result<u64, StoreError> {
    let query = match table {
        "run_events" => "SELECT COUNT(*) FROM run_events",
        "effect_intents" => "SELECT COUNT(*) FROM effect_intents",
        "authorization_consumption" => "SELECT COUNT(*) FROM authorization_consumption",
        "invocation_materials" => "SELECT COUNT(*) FROM invocation_materials",
        _ => {
            return Err(StoreError::Corrupt(format!(
                "unsupported internal count table `{table}`"
            )));
        }
    };
    let count: i64 = connection.query_row(query, [], |row| row.get(0))?;
    u64::try_from(count).map_err(|_| StoreError::SequenceOutOfRange)
}
