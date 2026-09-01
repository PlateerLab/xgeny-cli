//! Deterministic, offline conformance validation for the `XGENy` v0.1 protocol.

mod assets;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::OnceLock;

use jsonschema::{Draft, Registry};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use xgeny_domain::{
    API_VERSION_V1ALPHA1, ExecutionMode, ExecutionReceiptBody, PolicyDecisionBody,
    ProtocolDocument, ReceiptStatus, VerificationEvidence, VerificationResult, VerificationState,
    WorkGraphBody, WorkStepStatus,
};

const SCHEMA_BASE_URI: &str = "https://schemas.xgeny.dev/v1alpha1/";

struct BundledDocumentValidator {
    registry: Registry<'static>,
    validator: jsonschema::Validator,
}

static EXECUTION_RECEIPT_VALIDATOR: OnceLock<Result<BundledDocumentValidator, String>> =
    OnceLock::new();
static POLICY_DECISION_VALIDATOR: OnceLock<Result<BundledDocumentValidator, String>> =
    OnceLock::new();
static WORK_GRAPH_VALIDATOR: OnceLock<Result<BundledDocumentValidator, String>> = OnceLock::new();

/// Stable identifier for the first Core-owned Receipt construction and verification profile.
pub const CORE_RECEIPT_PROFILE_V1: &str = "xgeny.core-receipt/v1";
/// Core-owned Receipt profile for bounded Artifact commitments.
///
/// Version 1 permanently means that the Receipt has no artifacts. Version 2 is selected only for
/// execution semantics whose verified output must be represented by one or more bounded artifact
/// descriptors.
pub const CORE_RECEIPT_PROFILE_V2: &str = "xgeny.core-receipt/v2";
/// Maximum artifact commitments permitted by Core Receipt profile v2.
pub const CORE_RECEIPT_MAX_ARTIFACTS_V2: usize = 8;
/// Maximum byte size represented by one Core Receipt profile v2 artifact.
pub const CORE_RECEIPT_MAX_ARTIFACT_SIZE_BYTES_V2: u64 = 1024 * 1024;
/// Maximum aggregate byte size represented by Core Receipt profile v2 artifacts.
pub const CORE_RECEIPT_MAX_ARTIFACT_TOTAL_BYTES_V2: u64 = 4 * 1024 * 1024;

/// Check the non-provenance descriptor shape emitted by Core Receipt profile v2.
///
/// Receipt provenance and aggregate count/size are contextual and must be checked separately.
#[must_use]
pub fn core_artifact_descriptor_v2_is_valid(
    artifact_id: &str,
    name: Option<&str>,
    media_type: &str,
    size: u64,
    digest: &str,
) -> bool {
    let identifier_valid = !artifact_id.is_empty()
        && artifact_id.len() <= 200
        && artifact_id.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b':' | b'-'))
        });
    let name_valid = name.is_none_or(|name| {
        !name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)
    });
    let media_type_valid =
        (3..=200).contains(&media_type.len()) && !media_type.chars().any(char::is_control);
    let digest_valid = digest.strip_prefix("sha256:").is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    identifier_valid
        && name_valid
        && media_type_valid
        && size <= CORE_RECEIPT_MAX_ARTIFACT_SIZE_BYTES_V2
        && digest_valid
}
/// Secret-free input description required by the first Core Receipt profile.
pub const CORE_RECEIPT_INPUT_SUMMARY_V1: &str = "Invocation input retained by digest only.";
/// Exact redaction declarations required by the first Core Receipt profile.
pub const CORE_RECEIPT_REDACTIONS_V1: [&str; 2] = [
    "raw invocation arguments omitted",
    "raw tool output omitted",
];

/// Core-owned terminal meaning derived from the complete verification evidence set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreVerificationOutcome {
    Passed,
    Failed,
    Inconclusive,
}

/// Derive the deterministic Receipt identifier for the first Core Receipt profile.
#[must_use]
pub fn core_receipt_id_v1(effect_id: &str) -> String {
    format!(
        "receipt-{}",
        effect_id.strip_prefix("effect-").unwrap_or(effect_id)
    )
}

/// Return the only summary text allowed for one Core-owned verification observation.
#[must_use]
pub const fn core_verification_summary_v1(result: VerificationResult) -> &'static str {
    match result {
        VerificationResult::Passed => "Core-selected verification rule passed.",
        VerificationResult::Failed => "Core-selected verification rule failed.",
        VerificationResult::Inconclusive => "Core-selected verification rule was inconclusive.",
    }
}

/// Evaluate the first Core Receipt profile over an exact verification evidence set.
#[must_use]
pub fn evaluate_core_verification_v1(evidence: &[VerificationEvidence]) -> CoreVerificationOutcome {
    let required_failed = evidence
        .iter()
        .any(|item| item.required && item.result == VerificationResult::Failed);
    let required_inconclusive = evidence
        .iter()
        .any(|item| item.required && item.result == VerificationResult::Inconclusive);
    let any_passed = evidence
        .iter()
        .any(|item| item.result == VerificationResult::Passed);
    if required_failed {
        CoreVerificationOutcome::Failed
    } else if required_inconclusive || !any_passed {
        CoreVerificationOutcome::Inconclusive
    } else {
        CoreVerificationOutcome::Passed
    }
}

/// Map a Core verification outcome to its protocol Receipt status.
#[must_use]
pub const fn core_receipt_status_v1(outcome: CoreVerificationOutcome) -> ReceiptStatus {
    match outcome {
        CoreVerificationOutcome::Passed => ReceiptStatus::Succeeded,
        CoreVerificationOutcome::Failed => ReceiptStatus::Failed,
        CoreVerificationOutcome::Inconclusive => ReceiptStatus::Unknown,
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("asset `{asset}` is not valid JSON: {source}")]
    Json {
        asset: String,
        source: serde_json::Error,
    },
    #[error("schema validation failed: {0}")]
    Schema(String),
    #[error("fixture conformance failed: {0}")]
    Fixture(String),
    #[error("canonicalization failed: {0}")]
    Canonicalization(String),
    #[error("unsupported required extension `{0}`")]
    UnsupportedRequiredExtension(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    pub schema_count: usize,
    pub fixture_count: usize,
    pub valid_fixture_count: usize,
    pub invalid_fixture_count: usize,
    pub semantic_check_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixtureManifest {
    api_version: String,
    fixtures: Vec<FixtureEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixtureEntry {
    file: String,
    schema: String,
    expected_valid: bool,
}

/// Validate every bundled schema and fixture without filesystem or network access.
///
/// # Errors
///
/// Returns an error when a schema, fixture, Rust round-trip, digest, or cross-document
/// invariant does not conform to protocol v0.1.
pub fn check_bundled_protocol() -> Result<ConformanceReport, ProtocolError> {
    let schemas = parse_schemas()?;
    let registry = build_registry(&schemas)?;
    let manifest: FixtureManifest = parse_json("manifest.json", assets::FIXTURE_MANIFEST)?;

    for (name, schema) in &schemas {
        build_validator(schema, &registry, name)?;
    }
    validate_fixture_manifest(&manifest)?;

    let mut valid_fixture_count = 0;
    let mut invalid_fixture_count = 0;
    let mut semantic_check_count = 0;
    let mut valid_values = BTreeMap::new();

    for entry in &manifest.fixtures {
        let fixture_asset = assets::FIXTURES
            .iter()
            .find(|asset| asset.name == entry.file)
            .ok_or_else(|| {
                ProtocolError::Fixture(format!(
                    "manifest references unbundled fixture `{}`",
                    entry.file
                ))
            })?;
        let fixture: Value = parse_json(fixture_asset.name, fixture_asset.contents)?;
        let schema = schemas.get(entry.schema.as_str()).ok_or_else(|| {
            ProtocolError::Fixture(format!(
                "fixture `{}` references unknown schema `{}`",
                entry.file, entry.schema
            ))
        })?;
        let validator = build_validator(schema, &registry, &entry.schema)?;

        if entry.expected_valid {
            validator.validate(&fixture).map_err(|error| {
                ProtocolError::Fixture(format!(
                    "valid fixture `{}` was rejected at {}: {}",
                    entry.file,
                    error.instance_path(),
                    error
                ))
            })?;
            valid_fixture_count += 1;

            let document: ProtocolDocument =
                serde_json::from_value(fixture.clone()).map_err(|error| {
                    ProtocolError::Fixture(format!(
                        "valid fixture `{}` does not deserialize to its Rust domain type: {error}",
                        entry.file
                    ))
                })?;
            validate_required_extensions(&document, &BTreeSet::new())?;
            let round_trip = serde_json::to_value(&document).map_err(|error| {
                ProtocolError::Fixture(format!(
                    "valid fixture `{}` could not be serialized from its Rust domain type: {error}",
                    entry.file
                ))
            })?;
            validator.validate(&round_trip).map_err(|error| {
                ProtocolError::Fixture(format!(
                    "Rust round-trip for `{}` violates the schema at {}: {}",
                    entry.file,
                    error.instance_path(),
                    error
                ))
            })?;
            ensure_extension_round_trip(&entry.file, &fixture, &round_trip)?;
            semantic_check_count +=
                validate_document_semantics(&entry.file, &fixture, &document, &registry)?;
            valid_values.insert(entry.file.clone(), fixture);
        } else {
            if validator.is_valid(&fixture) {
                return Err(ProtocolError::Fixture(format!(
                    "invalid fixture `{}` unexpectedly passed `{}`",
                    entry.file, entry.schema
                )));
            }
            invalid_fixture_count += 1;
        }
    }

    semantic_check_count += validate_cross_document_links(&valid_values)?;

    Ok(ConformanceReport {
        schema_count: schemas.len(),
        fixture_count: manifest.fixtures.len(),
        valid_fixture_count,
        invalid_fixture_count,
        semantic_check_count,
    })
}

fn validate_fixture_manifest(manifest: &FixtureManifest) -> Result<(), ProtocolError> {
    if manifest.api_version != API_VERSION_V1ALPHA1 {
        return Err(ProtocolError::Fixture(format!(
            "manifest apiVersion is `{}`, expected `{API_VERSION_V1ALPHA1}`",
            manifest.api_version
        )));
    }

    let declared: BTreeSet<&str> = manifest
        .fixtures
        .iter()
        .map(|fixture| fixture.file.as_str())
        .collect();
    let bundled: BTreeSet<&str> = assets::FIXTURES.iter().map(|asset| asset.name).collect();
    if declared.len() != manifest.fixtures.len() || declared != bundled {
        return Err(ProtocolError::Fixture(format!(
            "manifest fixture set does not match bundled assets ({} entries, {} unique, {} bundled)",
            manifest.fixtures.len(),
            declared.len(),
            assets::FIXTURES.len()
        )));
    }
    Ok(())
}

/// Return an RFC 8785 / SHA-256 content digest.
///
/// # Errors
///
/// Returns an error when the JSON value cannot be encoded using RFC 8785.
pub fn canonical_digest(value: &Value) -> Result<String, ProtocolError> {
    let canonical = serde_jcs::to_vec(value)
        .map_err(|error| ProtocolError::Canonicalization(error.to_string()))?;
    let digest = Sha256::digest(canonical);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(format!("sha256:{encoded}"))
}

/// Calculate a canonical digest after removing one top-level derived field.
///
/// # Errors
///
/// Returns an error when the value is not an object, the named field is absent, or
/// canonical encoding fails.
pub fn canonical_digest_without_field(value: &Value, field: &str) -> Result<String, ProtocolError> {
    let mut unsigned = value.clone();
    let object = unsigned.as_object_mut().ok_or_else(|| {
        ProtocolError::Canonicalization("digest input must be a JSON object".to_owned())
    })?;
    if object.remove(field).is_none() {
        return Err(ProtocolError::Canonicalization(format!(
            "digest input has no `{field}` field"
        )));
    }
    canonical_digest(&unsigned)
}

/// Validate one typed `ExecutionReceipt` against the bundled schema and derived digest rules.
///
/// This is the runtime/store boundary for generated or persisted Receipts. Validation is fully
/// offline and includes the `kind` discriminator in the canonical digest input.
///
/// # Errors
///
/// Returns an error for schema, required-extension, serialization, or digest violations.
pub fn validate_execution_receipt(receipt: &ExecutionReceiptBody) -> Result<(), ProtocolError> {
    let context = cached_document_validator(
        &EXECUTION_RECEIPT_VALIDATOR,
        "execution-receipt.schema.json",
    )?;
    let document = ProtocolDocument::ExecutionReceipt(Box::new(receipt.clone()));
    let value = serde_json::to_value(&document).map_err(|source| ProtocolError::Json {
        asset: "runtime ExecutionReceipt".to_owned(),
        source,
    })?;
    context.validator.validate(&value).map_err(|error| {
        ProtocolError::Fixture(format!(
            "runtime ExecutionReceipt violates the schema at {}: {}",
            error.instance_path(),
            error
        ))
    })?;
    validate_required_extensions(&document, &BTreeSet::new())?;
    validate_document_semantics(
        "runtime ExecutionReceipt",
        &value,
        &document,
        &context.registry,
    )?;
    Ok(())
}

/// Validate one typed `PolicyDecision` against the bundled offline contract.
///
/// # Errors
///
/// Returns an error for schema, required-extension, serialization, or semantic violations.
pub fn validate_policy_decision(decision: &PolicyDecisionBody) -> Result<(), ProtocolError> {
    let context =
        cached_document_validator(&POLICY_DECISION_VALIDATOR, "policy-decision.schema.json")?;
    let document = ProtocolDocument::PolicyDecision(Box::new(decision.clone()));
    let value = serde_json::to_value(&document).map_err(|source| ProtocolError::Json {
        asset: "runtime PolicyDecision".to_owned(),
        source,
    })?;
    context.validator.validate(&value).map_err(|error| {
        ProtocolError::Fixture(format!(
            "runtime PolicyDecision violates the schema at {}: {}",
            error.instance_path(),
            error
        ))
    })?;
    validate_required_extensions(&document, &BTreeSet::new())?;
    validate_document_semantics(
        "runtime PolicyDecision",
        &value,
        &document,
        &context.registry,
    )?;
    Ok(())
}

/// Validate one typed `WorkGraph` snapshot against its bundled wire and DAG semantics.
///
/// # Errors
///
/// Returns an error for schema violations, duplicate/unknown/self/cyclic dependencies, or a Step
/// lifecycle that claims readiness or progress before every dependency completed.
pub fn validate_work_graph(graph: &WorkGraphBody) -> Result<(), ProtocolError> {
    let context = cached_document_validator(&WORK_GRAPH_VALIDATOR, "work-graph.schema.json")?;
    let document = ProtocolDocument::WorkGraph(Box::new(graph.clone()));
    let value = serde_json::to_value(&document).map_err(|source| ProtocolError::Json {
        asset: "runtime WorkGraph".to_owned(),
        source,
    })?;
    context.validator.validate(&value).map_err(|error| {
        ProtocolError::Fixture(format!(
            "runtime WorkGraph violates the schema at {}: {}",
            error.instance_path(),
            error
        ))
    })?;
    validate_required_extensions(&document, &BTreeSet::new())?;
    validate_document_semantics("runtime WorkGraph", &value, &document, &context.registry)?;
    Ok(())
}

fn cached_document_validator(
    cell: &'static OnceLock<Result<BundledDocumentValidator, String>>,
    schema_name: &'static str,
) -> Result<&'static BundledDocumentValidator, ProtocolError> {
    cell.get_or_init(|| {
        let schemas = parse_schemas().map_err(|error| error.to_string())?;
        let registry = build_registry(&schemas).map_err(|error| error.to_string())?;
        let schema = schemas
            .get(schema_name)
            .ok_or_else(|| format!("schema `{schema_name}` is not bundled"))?;
        let validator =
            build_validator(schema, &registry, schema_name).map_err(|error| error.to_string())?;
        Ok(BundledDocumentValidator {
            registry,
            validator,
        })
    })
    .as_ref()
    .map_err(|error| ProtocolError::Schema(error.clone()))
}

/// Fail closed when a document requires an extension unsupported by this reader.
///
/// # Errors
///
/// Returns [`ProtocolError::UnsupportedRequiredExtension`] for the first required URI
/// not present in `supported`.
pub fn ensure_required_extensions_supported(
    required: &[String],
    supported: &BTreeSet<String>,
) -> Result<(), ProtocolError> {
    if let Some(extension) = required
        .iter()
        .find(|extension| !supported.contains(extension.as_str()))
    {
        return Err(ProtocolError::UnsupportedRequiredExtension(
            extension.clone(),
        ));
    }
    Ok(())
}

fn parse_schemas() -> Result<BTreeMap<&'static str, Value>, ProtocolError> {
    let mut schemas = BTreeMap::new();
    for asset in assets::SCHEMAS {
        let schema: Value = parse_json(asset.name, asset.contents)?;
        jsonschema::meta::options()
            .validate(&schema)
            .map_err(|error| {
                ProtocolError::Schema(format!(
                    "schema `{}` is not valid JSON Schema: {error}",
                    asset.name
                ))
            })?;
        schemas.insert(asset.name, schema);
    }
    Ok(schemas)
}

fn build_registry(schemas: &BTreeMap<&str, Value>) -> Result<Registry<'static>, ProtocolError> {
    let mut builder = Registry::new();
    for (name, schema) in schemas {
        let id = schema
            .get("$id")
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolError::Schema(format!("schema `{name}` has no string `$id`")))?;
        let expected_id = format!("{SCHEMA_BASE_URI}{name}");
        if id != expected_id {
            return Err(ProtocolError::Schema(format!(
                "schema `{name}` has `$id` `{id}`, expected `{expected_id}`"
            )));
        }
        builder = builder.add(id, schema.clone()).map_err(|error| {
            ProtocolError::Schema(format!("could not register schema `{name}`: {error}"))
        })?;
    }
    builder
        .prepare()
        .map_err(|error| ProtocolError::Schema(format!("could not prepare registry: {error}")))
}

fn build_validator<'a>(
    schema: &'a Value,
    registry: &'a Registry<'a>,
    name: &str,
) -> Result<jsonschema::Validator, ProtocolError> {
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_registry(registry)
        .offline()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|error| {
            ProtocolError::Schema(format!(
                "could not compile schema `{name}` offline: {error}"
            ))
        })
}

fn parse_json<T>(asset: &str, contents: &str) -> Result<T, ProtocolError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(contents).map_err(|source| ProtocolError::Json {
        asset: asset.to_owned(),
        source,
    })
}

fn ensure_extension_round_trip(
    name: &str,
    original: &Value,
    round_trip: &Value,
) -> Result<(), ProtocolError> {
    for pointer in ["/extensions", "/requiredExtensions"] {
        let original_value = original.pointer(pointer);
        let round_trip_value = round_trip.pointer(pointer);
        let equal = original_value == round_trip_value
            || matches!((original_value, round_trip_value), (None, Some(value)) | (Some(value), None) if is_empty_extension_value(value));
        if !equal {
            return Err(ProtocolError::Fixture(format!(
                "Rust round-trip changed `{pointer}` in `{name}`"
            )));
        }
    }
    Ok(())
}

fn is_empty_extension_value(value: &Value) -> bool {
    value.as_object().is_some_and(serde_json::Map::is_empty)
        || value.as_array().is_some_and(Vec::is_empty)
}

fn validate_required_extensions(
    document: &ProtocolDocument,
    supported: &BTreeSet<String>,
) -> Result<(), ProtocolError> {
    let required = match document {
        ProtocolDocument::CapabilityDefinition(body) => &body.required_extensions,
        ProtocolDocument::CapabilityInstance(body) => &body.required_extensions,
        ProtocolDocument::PermissionRequest(body) => &body.required_extensions,
        ProtocolDocument::PolicyDecision(body) => &body.required_extensions,
        ProtocolDocument::InvocationPlan(body) => &body.required_extensions,
        ProtocolDocument::WorkGraph(body) => &body.required_extensions,
        ProtocolDocument::RunJournalEvent(body) => &body.required_extensions,
        ProtocolDocument::ExecutionReceipt(body) => &body.required_extensions,
    };
    ensure_required_extensions_supported(required, supported)
}

fn validate_document_semantics(
    name: &str,
    original: &Value,
    document: &ProtocolDocument,
    registry: &Registry<'_>,
) -> Result<usize, ProtocolError> {
    match document {
        ProtocolDocument::CapabilityDefinition(body) => {
            validate_embedded_schema(name, "inputSchema", &body.spec.input_schema, registry)?;
            validate_embedded_schema(name, "outputSchema", &body.spec.output_schema, registry)?;
            Ok(2)
        }
        ProtocolDocument::InvocationPlan(body) => {
            let size = serde_jcs::to_vec(&body.arguments)
                .map_err(|error| ProtocolError::Canonicalization(error.to_string()))?
                .len();
            if u64::try_from(size) != Ok(body.arguments_size_bytes) {
                return Err(ProtocolError::Fixture(format!(
                    "`{name}` declares argumentsSizeBytes={} but canonical arguments contain {size} bytes",
                    body.arguments_size_bytes
                )));
            }
            Ok(1)
        }
        ProtocolDocument::WorkGraph(body) => validate_work_graph_semantics(name, body),
        ProtocolDocument::RunJournalEvent(body) => {
            verify_derived_digest(name, original, "eventDigest", &body.event_digest)?;
            Ok(1)
        }
        ProtocolDocument::ExecutionReceipt(body) => {
            verify_derived_digest(name, original, "receiptDigest", &body.receipt_digest)?;
            Ok(1)
        }
        _ => Ok(0),
    }
}

fn validate_work_graph_semantics(
    name: &str,
    graph: &WorkGraphBody,
) -> Result<usize, ProtocolError> {
    let mut steps = BTreeMap::new();
    for step in &graph.steps {
        if steps.insert(step.step_id.as_str(), step).is_some() {
            return Err(ProtocolError::Fixture(format!(
                "`{name}` repeats WorkGraph stepId `{}`",
                step.step_id
            )));
        }
    }

    let mut remaining_dependencies = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = steps
        .keys()
        .copied()
        .map(|step_id| (step_id, Vec::new()))
        .collect();
    let mut semantic_checks = 1_usize;
    for (step_id, step) in &steps {
        let mut unique = BTreeSet::new();
        for dependency_id in &step.depends_on {
            semantic_checks = semantic_checks.saturating_add(1);
            if dependency_id == step_id {
                return Err(ProtocolError::Fixture(format!(
                    "`{name}` WorkGraph step `{step_id}` depends on itself"
                )));
            }
            if !unique.insert(dependency_id.as_str()) {
                return Err(ProtocolError::Fixture(format!(
                    "`{name}` WorkGraph step `{step_id}` repeats dependency `{dependency_id}`"
                )));
            }
            let Some(children) = dependents.get_mut(dependency_id.as_str()) else {
                return Err(ProtocolError::Fixture(format!(
                    "`{name}` WorkGraph step `{step_id}` refers to unknown dependency `{dependency_id}`"
                )));
            };
            children.push(step_id);
        }
        if graph.execution_mode == ExecutionMode::Direct && !step.depends_on.is_empty() {
            return Err(ProtocolError::Fixture(format!(
                "`{name}` Direct WorkGraph step cannot declare dependencies"
            )));
        }
        if matches!(
            step.status,
            WorkStepStatus::Ready
                | WorkStepStatus::Running
                | WorkStepStatus::WaitingInput
                | WorkStepStatus::Validating
                | WorkStepStatus::Completed
        ) {
            for dependency_id in &step.depends_on {
                if steps[dependency_id.as_str()].status != WorkStepStatus::Completed {
                    return Err(ProtocolError::Fixture(format!(
                        "`{name}` WorkGraph step `{step_id}` advanced before dependency `{dependency_id}` completed"
                    )));
                }
            }
        }
        if step.status == WorkStepStatus::Completed
            && step.verification_status != VerificationState::Passed
        {
            return Err(ProtocolError::Fixture(format!(
                "`{name}` completed WorkGraph step `{step_id}` is not verification-passed"
            )));
        }
        remaining_dependencies.insert(*step_id, step.depends_on.len());
    }

    let mut zero_indegree: BTreeSet<&str> = remaining_dependencies
        .iter()
        .filter_map(|(step_id, count)| (*count == 0).then_some(*step_id))
        .collect();
    let mut processed = 0_usize;
    while let Some(step_id) = zero_indegree.pop_first() {
        processed = processed.saturating_add(1);
        for child in &dependents[step_id] {
            let remaining = remaining_dependencies
                .get_mut(child)
                .expect("validated dependent should be indexed");
            *remaining = remaining
                .checked_sub(1)
                .expect("dependency count cannot underflow");
            if *remaining == 0 {
                zero_indegree.insert(child);
            }
        }
    }
    if processed != steps.len() {
        return Err(ProtocolError::Fixture(format!(
            "`{name}` WorkGraph contains a dependency cycle"
        )));
    }

    Ok(semantic_checks)
}

fn validate_embedded_schema(
    fixture: &str,
    field: &str,
    schema: &Value,
    registry: &Registry<'_>,
) -> Result<(), ProtocolError> {
    if let Some(dialect) = schema.get("$schema").and_then(Value::as_str)
        && dialect != "https://json-schema.org/draft/2020-12/schema"
    {
        return Err(ProtocolError::Fixture(format!(
            "embedded `{field}` in `{fixture}` declares unsupported dialect `{dialect}`"
        )));
    }
    jsonschema::meta::options()
        .validate(schema)
        .map_err(|error| {
            ProtocolError::Fixture(format!(
                "embedded `{field}` in `{fixture}` is not valid JSON Schema: {error}"
            ))
        })?;
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_registry(registry)
        .offline()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|error| {
            ProtocolError::Fixture(format!(
                "embedded `{field}` in `{fixture}` cannot compile offline: {error}"
            ))
        })?;
    Ok(())
}

fn verify_derived_digest(
    fixture: &str,
    value: &Value,
    field: &str,
    expected: &str,
) -> Result<(), ProtocolError> {
    let actual = canonical_digest_without_field(value, field)?;
    if actual != expected {
        return Err(ProtocolError::Fixture(format!(
            "`{fixture}` has `{field}` `{expected}`, recomputed value is `{actual}`"
        )));
    }
    Ok(())
}

fn validate_cross_document_links(values: &BTreeMap<String, Value>) -> Result<usize, ProtocolError> {
    let event = get_valid_fixture(values, "valid/run-journal-event.step-completed.json")?;
    let graph = get_valid_fixture(values, "valid/work-graph.direct-completed.json")?;
    let receipt = get_valid_fixture(values, "valid/execution-receipt.fs-read-success.json")?;
    let policy = get_valid_fixture(values, "valid/policy-decision.allow-once.json")?;

    require_equal_pointer(graph, "/runId", event, "/runId", "WorkGraph/Event runId")?;
    require_equal_pointer(
        graph,
        "/journalSequence",
        event,
        "/sequence",
        "WorkGraph journal cursor",
    )?;
    require_equal_pointer(
        graph,
        "/journalHeadDigest",
        event,
        "/eventDigest",
        "WorkGraph journal digest",
    )?;
    require_equal_pointer(
        receipt,
        "/policy/decisionId",
        policy,
        "/decisionId",
        "Receipt policy decisionId",
    )?;

    let expected_policy_digest = receipt
        .pointer("/policy/decisionDigest")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::Fixture("receipt has no policy decisionDigest".to_owned()))?;
    let actual_policy_digest = canonical_digest(policy)?;
    if expected_policy_digest != actual_policy_digest {
        return Err(ProtocolError::Fixture(format!(
            "Receipt policy digest `{expected_policy_digest}` does not match `{actual_policy_digest}`"
        )));
    }

    Ok(5)
}

fn get_valid_fixture<'a>(
    values: &'a BTreeMap<String, Value>,
    name: &str,
) -> Result<&'a Value, ProtocolError> {
    values
        .get(name)
        .ok_or_else(|| ProtocolError::Fixture(format!("missing validated fixture `{name}`")))
}

fn require_equal_pointer(
    left: &Value,
    left_pointer: &str,
    right: &Value,
    right_pointer: &str,
    label: &str,
) -> Result<(), ProtocolError> {
    let left_value = left.pointer(left_pointer);
    let right_value = right.pointer(right_pointer);
    if left_value != right_value {
        return Err(ProtocolError::Fixture(format!(
            "{label} mismatch: {left_value:?} != {right_value:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use xgeny_domain::VerificationStrategy;

    #[test]
    fn bundled_protocol_is_conformant() {
        let report = check_bundled_protocol().expect("bundled protocol should be conformant");
        assert_eq!(report.schema_count, 9);
        assert_eq!(report.fixture_count, 27);
        assert_eq!(report.valid_fixture_count, 17);
        assert_eq!(report.invalid_fixture_count, 10);
        assert!(report.semantic_check_count >= 10);
    }

    #[test]
    fn core_artifact_descriptor_v2_shape_is_closed_and_bounded() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(core_artifact_descriptor_v2_is_valid(
            "artifact-1",
            Some("output.json"),
            "application/json",
            128,
            &digest,
        ));
        assert!(!core_artifact_descriptor_v2_is_valid(
            "artifact-1",
            Some("output\n.json"),
            "application/json",
            128,
            &digest,
        ));
        assert!(!core_artifact_descriptor_v2_is_valid(
            "artifact-1",
            None,
            "application/json",
            CORE_RECEIPT_MAX_ARTIFACT_SIZE_BYTES_V2 + 1,
            &digest,
        ));
    }

    #[test]
    fn unknown_required_extension_fails_closed() {
        let required = vec!["https://example.com/extensions/required/v1".to_owned()];
        let error = ensure_required_extensions_supported(&required, &BTreeSet::new())
            .expect_err("unsupported extension must be rejected");
        assert!(matches!(
            error,
            ProtocolError::UnsupportedRequiredExtension(_)
        ));
    }

    #[test]
    fn supported_required_extension_is_accepted() {
        let extension = "https://example.com/extensions/required/v1".to_owned();
        let required = vec![extension.clone()];
        let supported = BTreeSet::from([extension]);
        ensure_required_extensions_supported(&required, &supported)
            .expect("supported extension should be accepted");
    }

    #[test]
    fn digest_excludes_only_the_named_field() {
        let value = json!({"a": 1, "digest": "derived", "nested": {"digest": "kept"}});
        let actual = canonical_digest_without_field(&value, "digest").expect("digest should work");
        let expected = canonical_digest(&json!({"a": 1, "nested": {"digest": "kept"}}))
            .expect("digest should work");
        assert_eq!(actual, expected);
    }

    #[test]
    fn arbitrary_execution_receipt_uses_the_bundled_schema_and_digest_rules() {
        let value: Value = serde_json::from_str(
            assets::FIXTURES
                .iter()
                .find(|asset| asset.name == "valid/execution-receipt.fs-read-success.json")
                .expect("fixture should be bundled")
                .contents,
        )
        .expect("fixture should parse");
        let ProtocolDocument::ExecutionReceipt(receipt) =
            serde_json::from_value(value).expect("fixture should deserialize")
        else {
            panic!("expected ExecutionReceipt")
        };
        validate_execution_receipt(&receipt).expect("fixture receipt should validate");

        let mut tampered = (*receipt).clone();
        tampered.output_digest = format!("sha256:{}", "e".repeat(64));
        assert!(validate_execution_receipt(&tampered).is_err());
    }

    #[test]
    fn runtime_work_graph_validation_enforces_dag_semantics() {
        let document: ProtocolDocument = serde_json::from_str(
            assets::FIXTURES
                .iter()
                .find(|asset| asset.name == "valid/work-graph.direct-completed.json")
                .expect("WorkGraph fixture should be bundled")
                .contents,
        )
        .expect("WorkGraph fixture should deserialize");
        let ProtocolDocument::WorkGraph(graph) = document else {
            panic!("expected WorkGraph")
        };
        let mut graph = *graph;
        graph.execution_mode = ExecutionMode::Persistent;
        graph.status = xgeny_domain::WorkGraphStatus::Running;
        let mut root = graph.steps[0].clone();
        root.step_id = "step-a".to_owned();
        root.depends_on.clear();
        let mut child = root.clone();
        child.step_id = "step-b".to_owned();
        child.depends_on = vec![root.step_id.clone()];
        child.status = WorkStepStatus::Ready;
        child.verification_status = VerificationState::NotStarted;
        child.output_digest = None;
        graph.steps = vec![root.clone(), child.clone()];
        validate_work_graph(&graph).expect("valid dependency DAG should pass");

        let mut unknown = graph.clone();
        unknown.steps[1].depends_on = vec!["step-missing".to_owned()];
        assert!(validate_work_graph(&unknown).is_err());

        let mut cycle = graph.clone();
        cycle.steps[0].status = WorkStepStatus::Pending;
        cycle.steps[0].verification_status = VerificationState::NotStarted;
        cycle.steps[0].depends_on = vec!["step-b".to_owned()];
        cycle.steps[1].status = WorkStepStatus::Pending;
        cycle.steps[1].depends_on = vec!["step-a".to_owned()];
        assert!(validate_work_graph(&cycle).is_err());

        let mut early_ready = graph.clone();
        early_ready.steps[0].status = WorkStepStatus::Pending;
        early_ready.steps[0].verification_status = VerificationState::NotStarted;
        assert!(validate_work_graph(&early_ready).is_err());

        let mut duplicate = graph;
        duplicate.steps[1].step_id = duplicate.steps[0].step_id.clone();
        duplicate.steps[1].depends_on.clear();
        assert!(validate_work_graph(&duplicate).is_err());
    }

    #[test]
    fn runtime_policy_decision_validation_rejects_an_invalid_timestamp() {
        let document: ProtocolDocument = serde_json::from_str(
            assets::FIXTURES
                .iter()
                .find(|asset| asset.name == "valid/policy-decision.allow-once.json")
                .expect("fixture should be bundled")
                .contents,
        )
        .expect("fixture should deserialize");
        let ProtocolDocument::PolicyDecision(mut decision) = document else {
            panic!("expected PolicyDecision")
        };
        validate_policy_decision(&decision).expect("fixture decision should validate");
        decision.decided_at = "RAW-POLICY-SENTINEL".to_owned();
        assert!(validate_policy_decision(&decision).is_err());
    }

    #[test]
    fn core_receipt_profile_handles_mixed_required_and_optional_rules() {
        let evidence = |required, result| VerificationEvidence {
            strategy: VerificationStrategy::Postcondition,
            required,
            result,
            summary: core_verification_summary_v1(result).to_owned(),
            evidence_digest: (result == VerificationResult::Passed)
                .then(|| format!("sha256:{}", "a".repeat(64))),
            artifact: None,
        };

        let optional_failure = vec![
            evidence(true, VerificationResult::Passed),
            evidence(false, VerificationResult::Failed),
        ];
        assert_eq!(
            evaluate_core_verification_v1(&optional_failure),
            CoreVerificationOutcome::Passed
        );
        assert_eq!(
            core_receipt_status_v1(evaluate_core_verification_v1(&optional_failure)),
            ReceiptStatus::Succeeded
        );

        let required_failure = vec![
            evidence(false, VerificationResult::Passed),
            evidence(true, VerificationResult::Failed),
        ];
        assert_eq!(
            evaluate_core_verification_v1(&required_failure),
            CoreVerificationOutcome::Failed
        );

        let required_inconclusive = vec![
            evidence(false, VerificationResult::Passed),
            evidence(true, VerificationResult::Inconclusive),
        ];
        assert_eq!(
            evaluate_core_verification_v1(&required_inconclusive),
            CoreVerificationOutcome::Inconclusive
        );
    }

    #[test]
    fn offline_validator_rejects_unbundled_ref() {
        let schema = json!({"$ref": "https://unbundled.example/schema.json"});
        assert!(jsonschema::options().offline().build(&schema).is_err());
    }

    #[test]
    fn tampered_derived_digest_is_rejected() {
        let mut event: Value = parse_json(
            "valid/run-journal-event.step-completed.json",
            assets::FIXTURES
                .iter()
                .find(|asset| asset.name == "valid/run-journal-event.step-completed.json")
                .expect("event fixture should be bundled")
                .contents,
        )
        .expect("event fixture should parse");
        event["payload"]["receiptId"] = json!("receipt-tampered");
        let expected = event["eventDigest"]
            .as_str()
            .expect("event digest should be a string");
        assert!(verify_derived_digest("tampered-event", &event, "eventDigest", expected).is_err());
    }

    #[test]
    fn embedded_schema_rejects_another_explicit_dialect() {
        let schemas = parse_schemas().expect("bundled schemas should parse");
        let registry = build_registry(&schemas).expect("bundled registry should build");
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "string"
        });
        assert!(validate_embedded_schema("fixture", "inputSchema", &schema, &registry).is_err());
    }

    #[test]
    fn embedded_schema_resolves_only_bundled_refs() {
        let schemas = parse_schemas().expect("bundled schemas should parse");
        let registry = build_registry(&schemas).expect("bundled registry should build");
        let bundled = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "https://schemas.xgeny.dev/v1alpha1/common.schema.json#/$defs/credentialRef"
        });
        validate_embedded_schema("fixture", "inputSchema", &bundled, &registry)
            .expect("bundled ref should resolve");

        let unbundled = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "https://unbundled.example/schema.json"
        });
        assert!(validate_embedded_schema("fixture", "inputSchema", &unbundled, &registry).is_err());
    }
}
