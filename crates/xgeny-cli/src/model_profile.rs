use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use cap_std::fs::Dir;
use getrandom::fill;
use keyring::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_provider_openai::OpenAiPlannerConfig;
use zeroize::Zeroizing;

const PROFILE_FILE: &str = "model-profiles.json";
const PROFILE_LOCK_FILE: &str = "model-profiles.lock";
const PROFILE_FORMAT_VERSION: u32 = 1;
const MAX_PROFILE_FILE_BYTES: u64 = 256 * 1024;
const MAX_PROFILES: usize = 64;
const MAX_PROFILE_NAME_BYTES: usize = 64;
const CREDENTIAL_SERVICE: &str = "com.plateer.xgeny.model";
const PROFILE_VALIDATION_PLANNER_ID: &str = "xgeny.cli.openai";
const TEMP_CREATE_ATTEMPTS: usize = 8;

/// One non-secret OpenAI-compatible model profile.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelProfile {
    name: String,
    base_url: String,
    model: String,
    tokenizer: String,
    credential_ref: Option<String>,
}

impl ModelProfile {
    /// Create and validate one non-secret profile.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid profile name, provider URL, model, or tokenizer.
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        tokenizer: impl Into<String>,
    ) -> Result<Self, ModelProfileError> {
        let profile = Self {
            name: name.into(),
            base_url: base_url.into(),
            model: model.into(),
            tokenizer: tokenizer.into(),
            credential_ref: None,
        };
        profile.validate()?;
        Ok(profile)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn tokenizer(&self) -> &str {
        &self.tokenizer
    }

    #[must_use]
    pub fn credential_reference(&self) -> Option<&str> {
        self.credential_ref.as_deref()
    }

    #[must_use]
    pub const fn has_stored_credential(&self) -> bool {
        self.credential_ref.is_some()
    }

    /// Replace the non-secret reference to a platform credential entry.
    ///
    /// # Errors
    ///
    /// Returns an error when the reference does not use the generated opaque grammar.
    pub fn set_credential_reference(
        &mut self,
        reference: Option<String>,
    ) -> Result<(), ModelProfileError> {
        if reference
            .as_deref()
            .is_some_and(|value| !valid_credential_ref(value))
        {
            return Err(ModelProfileError::InvalidProfile);
        }
        self.credential_ref = reference;
        Ok(())
    }

    fn validate(&self) -> Result<(), ModelProfileError> {
        if !valid_profile_name(&self.name)
            || self
                .credential_ref
                .as_deref()
                .is_some_and(|value| !valid_credential_ref(value))
        {
            return Err(ModelProfileError::InvalidProfile);
        }
        OpenAiPlannerConfig::new(
            &self.base_url,
            PROFILE_VALIDATION_PLANNER_ID,
            &self.model,
            &self.tokenizer,
        )
        .map(|_| ())
        .map_err(|_| ModelProfileError::InvalidProfile)
    }
}

impl std::fmt::Debug for ModelProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelProfile")
            .field("name", &self.name)
            .field("base_url", &"<redacted>")
            .field("model", &self.model)
            .field("tokenizer", &self.tokenizer)
            .field(
                "credential_ref",
                &self.credential_ref.as_ref().map(|_| "<present>"),
            )
            .finish()
    }
}

/// Optimistically versioned in-memory profile collection.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelProfiles {
    active: Option<String>,
    profiles: Vec<ModelProfile>,
    source_digest: Option<String>,
}

impl ModelProfiles {
    fn empty() -> Self {
        Self {
            active: None,
            profiles: Vec::new(),
            source_digest: None,
        }
    }

    #[must_use]
    pub fn active_name(&self) -> Option<&str> {
        self.active.as_deref()
    }

    #[must_use]
    pub fn active(&self) -> Option<&ModelProfile> {
        self.active.as_deref().and_then(|name| self.get(name))
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ModelProfile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModelProfile> {
        self.profiles.iter()
    }

    /// Insert or replace one validated profile.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid profile or when the bounded profile limit is reached.
    pub fn upsert(
        &mut self,
        profile: ModelProfile,
    ) -> Result<Option<ModelProfile>, ModelProfileError> {
        profile.validate()?;
        let previous = self
            .profiles
            .iter()
            .position(|candidate| candidate.name == profile.name)
            .map(|index| std::mem::replace(&mut self.profiles[index], profile.clone()));
        if previous.is_none() {
            if self.profiles.len() >= MAX_PROFILES {
                return Err(ModelProfileError::TooManyProfiles);
            }
            self.profiles.push(profile);
        }
        self.profiles
            .sort_by(|left, right| left.name.cmp(&right.name));
        Ok(previous)
    }

    /// Select an existing profile.
    ///
    /// # Errors
    ///
    /// Returns an error when the named profile does not exist.
    pub fn set_active(&mut self, name: &str) -> Result<(), ModelProfileError> {
        if self.get(name).is_none() {
            return Err(ModelProfileError::ProfileNotFound);
        }
        self.active = Some(name.to_owned());
        Ok(())
    }

    /// Remove and return a profile's non-secret secure-store reference.
    ///
    /// # Errors
    ///
    /// Returns an error when the named profile does not exist.
    pub fn clear_credential(&mut self, name: &str) -> Result<Option<String>, ModelProfileError> {
        let profile = self
            .profiles
            .iter_mut()
            .find(|profile| profile.name == name)
            .ok_or(ModelProfileError::ProfileNotFound)?;
        Ok(profile.credential_ref.take())
    }

    /// Remove one profile and deterministically select a replacement active profile.
    ///
    /// # Errors
    ///
    /// Returns an error when the named profile does not exist.
    pub fn remove(&mut self, name: &str) -> Result<ModelProfile, ModelProfileError> {
        let index = self
            .profiles
            .iter()
            .position(|profile| profile.name == name)
            .ok_or(ModelProfileError::ProfileNotFound)?;
        let removed = self.profiles.remove(index);
        if self.active.as_deref() == Some(name) {
            self.active = self.profiles.first().map(|profile| profile.name.clone());
        }
        Ok(removed)
    }

    fn validate(&self) -> Result<(), ModelProfileError> {
        if self.profiles.len() > MAX_PROFILES
            || self
                .active
                .as_deref()
                .is_some_and(|active| self.get(active).is_none())
        {
            return Err(ModelProfileError::InvalidProfileFile);
        }
        let mut names = BTreeSet::new();
        for profile in &self.profiles {
            profile
                .validate()
                .map_err(|_| ModelProfileError::InvalidProfileFile)?;
            if !names.insert(profile.name.as_str()) {
                return Err(ModelProfileError::InvalidProfileFile);
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for ModelProfiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelProfiles")
            .field("active", &self.active)
            .field("profile_count", &self.profiles.len())
            .field("source_digest", &self.source_digest)
            .finish()
    }
}

/// Private local file store for non-secret model profiles.
pub struct ModelProfileStore {
    root: PathBuf,
}

impl ModelProfileStore {
    /// Discover the platform-native `XGENy` configuration root.
    ///
    /// # Errors
    ///
    /// Returns an error when no safe absolute application configuration path is available.
    pub fn discover() -> Result<Self, ModelProfileError> {
        discover_config_root().map(|root| Self { root })
    }

    /// Construct an explicit profile store, primarily for isolated hosts and tests.
    ///
    /// # Errors
    ///
    /// Returns an error when `root` is not a safe absolute application-owned path.
    pub fn at(root: PathBuf) -> Result<Self, ModelProfileError> {
        validate_config_root(&root)?;
        Ok(Self { root })
    }

    /// Load profiles without creating the configuration directory when none exist.
    ///
    /// # Errors
    ///
    /// Returns a redacted error for unsafe paths, permissions, oversized data, or invalid JSON.
    pub fn load(&self) -> Result<ModelProfiles, ModelProfileError> {
        validate_config_root(&self.root)?;
        match fs::symlink_metadata(&self.root) {
            Ok(_) => validate_existing_private_root(&self.root)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(ModelProfiles::empty());
            }
            Err(_) => return Err(ModelProfileError::ProfileStoreUnavailable),
        }
        let path = self.root.join(PROFILE_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(ModelProfiles::empty()),
            Err(_) => return Err(ModelProfileError::ProfileStoreUnavailable),
        };
        if metadata.file_type().is_symlink()
            || is_windows_reparse_point(&metadata)
            || !metadata.is_file()
        {
            return Err(ModelProfileError::ProfileStoreUnavailable);
        }
        verify_private_file(&path)?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(usize::MAX)
                .min(usize::try_from(MAX_PROFILE_FILE_BYTES).unwrap_or(usize::MAX)),
        );
        File::open(&path)
            .map_err(|_| ModelProfileError::ProfileStoreUnavailable)?
            .take(MAX_PROFILE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROFILE_FILE_BYTES {
            return Err(ModelProfileError::InvalidProfileFile);
        }
        let document: StoredProfiles =
            serde_json::from_slice(&bytes).map_err(|_| ModelProfileError::InvalidProfileFile)?;
        let mut profiles = document.into_profiles()?;
        profiles.source_digest = Some(sha256(&bytes));
        Ok(profiles)
    }

    /// Acquire the cross-process mutation lock without waiting behind another writer.
    ///
    /// # Errors
    ///
    /// Returns `ConcurrentModification` when another process owns the lock, or a redacted store
    /// error when a safe private lock file cannot be opened.
    pub fn try_lock(&self) -> Result<ModelProfileLock, ModelProfileError> {
        ensure_private_root(&self.root)?;
        let path = self.root.join(PROFILE_LOCK_FILE);
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    || is_windows_reparse_point(&metadata)
                    || !metadata.is_file() =>
            {
                return Err(ModelProfileError::ProfileStoreUnavailable);
            }
            Ok(_) => verify_private_file(&path)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err(ModelProfileError::ProfileStoreUnavailable),
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
        if metadata.file_type().is_symlink()
            || is_windows_reparse_point(&metadata)
            || !metadata.is_file()
        {
            return Err(ModelProfileError::ProfileStoreUnavailable);
        }
        verify_private_file(&path)?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => ModelProfileError::ConcurrentModification,
            std::fs::TryLockError::Error(_) => ModelProfileError::ProfileStoreUnavailable,
        })?;
        Ok(ModelProfileLock { _file: file })
    }

    /// Atomically save a collection if its originally loaded file revision is still current.
    ///
    /// # Errors
    ///
    /// Returns `ConcurrentModification` instead of overwriting another process's update.
    pub fn save(&self, profiles: &mut ModelProfiles) -> Result<(), ModelProfileError> {
        profiles.validate()?;
        ensure_private_root(&self.root)?;
        let document = StoredProfiles::from_profiles(profiles);
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|_| ModelProfileError::InvalidProfileFile)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROFILE_FILE_BYTES {
            return Err(ModelProfileError::InvalidProfileFile);
        }
        let desired_digest = sha256(&bytes);
        let path = self.root.join(PROFILE_FILE);
        if current_file_digest(&path)? == Some(desired_digest.clone()) {
            profiles.source_digest = Some(desired_digest);
            return Ok(());
        }
        if current_file_digest(&path)? != profiles.source_digest {
            return Err(ModelProfileError::ConcurrentModification);
        }

        let (temporary_name, temporary_path, mut temporary) = create_temporary(&self.root)?;
        if temporary.write_all(&bytes).is_err() || temporary.sync_all().is_err() {
            drop(temporary);
            let _ = fs::remove_file(&temporary_path);
            return Err(ModelProfileError::ProfileStoreUnavailable);
        }
        drop(temporary);
        if current_file_digest(&path)? != profiles.source_digest {
            let _ = fs::remove_file(&temporary_path);
            return Err(ModelProfileError::ConcurrentModification);
        }
        let directory = Dir::open_ambient_dir(&self.root, cap_std::ambient_authority())
            .map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
        if directory
            .rename(&temporary_name, &directory, PROFILE_FILE)
            .is_err()
        {
            let _ = fs::remove_file(&temporary_path);
            return Err(ModelProfileError::ProfileCommitUnknown);
        }
        sync_directory(&self.root)?;
        if current_file_digest(&path)? != Some(desired_digest.clone()) {
            return Err(ModelProfileError::ProfileCommitUnknown);
        }
        profiles.source_digest = Some(desired_digest);
        Ok(())
    }
}

/// Held cross-process profile mutation lock; releasing the value releases the OS lock.
pub struct ModelProfileLock {
    _file: File,
}

impl std::fmt::Debug for ModelProfileLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelProfileLock")
            .field("file", &"<locked/redacted>")
            .finish()
    }
}

impl std::fmt::Debug for ModelProfileStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelProfileStore")
            .field("root", &"<redacted>")
            .finish()
    }
}

/// Minimal interface around a platform-native secure credential store.
pub trait ModelCredentialStore {
    /// Store one secret under an opaque reference.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when the platform secure store is unavailable.
    fn put(&self, reference: &str, secret: &str) -> Result<(), ModelProfileError>;
    /// Retrieve one secret under an opaque reference.
    ///
    /// # Errors
    ///
    /// Returns a redacted unavailable or not-found error.
    fn get(&self, reference: &str) -> Result<Zeroizing<String>, ModelProfileError>;
    /// Delete one secret, treating an already absent entry as success.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when the platform secure store is unavailable.
    fn delete(&self, reference: &str) -> Result<(), ModelProfileError>;
}

/// macOS Keychain, Windows Credential Manager, or freedesktop Secret Service backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsModelCredentialStore;

impl ModelCredentialStore for OsModelCredentialStore {
    fn put(&self, reference: &str, secret: &str) -> Result<(), ModelProfileError> {
        credential_entry(reference)?
            .set_password(secret)
            .map_err(|_| ModelProfileError::CredentialStoreUnavailable)
    }

    fn get(&self, reference: &str) -> Result<Zeroizing<String>, ModelProfileError> {
        credential_entry(reference)?
            .get_password()
            .map(Zeroizing::new)
            .map_err(|error| map_keyring_get_error(&error))
    }

    fn delete(&self, reference: &str) -> Result<(), ModelProfileError> {
        match credential_entry(reference)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(_) => Err(ModelProfileError::CredentialStoreUnavailable),
        }
    }
}

/// Generate one opaque, non-secret secure-store reference.
///
/// # Errors
///
/// Returns an error when the operating system random source is unavailable.
pub fn new_credential_reference() -> Result<String, ModelProfileError> {
    let mut random = [0_u8; 16];
    fill(&mut random).map_err(|_| ModelProfileError::CredentialStoreUnavailable)?;
    let mut encoded = String::with_capacity(37);
    encoded.push_str("cred-");
    for byte in random {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(encoded)
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ModelProfileError {
    #[error("model profile is invalid")]
    InvalidProfile,
    #[error("model profile file is invalid")]
    InvalidProfileFile,
    #[error("model profile was not found")]
    ProfileNotFound,
    #[error("model profile limit was reached")]
    TooManyProfiles,
    #[error("model profile store is unavailable")]
    ProfileStoreUnavailable,
    #[error("model profile changed concurrently")]
    ConcurrentModification,
    #[error("model profile commit outcome is unknown")]
    ProfileCommitUnknown,
    #[error("secure credential store is unavailable")]
    CredentialStoreUnavailable,
    #[error("stored credential was not found")]
    CredentialNotFound,
}

impl ModelProfileError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidProfile => "profile_invalid",
            Self::InvalidProfileFile => "profile_file_invalid",
            Self::ProfileNotFound => "profile_not_found",
            Self::TooManyProfiles => "profile_limit_reached",
            Self::ProfileStoreUnavailable => "profile_store_unavailable",
            Self::ConcurrentModification => "profile_changed_concurrently",
            Self::ProfileCommitUnknown => "profile_commit_unknown",
            Self::CredentialStoreUnavailable => "credential_store_unavailable",
            Self::CredentialNotFound => "credential_not_found",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredProfiles {
    format_version: u32,
    active_profile: Option<String>,
    profiles: Vec<StoredProfile>,
}

impl StoredProfiles {
    fn from_profiles(profiles: &ModelProfiles) -> Self {
        Self {
            format_version: PROFILE_FORMAT_VERSION,
            active_profile: profiles.active.clone(),
            profiles: profiles
                .profiles
                .iter()
                .map(StoredProfile::from_profile)
                .collect(),
        }
    }

    fn into_profiles(self) -> Result<ModelProfiles, ModelProfileError> {
        if self.format_version != PROFILE_FORMAT_VERSION {
            return Err(ModelProfileError::InvalidProfileFile);
        }
        let mut profiles = self
            .profiles
            .into_iter()
            .map(StoredProfile::into_profile)
            .collect::<Result<Vec<_>, _>>()?;
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        let collection = ModelProfiles {
            active: self.active_profile,
            profiles,
            source_digest: None,
        };
        collection.validate()?;
        Ok(collection)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredProfile {
    name: String,
    base_url: String,
    model: String,
    tokenizer: String,
    credential_ref: Option<String>,
}

impl StoredProfile {
    fn from_profile(profile: &ModelProfile) -> Self {
        Self {
            name: profile.name.clone(),
            base_url: profile.base_url.clone(),
            model: profile.model.clone(),
            tokenizer: profile.tokenizer.clone(),
            credential_ref: profile.credential_ref.clone(),
        }
    }

    fn into_profile(self) -> Result<ModelProfile, ModelProfileError> {
        let profile = ModelProfile {
            name: self.name,
            base_url: self.base_url,
            model: self.model,
            tokenizer: self.tokenizer,
            credential_ref: self.credential_ref,
        };
        profile
            .validate()
            .map(|()| profile)
            .map_err(|_| ModelProfileError::InvalidProfileFile)
    }
}

fn credential_entry(reference: &str) -> Result<Entry, ModelProfileError> {
    if !valid_credential_ref(reference) {
        return Err(ModelProfileError::InvalidProfile);
    }
    Entry::new(CREDENTIAL_SERVICE, reference)
        .map_err(|_| ModelProfileError::CredentialStoreUnavailable)
}

fn map_keyring_get_error(error: &KeyringError) -> ModelProfileError {
    match error {
        KeyringError::NoEntry => ModelProfileError::CredentialNotFound,
        _ => ModelProfileError::CredentialStoreUnavailable,
    }
}

fn valid_profile_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PROFILE_NAME_BYTES
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn valid_credential_ref(value: &str) -> bool {
    value.len() == 37
        && value.starts_with("cred-")
        && value[5..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn discover_config_root() -> Result<PathBuf, ModelProfileError> {
    if let Some(value) = env::var_os("XGENY_CONFIG_HOME") {
        let path = PathBuf::from(value);
        validate_config_root(&path)?;
        return Ok(path);
    }

    #[cfg(target_os = "windows")]
    let root = env::var_os("APPDATA")
        .or_else(|| env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .map(|path| path.join("XGENy"));
    #[cfg(target_os = "macos")]
    let root = env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join("Library/Application Support/XGENy"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .map(|path| path.join("xgeny"))
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".config/xgeny"))
        });
    #[cfg(not(any(unix, target_os = "windows")))]
    let root: Option<PathBuf> = None;

    let root = root.ok_or(ModelProfileError::ProfileStoreUnavailable)?;
    validate_config_root(&root)?;
    Ok(root)
}

fn validate_config_root(path: &Path) -> Result<(), ModelProfileError> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || path
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count()
            < 2
        || is_environment_base_directory(path)
        || !is_supported_root_namespace(path)
    {
        return Err(ModelProfileError::ProfileStoreUnavailable);
    }
    Ok(())
}

#[cfg(windows)]
fn is_supported_root_namespace(path: &Path) -> bool {
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
const fn is_supported_root_namespace(_path: &Path) -> bool {
    true
}

fn is_environment_base_directory(path: &Path) -> bool {
    let canonical_path = fs::canonicalize(path).ok();
    [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
    ]
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

fn ensure_private_root(path: &Path) -> Result<(), ModelProfileError> {
    match fs::symlink_metadata(path) {
        Ok(_) => return validate_existing_private_root(path),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => return Err(ModelProfileError::ProfileStoreUnavailable),
    }
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        let component = existing
            .file_name()
            .ok_or(ModelProfileError::ProfileStoreUnavailable)?;
        missing.push(component.to_os_string());
        existing = existing
            .parent()
            .ok_or(ModelProfileError::ProfileStoreUnavailable)?;
        match fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err(ModelProfileError::ProfileStoreUnavailable),
        }
    }
    if !fs::metadata(existing)
        .map_err(|_| ModelProfileError::ProfileStoreUnavailable)?
        .is_dir()
    {
        return Err(ModelProfileError::ProfileStoreUnavailable);
    }
    let mut physical =
        fs::canonicalize(existing).map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
    for component in missing.iter().rev() {
        physical.push(component);
        create_private_directory(&physical)?;
    }
    validate_existing_private_root(path)
}

fn validate_existing_private_root(path: &Path) -> Result<(), ModelProfileError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
    if metadata.file_type().is_symlink()
        || is_windows_reparse_point(&metadata)
        || !metadata.is_dir()
    {
        return Err(ModelProfileError::ProfileStoreUnavailable);
    }
    let physical =
        fs::canonicalize(path).map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
    validate_config_root(&physical)?;
    verify_private_directory(path)
}

fn create_private_directory(path: &Path) -> Result<(), ModelProfileError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|_| ModelProfileError::ProfileStoreUnavailable)
}

fn verify_private_directory(path: &Path) -> Result<(), ModelProfileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata =
            fs::symlink_metadata(path).map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ModelProfileError::ProfileStoreUnavailable);
        }
    }
    Ok(())
}

fn verify_private_file(path: &Path) -> Result<(), ModelProfileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata =
            fs::symlink_metadata(path).map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ModelProfileError::ProfileStoreUnavailable);
        }
    }
    Ok(())
}

fn current_file_digest(path: &Path) -> Result<Option<String>, ModelProfileError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ModelProfileError::ProfileStoreUnavailable),
    };
    if metadata.file_type().is_symlink()
        || is_windows_reparse_point(&metadata)
        || !metadata.is_file()
        || metadata.len() > MAX_PROFILE_FILE_BYTES
    {
        return Err(ModelProfileError::ProfileStoreUnavailable);
    }
    verify_private_file(path)?;
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| ModelProfileError::ProfileStoreUnavailable)?
        .take(MAX_PROFILE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROFILE_FILE_BYTES {
        return Err(ModelProfileError::ProfileStoreUnavailable);
    }
    Ok(Some(sha256(&bytes)))
}

fn create_temporary(root: &Path) -> Result<(String, PathBuf, File), ModelProfileError> {
    for _ in 0..TEMP_CREATE_ATTEMPTS {
        let mut random = [0_u8; 16];
        fill(&mut random).map_err(|_| ModelProfileError::ProfileStoreUnavailable)?;
        let mut name = String::with_capacity(53);
        name.push_str(".xgeny-model-profiles-");
        for byte in random {
            write!(&mut name, "{byte:02x}").expect("writing to String cannot fail");
        }
        name.push_str(".tmp");
        let path = root.join(&name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((name, path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(_) => return Err(ModelProfileError::ProfileStoreUnavailable),
        }
    }
    Err(ModelProfileError::ProfileStoreUnavailable)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ModelProfileError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ModelProfileError::ProfileCommitUnknown)
}

#[cfg(not(unix))]
const fn sync_directory(_path: &Path) -> Result<(), ModelProfileError> {
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;

    fn profile(name: &str) -> ModelProfile {
        ModelProfile::new(
            name,
            "https://provider.example/v1",
            "served-model",
            "tokenizer-id",
        )
        .unwrap()
    }

    #[test]
    fn profiles_round_trip_without_secret_material_and_with_optimistic_revision() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("config");
        let store = ModelProfileStore::at(root.clone()).unwrap();
        let mut profiles = store.load().unwrap();
        let mut configured = profile("default");
        configured
            .set_credential_reference(Some("cred-0123456789abcdef0123456789abcdef".to_owned()))
            .unwrap();
        profiles.upsert(configured).unwrap();
        profiles.set_active("default").unwrap();
        store.save(&mut profiles).unwrap();

        let bytes = fs::read(root.join(PROFILE_FILE)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("RAW-SECRET-SENTINEL"));
        let loaded = store.load().unwrap();
        assert_eq!(loaded.active().unwrap().model(), "served-model");
        assert!(loaded.active().unwrap().has_stored_credential());

        let mut stale = loaded.clone();
        let mut current = loaded;
        current.upsert(profile("second")).unwrap();
        store.save(&mut current).unwrap();
        stale.upsert(profile("third")).unwrap();
        assert_eq!(
            store.save(&mut stale),
            Err(ModelProfileError::ConcurrentModification)
        );
    }

    #[test]
    fn mutation_lock_is_cross_handle_exclusive_and_released_on_drop() {
        let directory = tempdir().unwrap();
        let store = ModelProfileStore::at(directory.path().join("config")).unwrap();
        let first = store.try_lock().unwrap();
        assert_eq!(
            store.try_lock().unwrap_err(),
            ModelProfileError::ConcurrentModification
        );
        drop(first);
        store.try_lock().unwrap();
    }

    #[test]
    fn invalid_names_duplicates_and_unknown_fields_fail_closed() {
        assert_eq!(
            ModelProfile::new("-bad", "https://provider.example/v1", "model", "tokenizer"),
            Err(ModelProfileError::InvalidProfile)
        );
        let directory = tempdir().unwrap();
        let root = directory.path().join("config");
        ensure_private_root(&root).unwrap();
        let invalid = br#"{
          "formatVersion": 1,
          "activeProfile": "same",
          "profiles": [
            {"name":"same","baseUrl":"https://provider.example/v1","model":"m","tokenizer":"t","credentialRef":null},
            {"name":"same","baseUrl":"https://provider.example/v1","model":"m","tokenizer":"t","credentialRef":null}
          ],
          "secret": "RAW-SECRET-SENTINEL"
        }"#;
        let path = root.join(PROFILE_FILE);
        fs::write(&path, invalid).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let store = ModelProfileStore::at(root).unwrap();
        assert_eq!(store.load(), Err(ModelProfileError::InvalidProfileFile));
    }

    #[cfg(unix)]
    #[test]
    fn permissive_or_symlinked_profile_storage_is_rejected_without_mutation() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempdir().unwrap();
        let permissive = directory.path().join("permissive");
        fs::create_dir(&permissive).unwrap();
        fs::set_permissions(&permissive, fs::Permissions::from_mode(0o755)).unwrap();
        let store = ModelProfileStore::at(permissive.clone()).unwrap();
        let mut profiles = ModelProfiles::empty();
        profiles.upsert(profile("default")).unwrap();
        assert_eq!(
            store.save(&mut profiles),
            Err(ModelProfileError::ProfileStoreUnavailable)
        );
        assert!(!permissive.join(PROFILE_FILE).exists());

        let real = directory.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let linked = directory.path().join("linked");
        symlink(&real, &linked).unwrap();
        let linked_store = ModelProfileStore::at(linked).unwrap();
        assert_eq!(
            linked_store.load(),
            Err(ModelProfileError::ProfileStoreUnavailable)
        );
    }

    #[derive(Default)]
    struct MemoryCredentials(RefCell<BTreeMap<String, String>>);

    impl ModelCredentialStore for MemoryCredentials {
        fn put(&self, reference: &str, secret: &str) -> Result<(), ModelProfileError> {
            self.0
                .borrow_mut()
                .insert(reference.to_owned(), secret.to_owned());
            Ok(())
        }

        fn get(&self, reference: &str) -> Result<Zeroizing<String>, ModelProfileError> {
            self.0
                .borrow()
                .get(reference)
                .cloned()
                .map(Zeroizing::new)
                .ok_or(ModelProfileError::CredentialNotFound)
        }

        fn delete(&self, reference: &str) -> Result<(), ModelProfileError> {
            self.0.borrow_mut().remove(reference);
            Ok(())
        }
    }

    #[test]
    fn credential_interface_keeps_secret_out_of_profile_debug_and_supports_delete() {
        let credentials = MemoryCredentials::default();
        let reference = new_credential_reference().unwrap();
        credentials.put(&reference, "RAW-SECRET-SENTINEL").unwrap();
        assert_eq!(
            credentials.get(&reference).unwrap().as_str(),
            "RAW-SECRET-SENTINEL"
        );
        let mut configured = profile("default");
        configured
            .set_credential_reference(Some(reference.clone()))
            .unwrap();
        assert!(!format!("{configured:?}").contains("RAW-SECRET-SENTINEL"));
        assert!(!format!("{configured:?}").contains(&reference));
        credentials.delete(&reference).unwrap();
        assert_eq!(
            credentials.get(&reference),
            Err(ModelProfileError::CredentialNotFound)
        );
    }
}
