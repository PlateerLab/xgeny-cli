use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::Arc;

use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsSyncExt as _};
use cap_std::fs::{Dir, File, OpenOptions};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use xgeny_domain::{InstanceFeatures, VerificationResult, VerificationStrategy};
use xgeny_runtime::{
    AdapterEvidenceDigest, AdapterExecutionObservation, AdapterPrepareFailure,
    AdapterPrepareRequest, AdapterReconcileRequest, AdapterReconciliationInconclusiveReason,
    AdapterReconciliationObservation, AdapterToolOutput, EffectAdapter, EffectVerifier,
    PreparedAdapterInvocation, RuleVerificationObservation, VerificationPortFailure,
    VerificationReport, VerificationRequest, VerifiedArtifactDescriptor, VerifierOutputDigest,
};
use xgeny_workgraph::EffectClass;

use crate::path::{RelativePath, canonical_resource, parse_canonical};
use crate::{
    LIST_DIRECTORY_CAPABILITY_ID, LIST_DIRECTORY_CONTRACT_VERSION, SEARCH_TEXT_CAPABILITY_ID,
    SEARCH_TEXT_CONTRACT_VERSION, STAT_CAPABILITY_ID, STAT_CONTRACT_VERSION, WorkspaceRoot,
};

/// Maximum entries returned by one list-directory observation.
pub const MAX_LIST_DIRECTORY_ENTRIES: usize = 512;
/// Maximum portable entries scanned in one directory before the operation fails closed.
pub const MAX_DIRECTORY_SCAN_ENTRIES: usize = 4_096;
/// Maximum literal query size accepted by search-text.
pub const MAX_SEARCH_QUERY_BYTES: usize = 256;
/// Maximum Unicode scalar values accepted by the public search schema and host validation.
pub const MAX_SEARCH_QUERY_UNICODE_SCALARS: usize = 64;
/// Maximum matches returned by one search-text observation.
pub const MAX_SEARCH_MATCHES: usize = 128;
/// Maximum entries visited recursively by one search-text observation.
pub const MAX_SEARCH_VISITED_ENTRIES: usize = 4_096;
/// Maximum bytes read from one candidate search file.
pub const MAX_SEARCH_FILE_BYTES: usize = 256 * 1024;
/// Maximum aggregate candidate bytes read by one search-text observation.
pub const MAX_SEARCH_TOTAL_BYTES: usize = 8 * 1024 * 1024;
/// Maximum UTF-8 bytes retained in one match preview.
pub const MAX_SEARCH_PREVIEW_BYTES: usize = 512;
/// Maximum canonical JCS bytes emitted by one list/stat/search tool output, including its digest.
pub const MAX_QUERY_OUTPUT_CANONICAL_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryOperation {
    ListDirectory,
    Stat,
    SearchText,
}

impl QueryOperation {
    const fn capability_id(self) -> &'static str {
        match self {
            Self::ListDirectory => LIST_DIRECTORY_CAPABILITY_ID,
            Self::Stat => STAT_CAPABILITY_ID,
            Self::SearchText => SEARCH_TEXT_CAPABILITY_ID,
        }
    }

    const fn contract_version(self) -> &'static str {
        match self {
            Self::ListDirectory => LIST_DIRECTORY_CONTRACT_VERSION,
            Self::Stat => STAT_CONTRACT_VERSION,
            Self::SearchText => SEARCH_TEXT_CONTRACT_VERSION,
        }
    }

    const fn artifact_id(self) -> &'static str {
        match self {
            Self::ListDirectory => "list-directory-output",
            Self::Stat => "stat-output",
            Self::SearchText => "search-text-output",
        }
    }

    const fn failure_domain(self) -> &'static str {
        match self {
            Self::ListDirectory => "xgeny.fs/list-directory/failure/v1",
            Self::Stat => "xgeny.fs/stat/failure/v1",
            Self::SearchText => "xgeny.fs/search-text/failure/v1",
        }
    }
}

/// Exact root-bound adapter for one bounded filesystem observation.
#[derive(Clone)]
pub struct FilesystemQueryAdapter {
    root: WorkspaceRoot,
    operation: QueryOperation,
}

impl FilesystemQueryAdapter {
    pub(crate) const fn list_directory(root: WorkspaceRoot) -> Self {
        Self {
            root,
            operation: QueryOperation::ListDirectory,
        }
    }

    pub(crate) const fn stat(root: WorkspaceRoot) -> Self {
        Self {
            root,
            operation: QueryOperation::Stat,
        }
    }

    pub(crate) const fn search_text(root: WorkspaceRoot) -> Self {
        Self {
            root,
            operation: QueryOperation::SearchText,
        }
    }

    /// Construct a verifier pinned to the same workspace and exact operation binding.
    #[must_use]
    pub fn verifier(&self) -> FilesystemQueryVerifier {
        FilesystemQueryVerifier {
            workspace_id: self.root.workspace_id.clone(),
            expected_binding: binding_for(&self.root, self.operation),
            operation: self.operation,
        }
    }
}

impl std::fmt::Debug for FilesystemQueryAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FilesystemQueryAdapter")
            .field("root", &"<preopened/redacted>")
            .field("operation", &self.operation)
            .finish()
    }
}

impl EffectAdapter for FilesystemQueryAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        let binding = binding_for(&self.root, self.operation);
        verify_contract(&request, &binding, self.operation)?;
        let arguments =
            parse_arguments(request.normalized_arguments(), &self.root, self.operation)?;
        Ok(Box::new(PreparedFilesystemQuery {
            directory: Arc::clone(&self.root.directory),
            relative_path: arguments.path,
            query: arguments.query,
            operation: self.operation,
        }))
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

/// Verifier for one exact filesystem query operation.
pub struct FilesystemQueryVerifier {
    workspace_id: crate::WorkspaceId,
    expected_binding: xgeny_domain::InstanceBinding,
    operation: QueryOperation,
}

impl std::fmt::Debug for FilesystemQueryVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FilesystemQueryVerifier")
            .field("workspace_id", &"<redacted>")
            .field("expected_binding", &"<redacted>")
            .field("operation", &self.operation)
            .finish()
    }
}

impl EffectVerifier for FilesystemQueryVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        verify_verifier_contract(&request, &self.expected_binding, self.operation)?;
        let output = request
            .tool_output()
            .ok_or(VerificationPortFailure::EvidenceUnavailable)?;
        let inspected = inspect_output(
            output.output(),
            request.outcome_evidence_digest().as_str(),
            &self.workspace_id,
            self.operation,
        )?;
        let evidence = AdapterEvidenceDigest::new(inspected.digest.clone())
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        let rules = request
            .definition()
            .spec
            .verification
            .iter()
            .map(|rule| {
                let result = match rule.strategy {
                    VerificationStrategy::OutputSchema => VerificationResult::Passed,
                    VerificationStrategy::Postcondition if inspected.postcondition => {
                        VerificationResult::Passed
                    }
                    VerificationStrategy::Postcondition => VerificationResult::Failed,
                    _ => return Err(VerificationPortFailure::UnsupportedStrategy),
                };
                Ok(RuleVerificationObservation::new(
                    rule.strategy,
                    result,
                    Some(
                        AdapterEvidenceDigest::new(evidence.as_str().to_owned())
                            .expect("validated digest remains canonical"),
                    ),
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let artifact = VerifiedArtifactDescriptor::new(
            self.operation.artifact_id(),
            Option::<String>::None,
            "application/json",
            inspected.byte_size,
            inspected.digest,
        )
        .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        VerificationReport::new(
            VerifierOutputDigest::new(output.output_digest().to_owned())
                .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?,
            rules,
        )
        .with_artifacts(vec![artifact])
        .map_err(|_| VerificationPortFailure::ResponseUnverifiable)
    }
}

struct PreparedFilesystemQuery {
    directory: Arc<Dir>,
    relative_path: RelativePath,
    query: Option<String>,
    operation: QueryOperation,
}

impl std::fmt::Debug for PreparedFilesystemQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedFilesystemQuery")
            .field("directory", &"<preopened/redacted>")
            .field("relative_path", &"<redacted>")
            .field("query", &self.query.as_ref().map(|_| "<redacted>"))
            .field("operation", &self.operation)
            .finish()
    }
}

impl PreparedAdapterInvocation for PreparedFilesystemQuery {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        let observation = match self.operation {
            QueryOperation::ListDirectory => list_directory(&self.directory, &self.relative_path),
            QueryOperation::Stat => stat_path(&self.directory, &self.relative_path),
            QueryOperation::SearchText => search_text(
                &self.directory,
                &self.relative_path,
                self.query
                    .as_deref()
                    .expect("search preparation always retains a query"),
            ),
        };
        match observation.and_then(ObservedOutput::finish) {
            Ok(observation) => AdapterExecutionObservation::SucceededWithOutput {
                evidence_digest: AdapterEvidenceDigest::new(observation.digest)
                    .expect("SHA-256 output digest is canonical"),
                output: AdapterToolOutput::new(observation.output),
            },
            Err(failure) => AdapterExecutionObservation::Failed {
                evidence_digest: AdapterEvidenceDigest::new(
                    failure.evidence_digest(self.operation),
                )
                .expect("SHA-256 failure digest is canonical"),
            },
        }
    }
}

struct ParsedArguments {
    path: RelativePath,
    query: Option<String>,
}

struct ObservedOutput {
    payload: Value,
}

struct FinishedOutput {
    output: Value,
    digest: String,
}

impl ObservedOutput {
    fn finish(self) -> Result<FinishedOutput, QueryFailure> {
        let canonical = serde_jcs::to_vec(&self.payload).map_err(|_| QueryFailure::Encode)?;
        let digest = sha256_digest(&canonical);
        let mut object = self
            .payload
            .as_object()
            .cloned()
            .ok_or(QueryFailure::Encode)?;
        object.insert("digest".to_owned(), Value::String(digest.clone()));
        let output = Value::Object(object);
        if serde_jcs::to_vec(&output)
            .map_err(|_| QueryFailure::Encode)?
            .len()
            > MAX_QUERY_OUTPUT_CANONICAL_BYTES
        {
            return Err(QueryFailure::Limit);
        }
        Ok(FinishedOutput { output, digest })
    }
}

fn bounded_collection_output(
    field: &'static str,
    candidates: Vec<Value>,
    mut truncated: bool,
) -> Result<ObservedOutput, QueryFailure> {
    let base = collection_output_size(field, &[], false)?;
    let mut projected_size = base;
    let mut retained = Vec::new();
    for candidate in candidates {
        let candidate_size = serde_jcs::to_vec(&candidate)
            .map_err(|_| QueryFailure::Encode)?
            .len();
        let separator = usize::from(!retained.is_empty());
        let Some(next_size) = projected_size
            .checked_add(separator)
            .and_then(|size| size.checked_add(candidate_size))
        else {
            truncated = true;
            break;
        };
        if next_size > MAX_QUERY_OUTPUT_CANONICAL_BYTES {
            truncated = true;
            break;
        }
        retained.push(candidate);
        projected_size = next_size;
    }
    let payload = collection_payload(field, retained, truncated);
    if collection_output_size_from_payload(&payload)? > MAX_QUERY_OUTPUT_CANONICAL_BYTES {
        return Err(QueryFailure::Limit);
    }
    Ok(ObservedOutput { payload })
}

fn collection_payload(field: &'static str, values: Vec<Value>, truncated: bool) -> Value {
    let mut payload = Map::new();
    payload.insert(field.to_owned(), Value::Array(values));
    payload.insert("truncated".to_owned(), Value::Bool(truncated));
    Value::Object(payload)
}

fn collection_output_size(
    field: &'static str,
    values: &[Value],
    truncated: bool,
) -> Result<usize, QueryFailure> {
    collection_output_size_from_payload(&collection_payload(field, values.to_vec(), truncated))
}

fn collection_output_size_from_payload(payload: &Value) -> Result<usize, QueryFailure> {
    let canonical = serde_jcs::to_vec(payload).map_err(|_| QueryFailure::Encode)?;
    let digest = sha256_digest(&canonical);
    let mut object = payload.as_object().cloned().ok_or(QueryFailure::Encode)?;
    object.insert("digest".to_owned(), Value::String(digest));
    serde_jcs::to_vec(&Value::Object(object))
        .map(|canonical| canonical.len())
        .map_err(|_| QueryFailure::Encode)
}

struct InspectedOutput {
    digest: String,
    byte_size: u64,
    postcondition: bool,
}

#[derive(Debug, Clone, Copy)]
enum QueryFailure {
    Open,
    Read,
    TooManyEntries,
    Limit,
    Encode,
}

impl QueryFailure {
    fn evidence_digest(self, operation: QueryOperation) -> String {
        let code = match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::TooManyEntries => "too-many-entries",
            Self::Limit => "limit",
            Self::Encode => "encode",
        };
        sha256_digest(format!("{}/{code}", operation.failure_domain()).as_bytes())
    }
}

fn binding_for(root: &WorkspaceRoot, operation: QueryOperation) -> xgeny_domain::InstanceBinding {
    match operation {
        QueryOperation::ListDirectory => root.list_directory_binding(),
        QueryOperation::Stat => root.stat_binding(),
        QueryOperation::SearchText => root.search_text_binding(),
    }
}

fn verify_contract(
    request: &AdapterPrepareRequest<'_>,
    expected_binding: &xgeny_domain::InstanceBinding,
    operation: QueryOperation,
) -> Result<(), AdapterPrepareFailure> {
    let intent = request.intent();
    let instance = request.instance();
    if intent.invocation.capability_id != operation.capability_id()
        || intent.invocation.contract_version != operation.contract_version()
        || instance.definition.capability_id != operation.capability_id()
        || instance.definition.contract_version != operation.contract_version()
        || intent.invocation.instance_id != instance.instance_id
        || instance.binding != *expected_binding
        || !supports_instance_features(&instance.features)
        || intent.effect_class != EffectClass::ReadOnly
        || intent.idempotency_key.is_some()
    {
        return Err(AdapterPrepareFailure::UnsupportedProtocol);
    }
    Ok(())
}

fn verify_verifier_contract(
    request: &VerificationRequest<'_>,
    expected_binding: &xgeny_domain::InstanceBinding,
    operation: QueryOperation,
) -> Result<(), VerificationPortFailure> {
    let rules = &request.definition().spec.verification;
    let intent = request.intent();
    let instance = request.instance();
    if intent.invocation.capability_id != operation.capability_id()
        || intent.invocation.contract_version != operation.contract_version()
        || instance.definition.capability_id != operation.capability_id()
        || instance.definition.contract_version != operation.contract_version()
        || intent.invocation.instance_id != instance.instance_id
        || instance.binding != *expected_binding
        || !supports_instance_features(&instance.features)
        || intent.effect_class != EffectClass::ReadOnly
        || intent.idempotency_key.is_some()
        || rules.len() != 2
        || rules.iter().any(|rule| {
            !rule.required
                || !matches!(
                    rule.strategy,
                    VerificationStrategy::OutputSchema | VerificationStrategy::Postcondition
                )
        })
        || rules
            .iter()
            .filter(|rule| rule.strategy == VerificationStrategy::OutputSchema)
            .count()
            != 1
        || rules
            .iter()
            .filter(|rule| rule.strategy == VerificationStrategy::Postcondition)
            .count()
            != 1
    {
        return Err(VerificationPortFailure::UnsupportedStrategy);
    }
    Ok(())
}

const fn supports_instance_features(features: &InstanceFeatures) -> bool {
    features.sync && !features.task && !features.cancellable && !features.idempotency_query
}

fn parse_arguments(
    arguments: &Value,
    root: &WorkspaceRoot,
    operation: QueryOperation,
) -> Result<ParsedArguments, AdapterPrepareFailure> {
    let object = arguments
        .as_object()
        .filter(|object| {
            object.len()
                == if operation == QueryOperation::SearchText {
                    2
                } else {
                    1
                }
        })
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let path = parse_canonical(&root.workspace_id, path)
        .map_err(|_| AdapterPrepareFailure::InvalidMaterial)?;
    let query = if operation == QueryOperation::SearchText {
        let query = object
            .get("query")
            .and_then(Value::as_str)
            .filter(|query| {
                !query.is_empty()
                    && query.len() <= MAX_SEARCH_QUERY_BYTES
                    && query.chars().count() <= MAX_SEARCH_QUERY_UNICODE_SCALARS
                    && !query.chars().any(char::is_control)
            })
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        Some(query.to_owned())
    } else {
        None
    };
    Ok(ParsedArguments { path, query })
}

fn list_directory(root: &Dir, path: &RelativePath) -> Result<ObservedOutput, QueryFailure> {
    let directory = open_directory(root, path)?;
    let mut entries = Vec::new();
    let mut incomplete = false;
    let mut scanned = 0_usize;
    for candidate in directory.entries().map_err(|_| QueryFailure::Read)? {
        scanned = scanned.checked_add(1).ok_or(QueryFailure::TooManyEntries)?;
        if scanned > MAX_DIRECTORY_SCAN_ENTRIES {
            return Err(QueryFailure::TooManyEntries);
        }
        let candidate = candidate.map_err(|_| QueryFailure::Read)?;
        let Ok(name) = candidate.file_name().into_string() else {
            incomplete = true;
            continue;
        };
        let Ok(child) = path.join(&name) else {
            incomplete = true;
            continue;
        };
        let file_type = candidate.file_type().map_err(|_| QueryFailure::Read)?;
        let (kind, size_bytes) = if file_type.is_symlink() {
            ("other", Value::Null)
        } else if file_type.is_dir() {
            if open_directory(root, &child).is_ok() {
                ("directory", Value::Null)
            } else {
                incomplete = true;
                ("other", Value::Null)
            }
        } else if file_type.is_file() {
            if let Ok(metadata) = open_regular_file(root, &child)
                .and_then(|file| file.metadata().map_err(|_| QueryFailure::Open))
            {
                ("file", json!(metadata.len()))
            } else {
                incomplete = true;
                ("other", Value::Null)
            }
        } else {
            ("other", Value::Null)
        };
        entries.push(json!({
            "name": name,
            "path": child.display(),
            "kind": kind,
            "sizeBytes": size_bytes,
        }));
    }
    entries.sort_by(|left, right| {
        left["path"]
            .as_str()
            .expect("constructed path is a string")
            .cmp(
                right["path"]
                    .as_str()
                    .expect("constructed path is a string"),
            )
    });
    if entries.len() > MAX_LIST_DIRECTORY_ENTRIES {
        entries.truncate(MAX_LIST_DIRECTORY_ENTRIES);
        incomplete = true;
    }
    bounded_collection_output("entries", entries, incomplete)
}

fn stat_path(root: &Dir, path: &RelativePath) -> Result<ObservedOutput, QueryFailure> {
    if path.components().is_empty() {
        open_directory(root, path)?;
        return Ok(ObservedOutput {
            payload: json!({"kind": "directory", "sizeBytes": null}),
        });
    }
    if let Ok(file) = open_regular_file(root, path) {
        let metadata = file.metadata().map_err(|_| QueryFailure::Open)?;
        return Ok(ObservedOutput {
            payload: json!({"kind": "file", "sizeBytes": metadata.len()}),
        });
    }
    open_directory(root, path)?;
    Ok(ObservedOutput {
        payload: json!({"kind": "directory", "sizeBytes": null}),
    })
}

fn search_text(
    root: &Dir,
    path: &RelativePath,
    query: &str,
) -> Result<ObservedOutput, QueryFailure> {
    open_directory(root, path)?;
    let mut state = SearchState {
        matches: Vec::new(),
        visited_entries: 0,
        scanned_bytes: 0,
        truncated: false,
        exhausted: false,
    };
    search_directory(root, path, query, &mut state)?;
    state.matches.sort_by(|left, right| {
        let left = search_match_key(left);
        let right = search_match_key(right);
        left.cmp(&right)
    });
    bounded_collection_output("matches", state.matches, state.truncated)
}

fn search_match_key(candidate: &Value) -> (&str, u64, u64) {
    (
        candidate["path"]
            .as_str()
            .expect("constructed search path is a string"),
        candidate["line"]
            .as_u64()
            .expect("constructed search line is an unsigned integer"),
        candidate["column"]
            .as_u64()
            .expect("constructed search column is an unsigned integer"),
    )
}

struct SearchState {
    matches: Vec<Value>,
    visited_entries: usize,
    scanned_bytes: usize,
    truncated: bool,
    exhausted: bool,
}

fn search_directory(
    root: &Dir,
    path: &RelativePath,
    query: &str,
    state: &mut SearchState,
) -> Result<(), QueryFailure> {
    if state.exhausted
        || state.matches.len() == MAX_SEARCH_MATCHES
        || state.visited_entries == MAX_SEARCH_VISITED_ENTRIES
    {
        state.truncated = true;
        return Ok(());
    }
    let directory = open_directory(root, path)?;
    let mut names = Vec::new();
    let mut scanned = 0_usize;
    for candidate in directory.entries().map_err(|_| QueryFailure::Read)? {
        if state.visited_entries == MAX_SEARCH_VISITED_ENTRIES {
            state.truncated = true;
            break;
        }
        state.visited_entries += 1;
        scanned = scanned.checked_add(1).ok_or(QueryFailure::TooManyEntries)?;
        if scanned > MAX_DIRECTORY_SCAN_ENTRIES {
            return Err(QueryFailure::TooManyEntries);
        }
        let candidate = candidate.map_err(|_| QueryFailure::Read)?;
        let Ok(name) = candidate.file_name().into_string() else {
            state.truncated = true;
            continue;
        };
        if path.join(&name).is_err() {
            state.truncated = true;
            continue;
        }
        names.push(name);
    }
    names.sort();

    for name in names {
        if state.exhausted || state.matches.len() == MAX_SEARCH_MATCHES {
            state.truncated = true;
            break;
        }
        let child = path.join(&name).map_err(|_| QueryFailure::Read)?;
        if matches!(name.as_str(), ".git" | ".hg" | ".svn") {
            state.truncated = true;
            continue;
        }
        if open_directory(root, &child).is_ok() {
            search_directory(root, &child, query, state)?;
            continue;
        }
        search_regular_file(root, &child, query, state)?;
        if state.exhausted {
            break;
        }
    }
    Ok(())
}

fn search_regular_file(
    root: &Dir,
    path: &RelativePath,
    query: &str,
    state: &mut SearchState,
) -> Result<(), QueryFailure> {
    let Ok(mut file) = open_regular_file(root, path) else {
        state.truncated = true;
        return Ok(());
    };
    let metadata = file.metadata().map_err(|_| QueryFailure::Open)?;
    if metadata.len() > u64::try_from(MAX_SEARCH_FILE_BYTES).expect("limit fits u64") {
        state.truncated = true;
        return Ok(());
    }
    let remaining = MAX_SEARCH_TOTAL_BYTES.saturating_sub(state.scanned_bytes);
    if remaining == 0
        || metadata.len() > u64::try_from(remaining).expect("aggregate limit fits u64")
    {
        state.truncated = true;
        state.exhausted = true;
        return Ok(());
    }
    let read_limit = remaining.min(MAX_SEARCH_FILE_BYTES.saturating_add(1));
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(MAX_SEARCH_FILE_BYTES)
            .min(MAX_SEARCH_FILE_BYTES)
            .saturating_add(1),
    );
    file.by_ref()
        .take(u64::try_from(read_limit).expect("read limit fits u64"))
        .read_to_end(&mut bytes)
        .map_err(|_| QueryFailure::Read)?;
    state.scanned_bytes = state
        .scanned_bytes
        .checked_add(bytes.len())
        .ok_or(QueryFailure::Limit)?;
    if state.scanned_bytes == MAX_SEARCH_TOTAL_BYTES {
        state.truncated = true;
        state.exhausted = true;
    }
    if bytes.len() > MAX_SEARCH_FILE_BYTES {
        state.truncated = true;
        return Ok(());
    }
    let Ok(content) = String::from_utf8(bytes) else {
        state.truncated = true;
        return Ok(());
    };
    collect_matches(path, &content, query, state)
}

fn collect_matches(
    path: &RelativePath,
    content: &str,
    query: &str,
    state: &mut SearchState,
) -> Result<(), QueryFailure> {
    for (line_index, line) in content.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        for (byte_index, matched) in line.match_indices(query) {
            if state.matches.len() == MAX_SEARCH_MATCHES {
                state.truncated = true;
                return Ok(());
            }
            let line_number = u64::try_from(line_index + 1).map_err(|_| QueryFailure::Limit)?;
            let column = u64::try_from(line[..byte_index].chars().count() + 1)
                .map_err(|_| QueryFailure::Limit)?;
            state.matches.push(json!({
                "path": path.display(),
                "line": line_number,
                "column": column,
                "preview": bounded_preview(line, byte_index, byte_index + matched.len()),
            }));
        }
    }
    Ok(())
}

fn bounded_preview(line: &str, match_start: usize, match_end: usize) -> String {
    if line.len() <= MAX_SEARCH_PREVIEW_BYTES {
        return line.to_owned();
    }
    let match_len = match_end.saturating_sub(match_start);
    let margin = MAX_SEARCH_PREVIEW_BYTES.saturating_sub(match_len) / 2;
    let mut start = match_start.saturating_sub(margin);
    while start > 0 && !line.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = start
        .saturating_add(MAX_SEARCH_PREVIEW_BYTES)
        .min(line.len());
    while end > match_end && !line.is_char_boundary(end) {
        end -= 1;
    }
    if end < match_end {
        end = match_end;
        while start > 0 && end.saturating_sub(start) > MAX_SEARCH_PREVIEW_BYTES {
            start += 1;
            while start < match_start && !line.is_char_boundary(start) {
                start += 1;
            }
        }
    }
    line[start..end].to_owned()
}

fn open_directory(root: &Dir, path: &RelativePath) -> Result<Dir, QueryFailure> {
    let mut directory = root.try_clone().map_err(|_| QueryFailure::Open)?;
    for component in path.components() {
        directory = directory
            .open_dir_nofollow(component)
            .map_err(|_| QueryFailure::Open)?;
        if is_windows_reparse_point(&directory.dir_metadata().map_err(|_| QueryFailure::Open)?) {
            return Err(QueryFailure::Open);
        }
    }
    Ok(directory)
}

fn open_regular_file(root: &Dir, path: &RelativePath) -> Result<File, QueryFailure> {
    let (leaf, parents) = path.components().split_last().ok_or(QueryFailure::Open)?;
    let parent = RelativePath::from_components(parents.to_vec());
    let directory = open_directory(root, &parent)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = directory
        .open_with(leaf, &options)
        .map_err(|_| QueryFailure::Open)?;
    let metadata = file.metadata().map_err(|_| QueryFailure::Open)?;
    if !metadata.is_file() || is_windows_reparse_point(&metadata) {
        return Err(QueryFailure::Open);
    }
    Ok(file)
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn is_windows_reparse_point(_metadata: &cap_std::fs::Metadata) -> bool {
    false
}

fn inspect_output(
    output: &Value,
    outcome_evidence_digest: &str,
    workspace_id: &crate::WorkspaceId,
    operation: QueryOperation,
) -> Result<InspectedOutput, VerificationPortFailure> {
    if serde_jcs::to_vec(output)
        .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?
        .len()
        > MAX_QUERY_OUTPUT_CANONICAL_BYTES
    {
        return Err(VerificationPortFailure::ResponseUnverifiable);
    }
    let object = output
        .as_object()
        .filter(|object| object.len() == 3)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let claimed = object
        .get("digest")
        .and_then(Value::as_str)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    AdapterEvidenceDigest::new(claimed.to_owned())
        .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
    validate_output_shape(object, workspace_id, operation)?;
    let mut payload = object.clone();
    payload.remove("digest");
    let canonical = serde_jcs::to_vec(&Value::Object(payload))
        .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
    let digest = sha256_digest(&canonical);
    Ok(InspectedOutput {
        postcondition: claimed == digest && outcome_evidence_digest == digest,
        byte_size: u64::try_from(canonical.len())
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?,
        digest,
    })
}

fn validate_output_shape(
    object: &Map<String, Value>,
    workspace_id: &crate::WorkspaceId,
    operation: QueryOperation,
) -> Result<(), VerificationPortFailure> {
    match operation {
        QueryOperation::ListDirectory => validate_list_output(object, workspace_id),
        QueryOperation::Stat => validate_stat_output(object),
        QueryOperation::SearchText => validate_search_output(object, workspace_id),
    }
}

fn validate_list_output(
    object: &Map<String, Value>,
    workspace_id: &crate::WorkspaceId,
) -> Result<(), VerificationPortFailure> {
    object
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let entries = object
        .get("entries")
        .and_then(Value::as_array)
        .filter(|entries| entries.len() <= MAX_LIST_DIRECTORY_ENTRIES)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let mut previous = None;
    for entry in entries {
        let entry = entry
            .as_object()
            .filter(|entry| entry.len() == 4)
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        let canonical = canonical_resource(workspace_id, path)
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        let parsed = parse_canonical(workspace_id, &canonical)
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        if parsed.display() != path
            || parsed.components().last().map(String::as_str) != Some(name)
            || previous.is_some_and(|previous: &str| previous >= path)
        {
            return Err(VerificationPortFailure::ResponseUnverifiable);
        }
        previous = Some(path);
        let kind = entry
            .get("kind")
            .and_then(Value::as_str)
            .filter(|kind| matches!(*kind, "file" | "directory" | "other"))
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        let size = entry
            .get("sizeBytes")
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        if (kind == "file" && size.as_u64().is_none()) || (kind != "file" && !size.is_null()) {
            return Err(VerificationPortFailure::ResponseUnverifiable);
        }
    }
    Ok(())
}

fn validate_stat_output(object: &Map<String, Value>) -> Result<(), VerificationPortFailure> {
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| matches!(*kind, "file" | "directory"))
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let size = object
        .get("sizeBytes")
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    if (kind == "file" && size.as_u64().is_none()) || (kind == "directory" && !size.is_null()) {
        return Err(VerificationPortFailure::ResponseUnverifiable);
    }
    Ok(())
}

fn validate_search_output(
    object: &Map<String, Value>,
    workspace_id: &crate::WorkspaceId,
) -> Result<(), VerificationPortFailure> {
    object
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let matches = object
        .get("matches")
        .and_then(Value::as_array)
        .filter(|matches| matches.len() <= MAX_SEARCH_MATCHES)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let mut previous: Option<(String, u64, u64)> = None;
    for candidate in matches {
        let candidate = candidate
            .as_object()
            .filter(|candidate| candidate.len() == 4)
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        let path = candidate
            .get("path")
            .and_then(Value::as_str)
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        let canonical = canonical_resource(workspace_id, path)
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        let parsed = parse_canonical(workspace_id, &canonical)
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        if parsed.components().is_empty() || parsed.display() != path {
            return Err(VerificationPortFailure::ResponseUnverifiable);
        }
        let line = candidate
            .get("line")
            .and_then(Value::as_u64)
            .filter(|line| *line > 0)
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        let column = candidate
            .get("column")
            .and_then(Value::as_u64)
            .filter(|column| *column > 0)
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        candidate
            .get("preview")
            .and_then(Value::as_str)
            .filter(|preview| preview.len() <= MAX_SEARCH_PREVIEW_BYTES)
            .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
        let key = (path.to_owned(), line, column);
        if previous.as_ref().is_some_and(|previous| previous >= &key) {
            return Err(VerificationPortFailure::ResponseUnverifiable);
        }
        previous = Some(key);
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::{WorkspaceId, WorkspaceRoot};

    struct Fixture {
        root: WorkspaceRoot,
        directory: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempdir().expect("temporary workspace should exist");
            let root = WorkspaceRoot::open_ambient(
                directory.path(),
                WorkspaceId::new("fixture").expect("workspace ID should validate"),
            )
            .expect("workspace should open");
            Self { root, directory }
        }

        fn relative(&self, path: &str) -> RelativePath {
            let canonical = canonical_resource(&self.root.workspace_id, path)
                .expect("fixture path should resolve");
            parse_canonical(&self.root.workspace_id, &canonical).expect("fixture path should parse")
        }
    }

    #[test]
    fn list_directory_is_sorted_bounded_and_root_relative() {
        let fixture = Fixture::new();
        fs::write(fixture.directory.path().join("z.txt"), "z").unwrap();
        fs::create_dir(fixture.directory.path().join("src")).unwrap();
        fs::write(fixture.directory.path().join("a.txt"), "alpha").unwrap();

        let output = list_directory(&fixture.root.directory, &fixture.relative("."))
            .unwrap()
            .finish()
            .unwrap()
            .output;

        assert_eq!(output["entries"][0]["path"], "a.txt");
        assert_eq!(output["entries"][0]["sizeBytes"], 5);
        assert_eq!(output["entries"][1]["path"], "src");
        assert_eq!(output["entries"][1]["kind"], "directory");
        assert_eq!(output["entries"][2]["path"], "z.txt");
        assert_eq!(output["truncated"], false);
        assert!(output["digest"].as_str().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn stat_reports_only_file_size_or_directory_kind() {
        let fixture = Fixture::new();
        fs::write(fixture.directory.path().join("file.txt"), "hello").unwrap();
        fs::create_dir(fixture.directory.path().join("dir")).unwrap();

        let file = stat_path(&fixture.root.directory, &fixture.relative("file.txt"))
            .unwrap()
            .finish()
            .unwrap()
            .output;
        let directory = stat_path(&fixture.root.directory, &fixture.relative("dir"))
            .unwrap()
            .finish()
            .unwrap()
            .output;
        let root = stat_path(&fixture.root.directory, &fixture.relative("."))
            .unwrap()
            .finish()
            .unwrap()
            .output;

        assert_eq!(file["kind"], "file");
        assert_eq!(file["sizeBytes"], 5);
        assert_eq!(directory["kind"], "directory");
        assert_eq!(directory["sizeBytes"], Value::Null);
        assert_eq!(root["kind"], "directory");
    }

    #[test]
    fn search_is_recursive_literal_utf8_and_deterministic() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.directory.path().join("src/nested")).unwrap();
        fs::write(
            fixture.directory.path().join("src/lib.rs"),
            "zero\nneedle here\nneedle again",
        )
        .unwrap();
        fs::write(
            fixture.directory.path().join("src/nested/mod.rs"),
            "앞 needle 뒤",
        )
        .unwrap();
        fs::write(
            fixture.directory.path().join("src/binary.bin"),
            [0xff, 0xfe],
        )
        .unwrap();

        let output = search_text(&fixture.root.directory, &fixture.relative("src"), "needle")
            .unwrap()
            .finish()
            .unwrap()
            .output;

        assert_eq!(output["matches"].as_array().unwrap().len(), 3);
        assert_eq!(output["matches"][0]["path"], "src/lib.rs");
        assert_eq!(output["matches"][0]["line"], 2);
        assert_eq!(output["matches"][0]["column"], 1);
        assert_eq!(output["matches"][2]["path"], "src/nested/mod.rs");
        assert_eq!(output["matches"][2]["column"], 3);
        assert_eq!(output["truncated"], true);
    }

    #[test]
    fn search_order_is_globally_lexical_across_path_prefixes() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.directory.path().join("a")).unwrap();
        fs::write(fixture.directory.path().join("a/z.txt"), "needle").unwrap();
        fs::write(fixture.directory.path().join("a.txt"), "needle").unwrap();

        let finished = search_text(&fixture.root.directory, &fixture.relative("."), "needle")
            .unwrap()
            .finish()
            .unwrap();

        assert_eq!(finished.output["matches"][0]["path"], "a.txt");
        assert_eq!(finished.output["matches"][1]["path"], "a/z.txt");
        assert!(
            inspect_output(
                &finished.output,
                &finished.digest,
                &fixture.root.workspace_id,
                QueryOperation::SearchText,
            )
            .unwrap()
            .postcondition
        );
    }

    #[test]
    fn collection_output_is_truncated_before_the_canonical_byte_limit() {
        let candidates = (0..MAX_LIST_DIRECTORY_ENTRIES)
            .map(|index| {
                json!({
                    "name": format!("entry-{index:04}"),
                    "path": format!("{}/entry-{index:04}", "x".repeat(4_000)),
                    "kind": "file",
                    "sizeBytes": 1,
                })
            })
            .collect();

        let finished = bounded_collection_output("entries", candidates, false)
            .unwrap()
            .finish()
            .unwrap();

        assert_eq!(finished.output["truncated"], true);
        assert!(finished.output["entries"].as_array().unwrap().len() < 512);
        assert!(
            serde_jcs::to_vec(&finished.output).unwrap().len() <= MAX_QUERY_OUTPUT_CANONICAL_BYTES
        );
    }

    #[test]
    fn aggregate_search_budget_counts_bytes_and_stops_globally() {
        let fixture = Fixture::new();
        let content = vec![b'x'; MAX_SEARCH_FILE_BYTES];
        for index in 0..=MAX_SEARCH_TOTAL_BYTES / MAX_SEARCH_FILE_BYTES {
            fs::write(
                fixture
                    .directory
                    .path()
                    .join(format!("file-{index:04}.txt")),
                &content,
            )
            .unwrap();
        }
        let mut state = SearchState {
            matches: Vec::new(),
            visited_entries: 0,
            scanned_bytes: 0,
            truncated: false,
            exhausted: false,
        };

        search_directory(
            &fixture.root.directory,
            &fixture.relative("."),
            "not-present",
            &mut state,
        )
        .unwrap();

        assert_eq!(state.scanned_bytes, MAX_SEARCH_TOTAL_BYTES);
        assert!(state.exhausted);
        assert!(state.truncated);
    }

    #[test]
    fn recursive_entry_budget_is_charged_while_directory_entries_are_collected() {
        let fixture = Fixture::new();
        fs::write(fixture.directory.path().join("a.txt"), "first").unwrap();
        fs::write(fixture.directory.path().join("b.txt"), "second").unwrap();
        let mut state = SearchState {
            matches: Vec::new(),
            visited_entries: MAX_SEARCH_VISITED_ENTRIES - 1,
            scanned_bytes: 0,
            truncated: false,
            exhausted: false,
        };

        search_directory(
            &fixture.root.directory,
            &fixture.relative("."),
            "absent",
            &mut state,
        )
        .unwrap();

        assert_eq!(state.visited_entries, MAX_SEARCH_VISITED_ENTRIES);
        assert!(state.truncated);
        assert!(state.scanned_bytes <= "second".len());
    }

    #[cfg(unix)]
    #[test]
    fn queries_never_follow_workspace_symlinks() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "OUTSIDE-SECRET").unwrap();
        symlink(outside.path(), fixture.directory.path().join("linked")).unwrap();

        assert!(list_directory(&fixture.root.directory, &fixture.relative("linked")).is_err());
        assert!(stat_path(&fixture.root.directory, &fixture.relative("linked")).is_err());
        assert!(
            search_text(
                &fixture.root.directory,
                &fixture.relative("linked"),
                "SECRET"
            )
            .is_err()
        );
        let root = list_directory(&fixture.root.directory, &fixture.relative("."))
            .unwrap()
            .finish()
            .unwrap()
            .output;
        assert_eq!(root["entries"][0]["kind"], "other");
    }

    #[cfg(windows)]
    #[test]
    fn queries_never_follow_workspace_junctions() {
        use std::process::Command;

        let fixture = Fixture::new();
        let outside = tempdir().expect("outside directory should exist");
        fs::write(outside.path().join("secret.txt"), "OUTSIDE-JUNCTION-SECRET")
            .expect("outside fixture should write");
        let junction = fixture.directory.path().join("junction");
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .status()
            .expect("junction command should run");
        assert!(status.success(), "junction fixture must be available in CI");

        assert!(list_directory(&fixture.root.directory, &fixture.relative("junction")).is_err());
        assert!(stat_path(&fixture.root.directory, &fixture.relative("junction")).is_err());
        assert!(
            search_text(
                &fixture.root.directory,
                &fixture.relative("junction"),
                "SECRET"
            )
            .is_err()
        );
        let root_search = search_text(&fixture.root.directory, &fixture.relative("."), "SECRET")
            .unwrap()
            .finish()
            .unwrap()
            .output;
        assert!(root_search["matches"].as_array().unwrap().is_empty());
        assert_eq!(root_search["truncated"], true);
        fs::remove_dir(&junction).expect("junction fixture should remove without following it");
    }

    #[test]
    fn parser_rejects_wrong_shapes_and_control_queries() {
        let fixture = Fixture::new();
        let maximum_unicode_query = "🦀".repeat(64);
        assert!(
            parse_arguments(
                &json!({"path": "workspace:fixture", "query": "needle"}),
                &fixture.root,
                QueryOperation::SearchText,
            )
            .is_ok()
        );
        assert!(
            parse_arguments(
                &json!({"path": "workspace:fixture", "query": maximum_unicode_query}),
                &fixture.root,
                QueryOperation::SearchText,
            )
            .is_ok()
        );
        for arguments in [
            json!({"path": "workspace:fixture"}),
            json!({"path": "workspace:fixture", "query": ""}),
            json!({"path": "workspace:fixture", "query": "bad\nquery"}),
            json!({"path": "workspace:fixture", "query": "a".repeat(65)}),
            json!({"path": "workspace:fixture", "query": "🦀".repeat(65)}),
            json!({"path": "workspace:other", "query": "needle"}),
        ] {
            assert_eq!(
                parse_arguments(&arguments, &fixture.root, QueryOperation::SearchText).map(|_| ()),
                Err(AdapterPrepareFailure::InvalidMaterial)
            );
        }
    }

    #[test]
    fn output_inspection_rejects_digest_or_order_tampering() {
        let fixture = Fixture::new();
        fs::write(fixture.directory.path().join("a.txt"), "a").unwrap();
        let output = list_directory(&fixture.root.directory, &fixture.relative("."))
            .unwrap()
            .finish()
            .unwrap()
            .output;
        let digest = output["digest"].as_str().unwrap().to_owned();
        assert!(
            inspect_output(
                &output,
                &digest,
                &fixture.root.workspace_id,
                QueryOperation::ListDirectory,
            )
            .unwrap()
            .postcondition
        );
        let mut tampered = output;
        tampered["entries"][0]["sizeBytes"] = json!(99);
        assert!(
            !inspect_output(
                &tampered,
                &digest,
                &fixture.root.workspace_id,
                QueryOperation::ListDirectory,
            )
            .unwrap()
            .postcondition
        );

        let canonical_path = ObservedOutput {
            payload: json!({
                "entries": [{
                    "name": "a.txt",
                    "path": "workspace:fixture/a.txt",
                    "kind": "file",
                    "sizeBytes": 1,
                }],
                "truncated": false,
            }),
        }
        .finish()
        .unwrap();
        assert!(
            inspect_output(
                &canonical_path.output,
                &canonical_path.digest,
                &fixture.root.workspace_id,
                QueryOperation::ListDirectory,
            )
            .is_err()
        );
    }
}
