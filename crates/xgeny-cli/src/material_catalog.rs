use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_adapter_process::{
    PROCESS_EXECUTE_CAPABILITY_ID, PROCESS_EXECUTE_CONTRACT_VERSION, ProcessWorkspace,
};
use xgeny_domain::CapabilityRef;
use xgeny_runtime::{
    InvocationMaterialProvider, MaterialProviderFailure, PlanMaterializationRequest,
    PlanMaterializer, PlanMaterializerFailure,
};
use xgeny_workgraph::ReconstructableMaterialReference;

use crate::allow_path::WorkspaceReadAuthorization;

pub(crate) const WORKSPACE_READ_MATERIAL_PROVIDER_ID: &str = "xgeny.cli.workspace-read-material.v1";
pub(crate) const WORKSPACE_READ_MATERIAL_CATALOG_SCHEMA_VERSION: i64 = 1;
pub(crate) const WORKSPACE_READ_RECIPE_FORMAT_VERSION: u32 = 1;
pub(crate) const WORKSPACE_READ_RECIPE_DOMAIN: &str = "xgeny.cli.workspace-read-recipe/v1";
pub(crate) const PROCESS_MATERIAL_PROVIDER_ID: &str = "xgeny.cli.process-material.v1";
pub(crate) const PROCESS_RECIPE_FORMAT_VERSION: u32 = 1;
pub(crate) const PROCESS_RECIPE_DOMAIN: &str = "xgeny.cli.process-recipe/v1";
pub(crate) const MAX_RECIPE_BYTES: usize = 512 * 1024;

const WORKSPACE_RECIPE_PROFILE: RecipeProfile = RecipeProfile {
    domain: WORKSPACE_READ_RECIPE_DOMAIN,
    format_version: WORKSPACE_READ_RECIPE_FORMAT_VERSION,
    provider_id: WORKSPACE_READ_MATERIAL_PROVIDER_ID,
};
const PROCESS_RECIPE_PROFILE: RecipeProfile = RecipeProfile {
    domain: PROCESS_RECIPE_DOMAIN,
    format_version: PROCESS_RECIPE_FORMAT_VERSION,
    provider_id: PROCESS_MATERIAL_PROVIDER_ID,
};

pub(crate) struct RunMaterialCatalog {
    connection: Connection,
    run_id: String,
}

impl RunMaterialCatalog {
    pub(crate) fn create(path: &Path, run_id: &str) -> Result<(), RunMaterialCatalogError> {
        validate_run_id(run_id)?;
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        drop(
            options
                .open(path)
                .map_err(|_| RunMaterialCatalogError::Unavailable)?,
        );
        let connection = open_connection(path)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = DELETE;
                 PRAGMA synchronous = FULL;
                 PRAGMA trusted_schema = OFF;
                 CREATE TABLE material_recipe (
                   reference_id TEXT PRIMARY KEY NOT NULL,
                   revision TEXT NOT NULL,
                   record BLOB NOT NULL
                 ) STRICT;
                 PRAGMA user_version = 1;",
            )
            .map_err(|_| RunMaterialCatalogError::Unavailable)?;
        Ok(())
    }

    pub(crate) fn open_existing(
        path: &Path,
        run_id: &str,
    ) -> Result<Self, RunMaterialCatalogError> {
        validate_run_id(run_id)?;
        validate_existing_file(path)?;
        let connection = open_connection(path)?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| RunMaterialCatalogError::Integrity)?;
        if version != WORKSPACE_READ_MATERIAL_CATALOG_SCHEMA_VERSION {
            return Err(RunMaterialCatalogError::Integrity);
        }
        let table_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'table' AND name = 'material_recipe'",
                [],
                |row| row.get(0),
            )
            .map_err(|_| RunMaterialCatalogError::Integrity)?;
        if table_count != 1 {
            return Err(RunMaterialCatalogError::Integrity);
        }
        Ok(Self {
            connection,
            run_id: run_id.to_owned(),
        })
    }

    fn persist_request(
        &mut self,
        request: &PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        self.persist_request_with_profile(request, WORKSPACE_RECIPE_PROFILE)
    }

    fn persist_process_request(
        &mut self,
        request: &PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        self.persist_request_with_profile(request, PROCESS_RECIPE_PROFILE)
    }

    #[cfg(test)]
    fn persist_record(
        &mut self,
        record: &RecipeRecord,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        self.persist_record_with_profile(record, WORKSPACE_RECIPE_PROFILE)
    }

    fn persist_request_with_profile(
        &mut self,
        request: &PlanMaterializationRequest<'_>,
        profile: RecipeProfile,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        if request.run_id() != self.run_id {
            return Err(PlanMaterializerFailure::Rejected);
        }
        let record = RecipeRecord {
            domain: profile.domain.to_owned(),
            format_version: profile.format_version,
            run_id: request.run_id().to_owned(),
            step_id: request.step_id().to_owned(),
            proposal_digest: request.proposal_digest().to_owned(),
            capability: request.capability().clone(),
            material_digest: request.material_digest().to_owned(),
            arguments: request.normalized_arguments().clone(),
        };
        self.persist_record_with_profile(&record, profile)
    }

    fn persist_record_with_profile(
        &mut self,
        record: &RecipeRecord,
        profile: RecipeProfile,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        if record.run_id != self.run_id || !record_shape_valid(record, profile) {
            return Err(PlanMaterializerFailure::Rejected);
        }
        let canonical =
            serde_jcs::to_vec(record).map_err(|_| PlanMaterializerFailure::PersistenceFailed)?;
        if canonical.len() > MAX_RECIPE_BYTES {
            return Err(PlanMaterializerFailure::Rejected);
        }
        let digest = sha256_hex(&canonical);
        let reference_id = format!("recipe-{digest}");
        let revision = format!("sha256-{digest}");
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| PlanMaterializerFailure::PersistenceFailed)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO material_recipe(reference_id, revision, record)
                 VALUES (?1, ?2, ?3)",
                params![reference_id, revision, canonical],
            )
            .map_err(|_| PlanMaterializerFailure::PersistenceFailed)?;
        let existing: (String, Vec<u8>) = transaction
            .query_row(
                "SELECT revision, record FROM material_recipe WHERE reference_id = ?1",
                params![reference_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| PlanMaterializerFailure::PersistenceFailed)?;
        if existing.0 != revision || existing.1 != canonical {
            return Err(PlanMaterializerFailure::PersistenceFailed);
        }
        transaction
            .commit()
            .map_err(|_| PlanMaterializerFailure::PersistenceFailed)?;
        ReconstructableMaterialReference::new(profile.provider_id, reference_id, revision)
            .map_err(|_| PlanMaterializerFailure::PersistenceFailed)
    }

    fn reconstruct_record(
        &self,
        reference_id: &str,
        revision: &str,
    ) -> Result<RecipeRecord, MaterialProviderFailure> {
        self.reconstruct_record_with_profile(reference_id, revision, WORKSPACE_RECIPE_PROFILE)
    }

    fn reconstruct_process_record(
        &self,
        reference_id: &str,
        revision: &str,
    ) -> Result<RecipeRecord, MaterialProviderFailure> {
        self.reconstruct_record_with_profile(reference_id, revision, PROCESS_RECIPE_PROFILE)
    }

    fn reconstruct_record_with_profile(
        &self,
        reference_id: &str,
        revision: &str,
        profile: RecipeProfile,
    ) -> Result<RecipeRecord, MaterialProviderFailure> {
        let stored: Option<(String, Vec<u8>)> = self
            .connection
            .query_row(
                "SELECT revision, record FROM material_recipe WHERE reference_id = ?1",
                params![reference_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| MaterialProviderFailure::Unavailable)?;
        let (stored_revision, bytes) = stored.ok_or(MaterialProviderFailure::NotFound)?;
        if stored_revision != revision {
            return Err(MaterialProviderFailure::RevisionChanged);
        }
        if bytes.len() > MAX_RECIPE_BYTES {
            return Err(MaterialProviderFailure::RevisionChanged);
        }
        let record: RecipeRecord =
            serde_json::from_slice(&bytes).map_err(|_| MaterialProviderFailure::RevisionChanged)?;
        let canonical =
            serde_jcs::to_vec(&record).map_err(|_| MaterialProviderFailure::RevisionChanged)?;
        let digest = sha256_hex(&canonical);
        if canonical != bytes
            || reference_id != format!("recipe-{digest}")
            || revision != format!("sha256-{digest}")
            || record.run_id != self.run_id
            || !record_shape_valid(&record, profile)
        {
            return Err(MaterialProviderFailure::RevisionChanged);
        }
        Ok(record)
    }
}

#[derive(Clone, Copy)]
struct RecipeProfile {
    domain: &'static str,
    format_version: u32,
    provider_id: &'static str,
}

impl fmt::Debug for RunMaterialCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunMaterialCatalog")
            .field("connection", &"<private/redacted>")
            .field("run_id", &self.run_id)
            .finish()
    }
}

pub(crate) struct WorkspaceReadMaterializer {
    authorization: WorkspaceReadAuthorization,
    catalog: RunMaterialCatalog,
}

impl WorkspaceReadMaterializer {
    pub(crate) fn new(
        authorization: WorkspaceReadAuthorization,
        catalog: RunMaterialCatalog,
    ) -> Self {
        Self {
            authorization,
            catalog,
        }
    }
}

impl PlanMaterializer for WorkspaceReadMaterializer {
    fn materialize(
        &mut self,
        request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        if !self
            .authorization
            .authorizes_material(request.capability(), request.normalized_arguments())
        {
            return Err(PlanMaterializerFailure::Rejected);
        }
        self.catalog.persist_request(&request)
    }
}

pub(crate) struct WorkspaceReadMaterialProvider {
    authorization: WorkspaceReadAuthorization,
    catalog: RunMaterialCatalog,
}

impl WorkspaceReadMaterialProvider {
    pub(crate) fn new(
        authorization: WorkspaceReadAuthorization,
        catalog: RunMaterialCatalog,
    ) -> Self {
        Self {
            authorization,
            catalog,
        }
    }
}

impl InvocationMaterialProvider for WorkspaceReadMaterialProvider {
    fn reconstruct(
        &mut self,
        reference_id: &str,
        revision: &str,
    ) -> Result<Value, MaterialProviderFailure> {
        let record = self.catalog.reconstruct_record(reference_id, revision)?;
        if !self
            .authorization
            .authorizes_material(&record.capability, &record.arguments)
        {
            return Err(MaterialProviderFailure::RevisionChanged);
        }
        Ok(record.arguments)
    }
}

pub(crate) struct ProcessMaterializer {
    workspace: ProcessWorkspace,
    catalog: RunMaterialCatalog,
}

impl ProcessMaterializer {
    pub(crate) const fn new(workspace: ProcessWorkspace, catalog: RunMaterialCatalog) -> Self {
        Self { workspace, catalog }
    }
}

impl PlanMaterializer for ProcessMaterializer {
    fn materialize(
        &mut self,
        request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        if request.capability().capability_id != PROCESS_EXECUTE_CAPABILITY_ID
            || request.capability().contract_version != PROCESS_EXECUTE_CONTRACT_VERSION
            || !self
                .workspace
                .accepts_normalized_material(request.normalized_arguments())
        {
            return Err(PlanMaterializerFailure::Rejected);
        }
        self.catalog.persist_process_request(&request)
    }
}

pub(crate) struct ProcessMaterialProvider {
    workspace: ProcessWorkspace,
    catalog: RunMaterialCatalog,
}

impl ProcessMaterialProvider {
    pub(crate) const fn new(workspace: ProcessWorkspace, catalog: RunMaterialCatalog) -> Self {
        Self { workspace, catalog }
    }
}

impl InvocationMaterialProvider for ProcessMaterialProvider {
    fn reconstruct(
        &mut self,
        reference_id: &str,
        revision: &str,
    ) -> Result<Value, MaterialProviderFailure> {
        let record = self
            .catalog
            .reconstruct_process_record(reference_id, revision)?;
        if record.capability.capability_id != PROCESS_EXECUTE_CAPABILITY_ID
            || record.capability.contract_version != PROCESS_EXECUTE_CONTRACT_VERSION
            || !self
                .workspace
                .accepts_normalized_material(&record.arguments)
        {
            return Err(MaterialProviderFailure::RevisionChanged);
        }
        Ok(record.arguments)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunMaterialCatalogError {
    #[error("Run material catalog is unavailable")]
    Unavailable,
    #[error("Run material catalog failed integrity validation")]
    Integrity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecipeRecord {
    domain: String,
    format_version: u32,
    run_id: String,
    step_id: String,
    proposal_digest: String,
    capability: CapabilityRef,
    material_digest: String,
    arguments: Value,
}

fn record_shape_valid(record: &RecipeRecord, profile: RecipeProfile) -> bool {
    record.domain == profile.domain
        && record.format_version == profile.format_version
        && valid_run_id(&record.run_id)
        && valid_identifier(&record.step_id, 256)
        && valid_digest(&record.proposal_digest)
        && valid_digest(&record.material_digest)
        && valid_capability_id(&record.capability.capability_id)
        && valid_identifier(&record.capability.contract_version, 64)
}

fn validate_run_id(run_id: &str) -> Result<(), RunMaterialCatalogError> {
    if valid_run_id(run_id) {
        Ok(())
    } else {
        Err(RunMaterialCatalogError::Unavailable)
    }
}

fn valid_run_id(value: &str) -> bool {
    crate::manifest::valid_run_id(value)
}

fn valid_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn valid_capability_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn open_connection(path: &Path) -> Result<Connection, RunMaterialCatalogError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| RunMaterialCatalogError::Unavailable)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| RunMaterialCatalogError::Unavailable)?;
    connection
        .execute_batch("PRAGMA trusted_schema = OFF;")
        .map_err(|_| RunMaterialCatalogError::Unavailable)?;
    Ok(connection)
}

fn validate_existing_file(path: &Path) -> Result<(), RunMaterialCatalogError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RunMaterialCatalogError::Unavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RunMaterialCatalogError::Unavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(RunMaterialCatalogError::Unavailable);
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    const RUN_ID: &str = "run-0123456789abcdef0123456789abcdef";
    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn record(arguments: Value) -> RecipeRecord {
        RecipeRecord {
            domain: WORKSPACE_READ_RECIPE_DOMAIN.to_owned(),
            format_version: WORKSPACE_READ_RECIPE_FORMAT_VERSION,
            run_id: RUN_ID.to_owned(),
            step_id: "step-1".to_owned(),
            proposal_digest: DIGEST.to_owned(),
            capability: CapabilityRef {
                capability_id: "xgeny.fs/search-text".to_owned(),
                contract_version: "1.0.0".to_owned(),
            },
            material_digest: DIGEST.to_owned(),
            arguments,
        }
    }

    fn process_record(arguments: Value) -> RecipeRecord {
        RecipeRecord {
            domain: PROCESS_RECIPE_DOMAIN.to_owned(),
            format_version: PROCESS_RECIPE_FORMAT_VERSION,
            run_id: RUN_ID.to_owned(),
            step_id: "step-process-1".to_owned(),
            proposal_digest: DIGEST.to_owned(),
            capability: CapabilityRef {
                capability_id: PROCESS_EXECUTE_CAPABILITY_ID.to_owned(),
                contract_version: PROCESS_EXECUTE_CONTRACT_VERSION.to_owned(),
            },
            material_digest: DIGEST.to_owned(),
            arguments,
        }
    }

    #[test]
    fn recipe_reconstructs_after_reopen_without_raw_reference_data() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("materials.sqlite3");
        RunMaterialCatalog::create(&path, RUN_ID).unwrap();
        let mut catalog = RunMaterialCatalog::open_existing(&path, RUN_ID).unwrap();
        assert_eq!(
            catalog
                .connection
                .query_row("PRAGMA trusted_schema", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let input = record(serde_json::json!({
            "path": "workspace:primary/src",
            "query": "WorkspaceRoot"
        }));
        let reference = catalog.persist_record(&input).unwrap();
        assert!(!reference.reference_id().contains("WorkspaceRoot"));
        assert!(!reference.reference_id().contains("src"));
        drop(catalog);

        let reopened = RunMaterialCatalog::open_existing(&path, RUN_ID).unwrap();
        assert_eq!(
            reopened
                .reconstruct_record(reference.reference_id(), reference.revision())
                .unwrap(),
            input
        );
    }

    #[test]
    fn revision_or_stored_record_tampering_fails_closed() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("materials.sqlite3");
        RunMaterialCatalog::create(&path, RUN_ID).unwrap();
        let mut catalog = RunMaterialCatalog::open_existing(&path, RUN_ID).unwrap();
        let reference = catalog
            .persist_record(&record(serde_json::json!({
                "path": "workspace:primary",
                "query": "needle"
            })))
            .unwrap();
        assert_eq!(
            catalog
                .reconstruct_record(reference.reference_id(), "sha256-wrong")
                .unwrap_err(),
            MaterialProviderFailure::RevisionChanged
        );
        catalog
            .connection
            .execute(
                "UPDATE material_recipe SET record = ?1 WHERE reference_id = ?2",
                params![b"{}".as_slice(), reference.reference_id()],
            )
            .unwrap();
        assert_eq!(
            catalog
                .reconstruct_record(reference.reference_id(), reference.revision())
                .unwrap_err(),
            MaterialProviderFailure::RevisionChanged
        );
    }

    #[test]
    fn process_recipes_are_durable_and_domain_separated() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("materials.sqlite3");
        RunMaterialCatalog::create(&path, RUN_ID).unwrap();
        let mut catalog = RunMaterialCatalog::open_existing(&path, RUN_ID).unwrap();
        let input = process_record(serde_json::json!({
            "executable": "process:primary/executables/cargo",
            "args": ["test", "--workspace"],
            "cwd": ".",
            "env": {},
            "timeoutMs": 600_000,
            "maxOutputBytes": 32768
        }));
        let reference = catalog
            .persist_record_with_profile(&input, PROCESS_RECIPE_PROFILE)
            .unwrap();
        assert!(!reference.reference_id().contains("cargo"));
        drop(catalog);

        let reopened = RunMaterialCatalog::open_existing(&path, RUN_ID).unwrap();
        assert_eq!(
            reopened
                .reconstruct_process_record(reference.reference_id(), reference.revision())
                .unwrap(),
            input
        );
        assert_eq!(
            reopened
                .reconstruct_record(reference.reference_id(), reference.revision())
                .unwrap_err(),
            MaterialProviderFailure::RevisionChanged
        );
    }

    #[cfg(unix)]
    #[test]
    fn catalog_file_is_private_and_symlink_reopen_is_rejected() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempdir().unwrap();
        let path = directory.path().join("materials.sqlite3");
        RunMaterialCatalog::create(&path, RUN_ID).unwrap();
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = directory.path().join("linked.sqlite3");
        symlink(&path, &link).unwrap();
        assert_eq!(
            RunMaterialCatalog::open_existing(&link, RUN_ID).unwrap_err(),
            RunMaterialCatalogError::Unavailable
        );
    }

    #[test]
    fn debug_does_not_expose_catalog_path_or_arguments() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("RAW-MATERIAL-SENTINEL.sqlite3");
        RunMaterialCatalog::create(&path, RUN_ID).unwrap();
        let catalog = RunMaterialCatalog::open_existing(&path, RUN_ID).unwrap();
        let debug = format!("{catalog:?}");
        assert!(!debug.contains("RAW-MATERIAL-SENTINEL"));
    }
}
