use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_adapter_filesystem::WorkspaceId;
use xgeny_workgraph::{AgentLoopBudget, ModelCallBudget};

const MANIFEST_FORMAT_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_PROFILE_TEXT_BYTES: usize = 512;
const AUTHORITY_PREFIX: &str = "local:xgeny-cli-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunManifest {
    record: RunManifestRecord,
    record_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunManifestRecord {
    format_version: u32,
    run_id: String,
    workspace_id: String,
    workspace_root_identity_profile: String,
    workspace_root_identity_digest: String,
    planner_id: String,
    model: String,
    tokenizer: String,
    request_profile_digest: String,
    model_data_boundary: ModelDataBoundary,
    allow_file_catalog_digest: String,
    local_execution_profile_digest: String,
    budget: ManifestBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModelDataBoundary {
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(clippy::struct_field_names)] // Keep every persisted field explicit about being a maximum.
pub(crate) struct ManifestBudget {
    pub(crate) max_model_turns: u32,
    pub(crate) max_model_calls: u32,
    pub(crate) max_planned_steps: u32,
    pub(crate) max_tool_calls: u32,
    pub(crate) max_context_bytes: u64,
}

impl Default for ManifestBudget {
    fn default() -> Self {
        Self {
            max_model_turns: 2,
            max_model_calls: 4,
            max_planned_steps: 1,
            max_tool_calls: 1,
            max_context_bytes: 512 * 1024,
        }
    }
}

impl ManifestBudget {
    pub(crate) const fn workspace_discovery() -> Self {
        Self {
            max_model_turns: 8,
            max_model_calls: 16,
            max_planned_steps: 8,
            max_tool_calls: 8,
            max_context_bytes: 512 * 1024,
        }
    }

    pub(crate) fn agent_loop(&self) -> Result<AgentLoopBudget, ManifestError> {
        AgentLoopBudget::new(
            self.max_model_turns,
            self.max_planned_steps,
            self.max_tool_calls,
            self.max_context_bytes,
        )
        .map_err(|_| ManifestError::Invalid)
    }

    pub(crate) fn model_calls(&self) -> Result<ModelCallBudget, ManifestError> {
        ModelCallBudget::new(self.max_model_calls).map_err(|_| ManifestError::Invalid)
    }
}

impl RunManifest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        run_id: &str,
        workspace_id: &WorkspaceId,
        workspace_root_identity_profile: &str,
        workspace_root_identity_digest: &str,
        planner_id: &str,
        model: &str,
        tokenizer: &str,
        request_profile_digest: &str,
        allow_file_catalog_digest: &str,
        local_execution_profile_digest: &str,
        budget: ManifestBudget,
    ) -> Result<Self, ManifestError> {
        let record = RunManifestRecord {
            format_version: MANIFEST_FORMAT_VERSION,
            run_id: run_id.to_owned(),
            workspace_id: workspace_id.as_str().to_owned(),
            workspace_root_identity_profile: workspace_root_identity_profile.to_owned(),
            workspace_root_identity_digest: workspace_root_identity_digest.to_owned(),
            planner_id: planner_id.to_owned(),
            model: model.to_owned(),
            tokenizer: tokenizer.to_owned(),
            request_profile_digest: request_profile_digest.to_owned(),
            model_data_boundary: ModelDataBoundary::Remote,
            allow_file_catalog_digest: allow_file_catalog_digest.to_owned(),
            local_execution_profile_digest: local_execution_profile_digest.to_owned(),
            budget,
        };
        validate_record(&record)?;
        let record_digest = digest_record(&record)?;
        Ok(Self {
            record,
            record_digest,
        })
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.is_empty() || bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::Invalid);
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(|_| ManifestError::Invalid)?;
        manifest.verify()?;
        Ok(manifest)
    }

    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        self.verify()?;
        let bytes = serde_jcs::to_vec(self).map_err(|_| ManifestError::Canonicalization)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::Invalid);
        }
        Ok(bytes)
    }

    pub(crate) fn verify(&self) -> Result<(), ManifestError> {
        validate_record(&self.record)?;
        if self.record_digest != digest_record(&self.record)? {
            return Err(ManifestError::DigestMismatch);
        }
        Ok(())
    }

    pub(crate) fn authority(&self) -> String {
        let digest = self
            .record_digest
            .strip_prefix("sha256:")
            .expect("verified manifest digest is canonical");
        format!("{AUTHORITY_PREFIX}{digest}")
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.record.run_id
    }

    pub(crate) fn workspace_id(&self) -> Result<WorkspaceId, ManifestError> {
        WorkspaceId::new(&self.record.workspace_id).map_err(|_| ManifestError::Invalid)
    }

    pub(crate) fn workspace_identity_profile(&self) -> &str {
        &self.record.workspace_root_identity_profile
    }

    pub(crate) fn workspace_identity_digest(&self) -> &str {
        &self.record.workspace_root_identity_digest
    }

    pub(crate) fn planner_id(&self) -> &str {
        &self.record.planner_id
    }

    pub(crate) fn model(&self) -> &str {
        &self.record.model
    }

    pub(crate) fn tokenizer(&self) -> &str {
        &self.record.tokenizer
    }

    pub(crate) fn request_profile_digest(&self) -> &str {
        &self.record.request_profile_digest
    }

    pub(crate) fn allow_file_catalog_digest(&self) -> &str {
        &self.record.allow_file_catalog_digest
    }

    pub(crate) fn local_execution_profile_digest(&self) -> &str {
        &self.record.local_execution_profile_digest
    }

    pub(crate) const fn budget(&self) -> &ManifestBudget {
        &self.record.budget
    }

    pub(crate) const fn remote_model_egress(&self) -> bool {
        matches!(self.record.model_data_boundary, ModelDataBoundary::Remote)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManifestError {
    #[error("run manifest is invalid")]
    Invalid,
    #[error("run manifest digest does not match")]
    DigestMismatch,
    #[error("run manifest could not be canonicalized")]
    Canonicalization,
}

fn validate_record(record: &RunManifestRecord) -> Result<(), ManifestError> {
    if record.format_version != MANIFEST_FORMAT_VERSION
        || !valid_run_id(&record.run_id)
        || WorkspaceId::new(&record.workspace_id).is_err()
        || !valid_identifier(&record.workspace_root_identity_profile, 128)
        || !valid_sha256_digest(&record.workspace_root_identity_digest)
        || !valid_identifier(&record.planner_id, 256)
        || !valid_profile_text(&record.model)
        || !valid_profile_text(&record.tokenizer)
        || !valid_sha256_digest(&record.request_profile_digest)
        || !valid_sha256_digest(&record.allow_file_catalog_digest)
        || !valid_sha256_digest(&record.local_execution_profile_digest)
        || record.budget.agent_loop().is_err()
        || record.budget.model_calls().is_err()
        || record.budget.max_model_calls < record.budget.max_model_turns
    {
        return Err(ManifestError::Invalid);
    }
    Ok(())
}

pub(crate) fn valid_run_id(value: &str) -> bool {
    value.len() == 36
        && value.strip_prefix("run-").is_some_and(|encoded| {
            encoded.len() == 32
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn valid_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_profile_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PROFILE_TEXT_BYTES
        && !value.chars().any(char::is_control)
}

fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn digest_record(record: &RunManifestRecord) -> Result<String, ManifestError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct DigestInput<'a> {
        domain: &'static str,
        record: &'a RunManifestRecord,
    }

    let canonical = serde_jcs::to_vec(&DigestInput {
        domain: "xgeny.cli.run-manifest/v1",
        record,
    })
    .map_err(|_| ManifestError::Canonicalization)?;
    Ok(format!("sha256:{}", sha256_hex(&canonical)))
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
    use serde_json::Value;

    use super::*;

    const DIGEST_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST_C: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const DIGEST_D: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn fixture() -> RunManifest {
        RunManifest::new(
            "run-0123456789abcdef0123456789abcdef",
            &WorkspaceId::new("ws-0123456789abcdef").unwrap(),
            "xgeny.workspace-root.unix-file-id.v1",
            DIGEST_A,
            "xgeny.cli.openai",
            "qwen3.8-27b",
            "Qwen-Qwen3.8-27B-FP8",
            DIGEST_B,
            DIGEST_C,
            DIGEST_D,
            ManifestBudget::default(),
        )
        .expect("manifest should construct")
    }

    #[test]
    fn canonical_round_trip_binds_authority() {
        let manifest = fixture();
        let bytes = manifest.to_bytes().expect("manifest should serialize");
        let loaded = RunManifest::from_bytes(&bytes).expect("manifest should load");
        assert_eq!(loaded, manifest);
        assert!(loaded.authority().starts_with(AUTHORITY_PREFIX));
        assert_eq!(loaded.authority().len(), AUTHORITY_PREFIX.len() + 64);
    }

    #[test]
    fn tampering_and_unknown_fields_fail_closed() {
        let manifest = fixture();
        let mut value: Value = serde_json::from_slice(&manifest.to_bytes().unwrap()).unwrap();
        value["record"]["model"] = Value::String("changed-model".to_owned());
        assert_eq!(
            RunManifest::from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            ManifestError::DigestMismatch
        );
        value["unknown"] = Value::Bool(true);
        assert_eq!(
            RunManifest::from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            ManifestError::Invalid
        );
    }

    #[test]
    fn manifest_excludes_endpoint_paths_goal_and_credentials() {
        let text = String::from_utf8(fixture().to_bytes().unwrap()).unwrap();
        for forbidden in [
            "http://127.0.0.1:18000",
            "/home/user/workspace",
            "README.md",
            "read the secret file",
            "Bearer",
        ] {
            assert!(!text.contains(forbidden));
        }
        assert!(text.contains("remote"));
    }
}
