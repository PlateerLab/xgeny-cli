use std::fmt::{self, Write as _};

use cap_std::fs::{Dir, Metadata};
use sha2::{Digest, Sha256};
use thiserror::Error;

const ROOT_IDENTITY_DOMAIN: &[u8] = b"xgeny.fs/workspace-root-identity/v1";

/// Opaque commitment to the physical directory represented by a preopened workspace handle.
///
/// The digest deliberately exposes neither the ambient path nor the operating-system identifiers
/// from which it was derived. It is an equality commitment for trusted host configuration, not a
/// secret or an authentication proof against a hostile local writer.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceRootIdentity(String);

impl WorkspaceRootIdentity {
    /// Return the domain-separated lowercase SHA-256 commitment.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkspaceRootIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkspaceRootIdentity(<redacted>)")
    }
}

/// Fixed failure returned when a physical identity cannot be obtained from the open handle.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("workspace physical identity is unavailable")]
pub struct WorkspaceRootIdentityError;

pub(crate) fn physical_identity(
    directory: &Dir,
) -> Result<WorkspaceRootIdentity, WorkspaceRootIdentityError> {
    // `dir_metadata` queries the retained directory handle. Do not replace this with metadata for
    // an ambient or reconstructed path: rename resistance and root-binding both depend on using
    // the exact capability handle that subsequent adapter operations use.
    let metadata = directory
        .dir_metadata()
        .map_err(|_| WorkspaceRootIdentityError)?;
    if !metadata.is_dir() {
        return Err(WorkspaceRootIdentityError);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Ok(platform_identity(&metadata))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(platform_identity(&metadata))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = metadata;
        Err(WorkspaceRootIdentityError)
    }
}

#[cfg(target_os = "linux")]
fn platform_identity(metadata: &Metadata) -> WorkspaceRootIdentity {
    use cap_fs_ext::MetadataExt as _;

    digest_identity(
        b"linux-dev-ino/v1",
        &metadata.dev().to_be_bytes(),
        &metadata.ino().to_be_bytes(),
    )
}

#[cfg(target_os = "macos")]
fn platform_identity(metadata: &Metadata) -> WorkspaceRootIdentity {
    use cap_fs_ext::MetadataExt as _;

    digest_identity(
        b"macos-dev-ino/v1",
        &metadata.dev().to_be_bytes(),
        &metadata.ino().to_be_bytes(),
    )
}

#[cfg(target_os = "windows")]
fn platform_identity(metadata: &Metadata) -> WorkspaceRootIdentity {
    use cap_fs_ext::MetadataExt as _;

    // cap-fs-ext obtains these values from the already-open directory handle even on stable Rust,
    // where std's by-handle MetadataExt methods are not exposed yet.
    digest_identity(
        b"windows-volume-file-index/v1",
        &metadata.dev().to_be_bytes(),
        &metadata.ino().to_be_bytes(),
    )
}

fn digest_identity(profile: &[u8], first: &[u8], second: &[u8]) -> WorkspaceRootIdentity {
    let mut hasher = Sha256::new();
    hasher.update(ROOT_IDENTITY_DOMAIN);
    hasher.update([0]);
    hasher.update(profile);
    hasher.update([0]);
    hasher.update(first);
    hasher.update(second);
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    WorkspaceRootIdentity(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_profile_and_field_boundaries_are_committed() {
        let base = digest_identity(b"profile-a", &[1, 2], &[3, 4]);
        assert_eq!(
            base.as_str(),
            "sha256:0b25fe9ee2075b2ca8632437feefba4467d85b9fa4df76036b38e1493477a2e0"
        );
        assert_ne!(base, digest_identity(b"profile-b", &[1, 2], &[3, 4]));
        assert_ne!(base, digest_identity(b"profile-a", &[1, 3], &[3, 4]));
        assert_ne!(base, digest_identity(b"profile-a", &[1, 2], &[3, 5]));
    }
}
