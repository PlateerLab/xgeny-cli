#![doc = "Non-production reference adapter for validating the public `XGENy` adapter boundary."]

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_domain::{InstanceBinding, VerificationResult, VerificationStrategy};
use xgeny_runtime::{
    AdapterEvidenceDigest, AdapterExecutionObservation, AdapterExecutionUnknownReason,
    AdapterPrepareFailure, AdapterPrepareRequest, AdapterReconcileRequest,
    AdapterReconciliationInconclusiveReason, AdapterReconciliationObservation, EffectAdapter,
    EffectVerifier, PreparedAdapterInvocation, RuleVerificationObservation,
    VerificationPortFailure, VerificationReport, VerificationRequest, VerifierOutputDigest,
};
use xgeny_workgraph::{EffectClass, EffectIntent};

pub const REFERENCE_CAPABILITY_ID: &str = "xgeny.fixture/commit-marker";
pub const REFERENCE_CONTRACT_VERSION: &str = "1.0.0";

const MAX_TARGET_REFERENCE_BYTES: usize = 128;
const MAX_CONFIGURED_MARKER_BYTES: usize = 64 * 1024;
const MAX_EVIDENCE_RECORD_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkerLimits {
    max_marker_bytes: NonZeroUsize,
}

impl MarkerLimits {
    /// Configure the maximum UTF-8 marker size accepted during side-effect-free preparation.
    ///
    /// # Errors
    ///
    /// Rejects zero or a value above the reference adapter's fixed upper bound.
    pub fn new(max_marker_bytes: usize) -> Result<Self, ReferenceAdapterConfigError> {
        let max_marker_bytes = NonZeroUsize::new(max_marker_bytes)
            .ok_or(ReferenceAdapterConfigError::InvalidMarkerLimit)?;
        if max_marker_bytes.get() > MAX_CONFIGURED_MARKER_BYTES {
            return Err(ReferenceAdapterConfigError::InvalidMarkerLimit);
        }
        Ok(Self { max_marker_bytes })
    }

    #[must_use]
    pub const fn max_marker_bytes(self) -> usize {
        self.max_marker_bytes.get()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceAdapterConfigError {
    #[error("reference adapter marker limit is invalid")]
    InvalidMarkerLimit,
    #[error("reference adapter target reference is invalid")]
    InvalidTargetReference,
    #[error("reference adapter target metadata is unavailable")]
    TargetMetadataUnavailable,
    #[error("reference adapter target is not a regular file")]
    TargetNotRegular,
}

pub struct PreopenedMarkerAdapter {
    target: Arc<Mutex<Box<dyn MarkerTarget>>>,
    expected_binding: InstanceBinding,
    canonical_target_ref: String,
    limits: MarkerLimits,
}

impl PreopenedMarkerAdapter {
    /// Build a reference adapter around a file handle opened by the trusted host.
    ///
    /// The target path is never accepted by this API or by invocation material. The handle may
    /// point only to a regular file. No target read or write occurs during construction.
    ///
    /// # Errors
    ///
    /// Returns a fixed configuration error without echoing the target reference or OS error.
    pub fn new(
        target: File,
        expected_binding: InstanceBinding,
        canonical_target_ref: impl Into<String>,
        limits: MarkerLimits,
    ) -> Result<Self, ReferenceAdapterConfigError> {
        let canonical_target_ref = canonical_target_ref.into();
        validate_target_reference(&canonical_target_ref)?;
        let metadata = target
            .metadata()
            .map_err(|_| ReferenceAdapterConfigError::TargetMetadataUnavailable)?;
        if !metadata.is_file() {
            return Err(ReferenceAdapterConfigError::TargetNotRegular);
        }
        Ok(Self {
            target: Arc::new(Mutex::new(Box::new(target))),
            expected_binding,
            canonical_target_ref,
            limits,
        })
    }

    /// Create a read-only verifier sharing the same preopened target.
    ///
    /// The verifier can be moved into an exact `EffectVerifierRegistry` before the adapter itself
    /// is registered. Verification never truncates, writes, or syncs the target.
    #[must_use]
    pub fn verifier(&self) -> PreopenedMarkerVerifier {
        PreopenedMarkerVerifier {
            target: Arc::clone(&self.target),
            expected_binding: self.expected_binding.clone(),
        }
    }
}

impl std::fmt::Debug for PreopenedMarkerAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreopenedMarkerAdapter")
            .field("target", &"<preopened/redacted>")
            .field("expected_binding", &"<redacted>")
            .field("canonical_target_ref", &"<redacted>")
            .field("limits", &self.limits)
            .finish()
    }
}

impl EffectAdapter for PreopenedMarkerAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        verify_contract(&request, &self.expected_binding)?;
        validate_arguments(
            request.normalized_arguments(),
            &self.canonical_target_ref,
            self.limits,
        )?;
        let (record, evidence_digest) = evidence_record(request.intent(), request.instance())?;
        Ok(Box::new(PreparedMarker {
            target: Arc::clone(&self.target),
            record,
            evidence_digest,
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

pub struct PreopenedMarkerVerifier {
    target: Arc<Mutex<Box<dyn MarkerTarget>>>,
    expected_binding: InstanceBinding,
}

impl std::fmt::Debug for PreopenedMarkerVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreopenedMarkerVerifier")
            .field("target", &"<preopened/redacted>")
            .field("expected_binding", &"<redacted>")
            .finish()
    }
}

impl EffectVerifier for PreopenedMarkerVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        if request.intent().invocation.capability_id != REFERENCE_CAPABILITY_ID
            || request.intent().invocation.contract_version != REFERENCE_CONTRACT_VERSION
            || request.instance().binding != self.expected_binding
            || request.definition().spec.verification.is_empty()
            || request
                .definition()
                .spec
                .verification
                .iter()
                .any(|rule| rule.strategy != VerificationStrategy::Postcondition)
        {
            return Err(VerificationPortFailure::UnsupportedStrategy);
        }
        let observed = self
            .target
            .lock()
            .map_err(|_| VerificationPortFailure::Unavailable)?
            .read_record(MAX_EVIDENCE_RECORD_BYTES + 1)
            .map_err(|_| VerificationPortFailure::EvidenceUnavailable)?;
        if observed.is_empty() || observed.len() > MAX_EVIDENCE_RECORD_BYTES {
            return Err(VerificationPortFailure::ResponseUnverifiable);
        }
        let observed_digest = AdapterEvidenceDigest::new(sha256_digest(&observed))
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        let result = if observed_digest.as_str() == request.outcome_evidence_digest().as_str() {
            VerificationResult::Passed
        } else {
            VerificationResult::Failed
        };
        let rules = request
            .definition()
            .spec
            .verification
            .iter()
            .map(|rule| {
                RuleVerificationObservation::new(
                    rule.strategy,
                    result,
                    Some(
                        AdapterEvidenceDigest::new(observed_digest.as_str().to_owned())
                            .expect("observed digest was already validated"),
                    ),
                )
            })
            .collect();
        Ok(VerificationReport::new(
            VerifierOutputDigest::new(sha256_digest(b"{}"))
                .expect("empty object digest is canonical"),
            rules,
        ))
    }
}

struct PreparedMarker {
    target: Arc<Mutex<Box<dyn MarkerTarget>>>,
    record: Vec<u8>,
    evidence_digest: AdapterEvidenceDigest,
}

impl std::fmt::Debug for PreparedMarker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedMarker")
            .field("target", &"<preopened/redacted>")
            .field("record", &"<redacted>")
            .field("evidence_digest", &self.evidence_digest)
            .finish()
    }
}

impl PreparedAdapterInvocation for PreparedMarker {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        if write_and_verify(&self.target, &self.record).is_err() {
            return AdapterExecutionObservation::Unknown {
                reason: AdapterExecutionUnknownReason::ResponseUnverifiable,
            };
        }
        AdapterExecutionObservation::Succeeded {
            evidence_digest: self.evidence_digest,
        }
    }
}

fn validate_target_reference(target_ref: &str) -> Result<(), ReferenceAdapterConfigError> {
    if target_ref.is_empty()
        || target_ref.len() > MAX_TARGET_REFERENCE_BYTES
        || matches!(target_ref, "." | "..")
        || !target_ref
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ReferenceAdapterConfigError::InvalidTargetReference);
    }
    Ok(())
}

fn verify_contract(
    request: &AdapterPrepareRequest<'_>,
    expected_binding: &InstanceBinding,
) -> Result<(), AdapterPrepareFailure> {
    let intent = request.intent();
    let instance = request.instance();
    if intent.invocation.capability_id != REFERENCE_CAPABILITY_ID
        || intent.invocation.contract_version != REFERENCE_CONTRACT_VERSION
        || instance.definition.capability_id != REFERENCE_CAPABILITY_ID
        || instance.definition.contract_version != REFERENCE_CONTRACT_VERSION
        || intent.invocation.instance_id != instance.instance_id
        || instance.binding != *expected_binding
        || intent.effect_class != EffectClass::Idempotent
        || intent.idempotency_key.as_deref().is_none_or(str::is_empty)
    {
        return Err(AdapterPrepareFailure::UnsupportedProtocol);
    }
    Ok(())
}

fn validate_arguments(
    arguments: &Value,
    canonical_target_ref: &str,
    limits: MarkerLimits,
) -> Result<(), AdapterPrepareFailure> {
    let object = arguments
        .as_object()
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    if object.len() != 2 {
        return Err(AdapterPrepareFailure::InvalidMaterial);
    }
    let target_ref = object
        .get("targetRef")
        .and_then(Value::as_str)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let marker = object
        .get("marker")
        .and_then(Value::as_str)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    if target_ref != canonical_target_ref
        || marker.is_empty()
        || marker.len() > limits.max_marker_bytes()
    {
        return Err(AdapterPrepareFailure::InvalidMaterial);
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MarkerEvidenceRecord<'a> {
    domain: &'static str,
    effect_id: &'a str,
    action_digest: &'a str,
    material_digest: &'a str,
    instance_id: &'a str,
    instance_binding_digest: &'a str,
    idempotency_key: &'a str,
}

fn evidence_record(
    intent: &EffectIntent,
    instance: &xgeny_domain::CapabilityInstanceBody,
) -> Result<(Vec<u8>, AdapterEvidenceDigest), AdapterPrepareFailure> {
    let idempotency_key = intent
        .idempotency_key
        .as_deref()
        .ok_or(AdapterPrepareFailure::UnsupportedProtocol)?;
    let record = serde_jcs::to_vec(&MarkerEvidenceRecord {
        domain: "xgeny.reference-marker-evidence/v1",
        effect_id: &intent.effect_id,
        action_digest: &intent.action_digest,
        material_digest: &intent.authorization.binding.material_digest,
        instance_id: &instance.instance_id,
        instance_binding_digest: &intent.invocation.instance_binding_digest,
        idempotency_key,
    })
    .map_err(|_| AdapterPrepareFailure::InvalidMaterial)?;
    if record.len() > MAX_EVIDENCE_RECORD_BYTES {
        return Err(AdapterPrepareFailure::InvalidMaterial);
    }
    let evidence_digest = AdapterEvidenceDigest::new(sha256_digest(&record))
        .map_err(|_| AdapterPrepareFailure::InvalidMaterial)?;
    Ok((record, evidence_digest))
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}

trait MarkerTarget: Send {
    fn reset(&mut self) -> io::Result<()>;
    fn write_record(&mut self, record: &[u8]) -> io::Result<()>;
    fn sync_record(&mut self) -> io::Result<()>;
    fn read_record(&mut self, maximum_bytes: usize) -> io::Result<Vec<u8>>;
}

impl MarkerTarget for File {
    fn reset(&mut self) -> io::Result<()> {
        self.seek(SeekFrom::Start(0))?;
        self.set_len(0)
    }

    fn write_record(&mut self, record: &[u8]) -> io::Result<()> {
        self.write_all(record)
    }

    fn sync_record(&mut self) -> io::Result<()> {
        self.sync_all()
    }

    fn read_record(&mut self, maximum_bytes: usize) -> io::Result<Vec<u8>> {
        self.seek(SeekFrom::Start(0))?;
        let mut observed = Vec::with_capacity(maximum_bytes);
        Read::by_ref(self)
            .take(maximum_bytes as u64)
            .read_to_end(&mut observed)?;
        Ok(observed)
    }
}

fn write_and_verify(target: &Mutex<Box<dyn MarkerTarget>>, record: &[u8]) -> Result<(), ()> {
    let mut target = target.lock().map_err(|_| ())?;
    target.reset().map_err(|_| ())?;
    target.write_record(record).map_err(|_| ())?;
    target.sync_record().map_err(|_| ())?;
    let observed = target
        .read_record(MAX_EVIDENCE_RECORD_BYTES + 1)
        .map_err(|_| ())?;
    if observed != record {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum InjectedFailure {
        PartialWrite,
        Sync,
    }

    struct FaultTarget {
        bytes: Arc<Mutex<Vec<u8>>>,
        failure: InjectedFailure,
    }

    impl MarkerTarget for FaultTarget {
        fn reset(&mut self) -> io::Result<()> {
            self.bytes.lock().expect("test bytes lock").clear();
            Ok(())
        }

        fn write_record(&mut self, record: &[u8]) -> io::Result<()> {
            let mut bytes = self.bytes.lock().expect("test bytes lock");
            match self.failure {
                InjectedFailure::PartialWrite => {
                    bytes.extend_from_slice(&record[..record.len() / 2]);
                    Err(io::Error::other("injected partial write"))
                }
                InjectedFailure::Sync => {
                    bytes.extend_from_slice(record);
                    Ok(())
                }
            }
        }

        fn sync_record(&mut self) -> io::Result<()> {
            match self.failure {
                InjectedFailure::PartialWrite => Ok(()),
                InjectedFailure::Sync => Err(io::Error::other("injected sync failure")),
            }
        }

        fn read_record(&mut self, _maximum_bytes: usize) -> io::Result<Vec<u8>> {
            Ok(self.bytes.lock().expect("test bytes lock").clone())
        }
    }

    #[test]
    fn partial_write_and_sync_failure_are_both_fixed_unknown_observations() {
        let record = b"canonical-reference-evidence".to_vec();

        for failure in [InjectedFailure::PartialWrite, InjectedFailure::Sync] {
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let prepared = PreparedMarker {
                target: Arc::new(Mutex::new(Box::new(FaultTarget {
                    bytes: Arc::clone(&bytes),
                    failure,
                }))),
                record: record.clone(),
                evidence_digest: AdapterEvidenceDigest::new(format!("sha256:{}", "0".repeat(64)))
                    .expect("test digest"),
            };

            assert_eq!(
                Box::new(prepared).execute(),
                AdapterExecutionObservation::Unknown {
                    reason: AdapterExecutionUnknownReason::ResponseUnverifiable,
                }
            );
            let observed = bytes.lock().expect("test bytes lock").clone();
            assert!(!observed.is_empty(), "failure must happen after mutation");
            match failure {
                InjectedFailure::PartialWrite => assert_ne!(observed, record),
                InjectedFailure::Sync => assert_eq!(observed, record),
            }
        }
    }
}
