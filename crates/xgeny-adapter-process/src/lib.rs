#![doc = "Shell-free bounded local process execution for `XGENy`."]

mod catalog;
mod execution;
mod path;
mod verifier;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use cap_std::fs::Dir;
use thiserror::Error;
use xgeny_domain::InstanceBinding;
use xgeny_runtime::{
    AdapterPrepareFailure, AdapterPrepareRequest, AdapterReconcileRequest,
    AdapterReconciliationInconclusiveReason, AdapterReconciliationObservation, EffectAdapter,
    PreparedAdapterInvocation,
};

pub use catalog::{
    ExecutableCatalog, ExecutableCatalogError, ProcessEnvironment, ProcessEnvironmentError,
    ProcessResourceResolver,
};
pub use execution::{MAX_CAPTURE_BYTES, MAX_PROCESS_TIMEOUT_MS, MIN_CAPTURE_BYTES};
pub use verifier::ProcessExecuteVerifier;

use crate::execution::parse_prepared;

/// Exact public Capability identifier implemented by this adapter.
pub const PROCESS_EXECUTE_CAPABILITY_ID: &str = "xgeny.process/execute";
/// Exact immutable contract version implemented by this adapter.
pub const PROCESS_EXECUTE_CONTRACT_VERSION: &str = "1.0.0";
/// Exact operation name used in a workspace/catalog-bound local Instance.
pub const PROCESS_EXECUTE_OPERATION_REF: &str = "execute";
/// Permission scope resolved for an explicitly catalogued executable.
pub const PROCESS_EXECUTE_SCOPE: &str = "process.execute";

const PROCESS_BINDING_PREFIX: &str = "builtin://core-os/process/workspaces/";

/// Stable non-secret identity assigned by the host to one process workspace mapping.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessWorkspaceId(String);

impl ProcessWorkspaceId {
    /// Validate one portable workspace identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, punctuation-leading, or non-portable identifiers.
    pub fn new(value: impl Into<String>) -> Result<Self, ProcessWorkspaceIdError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
            })
        {
            return Err(ProcessWorkspaceIdError);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProcessWorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProcessWorkspaceId(<redacted>)")
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("process workspace identity is invalid")]
pub struct ProcessWorkspaceIdError;

/// Host-owned workspace, executable catalog, and inherited environment snapshot.
///
/// The model can select only a logical executable identifier already present in `catalog`. The
/// adapter never receives an ambient executable path from invocation material.
#[derive(Clone)]
pub struct ProcessWorkspace {
    pub(crate) directory: Arc<Dir>,
    pub(crate) ambient_root: Arc<std::path::PathBuf>,
    pub(crate) workspace_id: ProcessWorkspaceId,
    pub(crate) catalog: ExecutableCatalog,
    pub(crate) environment: ProcessEnvironment,
    binding: InstanceBinding,
}

impl ProcessWorkspace {
    /// Open a user-selected workspace and bind it to an explicit executable/environment snapshot.
    ///
    /// This trusted composition-root API is the only ambient path entrypoint in the crate. The
    /// canonical path is retained only for `Command::current_dir` and never appears in `Debug`,
    /// durable material, receipts, or fixed errors.
    ///
    /// # Errors
    ///
    /// Returns a fixed failure if the root is unavailable, non-directory, or cannot be represented
    /// by the confined directory capability.
    pub fn open_ambient(
        root: impl AsRef<Path>,
        workspace_id: ProcessWorkspaceId,
        catalog: ExecutableCatalog,
        environment: ProcessEnvironment,
    ) -> Result<Self, ProcessWorkspaceError> {
        let ambient_root = std::fs::canonicalize(root).map_err(|_| ProcessWorkspaceError)?;
        if !std::fs::metadata(&ambient_root)
            .map_err(|_| ProcessWorkspaceError)?
            .is_dir()
        {
            return Err(ProcessWorkspaceError);
        }
        let directory = Dir::open_ambient_dir(&ambient_root, cap_std::ambient_authority())
            .map_err(|_| ProcessWorkspaceError)?;
        let root_digest =
            path::physical_directory_digest(&directory).map_err(|()| ProcessWorkspaceError)?;
        let binding = catalog.binding(&workspace_id, &environment, &root_digest);
        Ok(Self {
            directory: Arc::new(directory),
            ambient_root: Arc::new(ambient_root),
            workspace_id,
            catalog,
            environment,
            binding,
        })
    }

    #[must_use]
    pub fn resolver(&self) -> ProcessResourceResolver {
        ProcessResourceResolver::new(self.workspace_id.clone(), self.catalog.logical_ids())
    }

    #[must_use]
    pub fn binding(&self) -> InstanceBinding {
        self.binding.clone()
    }

    #[must_use]
    pub fn adapter(&self) -> ProcessExecuteAdapter {
        ProcessExecuteAdapter {
            workspace: self.clone(),
        }
    }
}

impl fmt::Debug for ProcessWorkspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessWorkspace")
            .field("directory", &"<preopened/redacted>")
            .field("ambient_root", &"<redacted>")
            .field("workspace_id", &"<redacted>")
            .field("catalog", &self.catalog)
            .field("environment", &self.environment)
            .field("binding", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("process workspace is unavailable")]
pub struct ProcessWorkspaceError;

/// Exact shell-free process adapter pinned to one host-owned workspace/catalog snapshot.
#[derive(Clone)]
pub struct ProcessExecuteAdapter {
    workspace: ProcessWorkspace,
}

impl ProcessExecuteAdapter {
    #[must_use]
    pub fn verifier(&self) -> ProcessExecuteVerifier {
        ProcessExecuteVerifier::new(self.workspace.binding())
    }
}

impl fmt::Debug for ProcessExecuteAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessExecuteAdapter")
            .field("workspace", &"<preopened/redacted>")
            .finish()
    }
}

impl EffectAdapter for ProcessExecuteAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        verify_adapter_contract(&request, &self.workspace.binding())?;
        parse_prepared(request.normalized_arguments(), &self.workspace)
    }

    fn reconcile(
        &mut self,
        _request: AdapterReconcileRequest<'_>,
    ) -> AdapterReconciliationObservation {
        AdapterReconciliationObservation::Inconclusive {
            reason: AdapterReconciliationInconclusiveReason::StableKeyUnsupported,
        }
    }
}

fn verify_adapter_contract(
    request: &AdapterPrepareRequest<'_>,
    expected_binding: &InstanceBinding,
) -> Result<(), AdapterPrepareFailure> {
    let intent = request.intent();
    let instance = request.instance();
    if intent.invocation.capability_id != PROCESS_EXECUTE_CAPABILITY_ID
        || intent.invocation.contract_version != PROCESS_EXECUTE_CONTRACT_VERSION
        || instance.definition.capability_id != PROCESS_EXECUTE_CAPABILITY_ID
        || instance.definition.contract_version != PROCESS_EXECUTE_CONTRACT_VERSION
        || intent.invocation.instance_id != instance.instance_id
        || instance.binding != *expected_binding
        || !execution::supports_instance_features(&instance.features)
        || intent.effect_class != xgeny_workgraph::EffectClass::NonIdempotent
        || intent.idempotency_key.as_deref().is_none_or(str::is_empty)
    {
        return Err(AdapterPrepareFailure::UnsupportedProtocol);
    }
    Ok(())
}

fn process_binding(
    workspace_id: &ProcessWorkspaceId,
    root_digest: &str,
    catalog_digest: &str,
    environment_digest: &str,
) -> InstanceBinding {
    let binding_digest = execution::sha256_digest(
        format!("xgeny.process/binding/v1/{root_digest}/{catalog_digest}/{environment_digest}")
            .as_bytes(),
    );
    let digest = binding_digest
        .strip_prefix("sha256:")
        .expect("the internal digest is canonical");
    InstanceBinding {
        binding_ref: format!(
            "{PROCESS_BINDING_PREFIX}{}/snapshots/{digest}",
            workspace_id.as_str()
        ),
        operation_ref: Some(PROCESS_EXECUTE_OPERATION_REF.to_owned()),
        protocol_version: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn workspace_debug_and_binding_never_expose_ambient_values() {
        let directory = tempdir().expect("temporary workspace should exist");
        let executable = std::env::current_exe().expect("test executable should resolve");
        let catalog = ExecutableCatalog::from_paths([("test-helper", executable)]).unwrap();
        let environment = ProcessEnvironment::new(BTreeMap::from([(
            "XGENY_PROCESS_SECRET_SENTINEL".to_owned(),
            "ENVIRONMENT-CONTENT-SENTINEL".to_owned(),
        )]))
        .unwrap();
        let workspace = ProcessWorkspace::open_ambient(
            directory.path(),
            ProcessWorkspaceId::new("fixture").unwrap(),
            catalog,
            environment,
        )
        .unwrap();

        let debug = format!("{workspace:?}");
        let binding = workspace.binding();
        for forbidden in [
            directory.path().to_string_lossy().as_ref(),
            "ENVIRONMENT-CONTENT-SENTINEL",
            "XGENY_PROCESS_SECRET_SENTINEL",
        ] {
            assert!(!debug.contains(forbidden));
            assert!(!binding.binding_ref.contains(forbidden));
        }
    }
}
