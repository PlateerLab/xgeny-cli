#![doc = "Capability-confined local filesystem adapters for `XGENy`."]

mod path;
mod read_text;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use cap_std::fs::Dir;
use thiserror::Error;
use xgeny_domain::InstanceBinding;
use xgeny_policy::{ResourceResolutionFailure, ResourceResolver};

pub use read_text::{
    MAX_READ_TEXT_BYTES, ReadTextAdapter, ReadTextLimits, ReadTextLimitsError, ReadTextVerifier,
};

/// Exact public Capability identifier implemented by this adapter.
pub const READ_TEXT_CAPABILITY_ID: &str = "xgeny.fs/read-text";
/// Exact immutable contract version implemented by this adapter.
pub const READ_TEXT_CONTRACT_VERSION: &str = "1.0.0";
/// Exact operation name used in a root-bound local Instance binding.
pub const READ_TEXT_OPERATION_REF: &str = "readText";
/// Resource scope accepted by the workspace resolver.
pub const FILESYSTEM_READ_SCOPE: &str = "filesystem.read";

const WORKSPACE_BINDING_PREFIX: &str = "builtin://core-os/filesystem/workspaces/";

/// Stable, non-secret identity assigned by the host to one workspace-root mapping.
///
/// The host must preserve the same identity-to-root mapping while durable Runs using it can be
/// resumed. The absolute root path is deliberately absent from invocation material and receipts.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Validate one portable workspace identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, punctuation-leading, or non-portable identifiers.
    pub fn new(value: impl Into<String>) -> Result<Self, WorkspaceIdError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
            })
        {
            return Err(WorkspaceIdError);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkspaceId(<redacted>)")
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("workspace identity is invalid")]
pub struct WorkspaceIdError;

/// A host-opened workspace directory capability and its durable logical identity.
#[derive(Clone)]
pub struct WorkspaceRoot {
    pub(crate) directory: Arc<Dir>,
    pub(crate) workspace_id: WorkspaceId,
}

impl WorkspaceRoot {
    /// Wrap a directory capability opened by the trusted composition root.
    #[must_use]
    pub fn from_dir(directory: Dir, workspace_id: WorkspaceId) -> Self {
        Self {
            directory: Arc::new(directory),
            workspace_id,
        }
    }

    /// Open a user-selected workspace using explicit ambient authority.
    ///
    /// This is the only API in this crate that accepts an ambient filesystem path. The path is not
    /// retained or exposed through `Debug` or errors. Product composition should call it once and
    /// pass the resulting capability to adapters.
    ///
    /// # Errors
    ///
    /// Returns a fixed error without echoing the root path or operating-system error.
    pub fn open_ambient(
        root: impl AsRef<Path>,
        workspace_id: WorkspaceId,
    ) -> Result<Self, WorkspaceRootError> {
        let directory = Dir::open_ambient_dir(root, cap_std::ambient_authority())
            .map_err(|_| WorkspaceRootError)?;
        Ok(Self::from_dir(directory, workspace_id))
    }

    /// Build the idempotent resolver that turns relative model paths into durable logical IDs.
    #[must_use]
    pub fn resolver(&self) -> WorkspaceResourceResolver {
        WorkspaceResourceResolver {
            workspace_id: self.workspace_id.clone(),
        }
    }

    /// Return the exact root-bound Instance binding required by this adapter and verifier.
    #[must_use]
    pub fn binding(&self) -> InstanceBinding {
        workspace_binding(&self.workspace_id)
    }

    /// Construct a read-text adapter pinned to this exact workspace capability and identity.
    #[must_use]
    pub fn read_text_adapter(&self, limits: ReadTextLimits) -> ReadTextAdapter {
        ReadTextAdapter::new(self.clone(), limits)
    }
}

impl fmt::Debug for WorkspaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceRoot")
            .field("directory", &"<preopened/redacted>")
            .field("workspace_id", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("workspace root is unavailable")]
pub struct WorkspaceRootError;

/// Side-effect-free, byte-exact logical resource resolver for one workspace.
#[derive(Clone)]
pub struct WorkspaceResourceResolver {
    workspace_id: WorkspaceId,
}

impl ResourceResolver for WorkspaceResourceResolver {
    fn resolve(&self, scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        if scope != FILESYSTEM_READ_SCOPE {
            return Err(ResourceResolutionFailure::UnsupportedScope);
        }
        path::canonical_resource(&self.workspace_id, resource).map_err(path::PathError::resolution)
    }
}

impl fmt::Debug for WorkspaceResourceResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceResourceResolver")
            .field("workspace_id", &"<redacted>")
            .finish()
    }
}

fn workspace_binding(workspace_id: &WorkspaceId) -> InstanceBinding {
    InstanceBinding {
        binding_ref: format!("{WORKSPACE_BINDING_PREFIX}{}", workspace_id.as_str()),
        operation_ref: Some(READ_TEXT_OPERATION_REF.to_owned()),
        protocol_version: None,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn workspace_identity_is_bound_into_resolver_and_instance_binding() {
        let directory = tempdir().expect("temporary workspace should exist");
        let root = WorkspaceRoot::open_ambient(
            directory.path(),
            WorkspaceId::new("primary-1").expect("workspace ID should validate"),
        )
        .expect("workspace should open");
        let resolver = root.resolver();
        let canonical = resolver
            .resolve(FILESYSTEM_READ_SCOPE, "README.md")
            .expect("relative path should resolve");

        assert_eq!(canonical, "workspace:primary-1/README.md");
        assert_eq!(
            resolver
                .resolve(FILESYSTEM_READ_SCOPE, &canonical)
                .expect("canonical path should resolve idempotently"),
            canonical
        );
        assert_eq!(
            root.binding(),
            InstanceBinding {
                binding_ref: "builtin://core-os/filesystem/workspaces/primary-1".to_owned(),
                operation_ref: Some(READ_TEXT_OPERATION_REF.to_owned()),
                protocol_version: None,
            }
        );
        assert_eq!(
            resolver.resolve("filesystem.write", "README.md"),
            Err(ResourceResolutionFailure::UnsupportedScope)
        );

        let another_root = WorkspaceRoot::open_ambient(
            directory.path(),
            WorkspaceId::new("secondary").expect("workspace ID should validate"),
        )
        .expect("same directory may have a distinct trusted logical mapping");
        assert_ne!(root.binding(), another_root.binding());
        assert_eq!(
            resolver.resolve(FILESYSTEM_READ_SCOPE, "workspace:secondary/README.md"),
            Err(ResourceResolutionFailure::OutsideHostBoundary)
        );
    }

    #[test]
    fn workspace_identity_and_root_errors_are_fixed_and_redacted() {
        for candidate in ["", "Upper", "-leading", "space value", "../escape"] {
            let error = WorkspaceId::new(candidate).expect_err("candidate should fail");
            let rendered = format!("{error} {error:?}");
            if !candidate.is_empty() {
                assert!(!rendered.contains(candidate));
            }
        }
        let error = WorkspaceRoot::open_ambient(
            "ABSOLUTE-ROOT-SENTINEL-THAT-DOES-NOT-EXIST",
            WorkspaceId::new("primary").expect("workspace ID should validate"),
        )
        .expect_err("missing root should fail");
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("ABSOLUTE-ROOT-SENTINEL"));
    }
}
