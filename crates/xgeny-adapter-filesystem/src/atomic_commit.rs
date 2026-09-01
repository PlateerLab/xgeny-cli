use std::fmt::Write as _;
use std::io::{ErrorKind, Read as _, Write as _};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsMaybeDirExt as _;
use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsSyncExt as _};
use cap_std::fs::{Dir, File, OpenOptions, Permissions};

use crate::path::RelativePath;
use crate::read_text::sha256_digest;

pub(crate) const MAX_ATOMIC_TEXT_BYTES: usize = 64 * 1024;
const TEMP_CREATE_ATTEMPTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomicCommitFailure {
    OpenParent,
    InspectTarget,
    Conflict,
    CreateTemporary,
    WriteTemporary,
    CommitRenameUnknown,
    CommitSyncUnknown,
    CommitVerifyUnknown,
}

impl AtomicCommitFailure {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::OpenParent => "open-parent",
            Self::InspectTarget => "inspect-target",
            Self::Conflict => "precondition-conflict",
            Self::CreateTemporary => "create-temporary",
            Self::WriteTemporary => "write-temporary",
            Self::CommitRenameUnknown => "commit-rename-unknown",
            Self::CommitSyncUnknown => "commit-sync-unknown",
            Self::CommitVerifyUnknown => "commit-verify-unknown",
        }
    }

    pub(crate) const fn outcome_is_unknown(self) -> bool {
        matches!(
            self,
            Self::CommitRenameUnknown | Self::CommitSyncUnknown | Self::CommitVerifyUnknown
        )
    }
}

pub(crate) fn commit_atomic(
    root: &Dir,
    relative_path: &RelativePath,
    content: &[u8],
    expected_digest: Option<&str>,
    desired_digest: &str,
) -> Result<bool, AtomicCommitFailure> {
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
            .map_err(|_| AtomicCommitFailure::WriteTemporary)?;
    }
    if temporary.write_all(content).is_err() || temporary.sync_all().is_err() {
        drop(temporary);
        let _ = parent.remove_file(&temporary_name);
        return Err(AtomicCommitFailure::WriteTemporary);
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
        return Err(AtomicCommitFailure::CommitRenameUnknown);
    }
    if !sync_parent(&parent) {
        return Err(AtomicCommitFailure::CommitSyncUnknown);
    }
    let committed =
        inspect_target(&parent, &leaf).map_err(|_| AtomicCommitFailure::CommitVerifyUnknown)?;
    if committed.as_ref().map(|target| target.digest.as_str()) != Some(desired_digest) {
        return Err(AtomicCommitFailure::CommitVerifyUnknown);
    }
    Ok(true)
}

struct TargetObservation {
    digest: String,
    permissions: Permissions,
}

fn open_parent(root: &Dir, path: &RelativePath) -> Result<(Dir, String), AtomicCommitFailure> {
    let (leaf, parents) = path
        .components()
        .split_last()
        .ok_or(AtomicCommitFailure::OpenParent)?;
    let mut directory = root
        .try_clone()
        .map_err(|_| AtomicCommitFailure::OpenParent)?;
    for component in parents {
        directory = directory
            .open_dir_nofollow(component)
            .map_err(|_| AtomicCommitFailure::OpenParent)?;
        if is_windows_reparse_point(
            &directory
                .dir_metadata()
                .map_err(|_| AtomicCommitFailure::OpenParent)?,
        ) {
            return Err(AtomicCommitFailure::OpenParent);
        }
    }
    Ok((directory, leaf.clone()))
}

fn inspect_target(
    parent: &Dir,
    leaf: &str,
) -> Result<Option<TargetObservation>, AtomicCommitFailure> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = match parent.open_with(leaf, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(AtomicCommitFailure::InspectTarget),
    };
    let metadata = file
        .metadata()
        .map_err(|_| AtomicCommitFailure::InspectTarget)?;
    if !metadata.is_file() || is_windows_reparse_point(&metadata) {
        return Err(AtomicCommitFailure::InspectTarget);
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(MAX_ATOMIC_TEXT_BYTES)
            .min(MAX_ATOMIC_TEXT_BYTES)
            .saturating_add(1),
    );
    std::io::Read::by_ref(&mut file)
        .take(u64::try_from(MAX_ATOMIC_TEXT_BYTES).expect("usize fits u64") + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AtomicCommitFailure::InspectTarget)?;
    if bytes.len() > MAX_ATOMIC_TEXT_BYTES {
        return Err(AtomicCommitFailure::InspectTarget);
    }
    Ok(Some(TargetObservation {
        digest: sha256_digest(&bytes),
        permissions: metadata.permissions(),
    }))
}

fn verify_precondition(
    target: Option<&TargetObservation>,
    expected_digest: Option<&str>,
) -> Result<(), AtomicCommitFailure> {
    match (target, expected_digest) {
        (None, None) => Ok(()),
        (Some(target), Some(expected)) if target.digest == expected => Ok(()),
        _ => Err(AtomicCommitFailure::Conflict),
    }
}

fn verify_same_observation(
    initial: Option<&TargetObservation>,
    current: Option<&TargetObservation>,
) -> Result<(), AtomicCommitFailure> {
    match (initial, current) {
        (None, None) => Ok(()),
        (Some(initial), Some(current))
            if initial.digest == current.digest && initial.permissions == current.permissions =>
        {
            Ok(())
        }
        _ => Err(AtomicCommitFailure::Conflict),
    }
}

fn create_temporary(parent: &Dir) -> Result<(String, File), AtomicCommitFailure> {
    for _ in 0..TEMP_CREATE_ATTEMPTS {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| AtomicCommitFailure::CreateTemporary)?;
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
            Err(_) => return Err(AtomicCommitFailure::CreateTemporary),
        }
    }
    Err(AtomicCommitFailure::CreateTemporary)
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
    use crate::path::parse_canonical;
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
        ) -> Result<bool, AtomicCommitFailure> {
            commit_atomic(
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
            Err(AtomicCommitFailure::Conflict)
        );
        assert_eq!(
            fixture.write("target.txt", "model edit", None),
            Err(AtomicCommitFailure::Conflict)
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
            Err(AtomicCommitFailure::Conflict)
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
}
