use xgeny_runtime::{
    MAX_VERIFIED_ARTIFACT_SIZE_BYTES, MAX_VERIFIED_ARTIFACTS, VerificationReport,
    VerificationReportError, VerifiedArtifactDescriptor, VerifierOutputDigest,
};

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn verified_artifacts_are_bounded_and_unique() {
    let artifact = VerifiedArtifactDescriptor::new(
        "artifact-output",
        Some("output.json"),
        "application/json",
        128,
        DIGEST,
    )
    .expect("bounded descriptor should be valid");
    let report = VerificationReport::new(
        VerifierOutputDigest::new(DIGEST).expect("digest should be valid"),
        Vec::new(),
    )
    .with_artifacts(vec![artifact.clone()])
    .expect("one artifact should be accepted");

    assert_eq!(report.artifacts(), std::slice::from_ref(&artifact));
    assert_eq!(artifact.artifact_id(), "artifact-output");
    assert_eq!(artifact.name(), Some("output.json"));
    assert_eq!(artifact.media_type(), "application/json");
    assert_eq!(artifact.size(), 128);
    assert_eq!(artifact.digest(), DIGEST);

    let duplicate = VerificationReport::new(
        VerifierOutputDigest::new(DIGEST).expect("digest should be valid"),
        Vec::new(),
    )
    .with_artifacts(vec![artifact.clone(), artifact]);
    assert_eq!(duplicate, Err(VerificationReportError::DuplicateArtifactId));

    let too_many = (0..=MAX_VERIFIED_ARTIFACTS)
        .map(|index| {
            VerifiedArtifactDescriptor::new(
                format!("artifact-{index}"),
                None::<String>,
                "application/json",
                0,
                DIGEST,
            )
            .expect("descriptor should be valid")
        })
        .collect();
    let oversized_report = VerificationReport::new(
        VerifierOutputDigest::new(DIGEST).expect("digest should be valid"),
        Vec::new(),
    )
    .with_artifacts(too_many);
    assert_eq!(
        oversized_report,
        Err(VerificationReportError::ArtifactCountExceeded)
    );

    let excessive_total = (0..5)
        .map(|index| {
            VerifiedArtifactDescriptor::new(
                format!("large-artifact-{index}"),
                None::<String>,
                "application/octet-stream",
                MAX_VERIFIED_ARTIFACT_SIZE_BYTES,
                DIGEST,
            )
            .expect("individual descriptor should remain within its bound")
        })
        .collect();
    let excessive_total_report = VerificationReport::new(
        VerifierOutputDigest::new(DIGEST).expect("digest should be valid"),
        Vec::new(),
    )
    .with_artifacts(excessive_total);
    assert_eq!(
        excessive_total_report,
        Err(VerificationReportError::ArtifactTotalSizeExceeded)
    );
}

#[test]
fn invalid_descriptor_errors_do_not_echo_candidates() {
    let secret = "secret-output-value";
    let error = VerifiedArtifactDescriptor::new(secret, None::<String>, "x", u64::MAX, secret)
        .expect_err("invalid descriptor must fail closed");

    assert!(!format!("{error:?} {error}").contains(secret));

    let descriptor =
        VerifiedArtifactDescriptor::new(secret, Some(secret), "application/json", 1, DIGEST)
            .expect(
                "valid sensitive metadata should remain accepted behind the trusted verifier port",
            );
    assert!(!format!("{descriptor:?}").contains(secret));
}
