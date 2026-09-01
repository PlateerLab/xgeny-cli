use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use xgeny_policy::{ResourceResolutionFailure, ResourceResolver};

use crate::{PROCESS_EXECUTE_SCOPE, ProcessWorkspaceId, execution::sha256_digest, process_binding};

const CANONICAL_EXECUTABLE_PREFIX: &str = "process:";
const MAX_EXECUTABLE_ID_BYTES: usize = 128;
const MAX_EXECUTABLES: usize = 256;
const MAX_HOST_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_ENVIRONMENT_KEY_BYTES: usize = 64;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 4_096;
const MAX_HOST_ENVIRONMENT_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub struct ExecutableCatalog {
    entries: Arc<BTreeMap<String, ExecutableEntry>>,
    digest: String,
}

#[derive(Clone)]
pub(crate) struct ExecutableEntry {
    path: Arc<PathBuf>,
    digest: String,
}

impl ExecutableCatalog {
    /// Snapshot a set of host-resolved OS-executable files under portable logical identifiers.
    ///
    /// Paths are canonicalized and the executable content digest is retained. Invocation material
    /// contains only the logical identifier; the ambient path never enters a Run journal.
    ///
    /// # Errors
    ///
    /// Rejects invalid/duplicate identifiers, non-executable files, or unreadable files.
    pub fn from_paths<I, K, P>(entries: I) -> Result<Self, ExecutableCatalogError>
    where
        I: IntoIterator<Item = (K, P)>,
        K: Into<String>,
        P: AsRef<Path>,
    {
        let mut catalog = BTreeMap::new();
        for (logical_id, path) in entries {
            if catalog.len() == MAX_EXECUTABLES {
                return Err(ExecutableCatalogError);
            }
            let logical_id = logical_id.into();
            validate_executable_id(&logical_id)?;
            let path = std::fs::canonicalize(path).map_err(|_| ExecutableCatalogError)?;
            let metadata = std::fs::metadata(&path).map_err(|_| ExecutableCatalogError)?;
            if !metadata.is_file() || !platform_executable(&path, &metadata) {
                return Err(ExecutableCatalogError);
            }
            let digest = executable_digest(&path)?;
            if catalog
                .insert(
                    logical_id,
                    ExecutableEntry {
                        path: Arc::new(path),
                        digest,
                    },
                )
                .is_some()
            {
                return Err(ExecutableCatalogError);
            }
        }
        if catalog.is_empty() {
            return Err(ExecutableCatalogError);
        }
        let digest = catalog_digest(&catalog)?;
        Ok(Self {
            entries: Arc::new(catalog),
            digest,
        })
    }

    pub(crate) fn entry(&self, logical_id: &str) -> Option<&ExecutableEntry> {
        self.entries.get(logical_id)
    }

    pub(crate) fn logical_ids(&self) -> BTreeSet<String> {
        self.entries.keys().cloned().collect()
    }

    pub(crate) fn binding(
        &self,
        workspace_id: &ProcessWorkspaceId,
        environment: &ProcessEnvironment,
        root_digest: &str,
    ) -> xgeny_domain::InstanceBinding {
        process_binding(
            workspace_id,
            root_digest,
            &self.digest,
            environment.digest(),
        )
    }
}

impl ExecutableEntry {
    pub(crate) fn verified_path(&self) -> Result<PathBuf, ExecutableCatalogError> {
        let metadata = std::fs::metadata(self.path.as_ref()).map_err(|_| ExecutableCatalogError)?;
        if !metadata.is_file() || !platform_executable(self.path.as_ref(), &metadata) {
            return Err(ExecutableCatalogError);
        }
        let observed = executable_digest(self.path.as_ref())?;
        if observed != self.digest {
            return Err(ExecutableCatalogError);
        }
        Ok(self.path.as_ref().clone())
    }
}

impl fmt::Debug for ExecutableCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutableCatalog")
            .field("entry_count", &self.entries.len())
            .field("paths", &"<redacted>")
            .field("digest", &self.digest)
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("executable catalog is invalid or unavailable")]
pub struct ExecutableCatalogError;

#[derive(Clone)]
pub struct ProcessEnvironment {
    values: Arc<BTreeMap<String, String>>,
    digest: String,
}

impl ProcessEnvironment {
    /// Freeze the non-secret host environment inherited by every process invocation.
    ///
    /// # Errors
    ///
    /// Rejects invalid keys, NUL-bearing/oversized values, or an oversized snapshot.
    pub fn new(values: BTreeMap<String, String>) -> Result<Self, ProcessEnvironmentError> {
        if values.len() > MAX_HOST_ENVIRONMENT_ENTRIES {
            return Err(ProcessEnvironmentError);
        }
        let mut total = 0_usize;
        let mut folded_keys = BTreeSet::new();
        for (key, value) in &values {
            validate_environment_pair(key, value).map_err(|()| ProcessEnvironmentError)?;
            if !folded_keys.insert(key.to_ascii_uppercase()) {
                return Err(ProcessEnvironmentError);
            }
            total = total
                .checked_add(key.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or(ProcessEnvironmentError)?;
            if total > MAX_HOST_ENVIRONMENT_BYTES {
                return Err(ProcessEnvironmentError);
            }
        }
        let digest = digest_map(&values).map_err(|_| ProcessEnvironmentError)?;
        Ok(Self {
            values: Arc::new(values),
            digest,
        })
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
}

impl Default for ProcessEnvironment {
    fn default() -> Self {
        Self::new(BTreeMap::new()).expect("an empty environment is valid")
    }
}

impl fmt::Debug for ProcessEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessEnvironment")
            .field("entry_count", &self.values.len())
            .field("values", &"<redacted>")
            .field("digest", &self.digest)
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("process environment snapshot is invalid")]
pub struct ProcessEnvironmentError;

/// Side-effect-free resolver for one exact host-owned executable catalog.
#[derive(Clone)]
pub struct ProcessResourceResolver {
    workspace_id: ProcessWorkspaceId,
    logical_ids: BTreeSet<String>,
}

impl ProcessResourceResolver {
    pub(crate) const fn new(
        workspace_id: ProcessWorkspaceId,
        logical_ids: BTreeSet<String>,
    ) -> Self {
        Self {
            workspace_id,
            logical_ids,
        }
    }
}

impl ResourceResolver for ProcessResourceResolver {
    fn resolve(&self, scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        if scope != PROCESS_EXECUTE_SCOPE {
            return Err(ResourceResolutionFailure::UnsupportedScope);
        }
        let logical_id = parse_resource(&self.workspace_id, resource)?;
        self.logical_ids
            .contains(logical_id)
            .then(|| canonical_resource(&self.workspace_id, logical_id))
            .ok_or(ResourceResolutionFailure::OutsideHostBoundary)
    }
}

impl fmt::Debug for ProcessResourceResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessResourceResolver")
            .field("workspace_id", &"<redacted>")
            .field("executable_count", &self.logical_ids.len())
            .finish()
    }
}

pub(crate) fn parse_resource<'a>(
    workspace_id: &ProcessWorkspaceId,
    resource: &'a str,
) -> Result<&'a str, ResourceResolutionFailure> {
    let prefix = format!(
        "{CANONICAL_EXECUTABLE_PREFIX}{}/executables/",
        workspace_id.as_str()
    );
    let logical_id = if let Some(logical_id) = resource.strip_prefix(&prefix) {
        logical_id
    } else if resource.starts_with(CANONICAL_EXECUTABLE_PREFIX) {
        return Err(ResourceResolutionFailure::OutsideHostBoundary);
    } else {
        resource
    };
    validate_executable_id(logical_id)
        .map(|()| logical_id)
        .map_err(|_| ResourceResolutionFailure::InvalidResource)
}

fn canonical_resource(workspace_id: &ProcessWorkspaceId, logical_id: &str) -> String {
    format!(
        "{CANONICAL_EXECUTABLE_PREFIX}{}/executables/{logical_id}",
        workspace_id.as_str()
    )
}

fn validate_executable_id(value: &str) -> Result<(), ExecutableCatalogError> {
    if value.is_empty()
        || value.len() > MAX_EXECUTABLE_ID_BYTES
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'+' | b'-'))
        })
    {
        return Err(ExecutableCatalogError);
    }
    Ok(())
}

pub(crate) fn validate_environment_pair(key: &str, value: &str) -> Result<(), ()> {
    if key.is_empty()
        || key.len() > MAX_ENVIRONMENT_KEY_BYTES
        || value.len() > MAX_ENVIRONMENT_VALUE_BYTES
        || value.contains('\0')
        || !key.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
        })
    {
        return Err(());
    }
    Ok(())
}

fn executable_digest(path: &Path) -> Result<String, ExecutableCatalogError> {
    let mut file = File::open(path).map_err(|_| ExecutableCatalogError)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = file.read(&mut buffer).map_err(|_| ExecutableCatalogError)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format_digest(hasher.finalize().as_slice()))
}

fn catalog_digest(
    entries: &BTreeMap<String, ExecutableEntry>,
) -> Result<String, ExecutableCatalogError> {
    let values: BTreeMap<&str, &str> = entries
        .iter()
        .map(|(id, entry)| (id.as_str(), entry.digest.as_str()))
        .collect();
    digest_map(&values).map_err(|_| ExecutableCatalogError)
}

fn digest_map<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let value: Value = serde_json::to_value(value)?;
    let bytes = serde_jcs::to_vec(&value)?;
    Ok(sha256_digest(&bytes))
}

fn format_digest(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("sha256:{encoded}")
}

#[cfg(unix)]
fn platform_executable(_path: &Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn platform_executable(path: &Path, _metadata: &std::fs::Metadata) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("com")
    })
}

#[cfg(not(any(unix, windows)))]
fn platform_executable(_path: &Path, _metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use xgeny_domain::{GrantLifetime, ProtocolDocument};
    use xgeny_policy::PermissionRequestResolver;

    use super::*;
    use crate::ProcessWorkspaceId;

    #[test]
    fn resolver_is_catalog_bound_and_idempotent() {
        let resolver = ProcessResourceResolver::new(
            ProcessWorkspaceId::new("fixture").unwrap(),
            BTreeSet::from(["cargo".to_owned()]),
        );
        let canonical = resolver.resolve(PROCESS_EXECUTE_SCOPE, "cargo").unwrap();
        assert_eq!(canonical, "process:fixture/executables/cargo");
        assert_eq!(
            resolver.resolve(PROCESS_EXECUTE_SCOPE, &canonical).unwrap(),
            canonical
        );
        assert!(matches!(
            resolver.resolve(PROCESS_EXECUTE_SCOPE, "git"),
            Err(ResourceResolutionFailure::OutsideHostBoundary)
        ));
        assert!(matches!(
            resolver.resolve(PROCESS_EXECUTE_SCOPE, "process:other/executables/cargo"),
            Err(ResourceResolutionFailure::OutsideHostBoundary)
        ));
    }

    #[test]
    fn environment_debug_is_redacted_and_digest_is_value_sensitive() {
        let first = ProcessEnvironment::new(BTreeMap::from([(
            "BUILD_MODE".to_owned(),
            "SECRET-SENTINEL-A".to_owned(),
        )]))
        .unwrap();
        let second = ProcessEnvironment::new(BTreeMap::from([(
            "BUILD_MODE".to_owned(),
            "SECRET-SENTINEL-B".to_owned(),
        )]))
        .unwrap();
        assert_ne!(first.digest(), second.digest());
        assert!(!format!("{first:?}").contains("SECRET-SENTINEL"));
        assert!(
            ProcessEnvironment::new(BTreeMap::from([
                ("BAD=KEY".to_owned(), "value".to_owned(),)
            ]))
            .is_err()
        );
        assert!(
            ProcessEnvironment::new(BTreeMap::from([
                ("PATH".to_owned(), "first".to_owned()),
                ("Path".to_owned(), "second".to_owned()),
            ]))
            .is_err()
        );
    }

    #[test]
    fn normalized_resource_still_conforms_to_the_public_input_schema() {
        let document: ProtocolDocument = serde_json::from_str(include_str!(
            "../../../protocol/fixtures/v1alpha1/valid/capability-definition.process-execute.json"
        ))
        .unwrap();
        let ProtocolDocument::CapabilityDefinition(definition) = document else {
            panic!("expected process Capability Definition")
        };
        let resolver = PermissionRequestResolver::new(ProcessResourceResolver::new(
            ProcessWorkspaceId::new("fixture").unwrap(),
            BTreeSet::from(["cargo".to_owned()]),
        ));
        let request = resolver
            .resolve_invocation(
                "permission-1",
                "run-1",
                "step-1",
                &definition,
                &serde_json::json!({
                    "executable": "cargo",
                    "args": ["test"],
                    "cwd": ".",
                    "env": {},
                    "timeoutMs": 120_000,
                    "maxOutputBytes": 32_768,
                }),
                GrantLifetime::Once,
            )
            .unwrap();
        assert_eq!(
            request.normalized_arguments()["executable"],
            "process:fixture/executables/cargo"
        );
        let validator = jsonschema::validator_for(&definition.spec.input_schema).unwrap();
        assert!(validator.is_valid(request.normalized_arguments()));
    }

    #[test]
    fn executable_content_drift_invalidates_the_catalog_entry() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory
            .path()
            .join(if cfg!(windows) { "tool.exe" } else { "tool" });
        std::fs::write(&executable, b"first executable bytes").unwrap();
        make_executable(&executable);
        let catalog = ExecutableCatalog::from_paths([("tool", &executable)]).unwrap();

        std::fs::write(&executable, b"different executable bytes").unwrap();
        assert!(catalog.entry("tool").unwrap().verified_path().is_err());
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(windows)]
    fn make_executable(_path: &Path) {}
}
