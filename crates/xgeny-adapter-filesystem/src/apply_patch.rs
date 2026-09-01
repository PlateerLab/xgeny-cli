use std::ops::Range;
use std::sync::Arc;

use cap_std::fs::Dir;
use serde_json::{Value, json};
use xgeny_domain::{InstanceFeatures, VerificationResult, VerificationStrategy};
use xgeny_runtime::{
    AdapterEvidenceDigest, AdapterExecutionObservation, AdapterExecutionUnknownReason,
    AdapterPrepareFailure, AdapterPrepareRequest, AdapterReconcileRequest,
    AdapterReconciliationInconclusiveReason, AdapterReconciliationObservation, AdapterToolOutput,
    EffectAdapter, EffectVerifier, PreparedAdapterInvocation, RuleVerificationObservation,
    VerificationPortFailure, VerificationReport, VerificationRequest, VerifiedArtifactDescriptor,
    VerifierOutputDigest,
};
use xgeny_workgraph::EffectClass;

use crate::atomic_commit::{AtomicCommitFailure, MAX_ATOMIC_TEXT_BYTES, commit_atomic};
use crate::path::{RelativePath, parse_canonical};
use crate::read_text::{ReadTextLimits, read_text, sha256_digest};
use crate::{APPLY_PATCH_CAPABILITY_ID, APPLY_PATCH_CONTRACT_VERSION, WorkspaceRoot};

/// Maximum total UTF-8 bytes across every `oldText` and `newText` patch field.
pub const MAX_APPLY_PATCH_BYTES: usize = MAX_ATOMIC_TEXT_BYTES;
/// Maximum number of exact contextual edits in one atomic patch.
pub const MAX_APPLY_PATCH_EDITS: usize = 32;

#[derive(Clone)]
pub struct ApplyPatchAdapter {
    root: WorkspaceRoot,
}

impl ApplyPatchAdapter {
    pub(crate) const fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn verifier(&self) -> ApplyPatchVerifier {
        ApplyPatchVerifier {
            root: self.root.clone(),
        }
    }
}

impl std::fmt::Debug for ApplyPatchAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApplyPatchAdapter")
            .field("root", &"<preopened/redacted>")
            .finish()
    }
}

impl EffectAdapter for ApplyPatchAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        verify_contract(&request, &self.root.apply_patch_binding())?;
        let arguments = parse_arguments(request.normalized_arguments(), &self.root)?;
        Ok(Box::new(PreparedApplyPatch {
            directory: Arc::clone(&self.root.directory),
            relative_path: arguments.relative_path,
            canonical_path: arguments.canonical_path,
            expected_digest: arguments.expected_digest,
            edits: arguments.edits,
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

pub struct ApplyPatchVerifier {
    root: WorkspaceRoot,
}

impl std::fmt::Debug for ApplyPatchVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApplyPatchVerifier")
            .field("root", &"<preopened/redacted>")
            .finish()
    }
}

impl EffectVerifier for ApplyPatchVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        verify_verifier_contract(&request, &self.root.apply_patch_binding())?;
        let output = request
            .tool_output()
            .ok_or(VerificationPortFailure::EvidenceUnavailable)?;
        let inspected =
            inspect_output(output.output(), request.outcome_evidence_digest().as_str())?;
        let relative = parse_canonical(&self.root.workspace_id, &inspected.path)
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        let observed = read_text(&self.root.directory, &relative, ReadTextLimits::default())
            .map_err(|_| VerificationPortFailure::EvidenceUnavailable)?;
        let postcondition = inspected.digest == observed.digest
            && inspected.byte_size == observed.content.len()
            && request.outcome_evidence_digest().as_str() == observed.digest;
        let evidence = AdapterEvidenceDigest::new(observed.digest.clone())
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        let rules = request
            .definition()
            .spec
            .verification
            .iter()
            .map(|rule| {
                let result = match rule.strategy {
                    VerificationStrategy::OutputSchema => VerificationResult::Passed,
                    VerificationStrategy::Postcondition if postcondition => {
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
                            .expect("the evidence digest was already validated"),
                    ),
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let artifact = VerifiedArtifactDescriptor::new(
            "apply-patch-output",
            Option::<String>::None,
            "text/plain; charset=utf-8",
            u64::try_from(observed.content.len())
                .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?,
            observed.digest,
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

#[derive(Clone)]
struct PatchEdit {
    old_text: String,
    new_text: String,
}

struct ParsedArguments {
    relative_path: RelativePath,
    canonical_path: String,
    expected_digest: String,
    edits: Vec<PatchEdit>,
}

struct PreparedApplyPatch {
    directory: Arc<Dir>,
    relative_path: RelativePath,
    canonical_path: String,
    expected_digest: String,
    edits: Vec<PatchEdit>,
}

impl std::fmt::Debug for PreparedApplyPatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedApplyPatch")
            .field("directory", &"<preopened/redacted>")
            .field("relative_path", &"<redacted>")
            .field("canonical_path", &"<redacted>")
            .field("expected_digest", &"<redacted>")
            .field("edit_count", &self.edits.len())
            .finish()
    }
}

impl PreparedAdapterInvocation for PreparedApplyPatch {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        match apply_patch_to_file(
            &self.directory,
            &self.relative_path,
            &self.expected_digest,
            &self.edits,
        ) {
            Ok(committed) => AdapterExecutionObservation::SucceededWithOutput {
                evidence_digest: AdapterEvidenceDigest::new(committed.digest.clone())
                    .expect("a SHA-256 digest is canonical"),
                output: AdapterToolOutput::new(json!({
                    "path": self.canonical_path,
                    "digest": committed.digest,
                    "byteSize": committed.byte_size,
                    "changed": committed.changed,
                    "editCount": self.edits.len(),
                })),
            },
            Err(PatchFailure::Atomic(failure)) if failure.outcome_is_unknown() => {
                AdapterExecutionObservation::Unknown {
                    reason: AdapterExecutionUnknownReason::TransportOutcomeUnknown,
                }
            }
            Err(failure) => AdapterExecutionObservation::Failed {
                evidence_digest: AdapterEvidenceDigest::new(failure.evidence_digest())
                    .expect("a SHA-256 digest is canonical"),
            },
        }
    }
}

#[derive(Debug)]
struct PatchCommit {
    digest: String,
    byte_size: usize,
    changed: bool,
}

struct InspectedOutput {
    path: String,
    digest: String,
    byte_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchFailure {
    ReadSource,
    PreconditionConflict,
    ContextMissing,
    ContextAmbiguous,
    OverlappingEdits,
    ResultTooLarge,
    Atomic(AtomicCommitFailure),
}

impl PatchFailure {
    fn evidence_digest(self) -> String {
        let code = match self {
            Self::ReadSource => "read-source",
            Self::PreconditionConflict => "precondition-conflict",
            Self::ContextMissing => "context-missing",
            Self::ContextAmbiguous => "context-ambiguous",
            Self::OverlappingEdits => "overlapping-edits",
            Self::ResultTooLarge => "result-too-large",
            Self::Atomic(failure) => failure.code(),
        };
        sha256_digest(format!("xgeny.fs/apply-patch/failure/v1/{code}").as_bytes())
    }
}

fn verify_contract(
    request: &AdapterPrepareRequest<'_>,
    expected_binding: &xgeny_domain::InstanceBinding,
) -> Result<(), AdapterPrepareFailure> {
    let intent = request.intent();
    let instance = request.instance();
    if intent.invocation.capability_id != APPLY_PATCH_CAPABILITY_ID
        || intent.invocation.contract_version != APPLY_PATCH_CONTRACT_VERSION
        || instance.definition.capability_id != APPLY_PATCH_CAPABILITY_ID
        || instance.definition.contract_version != APPLY_PATCH_CONTRACT_VERSION
        || intent.invocation.instance_id != instance.instance_id
        || instance.binding != *expected_binding
        || !supports_instance_features(&instance.features)
        || intent.effect_class != EffectClass::Idempotent
        || intent.idempotency_key.as_deref().is_none_or(str::is_empty)
    {
        return Err(AdapterPrepareFailure::UnsupportedProtocol);
    }
    Ok(())
}

fn verify_verifier_contract(
    request: &VerificationRequest<'_>,
    expected_binding: &xgeny_domain::InstanceBinding,
) -> Result<(), VerificationPortFailure> {
    let intent = request.intent();
    let instance = request.instance();
    let rules = &request.definition().spec.verification;
    if intent.invocation.capability_id != APPLY_PATCH_CAPABILITY_ID
        || intent.invocation.contract_version != APPLY_PATCH_CONTRACT_VERSION
        || instance.definition.capability_id != APPLY_PATCH_CAPABILITY_ID
        || instance.definition.contract_version != APPLY_PATCH_CONTRACT_VERSION
        || intent.invocation.instance_id != instance.instance_id
        || instance.binding != *expected_binding
        || !supports_instance_features(&instance.features)
        || intent.effect_class != EffectClass::Idempotent
        || intent.idempotency_key.as_deref().is_none_or(str::is_empty)
        || rules.len() != 2
        || rules.iter().any(|rule| {
            !rule.required
                || !matches!(
                    rule.strategy,
                    VerificationStrategy::OutputSchema | VerificationStrategy::Postcondition
                )
        })
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
) -> Result<ParsedArguments, AdapterPrepareFailure> {
    let object = arguments
        .as_object()
        .filter(|object| object.len() == 3)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let canonical_path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let relative_path = parse_canonical(&root.workspace_id, canonical_path)
        .map_err(|_| AdapterPrepareFailure::InvalidMaterial)?;
    if relative_path.components().is_empty() {
        return Err(AdapterPrepareFailure::InvalidMaterial);
    }
    let expected_digest = object
        .get("expectedDigest")
        .and_then(Value::as_str)
        .filter(|digest| canonical_digest(digest))
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?
        .to_owned();
    let values = object
        .get("edits")
        .and_then(Value::as_array)
        .filter(|edits| !edits.is_empty() && edits.len() <= MAX_APPLY_PATCH_EDITS)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let mut total_bytes = 0_usize;
    let mut edits = Vec::with_capacity(values.len());
    for value in values {
        let edit = value
            .as_object()
            .filter(|edit| edit.len() == 2)
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        let old_text = edit
            .get("oldText")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        if old_text == new_text {
            return Err(AdapterPrepareFailure::InvalidMaterial);
        }
        total_bytes = total_bytes
            .checked_add(old_text.len())
            .and_then(|total| total.checked_add(new_text.len()))
            .filter(|total| *total <= MAX_APPLY_PATCH_BYTES)
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        edits.push(PatchEdit {
            old_text: old_text.to_owned(),
            new_text: new_text.to_owned(),
        });
    }
    Ok(ParsedArguments {
        relative_path,
        canonical_path: canonical_path.to_owned(),
        expected_digest,
        edits,
    })
}

fn inspect_output(
    output: &Value,
    evidence_digest: &str,
) -> Result<InspectedOutput, VerificationPortFailure> {
    let object = output
        .as_object()
        .filter(|object| object.len() == 5)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let digest = object
        .get("digest")
        .and_then(Value::as_str)
        .filter(|digest| canonical_digest(digest))
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let byte_size = object
        .get("byteSize")
        .and_then(Value::as_u64)
        .and_then(|size| usize::try_from(size).ok())
        .filter(|size| *size <= MAX_ATOMIC_TEXT_BYTES)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let edit_count = object
        .get("editCount")
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .filter(|count| (1..=MAX_APPLY_PATCH_EDITS).contains(count));
    if !object.get("changed").is_some_and(Value::is_boolean)
        || edit_count.is_none()
        || digest != evidence_digest
    {
        return Err(VerificationPortFailure::ResponseUnverifiable);
    }
    Ok(InspectedOutput {
        path: path.to_owned(),
        digest: digest.to_owned(),
        byte_size,
    })
}

fn apply_patch_to_file(
    root: &Dir,
    relative_path: &RelativePath,
    expected_digest: &str,
    edits: &[PatchEdit],
) -> Result<PatchCommit, PatchFailure> {
    let observed = read_text(root, relative_path, ReadTextLimits::default())
        .map_err(|_| PatchFailure::ReadSource)?;
    if observed.digest != expected_digest {
        return Err(PatchFailure::PreconditionConflict);
    }
    let desired = apply_exact_edits(&observed.content, edits)?;
    let digest = sha256_digest(desired.as_bytes());
    let changed = commit_atomic(
        root,
        relative_path,
        desired.as_bytes(),
        Some(expected_digest),
        &digest,
    )
    .map_err(PatchFailure::Atomic)?;
    Ok(PatchCommit {
        digest,
        byte_size: desired.len(),
        changed,
    })
}

fn apply_exact_edits(source: &str, edits: &[PatchEdit]) -> Result<String, PatchFailure> {
    let mut resolved = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = unique_range(source, &edit.old_text)?;
        resolved.push((range, edit));
    }
    resolved.sort_by_key(|(range, _)| range.start);
    if resolved
        .windows(2)
        .any(|pair| pair[1].0.start < pair[0].0.end)
    {
        return Err(PatchFailure::OverlappingEdits);
    }
    let removed_bytes = resolved.iter().try_fold(0_usize, |total, (_, edit)| {
        total
            .checked_add(edit.old_text.len())
            .ok_or(PatchFailure::ResultTooLarge)
    })?;
    let inserted_bytes = resolved.iter().try_fold(0_usize, |total, (_, edit)| {
        total
            .checked_add(edit.new_text.len())
            .ok_or(PatchFailure::ResultTooLarge)
    })?;
    let result_size = source
        .len()
        .checked_sub(removed_bytes)
        .and_then(|size| size.checked_add(inserted_bytes))
        .filter(|size| *size <= MAX_ATOMIC_TEXT_BYTES)
        .ok_or(PatchFailure::ResultTooLarge)?;
    let mut result = source.to_owned();
    result.reserve(result_size.saturating_sub(source.len()));
    for (range, edit) in resolved.into_iter().rev() {
        result.replace_range(range, &edit.new_text);
    }
    Ok(result)
}

fn unique_range(source: &str, needle: &str) -> Result<Range<usize>, PatchFailure> {
    let start = source.find(needle).ok_or(PatchFailure::ContextMissing)?;
    let next_character = source[start..]
        .char_indices()
        .nth(1)
        .map(|(offset, _)| start + offset);
    if next_character.is_some_and(|offset| source[offset..].contains(needle)) {
        return Err(PatchFailure::ContextAmbiguous);
    }
    Ok(start..start + needle.len())
}

fn canonical_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
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
        fn new(content: &str) -> Self {
            let directory = tempdir().unwrap();
            fs::write(directory.path().join("target.txt"), content).unwrap();
            let root =
                WorkspaceRoot::open_ambient(directory.path(), WorkspaceId::new("fixture").unwrap())
                    .unwrap();
            Self { root, directory }
        }

        fn relative(&self) -> RelativePath {
            parse_canonical(&self.root.workspace_id, "workspace:fixture/target.txt").unwrap()
        }

        fn apply(&self, expected: &str, edits: &[PatchEdit]) -> Result<PatchCommit, PatchFailure> {
            apply_patch_to_file(&self.root.directory, &self.relative(), expected, edits)
        }

        fn content(&self) -> String {
            fs::read_to_string(self.directory.path().join("target.txt")).unwrap()
        }
    }

    fn edit(old_text: &str, new_text: &str) -> PatchEdit {
        PatchEdit {
            old_text: old_text.to_owned(),
            new_text: new_text.to_owned(),
        }
    }

    #[test]
    fn exact_non_overlapping_edits_commit_once_and_preserve_unmentioned_bytes() {
        let source = "alpha\r\n한글 old\r\nomega\r\n";
        let fixture = Fixture::new(source);
        let committed = fixture
            .apply(
                &sha256_digest(source.as_bytes()),
                &[edit("alpha", "ALPHA"), edit("old", "새 값")],
            )
            .unwrap();
        assert!(committed.changed);
        assert_eq!(fixture.content(), "ALPHA\r\n한글 새 값\r\nomega\r\n");
        assert_eq!(
            committed.digest,
            sha256_digest(fixture.content().as_bytes())
        );
    }

    #[test]
    fn stale_digest_missing_ambiguous_and_overlapping_context_never_mutate() {
        let cases = [
            (
                "one two one",
                sha256_digest(b"stale"),
                vec![edit("two", "TWO")],
                PatchFailure::PreconditionConflict,
            ),
            (
                "one two one",
                sha256_digest(b"one two one"),
                vec![edit("missing", "new")],
                PatchFailure::ContextMissing,
            ),
            (
                "one two one",
                sha256_digest(b"one two one"),
                vec![edit("one", "ONE")],
                PatchFailure::ContextAmbiguous,
            ),
            (
                "abcdef",
                sha256_digest(b"abcdef"),
                vec![edit("abc", ""), edit("cde", "")],
                PatchFailure::OverlappingEdits,
            ),
        ];
        for (source, expected, edits, failure) in cases {
            let fixture = Fixture::new(source);
            assert_eq!(fixture.apply(&expected, &edits).unwrap_err(), failure);
            assert_eq!(fixture.content(), source);
        }
    }

    #[test]
    fn overlapping_occurrences_are_ambiguous() {
        assert_eq!(
            unique_range("aaa", "aa"),
            Err(PatchFailure::ContextAmbiguous)
        );
    }

    #[test]
    fn result_cannot_expand_beyond_the_atomic_file_limit() {
        let source = format!("{}x", "a".repeat(MAX_ATOMIC_TEXT_BYTES - 1));
        assert_eq!(
            apply_exact_edits(&source, &[edit("x", "xx")]),
            Err(PatchFailure::ResultTooLarge)
        );
    }

    #[test]
    fn simultaneous_edits_use_the_final_size_not_an_ordered_intermediate_size() {
        let source = format!("x{}y", "a".repeat(MAX_ATOMIC_TEXT_BYTES - 2));
        let result = apply_exact_edits(&source, &[edit("x", "xx"), edit("y", "")]).unwrap();
        assert_eq!(result.len(), MAX_ATOMIC_TEXT_BYTES);
        assert!(result.starts_with("xx"));
        assert!(!result.ends_with('y'));
    }

    #[test]
    fn argument_shape_bytes_and_debug_are_closed_and_redacted() {
        let fixture = Fixture::new("secret old");
        let digest = sha256_digest(b"secret old");
        let parsed = parse_arguments(
            &json!({
                "path": "workspace:fixture/target.txt",
                "expectedDigest": digest,
                "edits": [{"oldText": "secret old", "newText": "secret new"}]
            }),
            &fixture.root,
        )
        .unwrap();
        let prepared = PreparedApplyPatch {
            directory: Arc::clone(&fixture.root.directory),
            relative_path: parsed.relative_path,
            canonical_path: parsed.canonical_path,
            expected_digest: parsed.expected_digest,
            edits: parsed.edits,
        };
        let debug = format!("{prepared:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("target.txt"));
        for invalid in [
            json!({
                "path": "workspace:fixture/target.txt",
                "expectedDigest": null,
                "edits": [{"oldText": "old", "newText": "new"}]
            }),
            json!({
                "path": "workspace:fixture/target.txt",
                "expectedDigest": digest,
                "edits": []
            }),
            json!({
                "path": "workspace:fixture/target.txt",
                "expectedDigest": digest,
                "edits": [{"oldText": "same", "newText": "same"}]
            }),
            json!({
                "path": "workspace:fixture/target.txt",
                "expectedDigest": digest,
                "edits": [{"oldText": "x", "newText": "y".repeat(MAX_APPLY_PATCH_BYTES)}]
            }),
        ] {
            assert!(parse_arguments(&invalid, &fixture.root).is_err());
        }
    }

    #[test]
    fn output_shape_never_accepts_patch_content() {
        let digest = sha256_digest(b"new");
        let output = json!({
            "path": "workspace:fixture/target.txt",
            "digest": digest,
            "byteSize": 3,
            "changed": true,
            "editCount": 1
        });
        assert!(inspect_output(&output, &digest).is_ok());
        let mut exposed = output;
        exposed["content"] = Value::String("new".to_owned());
        assert!(inspect_output(&exposed, &digest).is_err());
    }
}
