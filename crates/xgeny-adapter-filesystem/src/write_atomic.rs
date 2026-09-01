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
use crate::{WRITE_ATOMIC_CAPABILITY_ID, WRITE_ATOMIC_CONTRACT_VERSION, WorkspaceRoot};

/// Hard UTF-8 byte ceiling for one atomic write.
pub const MAX_WRITE_ATOMIC_BYTES: usize = MAX_ATOMIC_TEXT_BYTES;

#[derive(Clone)]
pub struct WriteAtomicAdapter {
    root: WorkspaceRoot,
}

impl WriteAtomicAdapter {
    pub(crate) const fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn verifier(&self) -> WriteAtomicVerifier {
        WriteAtomicVerifier {
            root: self.root.clone(),
        }
    }
}

impl std::fmt::Debug for WriteAtomicAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriteAtomicAdapter")
            .field("root", &"<preopened/redacted>")
            .finish()
    }
}

impl EffectAdapter for WriteAtomicAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        verify_contract(&request, &self.root.write_atomic_binding())?;
        let arguments = parse_arguments(request.normalized_arguments(), &self.root)?;
        Ok(Box::new(PreparedWriteAtomic {
            directory: Arc::clone(&self.root.directory),
            relative_path: arguments.relative_path,
            canonical_path: arguments.canonical_path,
            content: arguments.content,
            expected_digest: arguments.expected_digest,
            desired_digest: arguments.desired_digest,
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

pub struct WriteAtomicVerifier {
    root: WorkspaceRoot,
}

impl std::fmt::Debug for WriteAtomicVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriteAtomicVerifier")
            .field("root", &"<preopened/redacted>")
            .finish()
    }
}

impl EffectVerifier for WriteAtomicVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        verify_verifier_contract(&request, &self.root.write_atomic_binding())?;
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
            "write-atomic-output",
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

struct ParsedArguments {
    relative_path: RelativePath,
    canonical_path: String,
    content: Vec<u8>,
    expected_digest: Option<String>,
    desired_digest: String,
}

struct PreparedWriteAtomic {
    directory: Arc<Dir>,
    relative_path: RelativePath,
    canonical_path: String,
    content: Vec<u8>,
    expected_digest: Option<String>,
    desired_digest: String,
}

impl std::fmt::Debug for PreparedWriteAtomic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedWriteAtomic")
            .field("directory", &"<preopened/redacted>")
            .field("relative_path", &"<redacted>")
            .field("canonical_path", &"<redacted>")
            .field("content", &"<redacted>")
            .field(
                "expected_digest",
                &self.expected_digest.as_ref().map(|_| "<present>"),
            )
            .field("desired_digest", &"<redacted>")
            .finish()
    }
}

impl PreparedAdapterInvocation for PreparedWriteAtomic {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        match commit_atomic(
            &self.directory,
            &self.relative_path,
            &self.content,
            self.expected_digest.as_deref(),
            &self.desired_digest,
        ) {
            Ok(changed) => AdapterExecutionObservation::SucceededWithOutput {
                evidence_digest: AdapterEvidenceDigest::new(self.desired_digest.clone())
                    .expect("a SHA-256 digest is canonical"),
                output: AdapterToolOutput::new(json!({
                    "path": self.canonical_path,
                    "digest": self.desired_digest,
                    "byteSize": self.content.len(),
                    "changed": changed,
                })),
            },
            Err(failure) if failure.outcome_is_unknown() => AdapterExecutionObservation::Unknown {
                reason: AdapterExecutionUnknownReason::TransportOutcomeUnknown,
            },
            Err(failure) => AdapterExecutionObservation::Failed {
                evidence_digest: AdapterEvidenceDigest::new(write_failure_digest(failure))
                    .expect("a SHA-256 digest is canonical"),
            },
        }
    }
}

struct InspectedOutput {
    path: String,
    digest: String,
    byte_size: usize,
}

fn write_failure_digest(failure: AtomicCommitFailure) -> String {
    sha256_digest(format!("xgeny.fs/write-atomic/failure/v1/{}", failure.code()).as_bytes())
}

fn verify_contract(
    request: &AdapterPrepareRequest<'_>,
    expected_binding: &xgeny_domain::InstanceBinding,
) -> Result<(), AdapterPrepareFailure> {
    let intent = request.intent();
    let instance = request.instance();
    if intent.invocation.capability_id != WRITE_ATOMIC_CAPABILITY_ID
        || intent.invocation.contract_version != WRITE_ATOMIC_CONTRACT_VERSION
        || instance.definition.capability_id != WRITE_ATOMIC_CAPABILITY_ID
        || instance.definition.contract_version != WRITE_ATOMIC_CONTRACT_VERSION
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
    if intent.invocation.capability_id != WRITE_ATOMIC_CAPABILITY_ID
        || intent.invocation.contract_version != WRITE_ATOMIC_CONTRACT_VERSION
        || instance.definition.capability_id != WRITE_ATOMIC_CAPABILITY_ID
        || instance.definition.contract_version != WRITE_ATOMIC_CONTRACT_VERSION
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
    let content = object
        .get("content")
        .and_then(Value::as_str)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?
        .as_bytes()
        .to_vec();
    if content.len() > MAX_WRITE_ATOMIC_BYTES {
        return Err(AdapterPrepareFailure::InvalidMaterial);
    }
    let expected_digest = match object.get("expectedDigest") {
        Some(Value::Null) => None,
        Some(Value::String(value)) if canonical_digest(value) => Some(value.clone()),
        _ => return Err(AdapterPrepareFailure::InvalidMaterial),
    };
    let desired_digest = sha256_digest(&content);
    Ok(ParsedArguments {
        relative_path,
        canonical_path: canonical_path.to_owned(),
        content,
        expected_digest,
        desired_digest,
    })
}

fn inspect_output(
    output: &Value,
    evidence_digest: &str,
) -> Result<InspectedOutput, VerificationPortFailure> {
    let object = output
        .as_object()
        .filter(|object| object.len() == 4)
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
        .filter(|size| *size <= MAX_WRITE_ATOMIC_BYTES)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    if !object.get("changed").is_some_and(Value::is_boolean) || digest != evidence_digest {
        return Err(VerificationPortFailure::ResponseUnverifiable);
    }
    Ok(InspectedOutput {
        path: path.to_owned(),
        digest: digest.to_owned(),
        byte_size,
    })
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
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::{WorkspaceId, WorkspaceRoot};

    struct Fixture {
        root: WorkspaceRoot,
        _directory: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempdir().unwrap();
            let root =
                WorkspaceRoot::open_ambient(directory.path(), WorkspaceId::new("fixture").unwrap())
                    .unwrap();
            Self {
                root,
                _directory: directory,
            }
        }
    }

    #[test]
    fn arguments_are_bounded_and_debug_is_redacted() {
        let fixture = Fixture::new();
        let parsed = parse_arguments(
            &json!({
                "path": "workspace:fixture/secret.txt",
                "content": "CONTENT-SENTINEL",
                "expectedDigest": null
            }),
            &fixture.root,
        )
        .unwrap();
        let prepared = PreparedWriteAtomic {
            directory: Arc::clone(&fixture.root.directory),
            relative_path: parsed.relative_path,
            canonical_path: parsed.canonical_path,
            content: parsed.content,
            expected_digest: parsed.expected_digest,
            desired_digest: parsed.desired_digest,
        };
        let debug = format!("{prepared:?}");
        assert!(!debug.contains("CONTENT-SENTINEL"));
        assert!(!debug.contains("secret.txt"));
        assert!(
            parse_arguments(
                &json!({
                    "path": "workspace:fixture/too-large.txt",
                    "content": "x".repeat(MAX_WRITE_ATOMIC_BYTES + 1),
                    "expectedDigest": null
                }),
                &fixture.root,
            )
            .is_err()
        );
    }

    #[test]
    fn output_shape_and_evidence_are_exact() {
        let digest = sha256_digest(b"hello");
        assert!(
            inspect_output(
                &json!({
                    "path": "workspace:fixture/file.txt",
                    "digest": digest,
                    "byteSize": 5,
                    "changed": true
                }),
                &digest
            )
            .is_ok()
        );
        assert!(
            inspect_output(
                &json!({
                    "path": "workspace:fixture/file.txt",
                    "digest": digest,
                    "byteSize": 5,
                    "changed": true,
                    "content": "hello"
                }),
                &digest
            )
            .is_err()
        );
        assert!(
            inspect_output(
                &json!({
                    "path": "workspace:fixture/file.txt",
                    "digest": digest,
                    "byteSize": 5,
                    "changed": true
                }),
                &sha256_digest(b"other")
            )
            .is_err()
        );
    }
}
