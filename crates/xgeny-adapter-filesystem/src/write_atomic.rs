use std::fmt::Write as _;
use std::io::{ErrorKind, Read as _, Write as _};
use std::sync::Arc;

#[cfg(unix)]
use cap_fs_ext::OpenOptionsMaybeDirExt as _;
use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsSyncExt as _};
use cap_std::fs::{Dir, File, OpenOptions, Permissions};
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

use crate::path::{RelativePath, parse_canonical};
use crate::read_text::{ReadTextLimits, read_text, sha256_digest};
use crate::{WRITE_ATOMIC_CAPABILITY_ID, WRITE_ATOMIC_CONTRACT_VERSION, WorkspaceRoot};

/// Hard UTF-8 byte ceiling for one atomic write.
pub const MAX_WRITE_ATOMIC_BYTES: usize = 64 * 1024;
const TEMP_CREATE_ATTEMPTS: usize = 8;

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
        match write_atomic(
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
            Err(
                WriteFailure::CommitRenameUnknown
                | WriteFailure::CommitSyncUnknown
                | WriteFailure::CommitVerifyUnknown,
            ) => AdapterExecutionObservation::Unknown {
                reason: AdapterExecutionUnknownReason::TransportOutcomeUnknown,
            },
            Err(failure) => AdapterExecutionObservation::Failed {
                evidence_digest: AdapterEvidenceDigest::new(failure.evidence_digest())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteFailure {
    OpenParent,
    InspectTarget,
    Conflict,
    CreateTemporary,
    WriteTemporary,
    CommitRenameUnknown,
    CommitSyncUnknown,
    CommitVerifyUnknown,
}

impl WriteFailure {
    fn evidence_digest(self) -> String {
        let code = match self {
            Self::OpenParent => "open-parent",
            Self::InspectTarget => "inspect-target",
            Self::Conflict => "precondition-conflict",
            Self::CreateTemporary => "create-temporary",
            Self::WriteTemporary => "write-temporary",
            Self::CommitRenameUnknown => "commit-rename-unknown",
            Self::CommitSyncUnknown => "commit-sync-unknown",
            Self::CommitVerifyUnknown => "commit-verify-unknown",
        };
        sha256_digest(format!("xgeny.fs/write-atomic/failure/v1/{code}").as_bytes())
    }
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

fn write_atomic(
    root: &Dir,
    relative_path: &RelativePath,
    content: &[u8],
    expected_digest: Option<&str>,
    desired_digest: &str,
) -> Result<bool, WriteFailure> {
    let (parent, leaf) = open_parent(root, relative_path)?;
    let initial = inspect_target(&parent, &leaf)?;
    if initial
        .as_ref()
        .is_some_and(|target| target.digest == desired_digest)
    {
        return Ok(false);
    }
    verify_precondition(initial.as_ref(), expected_digest)?;
    let (temporary_name, mut temporary) = create_temporary(&parent)?;
    if let Some(target) = &initial {
        temporary
            .set_permissions(target.permissions.clone())
            .map_err(|_| WriteFailure::WriteTemporary)?;
    }
    if temporary.write_all(content).is_err() || temporary.sync_all().is_err() {
        drop(temporary);
        let _ = parent.remove_file(&temporary_name);
        return Err(WriteFailure::WriteTemporary);
    }
    drop(temporary);

    let current = match inspect_target(&parent, &leaf) {
        Ok(current) => current,
        Err(error) => {
            let _ = parent.remove_file(&temporary_name);
            return Err(error);
        }
    };
    if current
        .as_ref()
        .is_some_and(|target| target.digest == desired_digest)
    {
        let _ = parent.remove_file(&temporary_name);
        return Ok(false);
    }
    if let Err(error) = verify_same_observation(initial.as_ref(), current.as_ref()) {
        let _ = parent.remove_file(&temporary_name);
        return Err(error);
    }
    if parent.rename(&temporary_name, &parent, &leaf).is_err() {
        let _ = parent.remove_file(&temporary_name);
        return Err(WriteFailure::CommitRenameUnknown);
    }
    if !sync_parent(&parent) {
        return Err(WriteFailure::CommitSyncUnknown);
    }
    let committed =
        inspect_target(&parent, &leaf).map_err(|_| WriteFailure::CommitVerifyUnknown)?;
    if committed.as_ref().map(|target| target.digest.as_str()) != Some(desired_digest) {
        return Err(WriteFailure::CommitVerifyUnknown);
    }
    Ok(true)
}

struct TargetObservation {
    digest: String,
    permissions: Permissions,
}

fn open_parent(root: &Dir, path: &RelativePath) -> Result<(Dir, String), WriteFailure> {
    let (leaf, parents) = path
        .components()
        .split_last()
        .ok_or(WriteFailure::OpenParent)?;
    let mut directory = root.try_clone().map_err(|_| WriteFailure::OpenParent)?;
    for component in parents {
        directory = directory
            .open_dir_nofollow(component)
            .map_err(|_| WriteFailure::OpenParent)?;
        if is_windows_reparse_point(
            &directory
                .dir_metadata()
                .map_err(|_| WriteFailure::OpenParent)?,
        ) {
            return Err(WriteFailure::OpenParent);
        }
    }
    Ok((directory, leaf.clone()))
}

fn inspect_target(parent: &Dir, leaf: &str) -> Result<Option<TargetObservation>, WriteFailure> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = match parent.open_with(leaf, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(WriteFailure::InspectTarget),
    };
    let metadata = file.metadata().map_err(|_| WriteFailure::InspectTarget)?;
    if !metadata.is_file() || is_windows_reparse_point(&metadata) {
        return Err(WriteFailure::InspectTarget);
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(MAX_WRITE_ATOMIC_BYTES)
            .min(MAX_WRITE_ATOMIC_BYTES)
            .saturating_add(1),
    );
    std::io::Read::by_ref(&mut file)
        .take(u64::try_from(MAX_WRITE_ATOMIC_BYTES).expect("usize fits u64") + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| WriteFailure::InspectTarget)?;
    if bytes.len() > MAX_WRITE_ATOMIC_BYTES {
        return Err(WriteFailure::InspectTarget);
    }
    Ok(Some(TargetObservation {
        digest: sha256_digest(&bytes),
        permissions: metadata.permissions(),
    }))
}

fn verify_precondition(
    target: Option<&TargetObservation>,
    expected_digest: Option<&str>,
) -> Result<(), WriteFailure> {
    match (target, expected_digest) {
        (None, None) => Ok(()),
        (Some(target), Some(expected)) if target.digest == expected => Ok(()),
        _ => Err(WriteFailure::Conflict),
    }
}

fn verify_same_observation(
    initial: Option<&TargetObservation>,
    current: Option<&TargetObservation>,
) -> Result<(), WriteFailure> {
    match (initial, current) {
        (None, None) => Ok(()),
        (Some(initial), Some(current))
            if initial.digest == current.digest && initial.permissions == current.permissions =>
        {
            Ok(())
        }
        _ => Err(WriteFailure::Conflict),
    }
}

fn create_temporary(parent: &Dir) -> Result<(String, File), WriteFailure> {
    for _ in 0..TEMP_CREATE_ATTEMPTS {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| WriteFailure::CreateTemporary)?;
        let mut encoded = String::with_capacity(32);
        for byte in random {
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        let name = format!(".xgeny-write-{encoded}.tmp");
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No)
            .sync(false);
        match parent.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(_) => return Err(WriteFailure::CreateTemporary),
        }
    }
    Err(WriteFailure::CreateTemporary)
}

#[cfg(unix)]
fn sync_parent(parent: &Dir) -> bool {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .maybe_dir(true)
        .follow(FollowSymlinks::No);
    parent
        .open_with(".", &options)
        .and_then(|directory| directory.sync_all())
        .is_ok()
}

#[cfg(not(unix))]
const fn sync_parent(_parent: &Dir) -> bool {
    true
}

fn canonical_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
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
            let directory = tempdir().unwrap();
            let root =
                WorkspaceRoot::open_ambient(directory.path(), WorkspaceId::new("fixture").unwrap())
                    .unwrap();
            Self { root, directory }
        }

        fn relative(&self, path: &str) -> RelativePath {
            parse_canonical(
                &self.root.workspace_id,
                &format!("workspace:fixture/{path}"),
            )
            .unwrap()
        }

        fn write(
            &self,
            path: &str,
            content: &str,
            expected: Option<&str>,
        ) -> Result<bool, WriteFailure> {
            write_atomic(
                &self.root.directory,
                &self.relative(path),
                content.as_bytes(),
                expected,
                &sha256_digest(content.as_bytes()),
            )
        }
    }

    #[test]
    fn creates_and_replaces_without_exposing_partial_content() {
        let fixture = Fixture::new();
        assert!(fixture.write("new.txt", "first", None).unwrap());
        let first_digest = sha256_digest(b"first");
        assert!(
            fixture
                .write("new.txt", "second", Some(&first_digest))
                .unwrap()
        );
        assert_eq!(
            fs::read_to_string(fixture.directory.path().join("new.txt")).unwrap(),
            "second"
        );
        assert!(
            fs::read_dir(fixture.directory.path())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".xgeny-write-"))
        );
    }

    #[test]
    fn stale_digest_and_create_collision_do_not_mutate_target() {
        let fixture = Fixture::new();
        fs::write(fixture.directory.path().join("target.txt"), "user edit").unwrap();
        assert_eq!(
            fixture.write("target.txt", "model edit", Some(&sha256_digest(b"old"))),
            Err(WriteFailure::Conflict)
        );
        assert_eq!(
            fixture.write("target.txt", "model edit", None),
            Err(WriteFailure::Conflict)
        );
        assert_eq!(
            fs::read_to_string(fixture.directory.path().join("target.txt")).unwrap(),
            "user edit"
        );
    }

    #[test]
    fn exact_desired_bytes_are_an_idempotent_success() {
        let fixture = Fixture::new();
        fs::write(fixture.directory.path().join("target.txt"), "desired").unwrap();
        assert!(
            !fixture
                .write("target.txt", "desired", Some(&sha256_digest(b"stale")))
                .unwrap()
        );
        assert_eq!(
            fs::read_to_string(fixture.directory.path().join("target.txt")).unwrap(),
            "desired"
        );
    }

    #[test]
    fn permission_drift_is_a_conflict_even_when_content_is_unchanged() {
        let fixture = Fixture::new();
        fs::write(fixture.directory.path().join("target.txt"), "same").unwrap();
        let initial = inspect_target(&fixture.root.directory, "target.txt")
            .unwrap()
            .unwrap();
        let mut permissions = initial.permissions.clone();
        permissions.set_readonly(!permissions.readonly());
        let current = TargetObservation {
            digest: initial.digest.clone(),
            permissions,
        };
        assert_eq!(
            verify_same_observation(Some(&initial), Some(&current)),
            Err(WriteFailure::Conflict)
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_existing_unix_permission_bits() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::new();
        let target = fixture.directory.path().join("script.sh");
        fs::write(&target, "old\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o751)).unwrap();
        assert!(
            fixture
                .write("script.sh", "new\n", Some(&sha256_digest(b"old\n")))
                .unwrap()
        );
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_leaf_and_parent_never_write_outside_workspace() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            fixture.directory.path().join("leaf.txt"),
        )
        .unwrap();
        symlink(outside.path(), fixture.directory.path().join("linked")).unwrap();
        assert!(fixture.write("leaf.txt", "changed", None).is_err());
        assert!(
            fixture
                .write(
                    "linked/secret.txt",
                    "changed",
                    Some(&sha256_digest(b"outside"))
                )
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "outside"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_junction_parent_never_writes_outside_workspace() {
        use std::process::Command;

        let fixture = Fixture::new();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        let junction = fixture.directory.path().join("junction");
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .status()
            .expect("junction command should run");
        assert!(status.success(), "junction fixture must be available in CI");

        assert!(
            fixture
                .write(
                    "junction/secret.txt",
                    "changed",
                    Some(&sha256_digest(b"outside"))
                )
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "outside"
        );
        fs::remove_dir(&junction).expect("junction should remove without following it");
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
