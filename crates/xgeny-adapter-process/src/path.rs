use std::fmt::Write as _;
use std::path::PathBuf;

use cap_fs_ext::DirExt as _;
use cap_std::fs::Dir;
use sha2::{Digest as _, Sha256};
use xgeny_runtime::AdapterPrepareFailure;

use crate::ProcessWorkspace;

const MAX_RELATIVE_PATH_BYTES: usize = 4_096;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_COMPONENTS: usize = 256;

pub(crate) fn resolve_cwd(
    workspace: &ProcessWorkspace,
    value: &str,
) -> Result<PathBuf, AdapterPrepareFailure> {
    let components = parse_relative(value)?;
    let mut directory = workspace
        .directory
        .try_clone()
        .map_err(|_| AdapterPrepareFailure::ResourceUnavailable)?;
    for component in &components {
        directory = directory
            .open_dir_nofollow(component)
            .map_err(|_| AdapterPrepareFailure::ResourceUnavailable)?;
        if is_windows_reparse_point(
            &directory
                .dir_metadata()
                .map_err(|_| AdapterPrepareFailure::ResourceUnavailable)?,
        ) {
            return Err(AdapterPrepareFailure::ResourceUnavailable);
        }
    }
    let ambient_candidate = components.iter().fold(
        workspace.ambient_root.as_ref().clone(),
        |path, component| path.join(component),
    );
    let canonical_candidate = std::fs::canonicalize(ambient_candidate)
        .map_err(|_| AdapterPrepareFailure::ResourceUnavailable)?;
    let reopened = Dir::open_ambient_dir(&canonical_candidate, cap_std::ambient_authority())
        .map_err(|_| AdapterPrepareFailure::ResourceUnavailable)?;
    let expected = physical_directory_digest(&directory)
        .map_err(|()| AdapterPrepareFailure::ResourceUnavailable)?;
    let observed = physical_directory_digest(&reopened)
        .map_err(|()| AdapterPrepareFailure::ResourceUnavailable)?;
    if expected != observed {
        return Err(AdapterPrepareFailure::ResourceUnavailable);
    }
    Ok(canonical_candidate)
}

fn parse_relative(value: &str) -> Result<Vec<String>, AdapterPrepareFailure> {
    if value == "." {
        return Ok(Vec::new());
    }
    if value.is_empty() || value.len() > MAX_RELATIVE_PATH_BYTES || value.starts_with(['/', '\\']) {
        return Err(AdapterPrepareFailure::InvalidMaterial);
    }
    let mut components = Vec::new();
    for component in value.split('/') {
        if components.len() == MAX_COMPONENTS {
            return Err(AdapterPrepareFailure::InvalidMaterial);
        }
        validate_component(component)?;
        components.push(component.to_owned());
    }
    Ok(components)
}

fn validate_component(component: &str) -> Result<(), AdapterPrepareFailure> {
    if component.is_empty()
        || component.len() > MAX_COMPONENT_BYTES
        || matches!(component, "." | "..")
        || component.starts_with(' ')
        || component.ends_with([' ', '.'])
        || component.chars().any(|character| {
            character.is_control()
                || matches!(character, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
        || is_windows_device_name(component)
    {
        return Err(AdapterPrepareFailure::InvalidMaterial);
    }
    Ok(())
}

fn is_windows_device_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ');
    let upper = stem.to_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || numbered_device(&upper, "COM")
        || numbered_device(&upper, "LPT")
}

fn numbered_device(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        matches!(
            suffix,
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "⁰" | "¹" | "²" | "³"
        )
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

pub(crate) fn physical_directory_digest(directory: &Dir) -> Result<String, ()> {
    let metadata = directory.dir_metadata().map_err(|_| ())?;
    if !metadata.is_dir() {
        return Err(());
    }
    let mut hasher = Sha256::new();
    hasher.update(b"xgeny.process/workspace-root-identity/v1\0");
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    update_physical_identity(&mut hasher, &metadata);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    return Err(());
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(format!("sha256:{encoded}"))
}

#[cfg(target_os = "linux")]
fn update_physical_identity(hasher: &mut Sha256, metadata: &cap_std::fs::Metadata) {
    use cap_fs_ext::MetadataExt as _;

    hasher.update(b"linux-dev-ino/v1\0");
    hasher.update(metadata.dev().to_be_bytes());
    hasher.update(metadata.ino().to_be_bytes());
}

#[cfg(target_os = "macos")]
fn update_physical_identity(hasher: &mut Sha256, metadata: &cap_std::fs::Metadata) {
    use cap_fs_ext::MetadataExt as _;

    hasher.update(b"macos-dev-ino/v1\0");
    hasher.update(metadata.dev().to_be_bytes());
    hasher.update(metadata.ino().to_be_bytes());
}

#[cfg(target_os = "windows")]
fn update_physical_identity(hasher: &mut Sha256, metadata: &cap_std::fs::Metadata) {
    use cap_fs_ext::MetadataExt as _;

    hasher.update(b"windows-volume-file-index/v1\0");
    hasher.update(metadata.dev().to_be_bytes());
    hasher.update(metadata.ino().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_grammar_is_portable_and_root_relative() {
        assert_eq!(parse_relative(".").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_relative("crates/runtime").unwrap(),
            ["crates", "runtime"]
        );
        for invalid in [
            "", "..", "a/../b", "/tmp", r"C:\\tmp", r"..\\tmp", "a//b", "CON", "trail.",
        ] {
            assert!(parse_relative(invalid).is_err(), "{invalid:?} must fail");
        }
    }

    #[cfg(unix)]
    #[test]
    fn cwd_never_follows_a_workspace_symlink() {
        use std::os::unix::fs::symlink;

        use tempfile::tempdir;

        use crate::{ExecutableCatalog, ProcessEnvironment, ProcessWorkspace, ProcessWorkspaceId};

        let workspace_directory = tempdir().unwrap();
        let outside_directory = tempdir().unwrap();
        symlink(
            outside_directory.path(),
            workspace_directory.path().join("outside-link"),
        )
        .unwrap();
        let catalog =
            ExecutableCatalog::from_paths([("test", std::env::current_exe().unwrap())]).unwrap();
        let workspace = ProcessWorkspace::open_ambient(
            workspace_directory.path(),
            ProcessWorkspaceId::new("fixture").unwrap(),
            catalog,
            ProcessEnvironment::empty(),
        )
        .unwrap();

        assert!(resolve_cwd(&workspace, "outside-link").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cwd_rejects_an_ambient_root_retargeted_after_preopen() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let moved = parent.path().join("moved-workspace");
        std::fs::create_dir(&root).unwrap();
        let catalog =
            crate::ExecutableCatalog::from_paths([("test", std::env::current_exe().unwrap())])
                .unwrap();
        let workspace = crate::ProcessWorkspace::open_ambient(
            &root,
            crate::ProcessWorkspaceId::new("fixture").unwrap(),
            catalog,
            crate::ProcessEnvironment::empty(),
        )
        .unwrap();
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();

        assert!(resolve_cwd(&workspace, ".").is_err());
    }
}
