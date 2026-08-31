use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::manifest::{RunManifest, valid_run_id};

const RUNS_DIRECTORY: &str = "runs";
const MANIFEST_FILE: &str = "manifest.json";
const DATABASE_FILE: &str = "run.sqlite3";
const MATERIAL_CATALOG_FILE: &str = "materials.sqlite3";
const LEASE_FILE: &str = "run.lock";
const MAX_MANIFEST_FILE_BYTES: u64 = 64 * 1024;

pub(crate) struct RunLayout {
    run_id: String,
    directory: PathBuf,
}

impl RunLayout {
    pub(crate) fn create(state_root: &Path, run_id: &str) -> Result<Self, RunLayoutError> {
        validate_state_root(state_root)?;
        if !valid_run_id(run_id) {
            return Err(RunLayoutError::InvalidRunId);
        }
        ensure_private_state_root(state_root)?;
        let runs = state_root.join(RUNS_DIRECTORY);
        create_or_validate_private_directory(&runs)?;
        let directory = runs.join(run_id);
        create_private_directory(&directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                RunLayoutError::AlreadyExists
            } else {
                RunLayoutError::Unavailable
            }
        })?;
        Ok(Self {
            run_id: run_id.to_owned(),
            directory,
        })
    }

    pub(crate) fn existing(state_root: &Path, run_id: &str) -> Result<Self, RunLayoutError> {
        validate_state_root(state_root)?;
        if !valid_run_id(run_id) {
            return Err(RunLayoutError::InvalidRunId);
        }
        validate_existing_private_state_root(state_root)?;
        let runs = state_root.join(RUNS_DIRECTORY);
        reject_symlink_or_non_directory(&runs)?;
        verify_private_directory(&runs)?;
        let directory = runs.join(run_id);
        reject_symlink_or_non_directory(&directory)?;
        verify_private_directory(&directory)?;
        Ok(Self {
            run_id: run_id.to_owned(),
            directory,
        })
    }

    pub(crate) fn write_manifest(&self, manifest: &RunManifest) -> Result<(), RunLayoutError> {
        if manifest.run_id() != self.run_id {
            return Err(RunLayoutError::InvalidManifest);
        }
        let bytes = manifest
            .to_bytes()
            .map_err(|_| RunLayoutError::InvalidManifest)?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(self.manifest_path())
            .map_err(|_| RunLayoutError::Unavailable)?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| RunLayoutError::Unavailable)
    }

    pub(crate) fn read_manifest(&self) -> Result<RunManifest, RunLayoutError> {
        reject_symlink_or_non_file(&self.manifest_path())?;
        let file = File::open(self.manifest_path()).map_err(|_| RunLayoutError::Unavailable)?;
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| RunLayoutError::Unavailable)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_FILE_BYTES {
            return Err(RunLayoutError::InvalidManifest);
        }
        let manifest =
            RunManifest::from_bytes(&bytes).map_err(|_| RunLayoutError::InvalidManifest)?;
        if manifest.run_id() != self.run_id {
            return Err(RunLayoutError::InvalidManifest);
        }
        Ok(manifest)
    }

    pub(crate) fn database_path(&self) -> PathBuf {
        self.directory.join(DATABASE_FILE)
    }

    pub(crate) fn lease_path(&self) -> PathBuf {
        self.directory.join(LEASE_FILE)
    }

    pub(crate) fn material_catalog_path(&self) -> PathBuf {
        self.directory.join(MATERIAL_CATALOG_FILE)
    }

    fn manifest_path(&self) -> PathBuf {
        self.directory.join(MANIFEST_FILE)
    }
}

impl fmt::Debug for RunLayout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunLayout")
            .field("run_id", &self.run_id)
            .field("directory", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunLayoutError {
    #[error("Run identifier is invalid")]
    InvalidRunId,
    #[error("Run already exists")]
    AlreadyExists,
    #[error("Run layout is unavailable")]
    Unavailable,
    #[error("Run manifest is invalid")]
    InvalidManifest,
}

pub(crate) fn discover_state_root() -> Result<PathBuf, RunLayoutError> {
    if let Some(value) = env::var_os("XGENY_STATE_HOME") {
        let path = PathBuf::from(value);
        return validate_state_root(&path).map(|()| path);
    }

    #[cfg(target_os = "windows")]
    let root = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("XGENy"));
    #[cfg(target_os = "macos")]
    let root = env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join("Library/Application Support/XGENy"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .map(|path| path.join("xgeny"))
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".local/state/xgeny"))
        });
    #[cfg(not(any(unix, target_os = "windows")))]
    let root: Option<PathBuf> = None;

    let root = root.ok_or(RunLayoutError::Unavailable)?;
    validate_state_root(&root)?;
    Ok(root)
}

pub(crate) fn generate_run_id() -> Result<String, RunLayoutError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| RunLayoutError::Unavailable)?;
    let mut encoded = String::with_capacity(32);
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(format!("run-{encoded}"))
}

fn validate_state_root(path: &Path) -> Result<(), RunLayoutError> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || normal_component_count(path) < 2
        || is_environment_base_directory(path)
        || !is_supported_state_root_namespace(path)
    {
        return Err(RunLayoutError::Unavailable);
    }
    Ok(())
}

#[cfg(windows)]
fn is_supported_state_root_namespace(path: &Path) -> bool {
    use std::path::Prefix;

    path.components().next().is_some_and(|component| {
        matches!(
            component,
            Component::Prefix(prefix)
                if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        )
    })
}

#[cfg(not(windows))]
const fn is_supported_state_root_namespace(_path: &Path) -> bool {
    true
}

fn normal_component_count(path: &Path) -> usize {
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
}

fn is_environment_base_directory(path: &Path) -> bool {
    let canonical_path = fs::canonicalize(path).ok();
    ["HOME", "USERPROFILE", "LOCALAPPDATA", "XDG_STATE_HOME"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .chain(std::iter::once(env::temp_dir()))
        .any(|base| {
            if base == path {
                return true;
            }
            let canonical_base = fs::canonicalize(base).ok();
            canonical_base.as_deref() == Some(path)
                || canonical_path
                    .as_deref()
                    .zip(canonical_base.as_deref())
                    .is_some_and(|(candidate, base)| candidate == base)
        })
}

fn reject_symlink_or_non_directory(path: &Path) -> Result<(), RunLayoutError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RunLayoutError::Unavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RunLayoutError::Unavailable);
    }
    Ok(())
}

fn reject_symlink_or_non_file(path: &Path) -> Result<(), RunLayoutError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RunLayoutError::Unavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RunLayoutError::Unavailable);
    }
    Ok(())
}

fn ensure_private_state_root(path: &Path) -> Result<(), RunLayoutError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(RunLayoutError::Unavailable);
            }
            return validate_existing_private_state_root(path);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(RunLayoutError::Unavailable),
    }

    // Host platforms may expose ordinary absolute paths through an ancestor symlink (notably
    // macOS `/var -> private/var`). Resolve the deepest existing ancestor once, then create only
    // the app-owned missing suffix without following any newly introduced final-component link.
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        let component = existing.file_name().ok_or(RunLayoutError::Unavailable)?;
        missing.push(component.to_os_string());
        existing = existing.parent().ok_or(RunLayoutError::Unavailable)?;
        match fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(RunLayoutError::Unavailable),
        }
    }
    if !fs::metadata(existing)
        .map_err(|_| RunLayoutError::Unavailable)?
        .is_dir()
    {
        return Err(RunLayoutError::Unavailable);
    }
    let physical_parent = fs::canonicalize(existing).map_err(|_| RunLayoutError::Unavailable)?;
    let mut prospective = physical_parent.clone();
    for component in missing.iter().rev() {
        prospective.push(component);
    }
    validate_state_root(&prospective)?;

    let mut physical = physical_parent;
    for component in missing.iter().rev() {
        physical.push(component);
        create_private_directory(&physical).map_err(|_| RunLayoutError::Unavailable)?;
    }
    validate_existing_private_state_root(path)
}

fn validate_existing_private_state_root(path: &Path) -> Result<(), RunLayoutError> {
    reject_symlink_or_non_directory(path)?;
    let physical = fs::canonicalize(path).map_err(|_| RunLayoutError::Unavailable)?;
    validate_state_root(&physical)?;
    verify_private_directory(path)
}

fn create_or_validate_private_directory(path: &Path) -> Result<(), RunLayoutError> {
    match create_private_directory(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            reject_symlink_or_non_directory(path)?;
            verify_private_directory(path)
        }
        Err(_) => Err(RunLayoutError::Unavailable),
    }
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn verify_private_directory(path: &Path) -> Result<(), RunLayoutError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = fs::symlink_metadata(path).map_err(|_| RunLayoutError::Unavailable)?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(RunLayoutError::Unavailable);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use xgeny_adapter_filesystem::WorkspaceId;

    use crate::manifest::{ManifestBudget, RunManifest};

    use super::*;

    const DIGEST_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST_C: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const DIGEST_D: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn manifest(run_id: &str) -> RunManifest {
        RunManifest::new(
            run_id,
            &WorkspaceId::new("fixture").unwrap(),
            "xgeny.workspace-root.unix-file-id.v1",
            DIGEST_A,
            "xgeny.cli.openai",
            "model",
            "tokenizer",
            DIGEST_B,
            DIGEST_C,
            DIGEST_D,
            ManifestBudget::default(),
        )
        .unwrap()
    }

    #[test]
    fn create_is_exclusive_and_existing_never_creates() {
        let state = tempdir().unwrap();
        let root = state.path().join("state");
        let run_id = "run-0123456789abcdef0123456789abcdef";
        assert_eq!(
            RunLayout::existing(&root, run_id).unwrap_err(),
            RunLayoutError::Unavailable
        );
        let layout = RunLayout::create(&root, run_id).unwrap();
        layout.write_manifest(&manifest(run_id)).unwrap();
        assert_eq!(layout.read_manifest().unwrap(), manifest(run_id));
        assert_eq!(
            RunLayout::create(&root, run_id).unwrap_err(),
            RunLayoutError::AlreadyExists
        );
        assert!(layout.database_path().ends_with(DATABASE_FILE));
        assert!(layout.lease_path().ends_with(LEASE_FILE));
        assert!(
            layout
                .material_catalog_path()
                .ends_with(MATERIAL_CATALOG_FILE)
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_permissive_state_root_is_rejected_without_chmod() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = tempdir().unwrap();
        let root = parent.path().join("permissive-state");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let run_id = "run-11111111111111111111111111111111";

        assert_eq!(
            RunLayout::create(&root, run_id).unwrap_err(),
            RunLayoutError::Unavailable
        );
        assert_eq!(
            fs::symlink_metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!root.join(RUNS_DIRECTORY).exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_root_is_rejected_without_changing_target_permissions() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let parent = tempdir().unwrap();
        let target = parent.path().join("target");
        let root = parent.path().join("state-link");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, &root).unwrap();

        assert_eq!(
            RunLayout::create(&root, "run-22222222222222222222222222222222").unwrap_err(),
            RunLayoutError::Unavailable
        );
        assert_eq!(
            fs::symlink_metadata(&target).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_symlink_is_canonicalized_before_creating_the_private_suffix() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let parent = tempdir().unwrap();
        let physical_parent = parent.path().join("physical-parent");
        let alias = parent.path().join("platform-alias");
        fs::create_dir(&physical_parent).unwrap();
        symlink(&physical_parent, &alias).unwrap();
        let root = alias.join("nested/state");
        let run_id = "run-33333333333333333333333333333333";

        let layout = RunLayout::create(&root, run_id).unwrap();
        assert!(
            physical_parent
                .join("nested/state/runs")
                .join(run_id)
                .is_dir()
        );
        assert_eq!(
            fs::symlink_metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(RunLayout::existing(&root, run_id).unwrap().run_id, run_id);
        drop(layout);
    }

    #[cfg(unix)]
    #[test]
    fn environment_base_cannot_be_reached_through_an_ancestor_alias() {
        use std::os::unix::fs::symlink;

        let home = env::var_os("HOME").map(PathBuf::from).unwrap();
        let home_parent = home.parent().unwrap();
        let home_name = home.file_name().unwrap();
        let parent = tempdir().unwrap();
        let alias = parent.path().join("home-parent-alias");
        symlink(home_parent, &alias).unwrap();
        let aliased_home = alias.join(home_name);

        assert!(is_environment_base_directory(&aliased_home));
        assert_eq!(
            ensure_private_state_root(&aliased_home),
            Err(RunLayoutError::Unavailable)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_state_root_accepts_drive_letter_namespaces_and_rejects_unc_namespaces() {
        assert!(validate_state_root(Path::new(r"C:\Users\xgeny\state")).is_ok());
        assert_eq!(
            validate_state_root(Path::new(r"\\server\share\xgeny\state")),
            Err(RunLayoutError::Unavailable)
        );
        assert_eq!(
            validate_state_root(Path::new(r"\\?\UNC\server\share\xgeny\state")),
            Err(RunLayoutError::Unavailable)
        );
    }

    #[test]
    fn broad_or_ambiguous_state_roots_are_rejected() {
        assert_eq!(
            validate_state_root(Path::new("/")),
            Err(RunLayoutError::Unavailable)
        );
        assert_eq!(
            validate_state_root(Path::new("/tmp")),
            Err(RunLayoutError::Unavailable)
        );
        assert_eq!(
            validate_state_root(Path::new("/tmp/xgeny/../escape")),
            Err(RunLayoutError::Unavailable)
        );
    }

    #[test]
    fn debug_and_errors_do_not_expose_state_paths() {
        let state = tempdir().unwrap();
        let root = state.path().join("SENSITIVE-STATE-PATH");
        let run_id = "run-fedcba9876543210fedcba9876543210";
        let layout = RunLayout::create(&root, run_id).unwrap();
        assert!(!format!("{layout:?}").contains("SENSITIVE-STATE-PATH"));
        assert!(!format!("{:?}", RunLayoutError::Unavailable).contains("SENSITIVE"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_run_directory_is_rejected() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let state = tempdir().unwrap();
        let root = state.path().join("state");
        let target = state.path().join("target");
        fs::create_dir_all(root.join(RUNS_DIRECTORY)).unwrap();
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join(RUNS_DIRECTORY), fs::Permissions::from_mode(0o700)).unwrap();
        let run_id = "run-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        symlink(&target, root.join(RUNS_DIRECTORY).join(run_id)).unwrap();
        assert_eq!(
            RunLayout::existing(&root, run_id).unwrap_err(),
            RunLayoutError::Unavailable
        );
    }

    #[test]
    fn generated_ids_follow_the_strict_grammar() {
        let first = generate_run_id().unwrap();
        let second = generate_run_id().unwrap();
        assert!(valid_run_id(&first));
        assert!(valid_run_id(&second));
        assert_ne!(first, second);
    }
}
