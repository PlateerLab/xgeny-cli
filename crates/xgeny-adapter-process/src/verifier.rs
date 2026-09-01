use serde_json::Value;
use xgeny_domain::{InstanceBinding, InstanceFeatures, VerificationResult, VerificationStrategy};
use xgeny_runtime::{
    AdapterEvidenceDigest, EffectVerifier, RuleVerificationObservation, VerificationPortFailure,
    VerificationReport, VerificationRequest, VerifiedArtifactDescriptor, VerifierOutputDigest,
};
use xgeny_workgraph::EffectClass;

use crate::execution::{MAX_CAPTURE_BYTES, MAX_RESULT_DURATION_MS};
use crate::{PROCESS_EXECUTE_CAPABILITY_ID, PROCESS_EXECUTE_CONTRACT_VERSION};

/// Verifies the exact durable result emitted by one process Instance binding.
pub struct ProcessExecuteVerifier {
    expected_binding: InstanceBinding,
}

impl ProcessExecuteVerifier {
    pub(crate) const fn new(expected_binding: InstanceBinding) -> Self {
        Self { expected_binding }
    }
}

impl std::fmt::Debug for ProcessExecuteVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessExecuteVerifier")
            .field("expected_binding", &"<redacted>")
            .finish()
    }
}

impl EffectVerifier for ProcessExecuteVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        verify_contract(&request, &self.expected_binding)?;
        let output = request
            .tool_output()
            .ok_or(VerificationPortFailure::EvidenceUnavailable)?;
        let inspected = inspect_output(
            output.output(),
            Some(request.outcome_evidence_digest().as_str()),
        )
        .map_err(|()| VerificationPortFailure::ResponseUnverifiable)?;
        let evidence = AdapterEvidenceDigest::new(inspected.digest.clone())
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        let rules = request
            .definition()
            .spec
            .verification
            .iter()
            .map(|rule| {
                if rule.strategy != VerificationStrategy::OutputSchema {
                    return Err(VerificationPortFailure::UnsupportedStrategy);
                }
                Ok(RuleVerificationObservation::new(
                    rule.strategy,
                    VerificationResult::Passed,
                    Some(
                        AdapterEvidenceDigest::new(evidence.as_str().to_owned())
                            .expect("validated evidence remains canonical"),
                    ),
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let artifact = VerifiedArtifactDescriptor::new(
            "process-execute-output",
            Option::<String>::None,
            "application/json",
            inspected.canonical_size_bytes,
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

pub(crate) struct InspectedOutput {
    digest: String,
    canonical_size_bytes: u64,
}

pub(crate) fn inspect_output(
    output: &Value,
    expected_evidence_digest: Option<&str>,
) -> Result<InspectedOutput, ()> {
    let object = output
        .as_object()
        .filter(|object| object.len() == 8)
        .ok_or(())?;
    let outcome = object.get("outcome").and_then(Value::as_str).ok_or(())?;
    let success = object.get("success").and_then(Value::as_bool).ok_or(())?;
    let exit_code = match object.get("exitCode").ok_or(())? {
        Value::Null => None,
        value => value
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .map(Some)
            .ok_or(())?,
    };
    for field in ["stdout", "stderr"] {
        let stream = object.get(field).and_then(Value::as_str).ok_or(())?;
        if stream.len() > MAX_CAPTURE_BYTES {
            return Err(());
        }
    }
    if !object.get("stdoutTruncated").is_some_and(Value::is_boolean)
        || !object.get("stderrTruncated").is_some_and(Value::is_boolean)
    {
        return Err(());
    }
    object
        .get("durationMs")
        .and_then(Value::as_u64)
        .filter(|duration| *duration <= MAX_RESULT_DURATION_MS)
        .ok_or(())?;
    match outcome {
        "exited" if success == exit_code.is_some_and(|code| code == 0) => {}
        "timed_out" | "launch_failed" if !success && exit_code.is_none() => {}
        _ => return Err(()),
    }
    let canonical = serde_jcs::to_vec(output).map_err(|_| ())?;
    let digest = crate::execution::sha256_digest(&canonical);
    if expected_evidence_digest.is_some_and(|expected| expected != digest) {
        return Err(());
    }
    Ok(InspectedOutput {
        digest,
        canonical_size_bytes: u64::try_from(canonical.len()).map_err(|_| ())?,
    })
}

fn verify_contract(
    request: &VerificationRequest<'_>,
    expected_binding: &InstanceBinding,
) -> Result<(), VerificationPortFailure> {
    let intent = request.intent();
    let instance = request.instance();
    let rules = &request.definition().spec.verification;
    if intent.invocation.capability_id != PROCESS_EXECUTE_CAPABILITY_ID
        || intent.invocation.contract_version != PROCESS_EXECUTE_CONTRACT_VERSION
        || instance.definition.capability_id != PROCESS_EXECUTE_CAPABILITY_ID
        || instance.definition.contract_version != PROCESS_EXECUTE_CONTRACT_VERSION
        || intent.invocation.instance_id != instance.instance_id
        || instance.binding != *expected_binding
        || !supports_instance_features(&instance.features)
        || intent.effect_class != EffectClass::NonIdempotent
        || intent.idempotency_key.as_deref().is_none_or(str::is_empty)
        || !request.definition().spec.execution.durable_tool_output
        || rules.len() != 1
        || rules[0].strategy != VerificationStrategy::OutputSchema
        || !rules[0].required
    {
        return Err(VerificationPortFailure::UnsupportedStrategy);
    }
    Ok(())
}

const fn supports_instance_features(features: &InstanceFeatures) -> bool {
    features.sync && !features.task && !features.cancellable && !features.idempotency_query
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::execution::canonical_output_digest;

    fn valid_output() -> Value {
        json!({
            "outcome": "exited",
            "success": true,
            "exitCode": 0,
            "stdout": "ok\n",
            "stderr": "",
            "stdoutTruncated": false,
            "stderrTruncated": false,
            "durationMs": 12,
        })
    }

    #[test]
    fn output_shape_and_evidence_are_exact() {
        let output = valid_output();
        let digest = canonical_output_digest(&output).unwrap();
        assert!(inspect_output(&output, Some(&digest)).is_ok());

        let mut extra = output.clone();
        extra["extra"] = Value::Bool(true);
        assert!(inspect_output(&extra, None).is_err());
        assert!(inspect_output(&output, Some(&format!("sha256:{}", "0".repeat(64)))).is_err());
    }

    #[test]
    fn outcome_exit_code_and_bounds_are_consistent() {
        let mut output = valid_output();
        output["success"] = Value::Bool(false);
        assert!(inspect_output(&output, None).is_err());

        output["outcome"] = Value::String("timed_out".to_owned());
        output["exitCode"] = Value::Null;
        assert!(inspect_output(&output, None).is_ok());

        output["stdout"] = Value::String("x".repeat(MAX_CAPTURE_BYTES + 1));
        assert!(inspect_output(&output, None).is_err());

        output["stdout"] = Value::String("가".repeat((MAX_CAPTURE_BYTES / 3) + 1));
        assert!(output["stdout"].as_str().unwrap().chars().count() < MAX_CAPTURE_BYTES);
        assert!(inspect_output(&output, None).is_err());
    }
}
