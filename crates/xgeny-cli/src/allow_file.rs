use std::collections::BTreeSet;
use std::fmt;

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_adapter_filesystem::{
    FILESYSTEM_READ_SCOPE, READ_TEXT_CAPABILITY_ID, READ_TEXT_CONTRACT_VERSION,
    WorkspaceResourceResolver,
};
use xgeny_policy::ResourceResolver;
use xgeny_runtime::{
    InvocationMaterialProvider, MaterialProviderFailure, PlanMaterializationRequest,
    PlanMaterializer, PlanMaterializerFailure,
};
use xgeny_workgraph::ReconstructableMaterialReference;

pub(crate) const ALLOW_FILE_PROVIDER_ID: &str = "xgeny.cli.allow-file.v1";
const CATALOG_FORMAT_VERSION: u32 = 1;
const MAX_ALLOW_FILE_ENTRIES: usize = 64;

#[derive(Clone)]
pub(crate) struct AllowFileCatalog {
    entries: Vec<AllowFileEntry>,
    catalog_digest: String,
}

#[derive(Clone)]
struct AllowFileEntry {
    reference_id: String,
    revision: String,
    arguments: Value,
}

impl AllowFileCatalog {
    pub(crate) fn new<I, S>(
        resolver: &WorkspaceResourceResolver,
        paths: I,
    ) -> Result<Self, AllowFileCatalogError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut canonical_arguments = Vec::new();
        for path in paths {
            if canonical_arguments.len() == MAX_ALLOW_FILE_ENTRIES {
                return Err(AllowFileCatalogError::TooManyEntries);
            }
            let canonical = resolver
                .resolve(FILESYSTEM_READ_SCOPE, path.as_ref())
                .map_err(|_| AllowFileCatalogError::InvalidEntry)?;
            // Discovery capabilities may address the logical workspace root, but the legacy
            // read-text catalog must continue to contain exact regular-file resources only.
            if !canonical.contains('/') {
                return Err(AllowFileCatalogError::InvalidEntry);
            }
            canonical_arguments.push(json!({"path": canonical}));
        }
        if canonical_arguments.is_empty() {
            return Err(AllowFileCatalogError::Empty);
        }

        canonical_arguments.sort_by(|left, right| {
            serde_jcs::to_vec(left)
                .expect("validated JSON values must canonicalize")
                .cmp(&serde_jcs::to_vec(right).expect("validated JSON values must canonicalize"))
        });
        let unique = canonical_arguments
            .iter()
            .map(|arguments| {
                serde_jcs::to_vec(arguments).map_err(|_| AllowFileCatalogError::Canonicalization)
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if unique.len() != canonical_arguments.len() {
            return Err(AllowFileCatalogError::DuplicateEntry);
        }

        let entries = canonical_arguments
            .into_iter()
            .enumerate()
            .map(|(index, arguments)| {
                let ordinal =
                    u32::try_from(index + 1).map_err(|_| AllowFileCatalogError::TooManyEntries)?;
                let reference_id = format!("entry-{ordinal:08}");
                let revision_input = RevisionDigestInput {
                    domain: "xgeny.cli.allow-file-revision/v1",
                    format_version: CATALOG_FORMAT_VERSION,
                    ordinal,
                    arguments: &arguments,
                };
                let canonical = serde_jcs::to_vec(&revision_input)
                    .map_err(|_| AllowFileCatalogError::Canonicalization)?;
                let revision = format!("sha256-{}", sha256_hex(&canonical));
                Ok(AllowFileEntry {
                    reference_id,
                    revision,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let commitments = entries
            .iter()
            .map(|entry| CatalogCommitment {
                reference_id: &entry.reference_id,
                revision: &entry.revision,
            })
            .collect::<Vec<_>>();
        let canonical = serde_jcs::to_vec(&CatalogDigestInput {
            domain: "xgeny.cli.allow-file-catalog/v1",
            format_version: CATALOG_FORMAT_VERSION,
            entries: commitments,
        })
        .map_err(|_| AllowFileCatalogError::Canonicalization)?;

        Ok(Self {
            entries,
            catalog_digest: format!("sha256:{}", sha256_hex(&canonical)),
        })
    }

    pub(crate) fn catalog_digest(&self) -> &str {
        &self.catalog_digest
    }

    pub(crate) fn contains_canonical_resource(&self, resource: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.arguments.get("path").and_then(Value::as_str) == Some(resource))
    }

    fn reference_for_arguments(
        &self,
        arguments: &Value,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.arguments == *arguments)
            .ok_or(PlanMaterializerFailure::Rejected)?;
        ReconstructableMaterialReference::new(
            ALLOW_FILE_PROVIDER_ID,
            &entry.reference_id,
            &entry.revision,
        )
        .map_err(|_| PlanMaterializerFailure::Rejected)
    }
}

impl PlanMaterializer for AllowFileCatalog {
    fn materialize(
        &mut self,
        request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        if request.capability().capability_id != READ_TEXT_CAPABILITY_ID
            || request.capability().contract_version != READ_TEXT_CONTRACT_VERSION
        {
            return Err(PlanMaterializerFailure::Rejected);
        }
        self.reference_for_arguments(request.normalized_arguments())
    }
}

impl InvocationMaterialProvider for AllowFileCatalog {
    fn reconstruct(
        &mut self,
        reference_id: &str,
        revision: &str,
    ) -> Result<Value, MaterialProviderFailure> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.reference_id == reference_id)
            .ok_or(MaterialProviderFailure::NotFound)?;
        if entry.revision != revision {
            return Err(MaterialProviderFailure::RevisionChanged);
        }
        Ok(entry.arguments.clone())
    }
}

impl fmt::Debug for AllowFileCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AllowFileCatalog")
            .field("entry_count", &self.entries.len())
            .field("catalog_digest", &self.catalog_digest)
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllowFileCatalogError {
    #[error("allow-file catalog is empty")]
    Empty,
    #[error("allow-file catalog has too many entries")]
    TooManyEntries,
    #[error("allow-file catalog entry is invalid")]
    InvalidEntry,
    #[error("allow-file catalog contains a duplicate entry")]
    DuplicateEntry,
    #[error("allow-file catalog commitment failed")]
    Canonicalization,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RevisionDigestInput<'a> {
    domain: &'static str,
    format_version: u32,
    ordinal: u32,
    arguments: &'a Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogDigestInput<'a> {
    domain: &'static str,
    format_version: u32,
    entries: Vec<CatalogCommitment<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogCommitment<'a> {
    reference_id: &'a str,
    revision: &'a str,
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
    use std::fs;

    use tempfile::tempdir;
    use xgeny_adapter_filesystem::{WorkspaceId, WorkspaceRoot};

    use super::*;

    fn catalog(paths: &[&str]) -> AllowFileCatalog {
        let directory = tempdir().expect("temporary directory should exist");
        fs::write(directory.path().join("README.md"), "content")
            .expect("fixture file should write");
        fs::create_dir(directory.path().join("docs")).expect("fixture directory should create");
        fs::write(directory.path().join("docs/spec.md"), "spec")
            .expect("fixture file should write");
        let workspace = WorkspaceRoot::open_ambient(
            directory.path(),
            WorkspaceId::new("fixture").expect("workspace ID should validate"),
        )
        .expect("workspace should open");
        AllowFileCatalog::new(&workspace.resolver(), paths).expect("catalog should build")
    }

    #[test]
    fn catalog_is_order_independent_and_reconstructs_exact_normalized_arguments() {
        let mut first = catalog(&["README.md", "docs/spec.md"]);
        let second = catalog(&["docs/spec.md", "README.md"]);
        assert_eq!(first.catalog_digest(), second.catalog_digest());

        let reference = first
            .reference_for_arguments(&json!({"path": "workspace:fixture/README.md"}))
            .expect("allow-listed arguments should resolve");
        assert_eq!(reference.provider_id(), ALLOW_FILE_PROVIDER_ID);
        assert_eq!(
            first
                .reconstruct(reference.reference_id(), reference.revision())
                .expect("exact reference should reconstruct"),
            json!({"path": "workspace:fixture/README.md"})
        );
    }

    #[test]
    fn duplicate_or_outside_entries_fail_closed() {
        assert_eq!(
            AllowFileCatalog::new(
                &WorkspaceRoot::open_ambient(
                    tempdir().unwrap().path(),
                    WorkspaceId::new("fixture").unwrap(),
                )
                .unwrap()
                .resolver(),
                ["README.md", "README.md"],
            )
            .unwrap_err(),
            AllowFileCatalogError::DuplicateEntry
        );
        let fixture = catalog(&["README.md"]);
        assert!(
            fixture
                .reference_for_arguments(&json!({"path": "workspace:fixture/secret.txt"}))
                .is_err()
        );
    }

    #[test]
    fn legacy_catalog_rejects_the_workspace_root() {
        let directory = tempdir().expect("temporary directory should exist");
        let workspace = WorkspaceRoot::open_ambient(
            directory.path(),
            WorkspaceId::new("fixture").expect("workspace ID should validate"),
        )
        .expect("workspace should open");

        assert_eq!(
            AllowFileCatalog::new(&workspace.resolver(), ["."]).unwrap_err(),
            AllowFileCatalogError::InvalidEntry
        );
    }

    #[test]
    fn debug_and_opaque_reference_exclude_paths() {
        let catalog = catalog(&["docs/spec.md"]);
        let debug = format!("{catalog:?}");
        assert!(!debug.contains("docs"));
        assert!(!debug.contains("spec.md"));
        let reference = catalog
            .reference_for_arguments(&json!({"path": "workspace:fixture/docs/spec.md"}))
            .unwrap();
        assert!(!reference.reference_id().contains("docs"));
        assert!(!reference.revision().contains("docs"));
    }
}
