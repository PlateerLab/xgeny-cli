//! Compatibility for renamed public configuration, without migrating or duplicating stored data.

use std::ffi::OsString;
use std::path::PathBuf;

/// Read the current variable first, falling back to its legacy spelling only when absent.
/// Values remain in process memory and must never be logged.
#[doc(hidden)]
#[must_use]
pub fn compatible_environment(name: &str) -> Option<OsString> {
    resolve_environment(name, |name| std::env::var_os(name))
}

fn resolve_environment(name: &str, lookup: impl Fn(&str) -> Option<OsString>) -> Option<OsString> {
    lookup(name).or_else(|| {
        name.strip_prefix("XGEN_")
            .and_then(|suffix| lookup(&format!("XGENY_{suffix}")))
    })
}

/// Prefer the current root; reuse an existing legacy root without moving live SQLite or credentials.
pub(crate) fn compatible_root(current: PathBuf, legacy: PathBuf) -> PathBuf {
    if current
        .symlink_metadata()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        && legacy.symlink_metadata().is_ok()
    {
        legacy
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_environment_precedence_and_empty_values_are_explicit() {
        for suffix in ["CONFIG_HOME", "OPENAI_API_KEY"] {
            let current = format!("XGEN_{suffix}");
            let legacy = format!("XGENY_{suffix}");
            assert_eq!(
                resolve_environment(&current, |name| {
                    (name == legacy).then(|| OsString::from("legacy-fixture"))
                }),
                Some(OsString::from("legacy-fixture"))
            );
            assert_eq!(
                resolve_environment(&current, |name| {
                    if name == current {
                        Some(OsString::new())
                    } else if name == legacy {
                        Some(OsString::from("legacy-fixture"))
                    } else {
                        None
                    }
                }),
                Some(OsString::new())
            );
        }
        assert!(resolve_environment("HOME", |_| None).is_none());
    }

    #[test]
    fn existing_roots_are_reused_without_moving_or_merging_them() {
        let fixture = tempfile::tempdir().unwrap();
        for directory in ["config", "state"] {
            let current = fixture.path().join(format!("current-{directory}"));
            let legacy = fixture.path().join(format!("legacy-{directory}"));
            assert_eq!(compatible_root(current.clone(), legacy.clone()), current);
            std::fs::create_dir(&legacy).unwrap();
            assert_eq!(compatible_root(current.clone(), legacy.clone()), legacy);
            std::fs::create_dir(&current).unwrap();
            assert_eq!(compatible_root(current.clone(), legacy.clone()), current);
            assert!(legacy.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn broken_current_symlink_is_not_hidden_by_legacy_fallback() {
        let fixture = tempfile::tempdir().unwrap();
        let current = fixture.path().join("current");
        let legacy = fixture.path().join("legacy");
        std::fs::create_dir(&legacy).unwrap();
        std::os::unix::fs::symlink(fixture.path().join("missing"), &current).unwrap();
        assert_eq!(compatible_root(current.clone(), legacy), current);
    }
}
