use xgeny_policy::ResourceResolutionFailure;

use crate::WorkspaceId;

const CANONICAL_PREFIX: &str = "workspace:";
const MAX_RELATIVE_PATH_BYTES: usize = 4_096;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_COMPONENTS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathError {
    Invalid,
    OutsideBoundary,
}

impl PathError {
    pub(crate) const fn resolution(self) -> ResourceResolutionFailure {
        match self {
            Self::Invalid => ResourceResolutionFailure::InvalidResource,
            Self::OutsideBoundary => ResourceResolutionFailure::OutsideHostBoundary,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RelativePath {
    components: Vec<String>,
}

impl RelativePath {
    pub(crate) fn components(&self) -> &[String] {
        &self.components
    }
}

impl std::fmt::Debug for RelativePath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RelativePath(<redacted>)")
    }
}

pub(crate) fn canonical_resource(
    workspace_id: &WorkspaceId,
    resource: &str,
) -> Result<String, PathError> {
    let prefix = canonical_prefix(workspace_id);
    let relative = if let Some(relative) = resource.strip_prefix(&prefix) {
        relative
    } else if resource.starts_with(CANONICAL_PREFIX) {
        return Err(PathError::OutsideBoundary);
    } else {
        resource
    };
    validate_relative(relative)?;
    Ok(format!("{prefix}{relative}"))
}

pub(crate) fn parse_canonical(
    workspace_id: &WorkspaceId,
    resource: &str,
) -> Result<RelativePath, PathError> {
    let prefix = canonical_prefix(workspace_id);
    let relative = resource
        .strip_prefix(&prefix)
        .ok_or(PathError::OutsideBoundary)?;
    let components = validate_relative(relative)?;
    Ok(RelativePath { components })
}

fn canonical_prefix(workspace_id: &WorkspaceId) -> String {
    format!("{CANONICAL_PREFIX}{}/", workspace_id.as_str())
}

fn validate_relative(relative: &str) -> Result<Vec<String>, PathError> {
    if relative.is_empty() || relative.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(PathError::Invalid);
    }
    if relative.starts_with('/') || relative.starts_with('\\') {
        return Err(PathError::OutsideBoundary);
    }
    let mut components = Vec::new();
    for component in relative.split('/') {
        if components.len() == MAX_COMPONENTS {
            return Err(PathError::Invalid);
        }
        validate_component(component)?;
        components.push(component.to_owned());
    }
    Ok(components)
}

fn validate_component(component: &str) -> Result<(), PathError> {
    if component.is_empty() || component.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::Invalid);
    }
    if component == ".." {
        return Err(PathError::OutsideBoundary);
    }
    if component == "." {
        return Err(PathError::Invalid);
    }
    if component.starts_with(' ') || component.ends_with([' ', '.']) {
        return Err(PathError::Invalid);
    }
    if component.chars().any(|character| {
        character.is_control()
            || matches!(character, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
    }) {
        return Err(PathError::OutsideBoundary);
    }
    if is_windows_device_name(component) {
        return Err(PathError::Invalid);
    }
    Ok(())
}

fn is_windows_device_name(component: &str) -> bool {
    // Win32 treats spaces immediately before an extension separator as aliases of the reserved
    // stem too (for example `CON .txt`). Keep the portable grammar stricter on every host.
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ');
    let upper = stem.to_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || device_numbered(&upper, "COM")
        || device_numbered(&upper, "LPT")
}

fn device_numbered(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        matches!(
            suffix,
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "⁰" | "¹" | "²" | "³"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> WorkspaceId {
        WorkspaceId::new("primary").expect("fixture workspace ID should be valid")
    }

    #[test]
    fn canonicalization_is_idempotent_and_byte_exact() {
        let raw = "문서/Read Me.md";
        let canonical = canonical_resource(&workspace(), raw).expect("path should resolve");
        assert_eq!(canonical, "workspace:primary/문서/Read Me.md");
        assert_eq!(
            canonical_resource(&workspace(), &canonical).expect("canonical path should resolve"),
            canonical
        );
        assert_eq!(
            parse_canonical(&workspace(), &canonical)
                .expect("canonical path should parse")
                .components(),
            ["문서", "Read Me.md"]
        );
    }

    #[test]
    fn cross_platform_escape_and_alias_forms_are_rejected() {
        let candidates = [
            "",
            ".",
            "..",
            "a/../b",
            "/etc/passwd",
            "a//b",
            "a/",
            "./a",
            r"..\secret",
            r"C:\secret",
            r"C:secret",
            r"\\server\share",
            "file:stream",
            "a\0b",
            "a\nb",
            "<bad>",
            "trail.",
            "trail ",
            " leading",
            "CON",
            "con.txt",
            "CON .txt",
            "PRN.log",
            "AUX",
            "NUL.md",
            "NUL .log",
            "COM1",
            "COM1 .txt",
            "COM0",
            "COM⁰.txt",
            "com9.txt",
            "LPT1",
            "LPT1 .x",
            "LPT0",
            "LPT³.txt",
            "CLOCK$",
            "CONIN$",
            "CONOUT$.txt",
        ];
        for candidate in candidates {
            assert!(
                canonical_resource(&workspace(), candidate).is_err(),
                "candidate should fail: {candidate:?}"
            );
        }
        assert!(canonical_resource(&workspace(), "workspace:other/file.txt").is_err());
    }

    #[test]
    fn case_and_unicode_normalization_are_not_silently_changed() {
        let composed = canonical_resource(&workspace(), "Café.txt").expect("path should resolve");
        let decomposed =
            canonical_resource(&workspace(), "Cafe\u{301}.txt").expect("path should resolve");
        let differently_cased =
            canonical_resource(&workspace(), "CAFÉ.txt").expect("path should resolve");
        assert_ne!(composed, decomposed);
        assert_ne!(composed, differently_cased);
    }
}
