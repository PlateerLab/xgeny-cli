use std::collections::BTreeSet;
use std::fmt;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_adapter_filesystem::{
    FILESYSTEM_READ_SCOPE, LIST_DIRECTORY_CAPABILITY_ID, LIST_DIRECTORY_CONTRACT_VERSION,
    MAX_SEARCH_QUERY_BYTES, MAX_SEARCH_QUERY_UNICODE_SCALARS, MAX_WRITE_ATOMIC_BYTES,
    READ_TEXT_CAPABILITY_ID, READ_TEXT_CONTRACT_VERSION, SEARCH_TEXT_CAPABILITY_ID,
    SEARCH_TEXT_CONTRACT_VERSION, STAT_CAPABILITY_ID, STAT_CONTRACT_VERSION,
    WRITE_ATOMIC_CAPABILITY_ID, WRITE_ATOMIC_CONTRACT_VERSION, WorkspaceResourceResolver,
};
use xgeny_domain::CapabilityRef;
use xgeny_policy::ResourceResolver;

const CATALOG_FORMAT_VERSION: u32 = 1;
const MAX_ALLOW_PATH_ENTRIES: usize = 64;
const MAX_PLANNER_SCOPE_HINT_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct WorkspaceReadAuthorization {
    files: BTreeSet<String>,
    directories: BTreeSet<String>,
    catalog_digest: String,
    planner_scope_hint: String,
}

impl WorkspaceReadAuthorization {
    pub(crate) fn new<I, IS, D, DS>(
        resolver: &WorkspaceResourceResolver,
        files: I,
        directories: D,
    ) -> Result<Self, WorkspaceReadAuthorizationError>
    where
        I: IntoIterator<Item = IS>,
        IS: AsRef<str>,
        D: IntoIterator<Item = DS>,
        DS: AsRef<str>,
    {
        let files = canonical_set(resolver, files, false)?;
        let directories = canonical_set(resolver, directories, true)?;
        if directories.is_empty() {
            return Err(WorkspaceReadAuthorizationError::EmptyDirectorySet);
        }
        if files.len().saturating_add(directories.len()) > MAX_ALLOW_PATH_ENTRIES {
            return Err(WorkspaceReadAuthorizationError::TooManyEntries);
        }
        if files.iter().any(|path| directories.contains(path)) {
            return Err(WorkspaceReadAuthorizationError::DuplicateEntry);
        }
        let planner_scope_hint = planner_scope_hint(&files, &directories)?;
        let entries = files
            .iter()
            .map(|path| CatalogEntry { kind: "file", path })
            .chain(directories.iter().map(|path| CatalogEntry {
                kind: "directory",
                path,
            }))
            .collect::<Vec<_>>();
        let canonical = serde_jcs::to_vec(&CatalogDigestInput {
            domain: "xgeny.cli.workspace-read-authorization/v1",
            format_version: CATALOG_FORMAT_VERSION,
            entries,
        })
        .map_err(|_| WorkspaceReadAuthorizationError::Canonicalization)?;
        Ok(Self {
            files,
            directories,
            catalog_digest: format!("sha256:{}", sha256_hex(&canonical)),
            planner_scope_hint,
        })
    }

    pub(crate) fn catalog_digest(&self) -> &str {
        &self.catalog_digest
    }

    pub(crate) fn planner_scope_hint(&self) -> &str {
        &self.planner_scope_hint
    }

    pub(crate) fn authorizes_material(
        &self,
        capability: &CapabilityRef,
        arguments: &Value,
    ) -> bool {
        let Some(object) = arguments.as_object() else {
            return false;
        };
        let Some(path) = object.get("path").and_then(Value::as_str) else {
            return false;
        };
        match (
            capability.capability_id.as_str(),
            capability.contract_version.as_str(),
        ) {
            (READ_TEXT_CAPABILITY_ID, READ_TEXT_CONTRACT_VERSION)
            | (STAT_CAPABILITY_ID, STAT_CONTRACT_VERSION) => {
                object.len() == 1 && self.authorizes_file_or_descendant(path)
            }
            (LIST_DIRECTORY_CAPABILITY_ID, LIST_DIRECTORY_CONTRACT_VERSION) => {
                object.len() == 1 && self.authorizes_directory_or_descendant(path)
            }
            (SEARCH_TEXT_CAPABILITY_ID, SEARCH_TEXT_CONTRACT_VERSION) => {
                object.len() == 2
                    && object
                        .get("query")
                        .and_then(Value::as_str)
                        .is_some_and(valid_query)
                    && self.authorizes_directory_or_descendant(path)
            }
            (WRITE_ATOMIC_CAPABILITY_ID, WRITE_ATOMIC_CONTRACT_VERSION) => {
                object.len() == 3
                    && object
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|content| content.len() <= MAX_WRITE_ATOMIC_BYTES)
                    && object
                        .get("expectedDigest")
                        .is_some_and(valid_expected_digest)
                    && self.authorizes_directory_or_descendant(path)
            }
            _ => false,
        }
    }

    pub(crate) fn authorizes_resource(&self, capability: &CapabilityRef, resource: &str) -> bool {
        match (
            capability.capability_id.as_str(),
            capability.contract_version.as_str(),
        ) {
            (READ_TEXT_CAPABILITY_ID, READ_TEXT_CONTRACT_VERSION)
            | (STAT_CAPABILITY_ID, STAT_CONTRACT_VERSION) => {
                self.authorizes_file_or_descendant(resource)
            }
            (LIST_DIRECTORY_CAPABILITY_ID, LIST_DIRECTORY_CONTRACT_VERSION)
            | (SEARCH_TEXT_CAPABILITY_ID, SEARCH_TEXT_CONTRACT_VERSION)
            | (WRITE_ATOMIC_CAPABILITY_ID, WRITE_ATOMIC_CONTRACT_VERSION) => {
                self.authorizes_directory_or_descendant(resource)
            }
            _ => false,
        }
    }

    fn authorizes_file_or_descendant(&self, resource: &str) -> bool {
        self.files.contains(resource) || self.authorizes_directory_or_descendant(resource)
    }

    fn authorizes_directory_or_descendant(&self, resource: &str) -> bool {
        self.directories
            .iter()
            .any(|directory| contains_resource(directory, resource))
    }
}

impl fmt::Debug for WorkspaceReadAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceReadAuthorization")
            .field("file_count", &self.files.len())
            .field("directory_count", &self.directories.len())
            .field("catalog_digest", &self.catalog_digest)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceReadAuthorizationError {
    #[error("workspace read authorization requires at least one directory")]
    EmptyDirectorySet,
    #[error("workspace read authorization has too many entries")]
    TooManyEntries,
    #[error("workspace read authorization entry is invalid")]
    InvalidEntry,
    #[error("workspace read authorization contains a duplicate entry")]
    DuplicateEntry,
    #[error("workspace read authorization is too large to describe safely to the planner")]
    PlannerHintTooLarge,
    #[error("workspace read authorization commitment failed")]
    Canonicalization,
}

fn planner_scope_hint(
    files: &BTreeSet<String>,
    directories: &BTreeSet<String>,
) -> Result<String, WorkspaceReadAuthorizationError> {
    let directories = directories
        .iter()
        .map(|path| workspace_relative_display(path))
        .collect::<Vec<_>>();
    let files = files
        .iter()
        .map(|path| workspace_relative_display(path))
        .collect::<Vec<_>>();
    let directories = serde_json::to_string(&directories)
        .map_err(|_| WorkspaceReadAuthorizationError::Canonicalization)?;
    let files = serde_json::to_string(&files)
        .map_err(|_| WorkspaceReadAuthorizationError::Canonicalization)?;
    let hint = format!(
        "Caller-authorized directory roots (list/search targets and descendant stat/read/write-atomic targets): {directories}. Additional exact files (stat/read only): {files}. write-atomic requires expectedDigest=null for creation or the exact digest previously read for replacement."
    );
    if hint.len() > MAX_PLANNER_SCOPE_HINT_BYTES {
        return Err(WorkspaceReadAuthorizationError::PlannerHintTooLarge);
    }
    Ok(hint)
}

fn workspace_relative_display(canonical: &str) -> &str {
    canonical
        .split_once('/')
        .map_or(".", |(_, relative)| relative)
}

fn canonical_set<I, S>(
    resolver: &WorkspaceResourceResolver,
    values: I,
    allow_root: bool,
) -> Result<BTreeSet<String>, WorkspaceReadAuthorizationError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut set = BTreeSet::new();
    for value in values {
        if set.len() == MAX_ALLOW_PATH_ENTRIES {
            return Err(WorkspaceReadAuthorizationError::TooManyEntries);
        }
        let canonical = resolver
            .resolve(FILESYSTEM_READ_SCOPE, value.as_ref())
            .map_err(|_| WorkspaceReadAuthorizationError::InvalidEntry)?;
        if !allow_root && !canonical.contains('/') {
            return Err(WorkspaceReadAuthorizationError::InvalidEntry);
        }
        if !set.insert(canonical) {
            return Err(WorkspaceReadAuthorizationError::DuplicateEntry);
        }
    }
    Ok(set)
}

fn contains_resource(directory: &str, resource: &str) -> bool {
    resource == directory
        || resource
            .strip_prefix(directory)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn valid_query(query: &str) -> bool {
    !query.is_empty()
        && query.len() <= MAX_SEARCH_QUERY_BYTES
        && query.chars().count() <= MAX_SEARCH_QUERY_UNICODE_SCALARS
        && !query.chars().any(char::is_control)
}

fn valid_expected_digest(value: &Value) -> bool {
    value.is_null()
        || value.as_str().is_some_and(|digest| {
            digest.strip_prefix("sha256:").is_some_and(|hex| {
                hex.len() == 64
                    && hex
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            })
        })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogDigestInput<'a> {
    domain: &'static str,
    format_version: u32,
    entries: Vec<CatalogEntry<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogEntry<'a> {
    kind: &'static str,
    path: &'a str,
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
    use serde_json::json;
    use tempfile::tempdir;
    use xgeny_adapter_filesystem::{WorkspaceId, WorkspaceRoot};

    use super::*;

    fn authorization(files: &[&str], directories: &[&str]) -> WorkspaceReadAuthorization {
        let root = tempdir().unwrap();
        let workspace =
            WorkspaceRoot::open_ambient(root.path(), WorkspaceId::new("fixture").unwrap()).unwrap();
        WorkspaceReadAuthorization::new(&workspace.resolver(), files, directories).unwrap()
    }

    fn capability(id: &str) -> CapabilityRef {
        CapabilityRef {
            capability_id: id.to_owned(),
            contract_version: "1.0.0".to_owned(),
        }
    }

    #[test]
    fn directory_scope_authorizes_only_component_descendants() {
        let catalog = authorization(&["README.md"], &["src"]);
        assert!(catalog.authorizes_material(
            &capability(READ_TEXT_CAPABILITY_ID),
            &json!({"path": "workspace:fixture/src/lib.rs"}),
        ));
        assert!(catalog.authorizes_material(
            &capability(READ_TEXT_CAPABILITY_ID),
            &json!({"path": "workspace:fixture/README.md"}),
        ));
        assert!(!catalog.authorizes_material(
            &capability(READ_TEXT_CAPABILITY_ID),
            &json!({"path": "workspace:fixture/src-secret.txt"}),
        ));
        assert!(!catalog.authorizes_material(
            &capability(LIST_DIRECTORY_CAPABILITY_ID),
            &json!({"path": "workspace:fixture/README.md"}),
        ));
    }

    #[test]
    fn root_directory_enables_bounded_discovery_but_not_other_workspaces() {
        let catalog = authorization(&[], &["."]);
        assert!(catalog.authorizes_material(
            &capability(LIST_DIRECTORY_CAPABILITY_ID),
            &json!({"path": "workspace:fixture"}),
        ));
        assert!(catalog.authorizes_material(
            &capability(SEARCH_TEXT_CAPABILITY_ID),
            &json!({"path": "workspace:fixture/src", "query": "WorkspaceRoot"}),
        ));
        assert!(!catalog.authorizes_material(
            &capability(SEARCH_TEXT_CAPABILITY_ID),
            &json!({"path": "workspace:other", "query": "WorkspaceRoot"}),
        ));
    }

    #[test]
    fn query_shape_and_capability_version_are_closed() {
        let catalog = authorization(&[], &["."]);
        assert!(catalog.authorizes_material(
            &capability(SEARCH_TEXT_CAPABILITY_ID),
            &json!({"path": "workspace:fixture", "query": "🦀".repeat(64)}),
        ));
        for arguments in [
            json!({"path": "workspace:fixture", "query": ""}),
            json!({"path": "workspace:fixture", "query": "bad\nquery"}),
            json!({"path": "workspace:fixture", "query": "a".repeat(65)}),
            json!({"path": "workspace:fixture", "query": "🦀".repeat(65)}),
            json!({"path": "workspace:fixture", "query": "ok", "extra": true}),
        ] {
            assert!(
                !catalog.authorizes_material(&capability(SEARCH_TEXT_CAPABILITY_ID), &arguments,)
            );
        }
        let mut wrong = capability(SEARCH_TEXT_CAPABILITY_ID);
        wrong.contract_version = "2.0.0".to_owned();
        assert!(
            !catalog
                .authorizes_material(&wrong, &json!({"path": "workspace:fixture", "query": "ok"}),)
        );
    }

    #[test]
    fn write_material_is_bounded_and_requires_a_directory_scope() {
        let catalog = authorization(&["README.md"], &["src"]);
        let write = capability(WRITE_ATOMIC_CAPABILITY_ID);
        assert!(catalog.authorizes_material(
            &write,
            &json!({
                "path": "workspace:fixture/src/new.rs",
                "content": "fn main() {}",
                "expectedDigest": null
            }),
        ));
        assert!(!catalog.authorizes_material(
            &write,
            &json!({
                "path": "workspace:fixture/README.md",
                "content": "replacement",
                "expectedDigest": null
            }),
        ));
        assert!(!catalog.authorizes_material(
            &write,
            &json!({
                "path": "workspace:fixture/src/new.rs",
                "content": "x".repeat(MAX_WRITE_ATOMIC_BYTES + 1),
                "expectedDigest": null
            }),
        ));
        assert!(!catalog.authorizes_material(
            &write,
            &json!({
                "path": "workspace:fixture/src/new.rs",
                "content": "replacement",
                "expectedDigest": "sha256:UPPER"
            }),
        ));
    }

    #[test]
    fn digest_is_order_independent_and_debug_redacts_paths() {
        let first = authorization(&["README.md", "Cargo.toml"], &["src", "docs"]);
        let second = authorization(&["Cargo.toml", "README.md"], &["docs", "src"]);
        assert_eq!(first.catalog_digest(), second.catalog_digest());
        assert_eq!(first.planner_scope_hint(), second.planner_scope_hint());
        assert!(first.planner_scope_hint().contains("[\"docs\",\"src\"]"));
        assert!(
            first
                .planner_scope_hint()
                .contains("[\"Cargo.toml\",\"README.md\"]")
        );
        let debug = format!("{first:?}");
        assert!(!debug.contains("README"));
        assert!(!debug.contains("src"));
    }

    #[test]
    fn empty_duplicate_and_root_file_entries_fail_closed() {
        let root = tempdir().unwrap();
        let workspace =
            WorkspaceRoot::open_ambient(root.path(), WorkspaceId::new("fixture").unwrap()).unwrap();
        assert_eq!(
            WorkspaceReadAuthorization::new(
                &workspace.resolver(),
                ["README.md"],
                Vec::<&str>::new()
            )
            .unwrap_err(),
            WorkspaceReadAuthorizationError::EmptyDirectorySet
        );
        assert_eq!(
            WorkspaceReadAuthorization::new(&workspace.resolver(), Vec::<&str>::new(), [".", "."])
                .unwrap_err(),
            WorkspaceReadAuthorizationError::DuplicateEntry
        );
        assert_eq!(
            WorkspaceReadAuthorization::new(&workspace.resolver(), ["."], ["src"]).unwrap_err(),
            WorkspaceReadAuthorizationError::InvalidEntry
        );

        let oversized_hint = (0..MAX_ALLOW_PATH_ENTRIES)
            .map(|index| format!("d{index:02}{}", "x".repeat(251)))
            .collect::<Vec<_>>();
        assert_eq!(
            WorkspaceReadAuthorization::new(
                &workspace.resolver(),
                Vec::<String>::new(),
                &oversized_hint,
            )
            .unwrap_err(),
            WorkspaceReadAuthorizationError::PlannerHintTooLarge
        );
    }
}
