//! Fixed, value-free diagnostics for rejected untrusted invocation arguments.
use jsonschema::{Draft, error::ValidationErrorKind};
use serde_json::Value;

use crate::AdmissionError;

/// Optional terminal metadata, not durable evidence or recovery authority.
/// Fields are private so only this allowlist projection can construct a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationDiagnostic {
    category: &'static str,
    field: &'static str,
}

impl InvocationDiagnostic {
    #[must_use]
    pub const fn category(self) -> &'static str {
        self.category
    }

    #[must_use]
    pub const fn field(self) -> &'static str {
        self.field
    }

    pub(crate) fn from_admission(
        error: &AdmissionError,
        schema: &Value,
        arguments: &Value,
    ) -> Option<Self> {
        match error {
            AdmissionError::ArgumentsDoNotConform => schema_diagnostic(schema, arguments),
            AdmissionError::Resolution(_) => Some(Self {
                category: "resource_resolution",
                field: "other",
            }),
            _ => None,
        }
    }
}

fn known_field(name: &str) -> &'static str {
    match name {
        "path" => "path",
        "content" => "content",
        "expectedDigest" => "expectedDigest",
        _ => "other",
    }
}

fn schema_diagnostic(schema: &Value, arguments: &Value) -> Option<InvocationDiagnostic> {
    // Same offline validation policy as admission. Never format the error itself.
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .offline()
        .should_validate_formats(true)
        .build(schema)
        .ok()?;
    let error = validator.iter_errors(arguments).next()?;
    let path = error.instance_path().as_str();
    let field = match path {
        "/path" => "path",
        "/content" => "content",
        "/expectedDigest" => "expectedDigest",
        _ => "other",
    };
    let (category, field) = match error.kind() {
        ValidationErrorKind::FalseSchema
            if error
                .schema_path()
                .as_str()
                .ends_with("/additionalProperties") =>
        {
            ("schema_additional_property", "other")
        }
        ValidationErrorKind::Required { property } => (
            "schema_required",
            if path.is_empty() {
                known_field(property.as_str().unwrap_or(""))
            } else {
                "other"
            },
        ),
        ValidationErrorKind::Type { .. } => ("schema_type", field),
        ValidationErrorKind::AdditionalProperties { .. } => ("schema_additional_property", "other"),
        ValidationErrorKind::MinLength { .. } => ("schema_min_length", field),
        ValidationErrorKind::Pattern { .. } => ("schema_pattern", field),
        ValidationErrorKind::OneOfNotValid { .. }
        | ValidationErrorKind::OneOfMultipleValid { .. } => ("schema_one_of", field),
        _ => ("schema_other", field),
    };
    Some(InvocationDiagnostic { category, field })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_top_level_fields_have_fixed_categories() {
        let cases = [
            (
                json!({"required":["expectedDigest"]}),
                json!({}),
                "schema_required",
                "expectedDigest",
            ),
            (
                json!({"properties":{"content":{"type":"string"}}}),
                json!({"content":{"SECRET":"PRIVATE"}}),
                "schema_type",
                "content",
            ),
            (
                json!({"additionalProperties":false}),
                json!({"SECRET":"PRIVATE"}),
                "schema_additional_property",
                "other",
            ),
            (
                json!({"properties":{"path":{"minLength":1}}}),
                json!({"path":""}),
                "schema_min_length",
                "path",
            ),
            (
                json!({"properties":{"expectedDigest":{"pattern":"^sha256:"}}}),
                json!({"expectedDigest":"SECRET"}),
                "schema_pattern",
                "expectedDigest",
            ),
            (
                json!({"properties":{"expectedDigest":{"oneOf":[{"type":"null"},{"pattern":"^sha256:","type":"string"}]}}}),
                json!({"expectedDigest":"SECRET"}),
                "schema_one_of",
                "expectedDigest",
            ),
        ];
        for (schema, args, category, field) in cases {
            let diagnostic = schema_diagnostic(&schema, &args).unwrap();
            assert_eq!(diagnostic.category(), category);
            assert_eq!(diagnostic.field(), field);
            assert!(!format!("{diagnostic:?}").contains("SECRET"));
            assert!(!format!("{diagnostic:?}").contains("PRIVATE"));
        }
    }

    #[test]
    fn nested_and_dynamic_names_never_escape() {
        for (schema, args) in [
            (json!({"required":["SECRET"]}), json!({})),
            (
                json!({"properties":{"SECRET":{"required":["path"]}}}),
                json!({"SECRET":{}}),
            ),
            (
                json!({"properties":{"SECRET":{"type":"string"}}}),
                json!({"SECRET":123}),
            ),
        ] {
            let diagnostic = schema_diagnostic(&schema, &args).unwrap();
            assert_eq!(diagnostic.field(), "other");
            assert!(!format!("{diagnostic:?}").contains("SECRET"));
        }
        assert_eq!(
            schema_diagnostic(&json!({"type":"object"}), &json!({})),
            None
        );
        assert_eq!(
            schema_diagnostic(&json!({"type":"not-a-type"}), &json!({})),
            None
        );
    }
}
