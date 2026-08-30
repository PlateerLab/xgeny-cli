use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use xgeny_domain::{ExecutionReceiptBody, ProtocolDocument};
use xgeny_workgraph::{
    CompletionOutputRecord, EffectIntent, EventRecord, InvocationMaterialRecord,
    PlannedInvocationMaterialRecord, RunEvent, RunEventBody, RunState, ToolOutputRecord,
};

use crate::{
    AuditMetrics, Commit, CommitAnchors, CommitSidecars, ExpectedHead, RunPlanningSnapshot,
    RunSnapshot, RunStore, RunVerificationSnapshot, StoreError, StoredCompletionOutput,
    StoredExecutionReceipt, StoredToolOutput, VerifiedRunIndex, audit_snapshot,
    build_completion_output, build_planning_snapshot, index_tool_output, prepare_commit,
    verify_commit_sidecars, verify_completion_output_candidate, verify_completion_output_record,
    verify_material_point, verify_material_records, verify_plan_input_point,
    verify_plan_input_records, verify_planned_material_retention, verify_receipt_candidate,
    verify_receipt_records, verify_stored_tool_output, verify_tool_output_candidate,
    verify_tool_output_point,
};

const STORE_SCHEMA_VERSION: i64 = 8;

const CREATE_COMPLETION_OUTPUT_SCHEMA: &str = r"
CREATE TABLE completion_outputs (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    event_sequence INTEGER NOT NULL UNIQUE CHECK (event_sequence >= 1),
    candidate_id TEXT NOT NULL UNIQUE,
    turn_index INTEGER NOT NULL CHECK (turn_index >= 1),
    model_call_id TEXT NOT NULL UNIQUE,
    context_digest TEXT NOT NULL,
    proposal_digest TEXT NOT NULL,
    summary_size_bytes INTEGER NOT NULL CHECK (summary_size_bytes BETWEEN 1 AND 5000),
    summary_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL CHECK (length(record_json) BETWEEN 1 AND 20000),
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence)
) STRICT;
";

const CREATE_TOOL_OUTPUT_SCHEMA: &str = r"
CREATE TABLE tool_outputs (
    effect_id TEXT PRIMARY KEY,
    event_sequence INTEGER NOT NULL UNIQUE CHECK (event_sequence >= 1),
    step_id TEXT NOT NULL,
    output_id TEXT NOT NULL UNIQUE,
    capability_id TEXT NOT NULL,
    contract_version TEXT NOT NULL,
    definition_digest TEXT NOT NULL,
    canonical_size_bytes INTEGER NOT NULL CHECK (canonical_size_bytes BETWEEN 1 AND 1048576),
    output_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL CHECK (length(record_json) BETWEEN 1 AND 1100000),
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence),
    FOREIGN KEY (effect_id) REFERENCES effect_intents(effect_id)
) STRICT;
";

const CREATE_PLAN_INPUT_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS planned_invocations (
    step_id TEXT PRIMARY KEY,
    event_sequence INTEGER NOT NULL CHECK (event_sequence >= 1),
    plan_id TEXT NOT NULL UNIQUE,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL,
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence)
) STRICT;
";

const CREATE_RECEIPT_SCHEMA: &str = r"
CREATE TABLE execution_receipts (
    receipt_id TEXT PRIMARY KEY,
    event_sequence INTEGER NOT NULL UNIQUE CHECK (event_sequence >= 1),
    effect_id TEXT NOT NULL UNIQUE,
    step_id TEXT NOT NULL,
    previous_receipt_digest TEXT,
    receipt_digest TEXT NOT NULL UNIQUE,
    receipt_json BLOB NOT NULL,
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence),
    FOREIGN KEY (effect_id) REFERENCES effect_intents(effect_id),
    FOREIGN KEY (previous_receipt_digest) REFERENCES execution_receipts(receipt_digest)
) STRICT;
";

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

CREATE TABLE IF NOT EXISTS tool_outputs (
    effect_id TEXT PRIMARY KEY,
    event_sequence INTEGER NOT NULL UNIQUE CHECK (event_sequence >= 1),
    step_id TEXT NOT NULL,
    output_id TEXT NOT NULL UNIQUE,
    capability_id TEXT NOT NULL,
    contract_version TEXT NOT NULL,
    definition_digest TEXT NOT NULL,
    canonical_size_bytes INTEGER NOT NULL CHECK (canonical_size_bytes BETWEEN 1 AND 1048576),
    output_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL CHECK (length(record_json) BETWEEN 1 AND 1100000),
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence),
    FOREIGN KEY (effect_id) REFERENCES effect_intents(effect_id)
) STRICT;

CREATE TABLE IF NOT EXISTS planned_invocations (
    step_id TEXT PRIMARY KEY,
    event_sequence INTEGER NOT NULL CHECK (event_sequence >= 1),
    plan_id TEXT NOT NULL UNIQUE,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL,
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence)
) STRICT;

CREATE TABLE IF NOT EXISTS execution_receipts (
    receipt_id TEXT PRIMARY KEY,
    event_sequence INTEGER NOT NULL UNIQUE CHECK (event_sequence >= 1),
    effect_id TEXT NOT NULL UNIQUE,
    step_id TEXT NOT NULL,
    previous_receipt_digest TEXT,
    receipt_digest TEXT NOT NULL UNIQUE,
    receipt_json BLOB NOT NULL,
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence),
    FOREIGN KEY (effect_id) REFERENCES effect_intents(effect_id),
    FOREIGN KEY (previous_receipt_digest) REFERENCES execution_receipts(receipt_digest)
) STRICT;

CREATE TABLE IF NOT EXISTS completion_outputs (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    event_sequence INTEGER NOT NULL UNIQUE CHECK (event_sequence >= 1),
    candidate_id TEXT NOT NULL UNIQUE,
    turn_index INTEGER NOT NULL CHECK (turn_index >= 1),
    model_call_id TEXT NOT NULL UNIQUE,
    context_digest TEXT NOT NULL,
    proposal_digest TEXT NOT NULL,
    summary_size_bytes INTEGER NOT NULL CHECK (summary_size_bytes BETWEEN 1 AND 5000),
    summary_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL CHECK (length(record_json) BETWEEN 1 AND 20000),
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence)
) STRICT;
";

#[derive(Debug)]
pub struct SqliteRunStore {
    connection: Connection,
    cache: RefCell<Option<VerifiedSqliteCache>>,
    metrics: RefCell<AuditMetrics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedSqliteCache {
    data_version: i64,
    index: VerifiedRunIndex,
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
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                configure_connection(&connection)?;
                connection.execute_batch(CREATE_SCHEMA)?;
                connection.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            }
            3 => {
                configure_connection(&connection)?;
                migrate_schema_three(&mut connection)?;
            }
            4 => {
                configure_connection(&connection)?;
                migrate_schema_four(&mut connection)?;
            }
            5 => {
                configure_connection(&connection)?;
                migrate_schema_five(&mut connection)?;
            }
            6 => {
                configure_connection(&connection)?;
                migrate_schema_six(&mut connection)?;
            }
            7 => {
                configure_connection(&connection)?;
                migrate_schema_seven(&mut connection)?;
            }
            STORE_SCHEMA_VERSION => configure_connection(&connection)?,
            unsupported => return Err(StoreError::UnsupportedSchemaVersion(unsupported)),
        }

        let metrics = RefCell::new(AuditMetrics::default());
        let cache = build_verified_cache(&connection, &metrics)?;
        Ok(Self {
            connection,
            cache: RefCell::new(Some(cache)),
            metrics,
        })
    }

    #[allow(clippy::too_many_lines)] // Keep the complete transactional write/checkpoint order adjacent.
    fn append_internal(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        sidecars: CommitSidecars<'_>,
        mut checkpoint: impl FnMut(CommitStage) -> Result<(), StoreError>,
    ) -> Result<Commit, StoreError> {
        let CommitSidecars {
            plan_inputs,
            material,
            output,
            completion_output,
            receipt,
        } = sidecars;
        verify_commit_sidecars(&event, sidecars)?;
        let Self {
            connection,
            cache,
            metrics,
        } = self;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Err(error) = ensure_verified_cache(cache, metrics, &transaction) {
            cache.replace(None);
            return Err(error);
        }
        let (commit, output_anchor, completion_output_anchor, receipt_anchor) = {
            let cached = cache.borrow();
            let index = &cached
                .as_ref()
                .expect("cache refresh must install a verified index")
                .index;
            if let Some(material) = material {
                let planned_input = match &event.body {
                    RunEventBody::EffectIntentCommitted { step_id, .. } => {
                        load_planned_invocation(&transaction, step_id)?
                    }
                    _ => None,
                };
                verify_planned_material_retention(index, &event, material, planned_input.as_ref())?;
            }
            let commit = prepare_commit(index, expected, event)?;
            if let Some(material) = material
                && index.material_effect_ids.contains(material.effect_id())
            {
                return Err(StoreError::Corrupt(
                    "duplicate invocation material effect ID".to_owned(),
                ));
            }
            if let Some(inputs) = plan_inputs
                && inputs
                    .iter()
                    .any(|input| index.plan_input_step_ids.contains(input.step_id()))
            {
                return Err(StoreError::Corrupt(
                    "duplicate planned invocation input Step ID".to_owned(),
                ));
            }
            let receipt_anchor = receipt
                .map(|receipt| verify_receipt_candidate(index, &commit.record, receipt))
                .transpose()?;
            let output_anchor = output
                .map(|output| verify_tool_output_candidate(index, &commit.record, output))
                .transpose()?;
            let completion_output_anchor = completion_output
                .map(|output| verify_completion_output_candidate(index, &commit.record, output))
                .transpose()?;
            (
                commit,
                output_anchor,
                completion_output_anchor,
                receipt_anchor,
            )
        };
        metrics.borrow_mut().record_candidate_sidecars(sidecars);

        let write_result = (|| -> Result<(), StoreError> {
            insert_event(&transaction, &commit.record)?;
            checkpoint(CommitStage::Event)?;
            insert_effect_intent_index(&transaction, &commit.record)?;
            checkpoint(CommitStage::EffectIntentIndex)?;
            insert_authorization_consumption(&transaction, &commit.record)?;
            checkpoint(CommitStage::AuthorizationConsumption)?;
            insert_planned_invocations(&transaction, &commit.record, plan_inputs)?;
            checkpoint(CommitStage::PlannedInvocation)?;
            insert_invocation_material(&transaction, material)?;
            checkpoint(CommitStage::InvocationMaterial)?;
            insert_tool_output(&transaction, &commit.record, output)?;
            checkpoint(CommitStage::ToolOutput)?;
            insert_completion_output(&transaction, &commit.record, completion_output)?;
            checkpoint(CommitStage::CompletionOutput)?;
            insert_execution_receipt(&transaction, &commit.record, receipt)?;
            checkpoint(CommitStage::ExecutionReceipt)?;
            write_projection(&transaction, &commit.state)?;
            checkpoint(CommitStage::Projection)?;
            transaction.commit()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            cache.replace(None);
            return Err(error);
        }
        cache
            .borrow_mut()
            .as_mut()
            .expect("successful commit must retain its verified prefix")
            .index
            .apply_committed(
                &commit,
                sidecars,
                CommitAnchors {
                    output: output_anchor,
                    completion_output: completion_output_anchor,
                    receipt: receipt_anchor,
                },
            );
        Ok(commit)
    }

    #[cfg(test)]
    pub(crate) fn append_plain_with_fault(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::default(), |stage| {
            if stage == fault {
                Err(StoreError::InjectedFault(stage.label()))
            } else {
                Ok(())
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn append_with_fault(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        material: &InvocationMaterialRecord,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(
            expected,
            event,
            CommitSidecars::material(material),
            |stage| {
                if stage == fault {
                    Err(StoreError::InjectedFault(stage.label()))
                } else {
                    Ok(())
                }
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn append_plan_with_fault(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: &[PlannedInvocationMaterialRecord],
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(
            expected,
            event,
            CommitSidecars::plan_inputs(inputs),
            |stage| {
                if stage == fault {
                    Err(StoreError::InjectedFault(stage.label()))
                } else {
                    Ok(())
                }
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn append_receipt_with_fault(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        receipt: &ExecutionReceiptBody,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::receipt(receipt), |stage| {
            if stage == fault {
                Err(StoreError::InjectedFault(stage.label()))
            } else {
                Ok(())
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn append_tool_output_with_fault(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        output: &ToolOutputRecord,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::output(output), |stage| {
            if stage == fault {
                Err(StoreError::InjectedFault(stage.label()))
            } else {
                Ok(())
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn append_completion_output_with_fault(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        output: &CompletionOutputRecord,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(
            expected,
            event,
            CommitSidecars::completion_output(output),
            |stage| {
                if stage == fault {
                    Err(StoreError::InjectedFault(stage.label()))
                } else {
                    Ok(())
                }
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn append_receipt_and_exit_at(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        receipt: &ExecutionReceiptBody,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::receipt(receipt), |stage| {
            if stage == fault {
                std::process::exit(86);
            }
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn append_tool_output_and_exit_at(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        output: &ToolOutputRecord,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::output(output), |stage| {
            if stage == fault {
                std::process::exit(86);
            }
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn append_completion_output_and_exit_at(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        output: &CompletionOutputRecord,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(
            expected,
            event,
            CommitSidecars::completion_output(output),
            |stage| {
                if stage == fault {
                    std::process::exit(86);
                }
                Ok(())
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn append_and_exit_at(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        material: &InvocationMaterialRecord,
        fault: CommitStage,
    ) -> Result<Commit, StoreError> {
        self.append_internal(
            expected,
            event,
            CommitSidecars::material(material),
            |stage| {
                if stage == fault {
                    std::process::exit(86);
                }
                Ok(())
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn invalidate_cache(&self) {
        self.cache.replace(None);
    }

    #[cfg(test)]
    pub(crate) fn reset_test_metrics(&mut self) {
        *self.metrics.get_mut() = AuditMetrics::default();
    }

    #[cfg(test)]
    pub(crate) fn test_metrics(&self) -> AuditMetrics {
        *self.metrics.borrow()
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
    pub(crate) fn planned_invocation_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "planned_invocations")
    }

    #[cfg(test)]
    pub(crate) fn execution_receipt_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "execution_receipts")
    }

    #[cfg(test)]
    pub(crate) fn tool_output_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "tool_outputs")
    }

    #[cfg(test)]
    pub(crate) fn completion_output_count(&self) -> Result<u64, StoreError> {
        table_count(&self.connection, "completion_outputs")
    }

    #[cfg(test)]
    pub(crate) fn delete_completion_output(&self) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM completion_outputs", [])?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn corrupt_completion_output_document(&self) -> Result<(), StoreError> {
        let bytes: Vec<u8> = self.connection.query_row(
            "SELECT record_json FROM completion_outputs WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        value["summary"] = serde_json::json!("TAMPERED-COMPLETION-SUMMARY");
        self.connection.execute(
            "UPDATE completion_outputs SET record_json = ?1 WHERE singleton = 1",
            [serde_json::to_vec(&value)?],
        )?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_orphan_completion_output(&self) -> Result<(), StoreError> {
        self.connection
            .pragma_update(None, "ignore_check_constraints", true)?;
        let result = self.connection.execute(
            "INSERT INTO completion_outputs (singleton, event_sequence, candidate_id, turn_index, model_call_id, context_digest, proposal_digest, summary_size_bytes, summary_digest, record_digest, record_json) SELECT 2, 1, candidate_id || '-orphan', turn_index, model_call_id || '-orphan', context_digest, proposal_digest, summary_size_bytes, summary_digest, record_digest || '-orphan', record_json FROM completion_outputs WHERE singleton = 1",
            [],
        );
        self.connection
            .pragma_update(None, "ignore_check_constraints", false)?;
        result?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn delete_tool_outputs(&self) -> Result<(), StoreError> {
        self.connection.execute("DELETE FROM tool_outputs", [])?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn corrupt_tool_output_document(&self) -> Result<(), StoreError> {
        let bytes: Vec<u8> = self.connection.query_row(
            "SELECT record_json FROM tool_outputs LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        value["output"] = serde_json::json!({"secret": "tampered-output"});
        self.connection.execute(
            "UPDATE tool_outputs SET record_json = ?1",
            [serde_json::to_vec(&value)?],
        )?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn corrupt_tool_output_shape_with_secret(
        &self,
        secret: &str,
    ) -> Result<(), StoreError> {
        let invalid_record = format!(r#"{{"{secret}":null}}"#).into_bytes();
        self.connection
            .execute("UPDATE tool_outputs SET record_json = ?1", [invalid_record])?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn corrupt_tool_output_index(&self) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE tool_outputs SET output_digest = 'sha256:corrupted'",
            [],
        )?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_orphan_tool_output(&self) -> Result<(), StoreError> {
        self.connection.pragma_update(None, "foreign_keys", false)?;
        let result = self.connection.execute(
            "INSERT INTO tool_outputs (effect_id, event_sequence, step_id, output_id, capability_id, contract_version, definition_digest, canonical_size_bytes, output_digest, record_digest, record_json) SELECT 'orphan-effect', 999999, step_id, output_id || '-orphan', capability_id, contract_version, definition_digest, canonical_size_bytes, output_digest, record_digest || '-orphan', record_json FROM tool_outputs LIMIT 1",
            [],
        );
        self.connection.pragma_update(None, "foreign_keys", true)?;
        result?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn delete_execution_receipts(&self) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM execution_receipts", [])?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn corrupt_execution_receipt_document(&self) -> Result<(), StoreError> {
        let bytes: Vec<u8> = self.connection.query_row(
            "SELECT receipt_json FROM execution_receipts LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        value["outputDigest"] = serde_json::Value::String(format!("sha256:{}", "9".repeat(64)));
        self.connection.execute(
            "UPDATE execution_receipts SET receipt_json = ?1",
            [serde_json::to_vec(&value)?],
        )?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn corrupt_effect_index(&self) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE effect_intents SET action_digest = 'sha256:corrupted'",
            [],
        )?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn corrupt_invocation_material_index(&self) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE invocation_materials SET material_digest = 'sha256:corrupted'",
            [],
        )?;
        self.invalidate_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn delete_invocation_material(&self) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM invocation_materials", [])?;
        self.invalidate_cache();
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
        self.invalidate_cache();
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

#[cfg(test)]
pub(crate) fn write_schema_three_fixture(
    path: &Path,
    records: &[EventRecord],
    state: &RunState,
    materials: &[InvocationMaterialRecord],
) -> Result<(), StoreError> {
    let mut connection = Connection::open(path)?;
    configure_connection(&connection)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(CREATE_SCHEMA)?;
    transaction.execute("DROP TABLE execution_receipts", [])?;
    transaction.execute("DROP TABLE tool_outputs", [])?;
    transaction.execute("DROP TABLE completion_outputs", [])?;
    transaction.execute("DROP TABLE planned_invocations", [])?;
    for record in records {
        insert_event(&transaction, record)?;
        insert_effect_intent_index(&transaction, record)?;
        insert_authorization_consumption(&transaction, record)?;
    }
    for material in materials {
        insert_invocation_material(&transaction, Some(material))?;
    }
    write_projection(&transaction, state)?;
    transaction.pragma_update(None, "user_version", 3_i64)?;
    transaction.commit()?;
    Ok(())
}

fn expected_intents(index: &VerifiedRunIndex) -> Vec<&crate::EffectIntentAnchor> {
    let mut expected: Vec<_> = index.intents.values().collect();
    expected.sort_by(|left, right| {
        (left.event_sequence, &left.intent.effect_id)
            .cmp(&(right.event_sequence, &right.intent.effect_id))
    });
    expected
}

fn validate_effect_intent_index(
    connection: &Connection,
    index: &VerifiedRunIndex,
) -> Result<(), StoreError> {
    let expected = expected_intents(index);
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
    for (anchor, actual) in expected.into_iter().zip(actual) {
        let (effect_id, stored_sequence, stored_step_id, action_digest, intent_json) = actual;
        let stored_sequence =
            u64::try_from(stored_sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
        let stored_intent: EffectIntent = serde_json::from_slice(&intent_json)?;
        if effect_id != anchor.intent.effect_id
            || stored_sequence != anchor.event_sequence
            || stored_step_id != anchor.step_id
            || action_digest != anchor.intent.action_digest
            || stored_intent != anchor.intent
        {
            return Err(StoreError::Corrupt(format!(
                "effect index at event {} differs from committed intent",
                anchor.event_sequence
            )));
        }
    }
    Ok(())
}

fn validate_authorization_index(
    connection: &Connection,
    index: &VerifiedRunIndex,
) -> Result<(), StoreError> {
    let mut expected: Vec<_> = expected_intents(index)
        .into_iter()
        .map(|anchor| &anchor.intent)
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

fn validate_derived_state_without_receipts(
    connection: &Connection,
    index: &mut VerifiedRunIndex,
    metrics: &mut AuditMetrics,
) -> Result<(), StoreError> {
    validate_foreign_keys(connection)?;
    validate_effect_intent_index(connection, index)?;
    validate_authorization_index(connection, index)?;
    let plan_inputs = load_planned_invocations(connection, index)?;
    verify_plan_input_records(index, &plan_inputs, metrics)?;
    let materials = load_invocation_materials(connection)?;
    verify_material_records(index, &materials, metrics)
}

fn load_verified_store_data(
    connection: &Connection,
    metrics: &mut AuditMetrics,
) -> Result<
    (
        Option<RunSnapshot>,
        Vec<StoredExecutionReceipt>,
        VerifiedRunIndex,
    ),
    StoreError,
> {
    metrics.record_full_audit();
    let records = load_records(connection)?;
    let projection = load_projection(connection)?;
    let (snapshot, mut index) = audit_snapshot(records, projection, metrics)?;
    validate_derived_state_without_receipts(connection, &mut index, metrics)?;
    verify_sqlite_tool_outputs(connection, &mut index, metrics)?;
    let receipts = load_execution_receipts(connection)?;
    verify_receipt_records(&mut index, &receipts, metrics)?;
    let completion_output = load_completion_output_row(connection)?;
    verify_completion_output_record(&index, completion_output.as_ref())?;
    Ok((snapshot, receipts, index))
}

pub(crate) fn migrate_schema_three(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        3 => {
            transaction.execute_batch(CREATE_PLAN_INPUT_SCHEMA)?;
            let records = load_records(&transaction)?;
            let projection = load_projection(&transaction)?;
            let mut metrics = AuditMetrics::default();
            let (_, mut index) = audit_snapshot(records, projection, &mut metrics)?;
            validate_derived_state_without_receipts(&transaction, &mut index, &mut metrics)?;
            transaction.execute_batch(CREATE_RECEIPT_SCHEMA)?;
            transaction.execute_batch(CREATE_TOOL_OUTPUT_SCHEMA)?;
            transaction.execute_batch(CREATE_COMPLETION_OUTPUT_SCHEMA)?;
            verify_sqlite_tool_outputs(&transaction, &mut index, &mut metrics)?;
            verify_receipt_records(&mut index, &[], &mut metrics)?;
            verify_completion_output_record(&index, None)?;
            transaction.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            transaction.commit()?;
            Ok(())
        }
        4..=6 => {
            // Another schema-4 binary may have completed the 3 -> 4 migration after `open`
            // observed version 3 but before this immediate transaction acquired the writer lock.
            // Re-audit the now-current format and converge instead of requiring a process restart.
            transaction.execute_batch(CREATE_PLAN_INPUT_SCHEMA)?;
            transaction.execute_batch(CREATE_TOOL_OUTPUT_SCHEMA)?;
            transaction.execute_batch(CREATE_COMPLETION_OUTPUT_SCHEMA)?;
            let mut metrics = AuditMetrics::default();
            let _ = load_verified_store_data(&transaction, &mut metrics)?;
            transaction.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            transaction.commit()?;
            Ok(())
        }
        STORE_SCHEMA_VERSION => {
            transaction.commit()?;
            Ok(())
        }
        7 => migrate_schema_seven_transaction(transaction),
        unsupported => Err(StoreError::UnsupportedSchemaVersion(unsupported)),
    }
}

fn migrate_schema_four(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        4..=6 => {
            transaction.execute_batch(CREATE_PLAN_INPUT_SCHEMA)?;
            transaction.execute_batch(CREATE_TOOL_OUTPUT_SCHEMA)?;
            transaction.execute_batch(CREATE_COMPLETION_OUTPUT_SCHEMA)?;
            let mut metrics = AuditMetrics::default();
            let _ = load_verified_store_data(&transaction, &mut metrics)?;
            transaction.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            transaction.commit()?;
            Ok(())
        }
        STORE_SCHEMA_VERSION => {
            transaction.commit()?;
            Ok(())
        }
        7 => migrate_schema_seven_transaction(transaction),
        unsupported => Err(StoreError::UnsupportedSchemaVersion(unsupported)),
    }
}

fn migrate_schema_five(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        5 | 6 => {
            transaction.execute_batch(CREATE_PLAN_INPUT_SCHEMA)?;
            transaction.execute_batch(CREATE_TOOL_OUTPUT_SCHEMA)?;
            transaction.execute_batch(CREATE_COMPLETION_OUTPUT_SCHEMA)?;
            let mut metrics = AuditMetrics::default();
            let _ = load_verified_store_data(&transaction, &mut metrics)?;
            transaction.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            transaction.commit()?;
            Ok(())
        }
        STORE_SCHEMA_VERSION => {
            transaction.commit()?;
            Ok(())
        }
        7 => migrate_schema_seven_transaction(transaction),
        unsupported => Err(StoreError::UnsupportedSchemaVersion(unsupported)),
    }
}

fn migrate_schema_six(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        6 => {
            transaction.execute_batch(CREATE_TOOL_OUTPUT_SCHEMA)?;
            transaction.execute_batch(CREATE_COMPLETION_OUTPUT_SCHEMA)?;
            let mut metrics = AuditMetrics::default();
            let _ = load_verified_store_data(&transaction, &mut metrics)?;
            transaction.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            transaction.commit()?;
            Ok(())
        }
        STORE_SCHEMA_VERSION => {
            transaction.commit()?;
            Ok(())
        }
        7 => migrate_schema_seven_transaction(transaction),
        unsupported => Err(StoreError::UnsupportedSchemaVersion(unsupported)),
    }
}

fn migrate_schema_seven(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    migrate_schema_seven_transaction(transaction)
}

fn migrate_schema_seven_transaction(transaction: Transaction<'_>) -> Result<(), StoreError> {
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        7 => {
            transaction.execute_batch(CREATE_COMPLETION_OUTPUT_SCHEMA)?;
            let mut metrics = AuditMetrics::default();
            let _ = load_verified_store_data(&transaction, &mut metrics)?;
            transaction.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
            transaction.commit()?;
            Ok(())
        }
        STORE_SCHEMA_VERSION => {
            transaction.commit()?;
            Ok(())
        }
        unsupported => Err(StoreError::UnsupportedSchemaVersion(unsupported)),
    }
}

fn configure_connection(connection: &Connection) -> Result<(), StoreError> {
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn load_data_version(connection: &Connection) -> Result<i64, StoreError> {
    Ok(connection.pragma_query_value(None, "data_version", |row| row.get(0))?)
}

fn load_durable_head(connection: &Connection) -> Result<ExpectedHead, StoreError> {
    let row: Option<(i64, String)> = connection
        .query_row(
            "SELECT sequence, digest FROM run_events ORDER BY sequence DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map_or(Ok(ExpectedHead::Empty), |(sequence, digest)| {
        Ok(ExpectedHead::Exact {
            sequence: u64::try_from(sequence).map_err(|_| StoreError::SequenceOutOfRange)?,
            digest,
        })
    })
}

fn build_verified_cache(
    connection: &Connection,
    metrics: &RefCell<AuditMetrics>,
) -> Result<VerifiedSqliteCache, StoreError> {
    let transaction = connection.unchecked_transaction()?;
    let data_version = load_data_version(&transaction)?;
    let durable_head = load_durable_head(&transaction)?;
    let (_, _, index) = load_verified_store_data(&transaction, &mut metrics.borrow_mut())?;
    if index.head() != durable_head {
        return Err(StoreError::Corrupt(
            "verified journal head differs from the durable head".to_owned(),
        ));
    }
    transaction.commit()?;
    Ok(VerifiedSqliteCache {
        data_version,
        index,
    })
}

fn ensure_verified_cache(
    cache: &RefCell<Option<VerifiedSqliteCache>>,
    metrics: &RefCell<AuditMetrics>,
    connection: &Connection,
) -> Result<(), StoreError> {
    let data_version = load_data_version(connection)?;
    let durable_head = load_durable_head(connection)?;
    if cache.borrow().as_ref().is_some_and(|cached| {
        cached.data_version == data_version && cached.index.head() == durable_head
    }) {
        return Ok(());
    }

    cache.replace(None);
    let (_, _, index) = load_verified_store_data(connection, &mut metrics.borrow_mut())?;
    if index.head() != durable_head {
        return Err(StoreError::Corrupt(
            "verified journal head differs from the durable head".to_owned(),
        ));
    }
    cache.replace(Some(VerifiedSqliteCache {
        data_version,
        index,
    }));
    Ok(())
}

impl RunStore for SqliteRunStore {
    fn append(&mut self, expected: ExpectedHead, event: RunEvent) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::default(), |_| Ok(()))
    }

    fn append_with_plan_inputs(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        inputs: Vec<PlannedInvocationMaterialRecord>,
    ) -> Result<Commit, StoreError> {
        self.append_internal(
            expected,
            event,
            CommitSidecars::plan_inputs(&inputs),
            |_| Ok(()),
        )
    }

    fn append_with_invocation_material(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        material: InvocationMaterialRecord,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::material(&material), |_| {
            Ok(())
        })
    }

    fn append_with_execution_receipt(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        receipt: ExecutionReceiptBody,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::receipt(&receipt), |_| {
            Ok(())
        })
    }

    fn append_with_tool_output(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        output: ToolOutputRecord,
    ) -> Result<Commit, StoreError> {
        self.append_internal(expected, event, CommitSidecars::output(&output), |_| Ok(()))
    }

    fn append_with_completion_output(
        &mut self,
        expected: ExpectedHead,
        event: RunEvent,
        output: CompletionOutputRecord,
    ) -> Result<Commit, StoreError> {
        self.append_internal(
            expected,
            event,
            CommitSidecars::completion_output(&output),
            |_| Ok(()),
        )
    }

    fn load(&self) -> Result<Option<RunSnapshot>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let data_version = load_data_version(&transaction)?;
        let durable_head = load_durable_head(&transaction)?;
        let (snapshot, _, index) =
            load_verified_store_data(&transaction, &mut self.metrics.borrow_mut())?;
        if index.head() != durable_head {
            return Err(StoreError::Corrupt(
                "verified journal head differs from the durable head".to_owned(),
            ));
        }
        transaction.commit()?;
        self.cache.replace(Some(VerifiedSqliteCache {
            data_version,
            index,
        }));
        Ok(snapshot)
    }

    fn load_current(&self) -> Result<Option<RunState>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        ensure_verified_cache(&self.cache, &self.metrics, &transaction)?;
        let state = self
            .cache
            .borrow()
            .as_ref()
            .expect("cache refresh must install a verified index")
            .index
            .state
            .clone();
        transaction.commit()?;
        Ok(state)
    }

    fn load_invocation_material(
        &self,
        effect_id: &str,
    ) -> Result<Option<InvocationMaterialRecord>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        ensure_verified_cache(&self.cache, &self.metrics, &transaction)?;
        let material = load_invocation_material(&transaction, effect_id)?;
        verify_material_point(
            &self
                .cache
                .borrow()
                .as_ref()
                .expect("cache refresh must install a verified index")
                .index,
            effect_id,
            material.as_ref(),
        )?;
        transaction.commit()?;
        Ok(material)
    }

    fn load_planned_invocation(
        &self,
        step_id: &str,
    ) -> Result<Option<PlannedInvocationMaterialRecord>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        ensure_verified_cache(&self.cache, &self.metrics, &transaction)?;
        let input = load_planned_invocation(&transaction, step_id)?;
        verify_plan_input_point(
            &self
                .cache
                .borrow()
                .as_ref()
                .expect("cache refresh must install a verified index")
                .index,
            step_id,
            input.as_ref(),
        )?;
        transaction.commit()?;
        Ok(input)
    }

    fn load_execution_receipts(&self) -> Result<Vec<ExecutionReceiptBody>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        ensure_verified_cache(&self.cache, &self.metrics, &transaction)?;
        let receipts = load_execution_receipts(&transaction)?;
        transaction.commit()?;
        Ok(receipts.into_iter().map(|stored| stored.receipt).collect())
    }

    fn load_tool_output(&self, effect_id: &str) -> Result<Option<ToolOutputRecord>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        ensure_verified_cache(&self.cache, &self.metrics, &transaction)?;
        let output = load_tool_output(&transaction, effect_id)?;
        verify_tool_output_point(
            &self
                .cache
                .borrow()
                .as_ref()
                .expect("cache refresh must install a verified index")
                .index,
            effect_id,
            output.as_ref(),
        )?;
        transaction.commit()?;
        Ok(output.map(|stored| stored.record))
    }

    fn load_completion_output(
        &self,
        expected: ExpectedHead,
        candidate_id: &str,
    ) -> Result<Option<CompletionOutputRecord>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        ensure_verified_cache(&self.cache, &self.metrics, &transaction)?;
        let stored = load_completion_output_row(&transaction)?;
        let output = {
            let cache = self.cache.borrow();
            let index = &cache
                .as_ref()
                .expect("cache refresh must install a verified index")
                .index;
            build_completion_output(index, expected, candidate_id, stored)?
        };
        transaction.commit()?;
        Ok(output)
    }

    fn load_with_execution_receipts(
        &self,
    ) -> Result<(Option<RunSnapshot>, Vec<ExecutionReceiptBody>), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let data_version = load_data_version(&transaction)?;
        let durable_head = load_durable_head(&transaction)?;
        let (snapshot, receipts, index) =
            load_verified_store_data(&transaction, &mut self.metrics.borrow_mut())?;
        if index.head() != durable_head {
            return Err(StoreError::Corrupt(
                "verified journal head differs from the durable head".to_owned(),
            ));
        }
        transaction.commit()?;
        self.cache.replace(Some(VerifiedSqliteCache {
            data_version,
            index,
        }));
        Ok((
            snapshot,
            receipts.into_iter().map(|stored| stored.receipt).collect(),
        ))
    }

    fn load_verification_snapshot(
        &self,
        step_id: &str,
    ) -> Result<Option<RunVerificationSnapshot>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        ensure_verified_cache(&self.cache, &self.metrics, &transaction)?;
        let mut snapshot = self
            .cache
            .borrow()
            .as_ref()
            .expect("cache refresh must install a verified index")
            .index
            .verification_snapshot(step_id);
        if let Some(snapshot) = snapshot.as_mut()
            && let Some(effect_id) = snapshot
                .state
                .steps
                .get(step_id)
                .and_then(|step| step.intent.as_ref())
                .map(|intent| intent.effect_id.as_str())
        {
            let output = load_tool_output(&transaction, effect_id)?;
            verify_tool_output_point(
                &self
                    .cache
                    .borrow()
                    .as_ref()
                    .expect("cache refresh must install a verified index")
                    .index,
                effect_id,
                output.as_ref(),
            )?;
            snapshot.tool_output = output.map(|stored| stored.record);
        }
        transaction.commit()?;
        Ok(snapshot)
    }

    fn load_planning_snapshot(
        &self,
        expected: ExpectedHead,
        max_output_bytes: u64,
    ) -> Result<Option<RunPlanningSnapshot>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        ensure_verified_cache(&self.cache, &self.metrics, &transaction)?;
        let snapshot = {
            let cache = self.cache.borrow();
            let index = &cache
                .as_ref()
                .expect("cache refresh must install a verified index")
                .index;
            build_planning_snapshot(index, expected, max_output_bytes, |effect_id| {
                load_tool_output(&transaction, effect_id)
            })?
        };
        transaction.commit()?;
        Ok(snapshot)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitStage {
    Event,
    EffectIntentIndex,
    AuthorizationConsumption,
    PlannedInvocation,
    InvocationMaterial,
    ToolOutput,
    CompletionOutput,
    ExecutionReceipt,
    Projection,
}

impl CommitStage {
    #[cfg(test)]
    const fn label(self) -> &'static str {
        match self {
            Self::Event => "event insert",
            Self::EffectIntentIndex => "effect intent index",
            Self::AuthorizationConsumption => "authorization consumption",
            Self::PlannedInvocation => "planned invocation input",
            Self::InvocationMaterial => "invocation material",
            Self::ToolOutput => "tool output",
            Self::CompletionOutput => "completion output",
            Self::ExecutionReceipt => "execution receipt",
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

fn insert_planned_invocations(
    transaction: &Transaction<'_>,
    record: &EventRecord,
    inputs: Option<&[PlannedInvocationMaterialRecord]>,
) -> Result<(), StoreError> {
    let Some(inputs) = inputs else {
        return Ok(());
    };
    let sequence = i64::try_from(record.sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
    for input in inputs {
        let record_json = serde_json::to_vec(input)?;
        transaction.execute(
            "INSERT INTO planned_invocations \
             (step_id, event_sequence, plan_id, record_digest, record_json) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                input.step_id(),
                sequence,
                input.plan_id(),
                input.record_digest(),
                record_json
            ],
        )?;
    }
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

fn insert_tool_output(
    transaction: &Transaction<'_>,
    event: &EventRecord,
    output: Option<&ToolOutputRecord>,
) -> Result<(), StoreError> {
    let Some(output) = output else {
        return Ok(());
    };
    let RunEventBody::EffectSucceeded { step_id, .. } = &event.event.body else {
        unreachable!("tool-output bundle validation requires a success event")
    };
    let event_sequence =
        i64::try_from(event.sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
    let canonical_size_bytes =
        i64::try_from(output.canonical_size_bytes()).map_err(|_| StoreError::SequenceOutOfRange)?;
    let record_json = serde_json::to_vec(output)?;
    transaction.execute(
        "INSERT INTO tool_outputs (effect_id, event_sequence, step_id, output_id, capability_id, contract_version, definition_digest, canonical_size_bytes, output_digest, record_digest, record_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            output.effect_id(),
            event_sequence,
            step_id,
            output.output_id(),
            output.capability_id(),
            output.contract_version(),
            output.definition_digest(),
            canonical_size_bytes,
            output.output_digest(),
            output.record_digest(),
            record_json,
        ],
    )?;
    Ok(())
}

fn insert_completion_output(
    transaction: &Transaction<'_>,
    event: &EventRecord,
    output: Option<&CompletionOutputRecord>,
) -> Result<(), StoreError> {
    let Some(output) = output else {
        return Ok(());
    };
    let event_sequence =
        i64::try_from(event.sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
    let turn_index = i64::from(output.turn_index());
    let summary_size_bytes =
        i64::try_from(output.summary_size_bytes()).map_err(|_| StoreError::SequenceOutOfRange)?;
    let record_json = serde_json::to_vec(output)?;
    transaction.execute(
        "INSERT INTO completion_outputs (singleton, event_sequence, candidate_id, turn_index, model_call_id, context_digest, proposal_digest, summary_size_bytes, summary_digest, record_digest, record_json) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            event_sequence,
            output.candidate_id(),
            turn_index,
            output.model_call_id(),
            output.context_digest(),
            output.proposal_digest(),
            summary_size_bytes,
            output.summary_digest(),
            output.record_digest(),
            record_json,
        ],
    )?;
    Ok(())
}

fn insert_execution_receipt(
    transaction: &Transaction<'_>,
    record: &EventRecord,
    receipt: Option<&ExecutionReceiptBody>,
) -> Result<(), StoreError> {
    let Some(receipt) = receipt else {
        return Ok(());
    };
    let RunEventBody::VerificationRecorded {
        step_id, effect_id, ..
    } = &record.event.body
    else {
        unreachable!("receipt bundle validation requires a finalization event")
    };
    let sequence = i64::try_from(record.sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
    let document = ProtocolDocument::ExecutionReceipt(Box::new(receipt.clone()));
    let receipt_json = serde_json::to_vec(&document)?;
    transaction.execute(
        "INSERT INTO execution_receipts (receipt_id, event_sequence, effect_id, step_id, previous_receipt_digest, receipt_digest, receipt_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            receipt.receipt_id,
            sequence,
            effect_id,
            step_id,
            receipt.previous_receipt_digest,
            receipt.receipt_digest,
            receipt_json
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
        "SELECT sequence, event_id, previous_digest, digest, event_json \
         FROM run_events ORDER BY sequence",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    let mut records = Vec::new();
    for row in rows {
        let (sequence, event_id, previous_digest, digest, event_json) = row?;
        let sequence = u64::try_from(sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
        let event: RunEvent = serde_json::from_slice(&event_json)?;
        if event.event_id != event_id {
            return Err(StoreError::Corrupt(
                "run event ID index differs from the journal event".to_owned(),
            ));
        }
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

fn load_planned_invocations(
    connection: &Connection,
    index: &VerifiedRunIndex,
) -> Result<BTreeMap<String, PlannedInvocationMaterialRecord>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT step_id, event_sequence, plan_id, record_digest, record_json \
         FROM planned_invocations ORDER BY step_id",
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
    let mut inputs = BTreeMap::new();
    for row in rows {
        let (step_id, event_sequence, plan_id, record_digest, record_json) = row?;
        let event_sequence =
            u64::try_from(event_sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
        let input: PlannedInvocationMaterialRecord = serde_json::from_slice(&record_json)?;
        let anchor = index.planned_invocations.get(&step_id).ok_or_else(|| {
            StoreError::Corrupt(format!(
                "planned invocation input for Step `{step_id}` has no accepted event"
            ))
        })?;
        if event_sequence != anchor.event_sequence
            || step_id != input.step_id()
            || plan_id != input.plan_id()
            || record_digest != input.record_digest()
        {
            return Err(StoreError::Corrupt(format!(
                "planned invocation index for Step `{step_id}` differs from its record"
            )));
        }
        if inputs.insert(step_id.clone(), input).is_some() {
            return Err(StoreError::Corrupt(format!(
                "duplicate planned invocation input for Step `{step_id}`"
            )));
        }
    }
    Ok(inputs)
}

fn load_planned_invocation(
    connection: &Connection,
    step_id: &str,
) -> Result<Option<PlannedInvocationMaterialRecord>, StoreError> {
    let row: Option<(String, String, Vec<u8>)> = connection
        .query_row(
            "SELECT plan_id, record_digest, record_json \
             FROM planned_invocations WHERE step_id = ?1",
            [step_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(plan_id, record_digest, record_json)| {
        let input: PlannedInvocationMaterialRecord = serde_json::from_slice(&record_json)?;
        if step_id != input.step_id()
            || plan_id != input.plan_id()
            || record_digest != input.record_digest()
        {
            return Err(StoreError::Corrupt(format!(
                "planned invocation index for Step `{step_id}` differs from its record"
            )));
        }
        Ok(input)
    })
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

fn load_invocation_material(
    connection: &Connection,
    effect_id: &str,
) -> Result<Option<InvocationMaterialRecord>, StoreError> {
    let row: Option<(String, String, String, Vec<u8>)> = connection
        .query_row(
            "SELECT material_id, material_digest, record_digest, record_json FROM invocation_materials WHERE effect_id = ?1",
            [effect_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(
        |(material_id, material_digest, record_digest, record_json)| {
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
            Ok(material)
        },
    )
    .transpose()
}

fn load_tool_output(
    connection: &Connection,
    effect_id: &str,
) -> Result<Option<StoredToolOutput>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT event_sequence, step_id, output_id, capability_id, contract_version, definition_digest, canonical_size_bytes, output_digest, record_digest, length(record_json), record_json FROM tool_outputs WHERE effect_id = ?1",
    )?;
    let mut rows = statement.query([effect_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(decode_tool_output_row(effect_id, row)?))
}

fn decode_tool_output_row(
    effect_id: &str,
    row: &rusqlite::Row<'_>,
) -> Result<StoredToolOutput, StoreError> {
    let event_sequence =
        u64::try_from(row.get::<_, i64>(0)?).map_err(|_| StoreError::SequenceOutOfRange)?;
    let step_id: String = row.get(1)?;
    let output_id: String = row.get(2)?;
    let capability_id: String = row.get(3)?;
    let contract_version: String = row.get(4)?;
    let definition_digest: String = row.get(5)?;
    let canonical_size_bytes =
        u64::try_from(row.get::<_, i64>(6)?).map_err(|_| StoreError::SequenceOutOfRange)?;
    let output_digest: String = row.get(7)?;
    let record_digest: String = row.get(8)?;
    let record_json_size = row.get::<_, i64>(9)?;
    if !(1..=1_100_000).contains(&record_json_size) {
        return Err(StoreError::Corrupt(
            "tool-output record bytes exceed the fixed limit".to_owned(),
        ));
    }
    let record_json: Vec<u8> = row.get(10)?;
    let record = decode_tool_output_record(&record_json)?;
    if record.effect_id() != effect_id
        || record.step_id() != step_id
        || record.output_id() != output_id
        || record.capability_id() != capability_id
        || record.contract_version() != contract_version
        || record.definition_digest() != definition_digest
        || record.canonical_size_bytes() != canonical_size_bytes
        || record.output_digest() != output_digest
        || record.record_digest() != record_digest
    {
        return Err(StoreError::Corrupt(format!(
            "tool-output index for effect `{effect_id}` differs from its record"
        )));
    }
    Ok(StoredToolOutput {
        event_sequence,
        record,
    })
}

fn verify_sqlite_tool_outputs(
    connection: &Connection,
    index: &mut VerifiedRunIndex,
    metrics: &mut AuditMetrics,
) -> Result<(), StoreError> {
    let mut anchors: Vec<_> = index.tool_output_events.values().cloned().collect();
    anchors.sort_by_key(|anchor| anchor.event_sequence);
    let mut statement = connection.prepare(
        "SELECT effect_id, event_sequence, step_id, output_id, capability_id, contract_version, definition_digest, canonical_size_bytes, output_digest, record_digest, length(record_json), record_json FROM tool_outputs ORDER BY event_sequence, effect_id",
    )?;
    let mut rows = statement.query([])?;
    let mut position = 0_usize;
    while let Some(row) = rows.next()? {
        let effect_id: String = row.get(0)?;
        let anchor = anchors.get(position).ok_or_else(|| {
            StoreError::Corrupt("tool-output rows outnumber output-bound success events".to_owned())
        })?;
        let stored = decode_tool_output_row_offset(&effect_id, row, 1)?;
        metrics.record_historical_tool_output();
        verify_stored_tool_output(index, anchor, &stored)?;
        index_tool_output(index, anchor, &stored.record)?;
        position = position.saturating_add(1);
    }
    if position != anchors.len() {
        return Err(StoreError::Corrupt(format!(
            "tool-output count differs from output-bound success events: expected {}, actual {position}",
            anchors.len()
        )));
    }
    Ok(())
}

fn decode_tool_output_row_offset(
    effect_id: &str,
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> Result<StoredToolOutput, StoreError> {
    let event_sequence =
        u64::try_from(row.get::<_, i64>(offset)?).map_err(|_| StoreError::SequenceOutOfRange)?;
    let step_id: String = row.get(offset + 1)?;
    let output_id: String = row.get(offset + 2)?;
    let capability_id: String = row.get(offset + 3)?;
    let contract_version: String = row.get(offset + 4)?;
    let definition_digest: String = row.get(offset + 5)?;
    let canonical_size_bytes = u64::try_from(row.get::<_, i64>(offset + 6)?)
        .map_err(|_| StoreError::SequenceOutOfRange)?;
    let output_digest: String = row.get(offset + 7)?;
    let record_digest: String = row.get(offset + 8)?;
    let record_json_size = row.get::<_, i64>(offset + 9)?;
    if !(1..=1_100_000).contains(&record_json_size) {
        return Err(StoreError::Corrupt(
            "tool-output record bytes exceed the fixed limit".to_owned(),
        ));
    }
    let record_json: Vec<u8> = row.get(offset + 10)?;
    let record = decode_tool_output_record(&record_json)?;
    if record.effect_id() != effect_id
        || record.step_id() != step_id
        || record.output_id() != output_id
        || record.capability_id() != capability_id
        || record.contract_version() != contract_version
        || record.definition_digest() != definition_digest
        || record.canonical_size_bytes() != canonical_size_bytes
        || record.output_digest() != output_digest
        || record.record_digest() != record_digest
    {
        return Err(StoreError::Corrupt(format!(
            "tool-output index for effect `{effect_id}` differs from its record"
        )));
    }
    Ok(StoredToolOutput {
        event_sequence,
        record,
    })
}

fn decode_tool_output_record(record_json: &[u8]) -> Result<ToolOutputRecord, StoreError> {
    serde_json::from_slice(record_json)
        .map_err(|_| StoreError::Corrupt("tool-output record JSON is invalid".to_owned()))
}

fn load_completion_output_row(
    connection: &Connection,
) -> Result<Option<StoredCompletionOutput>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT singleton, event_sequence, candidate_id, turn_index, model_call_id, context_digest, proposal_digest, summary_size_bytes, summary_digest, record_digest, length(record_json), record_json FROM completion_outputs ORDER BY singleton",
    )?;
    let mut rows = statement.query([])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let singleton: i64 = row.get(0)?;
    if singleton != 1 {
        return Err(StoreError::Corrupt(
            "completion-output singleton identity is invalid".to_owned(),
        ));
    }
    let event_sequence =
        u64::try_from(row.get::<_, i64>(1)?).map_err(|_| StoreError::SequenceOutOfRange)?;
    let candidate_id: String = row.get(2)?;
    let turn_index =
        u32::try_from(row.get::<_, i64>(3)?).map_err(|_| StoreError::SequenceOutOfRange)?;
    let model_call_id: String = row.get(4)?;
    let context_digest: String = row.get(5)?;
    let proposal_digest: String = row.get(6)?;
    let summary_size_bytes =
        u64::try_from(row.get::<_, i64>(7)?).map_err(|_| StoreError::SequenceOutOfRange)?;
    let summary_digest: String = row.get(8)?;
    let record_digest: String = row.get(9)?;
    let record_json_size: i64 = row.get(10)?;
    if !(1..=20_000).contains(&record_json_size) {
        return Err(StoreError::Corrupt(
            "completion-output record bytes exceed the fixed limit".to_owned(),
        ));
    }
    let record_json: Vec<u8> = row.get(11)?;
    let record: CompletionOutputRecord = serde_json::from_slice(&record_json)
        .map_err(|_| StoreError::Corrupt("completion-output record JSON is invalid".to_owned()))?;
    if record.candidate_id() != candidate_id
        || record.turn_index() != turn_index
        || record.model_call_id() != model_call_id
        || record.context_digest() != context_digest
        || record.proposal_digest() != proposal_digest
        || record.summary_size_bytes() != summary_size_bytes
        || record.summary_digest() != summary_digest
        || record.record_digest() != record_digest
    {
        return Err(StoreError::Corrupt(
            "completion-output index differs from its record".to_owned(),
        ));
    }
    if rows.next()?.is_some() {
        return Err(StoreError::Corrupt(
            "completion-output rows outnumber completion events".to_owned(),
        ));
    }
    Ok(Some(StoredCompletionOutput {
        event_sequence,
        record,
    }))
}

fn load_execution_receipts(
    connection: &Connection,
) -> Result<Vec<StoredExecutionReceipt>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT receipt_id, event_sequence, effect_id, step_id, previous_receipt_digest, receipt_digest, receipt_json FROM execution_receipts ORDER BY event_sequence",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Vec<u8>>(6)?,
        ))
    })?;
    let mut receipts = Vec::new();
    for row in rows {
        let (
            receipt_id,
            event_sequence,
            effect_id,
            step_id,
            previous_receipt_digest,
            receipt_digest,
            receipt_json,
        ) = row?;
        let event_sequence =
            u64::try_from(event_sequence).map_err(|_| StoreError::SequenceOutOfRange)?;
        let document: ProtocolDocument = serde_json::from_slice(&receipt_json)?;
        let ProtocolDocument::ExecutionReceipt(receipt) = document else {
            return Err(StoreError::Corrupt(
                "receipt row does not contain an ExecutionReceipt document".to_owned(),
            ));
        };
        if receipt.receipt_id != receipt_id
            || receipt.step_id != step_id
            || receipt.previous_receipt_digest != previous_receipt_digest
            || receipt.receipt_digest != receipt_digest
        {
            return Err(StoreError::Corrupt(
                "execution receipt index differs from its document".to_owned(),
            ));
        }
        receipts.push(StoredExecutionReceipt {
            event_sequence,
            effect_id,
            receipt: *receipt,
        });
    }
    Ok(receipts)
}

#[cfg(test)]
fn table_count(connection: &Connection, table: &str) -> Result<u64, StoreError> {
    let query = match table {
        "run_events" => "SELECT COUNT(*) FROM run_events",
        "effect_intents" => "SELECT COUNT(*) FROM effect_intents",
        "authorization_consumption" => "SELECT COUNT(*) FROM authorization_consumption",
        "planned_invocations" => "SELECT COUNT(*) FROM planned_invocations",
        "invocation_materials" => "SELECT COUNT(*) FROM invocation_materials",
        "tool_outputs" => "SELECT COUNT(*) FROM tool_outputs",
        "completion_outputs" => "SELECT COUNT(*) FROM completion_outputs",
        "execution_receipts" => "SELECT COUNT(*) FROM execution_receipts",
        _ => {
            return Err(StoreError::Corrupt(format!(
                "unsupported internal count table `{table}`"
            )));
        }
    };
    let count: i64 = connection.query_row(query, [], |row| row.get(0))?;
    u64::try_from(count).map_err(|_| StoreError::SequenceOutOfRange)
}
