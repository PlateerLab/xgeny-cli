use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use xgeny_adapter_process::{
    ExecutableCatalog, PROCESS_EXECUTE_SCOPE, ProcessEnvironment, ProcessResourceResolver,
    ProcessWorkspace, ProcessWorkspaceId,
};
use xgeny_policy::ResourceResolver as _;

pub(crate) const PROCESS_EXECUTABLE_CATALOG_PROFILE: &str =
    "xgeny.cli.explicit-executable-path-catalog/v1";
pub(crate) const SAFE_PROCESS_ENVIRONMENT_PROFILE: &str = "xgeny.cli.safe-process-environment/v1";
const MAX_ALLOW_EXECUTABLES: usize = 64;
const MAX_EXECUTABLE_SPEC_BYTES: usize = 8 * 1024;
const MAX_PLANNER_HINT_BYTES: usize = 8 * 1024;

const SAFE_ENVIRONMENT_KEYS: &[&str] = &[
    "PATH",
    "PATHEXT",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "TMP",
    "TEMP",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "NO_COLOR",
    "TERM",
    "COLORTERM",
    "DEVELOPER_DIR",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "INCLUDE",
    "LIB",
    "LIBPATH",
    "VCINSTALLDIR",
    "VSINSTALLDIR",
    "WindowsSdkDir",
    "UCRTVersion",
    "UniversalCRTSdkDir",
    "VCToolsInstallDir",
];

#[derive(Clone)]
pub(crate) struct ProcessTooling {
    workspace: ProcessWorkspace,
    authorization: ProcessExecutionAuthorization,
}

impl ProcessTooling {
    pub(crate) fn build<I, S>(
        root: &Path,
        workspace_id: &str,
        specifications: I,
    ) -> Result<Option<Self>, ProcessToolingError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut entries = Vec::new();
        let mut logical_ids = Vec::new();
        for specification in specifications {
            if entries.len() == MAX_ALLOW_EXECUTABLES {
                return Err(ProcessToolingError);
            }
            let specification = specification.as_ref();
            if specification.is_empty()
                || specification.len() > MAX_EXECUTABLE_SPEC_BYTES
                || specification.chars().any(char::is_control)
            {
                return Err(ProcessToolingError);
            }
            let (logical_id, path) = specification
                .split_once('=')
                .filter(|(logical_id, path)| !logical_id.is_empty() && !path.is_empty())
                .ok_or(ProcessToolingError)?;
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(ProcessToolingError);
            }
            logical_ids.push(logical_id.to_owned());
            entries.push((logical_id.to_owned(), path));
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let catalog = ExecutableCatalog::from_paths(entries).map_err(|_| ProcessToolingError)?;
        let environment = safe_process_environment()?;
        let workspace = ProcessWorkspace::open_ambient(
            root,
            ProcessWorkspaceId::new(workspace_id).map_err(|_| ProcessToolingError)?,
            catalog,
            environment,
        )
        .map_err(|_| ProcessToolingError)?;
        let authorization = ProcessExecutionAuthorization::new(&workspace.resolver(), logical_ids)?;
        Ok(Some(Self {
            workspace,
            authorization,
        }))
    }

    pub(crate) const fn workspace(&self) -> &ProcessWorkspace {
        &self.workspace
    }

    pub(crate) const fn authorization(&self) -> &ProcessExecutionAuthorization {
        &self.authorization
    }
}

impl fmt::Debug for ProcessTooling {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessTooling")
            .field("workspace", &"<redacted>")
            .field("authorization", &self.authorization)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct ProcessExecutionAuthorization {
    canonical_resources: BTreeSet<String>,
    planner_scope_hint: String,
    catalog_digest: String,
}

impl ProcessExecutionAuthorization {
    fn new<I, S>(
        resolver: &ProcessResourceResolver,
        logical_ids: I,
    ) -> Result<Self, ProcessToolingError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut ids = BTreeSet::new();
        let mut canonical_resources = BTreeSet::new();
        for logical_id in logical_ids {
            let logical_id = logical_id.as_ref();
            if !ids.insert(logical_id.to_owned()) {
                return Err(ProcessToolingError);
            }
            let canonical = resolver
                .resolve(PROCESS_EXECUTE_SCOPE, logical_id)
                .map_err(|_| ProcessToolingError)?;
            if !canonical_resources.insert(canonical) {
                return Err(ProcessToolingError);
            }
        }
        if ids.is_empty() || ids.len() > MAX_ALLOW_EXECUTABLES {
            return Err(ProcessToolingError);
        }
        let encoded_ids = serde_json::to_string(&ids).map_err(|_| ProcessToolingError)?;
        let planner_scope_hint = format!(
            "Caller-catalogued process executable logical IDs: {encoded_ids}. process.execute must select one listed ID, pass a literal argv array without shell syntax, use a workspace-relative cwd, and choose bounded timeoutMs and maxOutputBytes."
        );
        if planner_scope_hint.len() > MAX_PLANNER_HINT_BYTES {
            return Err(ProcessToolingError);
        }
        let canonical = serde_jcs::to_vec(&CatalogDigestInput {
            domain: PROCESS_EXECUTABLE_CATALOG_PROFILE,
            logical_ids: ids.iter().map(String::as_str).collect(),
        })
        .map_err(|_| ProcessToolingError)?;
        Ok(Self {
            canonical_resources,
            planner_scope_hint,
            catalog_digest: sha256_digest(&canonical),
        })
    }

    pub(crate) fn authorizes_resource(&self, resource: &str) -> bool {
        self.canonical_resources.contains(resource)
    }

    pub(crate) fn planner_scope_hint(&self) -> &str {
        &self.planner_scope_hint
    }

    pub(crate) fn catalog_digest(&self) -> &str {
        &self.catalog_digest
    }
}

impl fmt::Debug for ProcessExecutionAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessExecutionAuthorization")
            .field("executable_count", &self.canonical_resources.len())
            .field("resources", &"<redacted>")
            .field("planner_scope_hint", &"<redacted>")
            .field("catalog_digest", &self.catalog_digest)
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("process executable catalog or environment is invalid")]
pub(crate) struct ProcessToolingError;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogDigestInput<'a> {
    domain: &'static str,
    logical_ids: Vec<&'a str>,
}

fn safe_process_environment() -> Result<ProcessEnvironment, ProcessToolingError> {
    safe_process_environment_with(|key| env::var(key).ok())
}

fn safe_process_environment_with(
    mut read: impl FnMut(&str) -> Option<String>,
) -> Result<ProcessEnvironment, ProcessToolingError> {
    let values = SAFE_ENVIRONMENT_KEYS
        .iter()
        .filter_map(|key| read(key).map(|value| ((*key).to_owned(), value)))
        .collect::<BTreeMap<_, _>>();
    ProcessEnvironment::new(values).map_err(|_| ProcessToolingError)
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("sha256:{encoded}")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn executable_specs_are_absolute_deterministic_and_path_redacted() {
        let directory = tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let specification = format!("test-helper={}", executable.display());
        let tooling = ProcessTooling::build(directory.path(), "primary", [&specification])
            .unwrap()
            .unwrap();
        let debug = format!("{tooling:?}");

        assert!(
            tooling
                .authorization()
                .planner_scope_hint()
                .contains("test-helper")
        );
        assert!(!debug.contains(&executable.to_string_lossy().into_owned()));
        assert!(!debug.contains("test-helper"));
        assert!(ProcessTooling::build(directory.path(), "primary", ["test=relative"]).is_err());
        assert!(
            ProcessTooling::build(
                directory.path(),
                "primary",
                [&specification, &specification]
            )
            .is_err()
        );
    }

    #[test]
    fn safe_environment_never_reads_or_forwards_credential_keys() {
        let mut requested = Vec::new();
        let environment = safe_process_environment_with(|key| {
            requested.push(key.to_owned());
            (key == "PATH").then(|| "SAFE-PATH-SENTINEL".to_owned())
        })
        .unwrap();
        let debug = format!("{environment:?}");

        assert!(requested.iter().any(|key| key == "PATH"));
        for forbidden in [
            "XGENY_OPENAI_API_KEY",
            "OPENAI_API_KEY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!requested.iter().any(|key| key == forbidden));
            assert!(!debug.contains(forbidden));
        }
        assert!(!debug.contains("SAFE-PATH-SENTINEL"));
    }
}
