#![doc = "Capability-confined local filesystem adapters for `XGENy`."]

mod path;
mod query;
mod read_text;
mod root_identity;
mod write_atomic;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use cap_std::fs::Dir;
use thiserror::Error;
use xgeny_domain::InstanceBinding;
use xgeny_policy::{ResourceResolutionFailure, ResourceResolver};

pub use query::{
    FilesystemQueryAdapter, FilesystemQueryVerifier, MAX_DIRECTORY_SCAN_ENTRIES,
    MAX_LIST_DIRECTORY_ENTRIES, MAX_QUERY_OUTPUT_CANONICAL_BYTES, MAX_SEARCH_FILE_BYTES,
    MAX_SEARCH_MATCHES, MAX_SEARCH_PREVIEW_BYTES, MAX_SEARCH_QUERY_BYTES,
    MAX_SEARCH_QUERY_UNICODE_SCALARS, MAX_SEARCH_TOTAL_BYTES, MAX_SEARCH_VISITED_ENTRIES,
};
pub use read_text::{
    MAX_READ_TEXT_BYTES, ReadTextAdapter, ReadTextLimits, ReadTextLimitsError, ReadTextVerifier,
};
pub use root_identity::{WorkspaceRootIdentity, WorkspaceRootIdentityError};
pub use write_atomic::{MAX_WRITE_ATOMIC_BYTES, WriteAtomicAdapter, WriteAtomicVerifier};

/// Exact public Capability identifier implemented by this adapter.
pub const READ_TEXT_CAPABILITY_ID: &str = "xgeny.fs/read-text";
/// Exact immutable contract version implemented by this adapter.
pub const READ_TEXT_CONTRACT_VERSION: &str = "1.0.0";
/// Exact operation name used in a root-bound local Instance binding.
pub const READ_TEXT_OPERATION_REF: &str = "readText";
/// Exact public Capability identifier for one-level directory observation.
pub const LIST_DIRECTORY_CAPABILITY_ID: &str = "xgeny.fs/list-directory";
/// Exact immutable list-directory contract version.
pub const LIST_DIRECTORY_CONTRACT_VERSION: &str = "1.0.0";
/// Exact operation name used in the root-bound list Instance.
pub const LIST_DIRECTORY_OPERATION_REF: &str = "listDirectory";
/// Exact public Capability identifier for path metadata observation.
pub const STAT_CAPABILITY_ID: &str = "xgeny.fs/stat";
/// Exact immutable stat contract version.
pub const STAT_CONTRACT_VERSION: &str = "1.0.0";
/// Exact operation name used in the root-bound stat Instance.
pub const STAT_OPERATION_REF: &str = "stat";
/// Exact public Capability identifier for recursive literal text search.
pub const SEARCH_TEXT_CAPABILITY_ID: &str = "xgeny.fs/search-text";
/// Exact immutable search-text contract version.
pub const SEARCH_TEXT_CONTRACT_VERSION: &str = "1.0.0";
/// Exact operation name used in the root-bound search Instance.
pub const SEARCH_TEXT_OPERATION_REF: &str = "searchText";
/// Exact public Capability identifier for one atomic UTF-8 file replacement.
pub const WRITE_ATOMIC_CAPABILITY_ID: &str = "xgeny.fs/write-atomic";
/// Exact immutable write-atomic contract version.
pub const WRITE_ATOMIC_CONTRACT_VERSION: &str = "1.0.0";
/// Exact operation name used in the root-bound write Instance.
pub const WRITE_ATOMIC_OPERATION_REF: &str = "writeAtomic";
/// Resource scope accepted by the workspace resolver.
pub const FILESYSTEM_READ_SCOPE: &str = "filesystem.read";
/// Resource scope accepted for explicit workspace mutations.
pub const FILESYSTEM_WRITE_SCOPE: &str = "filesystem.write";

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
        workspace_binding(&self.workspace_id, READ_TEXT_OPERATION_REF)
    }

    /// Commit the physical directory identity obtained from this exact preopened capability.
    ///
    /// Linux and macOS commit the handle's device and inode identifiers. Windows commits its
    /// volume serial number and file index. The returned value contains only a domain-separated
    /// SHA-256 digest; neither the ambient path nor raw operating-system identifiers are exposed.
    ///
    /// # Errors
    ///
    /// Returns a fixed error when handle metadata or a required platform identifier is unavailable,
    /// or when the retained handle is not a directory. Unsupported operating systems fail closed.
    pub fn physical_identity(&self) -> Result<WorkspaceRootIdentity, WorkspaceRootIdentityError> {
        root_identity::physical_identity(&self.directory)
    }

    /// Construct a read-text adapter pinned to this exact workspace capability and identity.
    #[must_use]
    pub fn read_text_adapter(&self, limits: ReadTextLimits) -> ReadTextAdapter {
        ReadTextAdapter::new(self.clone(), limits)
    }

    /// Return the exact root-bound binding for the list-directory adapter.
    #[must_use]
    pub fn list_directory_binding(&self) -> InstanceBinding {
        workspace_binding(&self.workspace_id, LIST_DIRECTORY_OPERATION_REF)
    }

    /// Return the exact root-bound binding for the stat adapter.
    #[must_use]
    pub fn stat_binding(&self) -> InstanceBinding {
        workspace_binding(&self.workspace_id, STAT_OPERATION_REF)
    }

    /// Return the exact root-bound binding for the search-text adapter.
    #[must_use]
    pub fn search_text_binding(&self) -> InstanceBinding {
        workspace_binding(&self.workspace_id, SEARCH_TEXT_OPERATION_REF)
    }

    /// Return the exact root-bound binding for atomic writes.
    #[must_use]
    pub fn write_atomic_binding(&self) -> InstanceBinding {
        workspace_binding(&self.workspace_id, WRITE_ATOMIC_OPERATION_REF)
    }

    /// Construct the bounded one-level directory adapter.
    #[must_use]
    pub fn list_directory_adapter(&self) -> FilesystemQueryAdapter {
        FilesystemQueryAdapter::list_directory(self.clone())
    }

    /// Construct the bounded path metadata adapter.
    #[must_use]
    pub fn stat_adapter(&self) -> FilesystemQueryAdapter {
        FilesystemQueryAdapter::stat(self.clone())
    }

    /// Construct the bounded recursive literal text search adapter.
    #[must_use]
    pub fn search_text_adapter(&self) -> FilesystemQueryAdapter {
        FilesystemQueryAdapter::search_text(self.clone())
    }

    /// Construct the bounded atomic UTF-8 write adapter.
    #[must_use]
    pub fn write_atomic_adapter(&self) -> WriteAtomicAdapter {
        WriteAtomicAdapter::new(self.clone())
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
        if !matches!(scope, FILESYSTEM_READ_SCOPE | FILESYSTEM_WRITE_SCOPE) {
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

fn workspace_binding(workspace_id: &WorkspaceId, operation_ref: &str) -> InstanceBinding {
    InstanceBinding {
        binding_ref: format!("{WORKSPACE_BINDING_PREFIX}{}", workspace_id.as_str()),
        operation_ref: Some(operation_ref.to_owned()),
        protocol_version: None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

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
            resolver
                .resolve(FILESYSTEM_WRITE_SCOPE, "README.md")
                .expect("write scope uses the same confined path grammar"),
            canonical
        );
        assert_eq!(
            resolver.resolve("filesystem.execute", "README.md"),
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

    #[test]
    fn physical_root_identity_is_stable_for_the_same_preopened_directory() {
        let directory = tempdir().expect("temporary workspace should exist");
        let first = WorkspaceRoot::open_ambient(
            directory.path(),
            WorkspaceId::new("first-logical-id").expect("workspace ID should validate"),
        )
        .expect("workspace should open");
        let first_observation = first
            .physical_identity()
            .expect("physical identity should be available");
        let repeated_observation = first
            .physical_identity()
            .expect("same handle should retain its identity");
        let second = WorkspaceRoot::open_ambient(
            directory.path(),
            WorkspaceId::new("second-logical-id").expect("workspace ID should validate"),
        )
        .expect("same workspace should open again");
        let second_observation = second
            .physical_identity()
            .expect("second handle should identify the same directory");

        assert_eq!(first_observation, repeated_observation);
        assert_eq!(first_observation, second_observation);
        assert!(first_observation.as_str().starts_with("sha256:"));
        assert_eq!(first_observation.as_str().len(), 71);
        assert!(
            first_observation
                .as_str()
                .strip_prefix("sha256:")
                .expect("identity should use the SHA-256 profile")
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_eq!(
            format!("{first_observation:?}"),
            "WorkspaceRootIdentity(<redacted>)"
        );
        assert!(
            !format!("{first_observation:?}").contains(directory.path().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn different_physical_roots_have_different_identities() {
        let first_directory = tempdir().expect("first workspace should exist");
        let second_directory = tempdir().expect("second workspace should exist");
        let workspace_id =
            WorkspaceId::new("shared-logical-id").expect("workspace ID should validate");
        let first = WorkspaceRoot::open_ambient(first_directory.path(), workspace_id.clone())
            .expect("first workspace should open");
        let second = WorkspaceRoot::open_ambient(second_directory.path(), workspace_id)
            .expect("second workspace should open");

        assert_ne!(
            first
                .physical_identity()
                .expect("first identity should be available"),
            second
                .physical_identity()
                .expect("second identity should be available")
        );
    }

    #[test]
    fn physical_root_identity_survives_directory_rename() {
        let container = tempdir().expect("temporary container should exist");
        let original = container.path().join("workspace-before-rename");
        let renamed = container.path().join("workspace-after-rename");
        fs::create_dir(&original).expect("workspace should create");
        let before_root = WorkspaceRoot::open_ambient(
            &original,
            WorkspaceId::new("before-rename").expect("workspace ID should validate"),
        )
        .expect("workspace should open");
        let before = before_root
            .physical_identity()
            .expect("identity before rename should be available");

        // Windows deliberately opens capability directories without FILE_SHARE_DELETE, so close
        // that handle before the trusted host renames the directory. Both observations still come
        // from the respective preopened directory handle, never from a path metadata lookup.
        drop(before_root);
        fs::rename(&original, &renamed).expect("same directory should rename");
        let after = WorkspaceRoot::open_ambient(
            &renamed,
            WorkspaceId::new("after-rename").expect("workspace ID should validate"),
        )
        .expect("renamed workspace should open")
        .physical_identity()
        .expect("identity after rename should be available");

        assert_eq!(before, after);
    }

    #[test]
    fn physical_root_identity_fails_closed_without_exposing_handle_details() {
        let directory = tempdir().expect("temporary container should exist");
        let sentinel = directory.path().join("RAW-ROOT-IDENTITY-SENTINEL");
        fs::write(&sentinel, b"not a directory").expect("sentinel file should write");
        let file = fs::File::open(&sentinel).expect("sentinel file should open");
        let invalid_root = WorkspaceRoot::from_dir(
            cap_std::fs::Dir::from_std_file(file),
            WorkspaceId::new("invalid-handle").expect("workspace ID should validate"),
        );

        let error = invalid_root
            .physical_identity()
            .expect_err("a non-directory handle must fail closed");
        let rendered = format!("{error} {error:?}");
        assert_eq!(
            rendered,
            "workspace physical identity is unavailable WorkspaceRootIdentityError"
        );
        assert!(!rendered.contains("RAW-ROOT-IDENTITY-SENTINEL"));
    }
}
