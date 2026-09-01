use std::fmt::Write as _;
use std::io::Read;
use std::num::NonZeroUsize;
use std::sync::Arc;

use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsSyncExt as _};
use cap_std::fs::{Dir, File, OpenOptions};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_domain::{InstanceFeatures, VerificationResult, VerificationStrategy};
use xgeny_runtime::{
    AdapterEvidenceDigest, AdapterExecutionObservation, AdapterPrepareFailure,
    AdapterPrepareRequest, AdapterReconcileRequest, AdapterReconciliationInconclusiveReason,
    AdapterReconciliationObservation, AdapterToolOutput, EffectAdapter, EffectVerifier,
    PreparedAdapterInvocation, RuleVerificationObservation, VerificationPortFailure,
    VerificationReport, VerificationRequest, VerifiedArtifactDescriptor, VerifierOutputDigest,
};
use xgeny_workgraph::EffectClass;

use crate::path::{RelativePath, parse_canonical};
use crate::{READ_TEXT_CAPABILITY_ID, READ_TEXT_CONTRACT_VERSION, WorkspaceRoot};

/// Hard product ceiling chosen so worst-case JSON escaping remains below the durable-output bound
/// and the Core 512 KiB planning-context ceiling used by the MVP composition.
pub const MAX_READ_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadTextLimits {
    max_bytes: NonZeroUsize,
}

impl ReadTextLimits {
    /// Configure a positive read ceiling no greater than the product hard maximum.
    ///
    /// # Errors
    ///
    /// Rejects zero or a value above [`MAX_READ_TEXT_BYTES`].
    pub fn new(max_bytes: usize) -> Result<Self, ReadTextLimitsError> {
        let max_bytes = NonZeroUsize::new(max_bytes).ok_or(ReadTextLimitsError)?;
        if max_bytes.get() > MAX_READ_TEXT_BYTES {
            return Err(ReadTextLimitsError);
        }
        Ok(Self { max_bytes })
    }

    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.max_bytes.get()
    }
}

impl Default for ReadTextLimits {
    fn default() -> Self {
        Self {
            max_bytes: NonZeroUsize::new(MAX_READ_TEXT_BYTES)
                .expect("the fixed maximum is non-zero"),
        }
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("read-text byte limit is invalid")]
pub struct ReadTextLimitsError;

#[derive(Clone)]
pub struct ReadTextAdapter {
    root: WorkspaceRoot,
    limits: ReadTextLimits,
}

impl ReadTextAdapter {
    pub(crate) const fn new(root: WorkspaceRoot, limits: ReadTextLimits) -> Self {
        Self { root, limits }
    }

    /// Construct a verifier pinned to the same root-bound Instance identity.
    #[must_use]
    pub fn verifier(&self) -> ReadTextVerifier {
        ReadTextVerifier {
            expected_binding: self.root.binding(),
        }
    }
}

impl std::fmt::Debug for ReadTextAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadTextAdapter")
            .field("root", &"<preopened/redacted>")
            .field("limits", &self.limits)
            .finish()
    }
}

impl EffectAdapter for ReadTextAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
        verify_contract(&request, &self.root.binding())?;
        let relative_path = parse_arguments(request.normalized_arguments(), &self.root)?;
        Ok(Box::new(PreparedReadText {
            directory: Arc::clone(&self.root.directory),
            relative_path,
            limits: self.limits,
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

pub struct ReadTextVerifier {
    expected_binding: xgeny_domain::InstanceBinding,
}

impl std::fmt::Debug for ReadTextVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadTextVerifier")
            .field("expected_binding", &"<redacted>")
            .finish()
    }
}

impl EffectVerifier for ReadTextVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure> {
        verify_verifier_contract(&request, &self.expected_binding)?;
        let output = request
            .tool_output()
            .ok_or(VerificationPortFailure::EvidenceUnavailable)?;
        let verified_output =
            inspect_output(output.output(), request.outcome_evidence_digest().as_str())?;
        let evidence = AdapterEvidenceDigest::new(verified_output.actual_digest.clone())
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
        let rules = request
            .definition()
            .spec
            .verification
            .iter()
            .map(|rule| {
                let result = match rule.strategy {
                    VerificationStrategy::OutputSchema => VerificationResult::Passed,
                    VerificationStrategy::Postcondition if verified_output.postcondition => {
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
            "read-text-output",
            Option::<String>::None,
            "text/plain; charset=utf-8",
            verified_output.byte_size,
            verified_output.actual_digest,
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

struct PreparedReadText {
    directory: Arc<Dir>,
    relative_path: RelativePath,
    limits: ReadTextLimits,
}

impl std::fmt::Debug for PreparedReadText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedReadText")
            .field("directory", &"<preopened/redacted>")
            .field("relative_path", &"<redacted>")
            .field("limits", &self.limits)
            .finish()
    }
}

impl PreparedAdapterInvocation for PreparedReadText {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        match read_text(&self.directory, &self.relative_path, self.limits) {
            Ok(observation) => AdapterExecutionObservation::SucceededWithOutput {
                evidence_digest: AdapterEvidenceDigest::new(observation.digest.clone())
                    .expect("a SHA-256 digest is canonical"),
                output: AdapterToolOutput::new(json!({
                    "content": observation.content,
                    "digest": observation.digest,
                })),
            },
            Err(failure) => AdapterExecutionObservation::Failed {
                evidence_digest: AdapterEvidenceDigest::new(failure.evidence_digest())
                    .expect("a SHA-256 digest is canonical"),
            },
        }
    }
}

pub(crate) struct ReadObservation {
    pub(crate) content: String,
    pub(crate) digest: String,
}

struct InspectedOutput {
    actual_digest: String,
    byte_size: u64,
    postcondition: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ReadFailure {
    Open,
    NonRegular,
    TooLarge,
    Read,
    InvalidUtf8,
}

impl ReadFailure {
    fn evidence_digest(self) -> String {
        let code = match self {
            Self::Open => "open",
            Self::NonRegular => "non-regular",
            Self::TooLarge => "too-large",
            Self::Read => "read",
            Self::InvalidUtf8 => "invalid-utf8",
        };
        sha256_digest(format!("xgeny.fs/read-text/failure/v1/{code}").as_bytes())
    }
}

fn verify_contract(
    request: &AdapterPrepareRequest<'_>,
    expected_binding: &xgeny_domain::InstanceBinding,
) -> Result<(), AdapterPrepareFailure> {
    let intent = request.intent();
    let instance = request.instance();
    if intent.invocation.capability_id != READ_TEXT_CAPABILITY_ID
        || intent.invocation.contract_version != READ_TEXT_CONTRACT_VERSION
        || instance.definition.capability_id != READ_TEXT_CAPABILITY_ID
        || instance.definition.contract_version != READ_TEXT_CONTRACT_VERSION
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
) -> Result<(), VerificationPortFailure> {
    let intent = request.intent();
    let instance = request.instance();
    let rules = &request.definition().spec.verification;
    let output_schema_rules = rules
        .iter()
        .filter(|rule| rule.strategy == VerificationStrategy::OutputSchema)
        .count();
    let postcondition_rules = rules
        .iter()
        .filter(|rule| rule.strategy == VerificationStrategy::Postcondition)
        .count();
    if intent.invocation.capability_id != READ_TEXT_CAPABILITY_ID
        || intent.invocation.contract_version != READ_TEXT_CONTRACT_VERSION
        || instance.definition.capability_id != READ_TEXT_CAPABILITY_ID
        || instance.definition.contract_version != READ_TEXT_CONTRACT_VERSION
        || intent.invocation.instance_id != instance.instance_id
        || instance.binding != *expected_binding
        || !supports_instance_features(&instance.features)
        || intent.effect_class != EffectClass::ReadOnly
        || intent.idempotency_key.is_some()
        || rules.len() != 2
        || output_schema_rules != 1
        || postcondition_rules != 1
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
) -> Result<RelativePath, AdapterPrepareFailure> {
    let object = arguments
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    parse_canonical(&root.workspace_id, path).map_err(|_| AdapterPrepareFailure::InvalidMaterial)
}

fn inspect_output(
    output: &Value,
    outcome_evidence_digest: &str,
) -> Result<InspectedOutput, VerificationPortFailure> {
    let object = output
        .as_object()
        .filter(|object| object.len() == 2)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    let content = object
        .get("content")
        .and_then(Value::as_str)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    if content.len() > MAX_READ_TEXT_BYTES {
        return Err(VerificationPortFailure::ResponseUnverifiable);
    }
    let claimed_digest = object
        .get("digest")
        .and_then(Value::as_str)
        .ok_or(VerificationPortFailure::ResponseUnverifiable)?;
    AdapterEvidenceDigest::new(claimed_digest.to_owned())
        .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?;
    let actual_digest = sha256_digest(content.as_bytes());
    Ok(InspectedOutput {
        postcondition: claimed_digest == actual_digest && outcome_evidence_digest == actual_digest,
        byte_size: u64::try_from(content.len())
            .map_err(|_| VerificationPortFailure::ResponseUnverifiable)?,
        actual_digest,
    })
}

pub(crate) fn read_text(
    root: &Dir,
    relative_path: &RelativePath,
    limits: ReadTextLimits,
) -> Result<ReadObservation, ReadFailure> {
    let file = open_file(root, relative_path)?;
    read_opened_file(file, limits)
}

fn read_opened_file(
    mut file: File,
    limits: ReadTextLimits,
) -> Result<ReadObservation, ReadFailure> {
    let metadata = file.metadata().map_err(|_| ReadFailure::Open)?;
    if !metadata.is_file() || is_windows_reparse_point(&metadata) {
        return Err(ReadFailure::NonRegular);
    }
    let maximum = limits.max_bytes();
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(maximum)
            .min(maximum)
            .saturating_add(1),
    );
    file.by_ref()
        .take(u64::try_from(maximum).expect("usize fits u64") + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadFailure::Read)?;
    if bytes.len() > maximum {
        return Err(ReadFailure::TooLarge);
    }
    let digest = sha256_digest(&bytes);
    let content = String::from_utf8(bytes).map_err(|_| ReadFailure::InvalidUtf8)?;
    Ok(ReadObservation { content, digest })
}

fn open_file(root: &Dir, relative_path: &RelativePath) -> Result<File, ReadFailure> {
    let (leaf, parents) = relative_path
        .components()
        .split_last()
        .ok_or(ReadFailure::Open)?;
    let mut directory = root.try_clone().map_err(|_| ReadFailure::Open)?;
    for component in parents {
        directory = directory
            .open_dir_nofollow(component)
            .map_err(|_| ReadFailure::Open)?;
        if is_windows_reparse_point(&directory.dir_metadata().map_err(|_| ReadFailure::Open)?) {
            return Err(ReadFailure::Open);
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    directory
        .open_with(leaf, &options)
        .map_err(|_| ReadFailure::Open)
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

pub(crate) fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("sha256:{encoded}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::{WorkspaceId, WorkspaceRoot};
    use xgeny_workgraph::validate_tool_output_candidate;

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
            .expect("temporary workspace should open");
            Self { root, directory }
        }

        fn relative(&self, path: &str) -> RelativePath {
            parse_canonical(
                &self.root.workspace_id,
                &format!("workspace:fixture/{path}"),
            )
            .expect("fixture path should validate")
        }

        fn read(&self, path: &str, limits: ReadTextLimits) -> Result<ReadObservation, ReadFailure> {
            read_text(&self.root.directory, &self.relative(path), limits)
        }
    }

    #[test]
    fn reads_exact_utf8_bytes_and_digest_from_the_opened_handle() {
        let fixture = Fixture::new();
        let content = "첫 줄\nsecond \"quoted\" line\n\t끝";
        fs::write(fixture.directory.path().join("README.md"), content)
            .expect("fixture should write");

        let observation = fixture
            .read("README.md", ReadTextLimits::default())
            .expect("regular UTF-8 file should read");

        assert_eq!(observation.content.as_bytes(), content.as_bytes());
        assert_eq!(observation.digest, sha256_digest(content.as_bytes()));
    }

    #[test]
    fn bounded_reader_accepts_the_limit_and_rejects_limit_plus_one() {
        let fixture = Fixture::new();
        let limits = ReadTextLimits::new(32).expect("limit should validate");
        fs::write(fixture.directory.path().join("exact.txt"), [b'x'; 32])
            .expect("exact fixture should write");
        fs::write(fixture.directory.path().join("over.txt"), [b'y'; 33])
            .expect("oversized fixture should write");

        assert_eq!(
            fixture
                .read("exact.txt", limits)
                .expect("exact limit should read")
                .content
                .len(),
            32
        );
        assert!(matches!(
            fixture.read("over.txt", limits),
            Err(ReadFailure::TooLarge)
        ));
    }

    #[test]
    fn product_maximum_is_accepted_but_maximum_plus_one_is_rejected() {
        let fixture = Fixture::new();
        fs::write(
            fixture.directory.path().join("maximum.txt"),
            vec![b'x'; MAX_READ_TEXT_BYTES],
        )
        .expect("maximum fixture should write");
        fs::write(
            fixture.directory.path().join("maximum-plus-one.txt"),
            vec![b'y'; MAX_READ_TEXT_BYTES + 1],
        )
        .expect("oversized fixture should write");

        assert_eq!(
            fixture
                .read("maximum.txt", ReadTextLimits::default())
                .expect("product maximum should read")
                .content
                .len(),
            MAX_READ_TEXT_BYTES
        );
        assert!(matches!(
            fixture.read("maximum-plus-one.txt", ReadTextLimits::default()),
            Err(ReadFailure::TooLarge)
        ));

        let control_heavy = "\0".repeat(MAX_READ_TEXT_BYTES);
        let candidate = json!({
            "content": control_heavy,
            "digest": sha256_digest(control_heavy.as_bytes()),
        });
        validate_tool_output_candidate(&candidate)
            .expect("worst-case escaped product output must fit the Core hard bound");
    }

    #[test]
    fn invalid_utf8_missing_and_directory_targets_fail_closed() {
        let fixture = Fixture::new();
        fs::write(
            fixture.directory.path().join("invalid.bin"),
            [0xf0, 0x28, 0x8c, 0x28],
        )
        .expect("invalid UTF-8 fixture should write");
        fs::create_dir(fixture.directory.path().join("directory"))
            .expect("directory fixture should create");

        assert!(matches!(
            fixture.read("invalid.bin", ReadTextLimits::default()),
            Err(ReadFailure::InvalidUtf8)
        ));
        assert!(matches!(
            fixture.read("missing.txt", ReadTextLimits::default()),
            Err(ReadFailure::Open)
        ));
        assert!(matches!(
            fixture.read("directory", ReadTextLimits::default()),
            Err(ReadFailure::Open | ReadFailure::NonRegular)
        ));
    }

    #[test]
    fn an_opened_handle_never_reads_replacement_path_bytes() {
        let fixture = Fixture::new();
        let target = fixture.directory.path().join("target.txt");
        let moved = fixture.directory.path().join("opened-original.txt");
        let original = "ORIGINAL-HANDLE-CONTENT";
        let replacement = "OUTSIDE-REPLACEMENT-SECRET";
        fs::write(&target, original).expect("original fixture should write");
        let file = open_file(&fixture.root.directory, &fixture.relative("target.txt"))
            .expect("target should open");

        fs::rename(&target, &moved)
            .expect("the path entry must be replaced while the original handle remains open");
        fs::write(&target, replacement).expect("replacement fixture should write");
        let observation = read_opened_file(file, ReadTextLimits::default())
            .expect("opened handle should remain readable");

        assert_eq!(observation.content, original);
        assert!(!observation.content.contains(replacement));
        assert_eq!(
            fs::read_to_string(&target).expect("replacement path should remain readable"),
            replacement
        );
    }

    #[cfg(unix)]
    #[test]
    fn leaf_and_intermediate_symlinks_never_reach_an_outside_file() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = tempdir().expect("outside directory should exist");
        fs::write(outside.path().join("secret.txt"), "OUTSIDE-SYMLINK-SECRET")
            .expect("outside fixture should write");
        symlink(
            outside.path().join("secret.txt"),
            fixture.directory.path().join("leaf.txt"),
        )
        .expect("leaf symlink should create");
        symlink(outside.path(), fixture.directory.path().join("linked-dir"))
            .expect("directory symlink should create");

        assert!(matches!(
            fixture.read("leaf.txt", ReadTextLimits::default()),
            Err(ReadFailure::Open | ReadFailure::NonRegular)
        ));
        assert!(matches!(
            fixture.read("linked-dir/secret.txt", ReadTextLimits::default()),
            Err(ReadFailure::Open)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_is_rejected_without_blocking() {
        use std::os::unix::net::UnixListener;

        let fixture = Fixture::new();
        let socket = fixture.directory.path().join("agent.sock");
        let _listener = UnixListener::bind(&socket).expect("socket fixture should bind");

        assert!(matches!(
            fixture.read("agent.sock", ReadTextLimits::default()),
            Err(ReadFailure::Open | ReadFailure::NonRegular)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_junction_cannot_be_used_as_an_intermediate_component() {
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

        assert!(matches!(
            fixture.read("junction/secret.txt", ReadTextLimits::default()),
            Err(ReadFailure::Open)
        ));
        assert!(matches!(
            fixture.read("junction", ReadTextLimits::default()),
            Err(ReadFailure::Open | ReadFailure::NonRegular)
        ));
        fs::remove_dir(&junction).expect("junction fixture should remove without following it");
    }

    #[test]
    fn prepared_failures_are_fixed_redacted_failed_observations() {
        let fixture = Fixture::new();
        let execute = |path: &str| {
            Box::new(PreparedReadText {
                directory: Arc::clone(&fixture.root.directory),
                relative_path: fixture.relative(path),
                limits: ReadTextLimits::default(),
            })
            .execute()
        };
        let first = execute("missing-one-SENTINEL.txt");
        let second = execute("missing-two-SENTINEL.txt");
        let digest = |observation: AdapterExecutionObservation| match observation {
            AdapterExecutionObservation::Failed { evidence_digest } => {
                evidence_digest.as_str().to_owned()
            }
            other => panic!("missing read must be a definite failure, got {other:?}"),
        };
        let first_digest = digest(first);
        let second_digest = digest(second);

        assert_eq!(first_digest, second_digest);
        assert!(!first_digest.contains("SENTINEL"));
        let prepared = PreparedReadText {
            directory: Arc::clone(&fixture.root.directory),
            relative_path: fixture.relative("prepared-SENTINEL.txt"),
            limits: ReadTextLimits::default(),
        };
        assert!(!format!("{prepared:?}").contains("SENTINEL"));
    }

    #[test]
    fn argument_and_verifier_output_mismatches_fail_closed() {
        let fixture = Fixture::new();
        assert!(
            parse_arguments(
                &json!({"path": "workspace:fixture/README.md"}),
                &fixture.root,
            )
            .is_ok()
        );
        for arguments in [
            json!({"path": "README.md"}),
            json!({"path": "workspace:other/README.md"}),
            json!({"path": "workspace:fixture/README.md", "extra": true}),
            json!({"path": 3}),
        ] {
            assert_eq!(
                parse_arguments(&arguments, &fixture.root),
                Err(AdapterPrepareFailure::InvalidMaterial)
            );
        }

        let content = "durable observation";
        let digest = sha256_digest(content.as_bytes());
        let valid = inspect_output(&json!({"content": content, "digest": digest}), &digest)
            .expect("matching output should inspect");
        assert!(valid.postcondition);
        assert_eq!(valid.byte_size, content.len() as u64);
        let wrong_claim = sha256_digest(b"another observation");
        assert!(
            !inspect_output(&json!({"content": content, "digest": wrong_claim}), &digest,)
                .expect("canonical mismatch should produce a failed postcondition")
                .postcondition
        );
        assert!(
            !inspect_output(&json!({"content": content, "digest": digest}), &wrong_claim,)
                .expect("evidence mismatch should produce a failed postcondition")
                .postcondition
        );
        for output in [
            json!({"content": content}),
            json!({"content": content, "digest": "not-a-digest"}),
            json!({"content": content, "digest": digest, "extra": true}),
        ] {
            assert_eq!(
                inspect_output(&output, &digest).map(|_| ()),
                Err(VerificationPortFailure::ResponseUnverifiable)
            );
        }

        let maximum = "x".repeat(MAX_READ_TEXT_BYTES);
        let maximum_digest = sha256_digest(maximum.as_bytes());
        assert!(
            inspect_output(
                &json!({"content": maximum, "digest": maximum_digest}),
                &maximum_digest,
            )
            .is_ok()
        );
        let oversized = "x".repeat(MAX_READ_TEXT_BYTES + 1);
        let oversized_digest = sha256_digest(oversized.as_bytes());
        assert_eq!(
            inspect_output(
                &json!({"content": oversized, "digest": oversized_digest}),
                &oversized_digest,
            )
            .map(|_| ()),
            Err(VerificationPortFailure::ResponseUnverifiable)
        );
    }

    #[test]
    fn limits_and_debug_surfaces_do_not_disclose_candidates() {
        assert_eq!(ReadTextLimits::new(0), Err(ReadTextLimitsError));
        assert_eq!(
            ReadTextLimits::new(MAX_READ_TEXT_BYTES + 1),
            Err(ReadTextLimitsError)
        );
        let fixture = Fixture::new();
        let adapter = fixture.root.read_text_adapter(ReadTextLimits::default());
        let verifier = adapter.verifier();
        let rendered = format!("{adapter:?} {verifier:?} {:?}", fixture.root);
        assert!(!rendered.contains(fixture.directory.path().to_string_lossy().as_ref()));
        assert!(!rendered.contains("fixture"));
        assert!(!format!("{:?}", fixture.relative("secret.txt")).contains("secret"));
    }

    #[test]
    fn instance_must_not_advertise_unimplemented_execution_features() {
        let supported = InstanceFeatures {
            sync: true,
            task: false,
            cancellable: false,
            idempotency_query: false,
        };
        assert!(supports_instance_features(&supported));

        for unsupported in [
            InstanceFeatures {
                sync: false,
                ..supported.clone()
            },
            InstanceFeatures {
                task: true,
                ..supported.clone()
            },
            InstanceFeatures {
                cancellable: true,
                ..supported.clone()
            },
            InstanceFeatures {
                idempotency_query: true,
                ..supported
            },
        ] {
            assert!(!supports_instance_features(&unsupported));
        }
    }
}
