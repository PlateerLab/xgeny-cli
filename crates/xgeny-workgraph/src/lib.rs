#![doc = "I/O-free state transitions and hash-chained events for a durable `XGENy` run."]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunEvent {
    pub event_id: String,
    pub run_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub recorded_at: String,
    pub body: RunEventBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RunEventBody {
    RunCreated {
        goal: String,
    },
    StepPlanned {
        step_id: String,
        objective: String,
    },
    EffectIntentCommitted {
        step_id: String,
        intent: Box<EffectIntent>,
    },
    InvocationMaterialUnavailable {
        step_id: String,
        effect_id: String,
        reason: InvocationMaterialUnavailableReason,
    },
    EffectExecutionStarted {
        step_id: String,
        effect_id: String,
    },
    EffectSucceeded {
        step_id: String,
        effect_id: String,
        #[serde(rename = "receiptDigest")]
        evidence_digest: String,
    },
    EffectFailed {
        step_id: String,
        effect_id: String,
        #[serde(rename = "receiptDigest")]
        evidence_digest: String,
    },
    EffectBecameUnknown {
        step_id: String,
        effect_id: String,
        reason: String,
    },
    ReconciliationStarted {
        step_id: String,
        effect_id: String,
    },
    ReconciliationResolved {
        step_id: String,
        effect_id: String,
        resolution: ReconciliationResolution,
        evidence_digest: String,
    },
    ManualInterventionRequired {
        step_id: String,
        effect_id: String,
        reason: String,
    },
    VerificationPassed {
        step_id: String,
    },
    VerificationFailed {
        step_id: String,
        reason: String,
    },
    VerificationRecorded {
        step_id: String,
        effect_id: String,
        disposition: VerificationDisposition,
        receipt_id: String,
        receipt_digest: String,
    },
}

impl RunEventBody {
    fn kind(&self) -> &'static str {
        match self {
            Self::RunCreated { .. } => "run_created",
            Self::StepPlanned { .. } => "step_planned",
            Self::EffectIntentCommitted { .. } => "effect_intent_committed",
            Self::InvocationMaterialUnavailable { .. } => "invocation_material_unavailable",
            Self::EffectExecutionStarted { .. } => "effect_execution_started",
            Self::EffectSucceeded { .. } => "effect_succeeded",
            Self::EffectFailed { .. } => "effect_failed",
            Self::EffectBecameUnknown { .. } => "effect_became_unknown",
            Self::ReconciliationStarted { .. } => "reconciliation_started",
            Self::ReconciliationResolved { .. } => "reconciliation_resolved",
            Self::ManualInterventionRequired { .. } => "manual_intervention_required",
            Self::VerificationPassed { .. } => "verification_passed",
            Self::VerificationFailed { .. } => "verification_failed",
            Self::VerificationRecorded { .. } => "verification_recorded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectIntent {
    pub effect_id: String,
    pub action_digest: String,
    pub invocation: InvocationBinding,
    pub effect_class: EffectClass,
    pub idempotency_key: Option<String>,
    pub sink_guarantee: SinkGuarantee,
    pub authorization: AuthorizationUse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_provenance: Option<ReceiptProvenance>,
}

/// Core-issued, secret-free facts needed to construct a protocol `ExecutionReceipt` after a
/// process restart. The adapter cannot create or modify this binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptProvenance {
    pub profile_version: String,
    pub invocation_id: String,
    pub plan_id: String,
    pub policy_decision_id: String,
    pub policy_decision_digest: String,
    pub executor_id: String,
    pub executor_placement: ReceiptPlacement,
    pub executor_platform: String,
    pub input_summary: String,
    pub verification_plan: Vec<ReceiptVerificationRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptPlacement {
    Local,
    Device,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptVerificationStrategy {
    OutputSchema,
    Postcondition,
    ArtifactDigest,
    Receipt,
    Human,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptVerificationRule {
    pub strategy: ReceiptVerificationStrategy,
    pub required: bool,
}

/// Final core verification result bound to one persisted `ExecutionReceipt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDisposition {
    Passed,
    Failed,
    Inconclusive,
}

/// Calculate the canonical digest covered by a new authorization binding.
///
/// # Errors
///
/// Returns an error when RFC 8785 canonicalization fails.
pub fn receipt_provenance_digest(
    provenance: &ReceiptProvenance,
) -> Result<String, AuthorizationDigestError> {
    let canonical = serde_jcs::to_vec(provenance)
        .map_err(|error| AuthorizationDigestError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

/// Immutable executable binding retained with a durable effect intent.
///
/// Dynamic health, authentication state, and cost hints are deliberately excluded. The binding
/// digest identifies the adapter endpoint/operation selected by the trusted admission path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationBinding {
    pub capability_id: String,
    pub contract_version: String,
    pub definition_digest: String,
    pub instance_id: String,
    pub instance_binding_digest: String,
}

pub const INVOCATION_MATERIAL_FORMAT_VERSION: u32 = 1;
const MAX_MATERIAL_REFERENCE_COMPONENT_BYTES: usize = 128;

/// Secret-free, version-pinned recipe reference used to reconstruct invocation arguments.
///
/// A reference is an identifier, never a path, URL, bearer token, raw argument, or credential.
/// Its provider owns the durable recipe and must return canonical invocation arguments when asked
/// for this exact `(reference_id, revision)` pair.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReconstructableMaterialReference {
    provider_id: String,
    reference_id: String,
    revision: String,
}

impl ReconstructableMaterialReference {
    /// Build a bounded opaque reference. Components intentionally reject path and URI syntax.
    ///
    /// # Errors
    ///
    /// Returns an error when a component is empty, oversized, or contains characters outside the
    /// identifier alphabet `[A-Za-z0-9._-]`.
    pub fn new(
        provider_id: impl Into<String>,
        reference_id: impl Into<String>,
        revision: impl Into<String>,
    ) -> Result<Self, InvocationMaterialError> {
        let reference = Self {
            provider_id: provider_id.into(),
            reference_id: reference_id.into(),
            revision: revision.into(),
        };
        validate_reference_component("provider_id", &reference.provider_id)?;
        validate_reference_component("reference_id", &reference.reference_id)?;
        validate_reference_component("revision", &reference.revision)?;
        Ok(reference)
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    #[must_use]
    pub fn reference_id(&self) -> &str {
        &self.reference_id
    }

    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    fn validate(&self) -> Result<(), InvocationMaterialError> {
        validate_reference_component("provider_id", &self.provider_id)?;
        validate_reference_component("reference_id", &self.reference_id)?;
        validate_reference_component("revision", &self.revision)
    }
}

impl std::fmt::Debug for ReconstructableMaterialReference {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReconstructableMaterialReference")
            .field("provider_id", &self.provider_id)
            .field("reference_id", &"<redacted>")
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    content = "reference",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InvocationMaterialRetention {
    Ephemeral,
    ReconstructableReference(ReconstructableMaterialReference),
}

impl std::fmt::Debug for InvocationMaterialRetention {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ephemeral => formatter.write_str("Ephemeral"),
            Self::ReconstructableReference(reference) => formatter
                .debug_tuple("ReconstructableReference")
                .field(reference)
                .finish(),
        }
    }
}

/// Durable, secret-free sidecar binding for one exact effect intent.
///
/// The record deliberately excludes invocation arguments and credentials. It binds a recovery
/// mode and canonical material digest to the Run, Step, effect, semantic action, and selected
/// executable Instance. It is committed atomically with the intent by a supporting `RunStore`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationMaterialRecord {
    format_version: u32,
    material_id: String,
    run_id: String,
    step_id: String,
    effect_id: String,
    action_digest: String,
    invocation: InvocationBinding,
    material_digest: String,
    retention: InvocationMaterialRetention,
    record_digest: String,
}

impl InvocationMaterialRecord {
    /// Create a self-verifying material binding for a committed effect intent.
    ///
    /// # Errors
    ///
    /// Returns an error for missing identity fields, invalid references, or canonicalization
    /// failures.
    pub fn new(
        run_id: impl Into<String>,
        step_id: impl Into<String>,
        intent: &EffectIntent,
        material_digest: impl Into<String>,
        retention: InvocationMaterialRetention,
    ) -> Result<Self, InvocationMaterialError> {
        let mut record = Self {
            format_version: INVOCATION_MATERIAL_FORMAT_VERSION,
            material_id: String::new(),
            run_id: run_id.into(),
            step_id: step_id.into(),
            effect_id: intent.effect_id.clone(),
            action_digest: intent.action_digest.clone(),
            invocation: intent.invocation.clone(),
            material_digest: material_digest.into(),
            retention,
            record_digest: String::new(),
        };
        record.validate_shape()?;
        record.material_id =
            invocation_material_id(&record.run_id, &record.effect_id, &record.material_digest)?;
        record.record_digest = invocation_material_record_digest(&record)?;
        record.verify_for(&record.run_id, &record.step_id, intent)?;
        Ok(record)
    }

    /// Verify content integrity and exact binding to a durable intent.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed fields, tampering, or cross-intent
    /// record reuse.
    pub fn verify_for(
        &self,
        run_id: &str,
        step_id: &str,
        intent: &EffectIntent,
    ) -> Result<(), InvocationMaterialError> {
        if self.format_version != INVOCATION_MATERIAL_FORMAT_VERSION {
            return Err(InvocationMaterialError::UnsupportedFormatVersion(
                self.format_version,
            ));
        }
        self.validate_shape()?;
        if self.run_id != run_id {
            return Err(InvocationMaterialError::BindingMismatch("run_id"));
        }
        if self.step_id != step_id {
            return Err(InvocationMaterialError::BindingMismatch("step_id"));
        }
        if self.effect_id != intent.effect_id {
            return Err(InvocationMaterialError::BindingMismatch("effect_id"));
        }
        if self.action_digest != intent.action_digest {
            return Err(InvocationMaterialError::BindingMismatch("action_digest"));
        }
        if self.invocation != intent.invocation {
            return Err(InvocationMaterialError::BindingMismatch("invocation"));
        }
        if intent.authorization.binding.material_digest != self.material_digest {
            return Err(InvocationMaterialError::BindingMismatch("material_digest"));
        }
        if intent.authorization.binding.material_retention_digest
            != invocation_material_retention_digest(&self.retention)?
        {
            return Err(InvocationMaterialError::BindingMismatch(
                "material_retention_digest",
            ));
        }
        let expected_id =
            invocation_material_id(&self.run_id, &self.effect_id, &self.material_digest)?;
        if self.material_id != expected_id {
            return Err(InvocationMaterialError::MaterialIdMismatch);
        }
        let expected_digest = invocation_material_record_digest(self)?;
        if self.record_digest != expected_digest {
            return Err(InvocationMaterialError::RecordDigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    #[must_use]
    pub fn material_id(&self) -> &str {
        &self.material_id
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[must_use]
    pub fn step_id(&self) -> &str {
        &self.step_id
    }

    #[must_use]
    pub fn effect_id(&self) -> &str {
        &self.effect_id
    }

    #[must_use]
    pub fn action_digest(&self) -> &str {
        &self.action_digest
    }

    #[must_use]
    pub const fn invocation(&self) -> &InvocationBinding {
        &self.invocation
    }

    #[must_use]
    pub fn material_digest(&self) -> &str {
        &self.material_digest
    }

    #[must_use]
    pub const fn retention(&self) -> &InvocationMaterialRetention {
        &self.retention
    }

    #[must_use]
    pub fn record_digest(&self) -> &str {
        &self.record_digest
    }

    fn validate_shape(&self) -> Result<(), InvocationMaterialError> {
        for (field, value) in [
            ("run_id", self.run_id.as_str()),
            ("step_id", self.step_id.as_str()),
            ("effect_id", self.effect_id.as_str()),
            ("action_digest", self.action_digest.as_str()),
            ("material_digest", self.material_digest.as_str()),
        ] {
            if value.is_empty() {
                return Err(InvocationMaterialError::EmptyField(field));
            }
        }
        if let InvocationMaterialRetention::ReconstructableReference(reference) = &self.retention {
            reference.validate()?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for InvocationMaterialRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InvocationMaterialRecord")
            .field("format_version", &self.format_version)
            .field("material_id", &self.material_id)
            .field("run_id", &self.run_id)
            .field("step_id", &self.step_id)
            .field("effect_id", &self.effect_id)
            .field("action_digest", &self.action_digest)
            .field("invocation", &self.invocation)
            .field("material_digest", &self.material_digest)
            .field("retention", &self.retention)
            .field("record_digest", &self.record_digest)
            .finish()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InvocationMaterialDigestInput<'a, T: Serialize + ?Sized> {
    domain: &'static str,
    material: &'a T,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InvocationMaterialIdInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    effect_id: &'a str,
    material_digest: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InvocationMaterialRecordDigestInput<'a> {
    domain: &'static str,
    format_version: u32,
    material_id: &'a str,
    run_id: &'a str,
    step_id: &'a str,
    effect_id: &'a str,
    action_digest: &'a str,
    invocation: &'a InvocationBinding,
    material_digest: &'a str,
    retention: &'a InvocationMaterialRetention,
}

/// Commit to canonical invocation material without storing it.
///
/// # Errors
///
/// Returns an error when RFC 8785 canonicalization fails.
pub fn invocation_material_digest<T: Serialize + ?Sized>(
    material: &T,
) -> Result<String, InvocationMaterialError> {
    let canonical = serde_jcs::to_vec(&InvocationMaterialDigestInput {
        domain: "xgeny.invocation-material.payload/v1",
        material,
    })
    .map_err(|error| InvocationMaterialError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

/// Commit an authorization to the host-selected material retention recipe.
///
/// # Errors
///
/// Returns an error when RFC 8785 canonicalization fails.
pub fn invocation_material_retention_digest(
    retention: &InvocationMaterialRetention,
) -> Result<String, InvocationMaterialError> {
    let canonical = serde_jcs::to_vec(&InvocationMaterialDigestInput {
        domain: "xgeny.invocation-material.retention/v1",
        material: retention,
    })
    .map_err(|error| InvocationMaterialError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn invocation_material_id(
    run_id: &str,
    effect_id: &str,
    material_digest: &str,
) -> Result<String, InvocationMaterialError> {
    let canonical = serde_jcs::to_vec(&InvocationMaterialIdInput {
        domain: "xgeny.invocation-material.id/v1",
        run_id,
        effect_id,
        material_digest,
    })
    .map_err(|error| InvocationMaterialError::Canonicalization(error.to_string()))?;
    let digest = sha256_digest(&canonical);
    let encoded = digest.strip_prefix("sha256:").unwrap_or(&digest);
    Ok(format!("material-{encoded}"))
}

fn invocation_material_record_digest(
    record: &InvocationMaterialRecord,
) -> Result<String, InvocationMaterialError> {
    let canonical = serde_jcs::to_vec(&InvocationMaterialRecordDigestInput {
        domain: "xgeny.invocation-material.record/v1",
        format_version: record.format_version,
        material_id: &record.material_id,
        run_id: &record.run_id,
        step_id: &record.step_id,
        effect_id: &record.effect_id,
        action_digest: &record.action_digest,
        invocation: &record.invocation,
        material_digest: &record.material_digest,
        retention: &record.retention,
    })
    .map_err(|error| InvocationMaterialError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn validate_reference_component(
    field: &'static str,
    value: &str,
) -> Result<(), InvocationMaterialError> {
    if value.is_empty() {
        return Err(InvocationMaterialError::InvalidReferenceComponent(field));
    }
    if matches!(value, "." | "..")
        || value.len() > MAX_MATERIAL_REFERENCE_COMPONENT_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(InvocationMaterialError::InvalidReferenceComponent(field));
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum InvocationMaterialError {
    #[error("unsupported invocation material format version {0}")]
    UnsupportedFormatVersion(u32),
    #[error("invocation material field `{0}` must not be empty")]
    EmptyField(&'static str),
    #[error("invocation material reference component `{0}` is invalid")]
    InvalidReferenceComponent(&'static str),
    #[error("invocation material binding differs at `{0}`")]
    BindingMismatch(&'static str),
    #[error("invocation material identifier does not match its binding")]
    MaterialIdMismatch,
    #[error("invocation material record digest does not match its content")]
    RecordDigestMismatch,
    #[error("invocation material canonicalization failed: {0}")]
    Canonicalization(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationMaterialUnavailableReason {
    EphemeralMaterialLost,
    ReferenceUnavailable,
    ReferenceChanged,
    AdapterBindingUnavailable,
    CredentialBindingChanged,
    UnsupportedMaterialVersion,
}

impl InvocationMaterialUnavailableReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::EphemeralMaterialLost => "ephemeral_material_lost",
            Self::ReferenceUnavailable => "reference_unavailable",
            Self::ReferenceChanged => "reference_changed",
            Self::AdapterBindingUnavailable => "adapter_binding_unavailable",
            Self::CredentialBindingChanged => "credential_binding_changed",
            Self::UnsupportedMaterialVersion => "unsupported_material_version",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Reversible,
    Idempotent,
    NonIdempotent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkGuarantee {
    None,
    DeduplicateByKey,
    QueryByKey,
    DeduplicateAndQuery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationUse {
    pub grant_id: String,
    pub grant_digest: String,
    pub max_uses: u32,
    pub binding: AuthorizationBinding,
}

/// Run-local facts covered by one issued authorization digest.
///
/// The journal head is the state against which policy and routing were evaluated. Persisting the
/// binding lets replay reject copying an intent to another Run, Step, authority epoch, action, or
/// executable Instance even though the low-level journal types remain serializable primitives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationBinding {
    pub run_id: String,
    pub step_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub issued_at_sequence: u64,
    pub issued_at_head_digest: String,
    pub capability_id: String,
    pub contract_version: String,
    pub definition_digest: String,
    pub instance_id: String,
    pub instance_binding_digest: String,
    pub action_digest: String,
    pub material_digest: String,
    pub material_retention_digest: String,
    pub policy_evidence_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_provenance_digest: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizationDigestInput<'a> {
    domain: &'static str,
    binding: &'a AuthorizationBinding,
    max_uses: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OnceAuthorizationIdInput<'a> {
    domain: &'static str,
    run_id: &'a str,
    action_digest: &'a str,
}

/// Derive the stable budget identity for one semantic action within a Run.
///
/// Keeping this derivation in the reducer's crate prevents a caller from changing only the grant
/// ID to mint a fresh one-shot budget for the same Run/action pair.
///
/// # Errors
///
/// Returns an error if RFC 8785 canonical JSON encoding fails.
pub fn once_authorization_id(
    run_id: &str,
    action_digest: &str,
) -> Result<String, AuthorizationDigestError> {
    let canonical = serde_jcs::to_vec(&OnceAuthorizationIdInput {
        domain: "xgeny.authorization-budget.once/v1",
        run_id,
        action_digest,
    })
    .map_err(|error| AuthorizationDigestError::Canonicalization(error.to_string()))?;
    let digest = sha256_digest(&canonical);
    let encoded = digest.strip_prefix("sha256:").unwrap_or(&digest);
    Ok(format!("authorization-{encoded}"))
}

/// Calculate the content digest the reducer expects for a durable authorization binding.
///
/// # Errors
///
/// Returns an error if RFC 8785 canonical JSON encoding fails.
pub fn authorization_digest(
    binding: &AuthorizationBinding,
    max_uses: u32,
) -> Result<String, AuthorizationDigestError> {
    let canonical = serde_jcs::to_vec(&AuthorizationDigestInput {
        domain: "xgeny.authorization.once/v2",
        binding,
        max_uses,
    })
    .map_err(|error| AuthorizationDigestError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationResolution {
    ProvedApplied,
    ProvedNotApplied,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventRecord {
    pub sequence: u64,
    pub previous_digest: Option<String>,
    pub event: RunEvent,
    pub digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DigestInput<'a> {
    sequence: u64,
    previous_digest: Option<&'a str>,
    event: &'a RunEvent,
}

impl EventRecord {
    /// Build the next immutable record in a hash chain.
    ///
    /// # Errors
    ///
    /// Returns an error if RFC 8785 canonical JSON encoding fails.
    pub fn next(previous: Option<&Self>, event: RunEvent) -> Result<Self, RecordError> {
        let sequence = previous.map_or(Ok(1), |record| {
            record
                .sequence
                .checked_add(1)
                .ok_or(RecordError::SequenceOverflow)
        })?;
        let previous_digest = previous.map(|record| record.digest.clone());
        let digest = record_digest(sequence, previous_digest.as_deref(), &event)?;
        Ok(Self {
            sequence,
            previous_digest,
            event,
            digest,
        })
    }

    /// Verify this record's derived digest.
    ///
    /// # Errors
    ///
    /// Returns an error if canonicalization fails or the stored digest differs.
    pub fn verify_digest(&self) -> Result<(), RecordError> {
        let actual = record_digest(self.sequence, self.previous_digest.as_deref(), &self.event)?;
        if actual != self.digest {
            return Err(RecordError::DigestMismatch {
                sequence: self.sequence,
                expected: self.digest.clone(),
                actual,
            });
        }
        Ok(())
    }
}

fn record_digest(
    sequence: u64,
    previous_digest: Option<&str>,
    event: &RunEvent,
) -> Result<String, RecordError> {
    let canonical = serde_jcs::to_vec(&DigestInput {
        sequence,
        previous_digest,
        event,
    })
    .map_err(|error| RecordError::Canonicalization(error.to_string()))?;
    Ok(sha256_digest(&canonical))
}

fn sha256_digest(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunState {
    pub run_id: String,
    pub authority: String,
    pub authority_epoch: u64,
    pub goal: String,
    pub revision: u64,
    pub journal_sequence: u64,
    pub journal_head_digest: String,
    pub steps: BTreeMap<String, StepState>,
    pub authorization_consumption: BTreeMap<String, AuthorizationConsumption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepState {
    pub step_id: String,
    pub objective: String,
    pub status: StepStatus,
    pub attempts: u32,
    pub intent: Option<EffectIntent>,
    #[serde(rename = "receiptDigest")]
    pub effect_evidence_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_receipt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_receipt_digest: Option<String>,
    pub uncertainty_reason: Option<String>,
    pub reconciliation_evidence_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Planned,
    IntentCommitted,
    Executing,
    EffectUnknown,
    Reconciling,
    Validating,
    Completed,
    Failed,
    ManualRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationConsumption {
    pub grant_digest: String,
    pub max_uses: u32,
    pub uses: u32,
    pub effect_ids: BTreeSet<String>,
}

/// Apply one committed event without performing I/O or invoking an effect.
///
/// # Errors
///
/// Returns an error when chain metadata, authority, or lifecycle invariants fail.
pub fn apply_record(
    current: Option<&RunState>,
    record: &EventRecord,
) -> Result<RunState, TransitionError> {
    verify_record_against_state(current, record)?;
    let mut state = match (current, &record.event.body) {
        (None, RunEventBody::RunCreated { goal }) => RunState {
            run_id: record.event.run_id.clone(),
            authority: record.event.authority.clone(),
            authority_epoch: record.event.authority_epoch,
            goal: goal.clone(),
            revision: 0,
            journal_sequence: 0,
            journal_head_digest: String::new(),
            steps: BTreeMap::new(),
            authorization_consumption: BTreeMap::new(),
        },
        (None, _) => return Err(TransitionError::FirstEventMustCreateRun),
        (Some(_), RunEventBody::RunCreated { .. }) => {
            return Err(TransitionError::RunAlreadyCreated);
        }
        (Some(state), _) => state.clone(),
    };

    if current.is_some() {
        apply_body(&mut state, &record.event.body)?;
    }
    state.revision = record.sequence;
    state.journal_sequence = record.sequence;
    state.journal_head_digest.clone_from(&record.digest);
    Ok(state)
}

fn verify_record_against_state(
    current: Option<&RunState>,
    record: &EventRecord,
) -> Result<(), TransitionError> {
    let expected_sequence = current.map_or(Ok(1), |state| {
        state
            .journal_sequence
            .checked_add(1)
            .ok_or(RecordError::SequenceOverflow)
    })?;
    if record.sequence != expected_sequence {
        return Err(TransitionError::UnexpectedSequence {
            expected: expected_sequence,
            actual: record.sequence,
        });
    }
    let expected_previous = current.map(|state| state.journal_head_digest.clone());
    if record.previous_digest != expected_previous {
        return Err(TransitionError::PreviousDigestMismatch {
            sequence: record.sequence,
            expected: expected_previous,
            actual: record.previous_digest.clone(),
        });
    }
    record.verify_digest()?;
    if let Some(state) = current {
        if record.event.run_id != state.run_id {
            return Err(TransitionError::RunIdMismatch);
        }
        if record.event.authority != state.authority {
            return Err(TransitionError::AuthorityMismatch);
        }
        if record.event.authority_epoch != state.authority_epoch {
            return Err(TransitionError::AuthorityEpochMismatch);
        }
    }
    Ok(())
}

fn apply_body(state: &mut RunState, body: &RunEventBody) -> Result<(), TransitionError> {
    match body {
        RunEventBody::RunCreated { .. } => unreachable!("handled by apply_record"),
        RunEventBody::StepPlanned { step_id, objective } => {
            if state.steps.contains_key(step_id) {
                return Err(TransitionError::DuplicateStep(step_id.clone()));
            }
            state.steps.insert(
                step_id.clone(),
                StepState {
                    step_id: step_id.clone(),
                    objective: objective.clone(),
                    status: StepStatus::Planned,
                    attempts: 0,
                    intent: None,
                    effect_evidence_digest: None,
                    execution_receipt_id: None,
                    execution_receipt_digest: None,
                    uncertainty_reason: None,
                    reconciliation_evidence_digest: None,
                },
            );
        }
        RunEventBody::EffectIntentCommitted { step_id, intent } => {
            commit_effect_intent(state, step_id, intent)?;
        }
        _ => apply_effect_lifecycle(state, body)?,
    }
    Ok(())
}

fn apply_effect_lifecycle(
    state: &mut RunState,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    match body {
        RunEventBody::RunCreated { .. }
        | RunEventBody::StepPlanned { .. }
        | RunEventBody::EffectIntentCommitted { .. } => {
            unreachable!("handled by apply_body")
        }
        RunEventBody::InvocationMaterialUnavailable {
            step_id,
            effect_id,
            reason,
        } => mark_material_unavailable(state, step_id, effect_id, *reason, body)?,
        RunEventBody::EffectExecutionStarted { step_id, effect_id } => {
            record_execution_started(state, step_id, effect_id, body)?;
        }
        RunEventBody::EffectSucceeded {
            step_id,
            effect_id,
            evidence_digest,
        } => record_effect_observation(
            state,
            step_id,
            effect_id,
            evidence_digest,
            StepStatus::Validating,
            body,
        )?,
        RunEventBody::EffectFailed {
            step_id,
            effect_id,
            evidence_digest,
        } => record_effect_observation(
            state,
            step_id,
            effect_id,
            evidence_digest,
            StepStatus::Failed,
            body,
        )?,
        RunEventBody::EffectBecameUnknown {
            step_id,
            effect_id,
            reason,
        } => record_effect_unknown(state, step_id, effect_id, reason, body)?,
        RunEventBody::ReconciliationStarted { step_id, effect_id } => {
            let step = matching_step_mut(state, step_id, effect_id)?;
            require_status(step, StepStatus::EffectUnknown, body)?;
            step.status = StepStatus::Reconciling;
        }
        RunEventBody::ReconciliationResolved {
            step_id,
            effect_id,
            resolution,
            evidence_digest,
        } => record_reconciliation(
            state,
            step_id,
            effect_id,
            *resolution,
            evidence_digest,
            body,
        )?,
        RunEventBody::ManualInterventionRequired {
            step_id,
            effect_id,
            reason,
        } => record_manual_required(state, step_id, effect_id, reason, body)?,
        RunEventBody::VerificationPassed { step_id } => {
            let step = step_mut(state, step_id)?;
            require_status(step, StepStatus::Validating, body)?;
            step.status = StepStatus::Completed;
        }
        RunEventBody::VerificationFailed { step_id, .. } => {
            let step = step_mut(state, step_id)?;
            require_status(step, StepStatus::Validating, body)?;
            step.status = StepStatus::Failed;
        }
        RunEventBody::VerificationRecorded {
            step_id,
            effect_id,
            disposition,
            receipt_id,
            receipt_digest,
        } => record_verification(
            state,
            step_id,
            effect_id,
            *disposition,
            receipt_id,
            receipt_digest,
            body,
        )?,
    }
    Ok(())
}

fn record_execution_started(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::IntentCommitted, body)?;
    step.status = StepStatus::Executing;
    step.attempts =
        step.attempts
            .checked_add(1)
            .ok_or_else(|| TransitionError::AttemptOverflow {
                step_id: step_id.to_owned(),
            })?;
    Ok(())
}

fn record_effect_unknown(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    reason: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::Executing, body)?;
    step.status = StepStatus::EffectUnknown;
    step.uncertainty_reason = Some(reason.to_owned());
    Ok(())
}

fn record_reconciliation(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    resolution: ReconciliationResolution,
    evidence_digest: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::Reconciling, body)?;
    step.reconciliation_evidence_digest = Some(evidence_digest.to_owned());
    step.uncertainty_reason = None;
    step.status = match resolution {
        ReconciliationResolution::ProvedApplied => StepStatus::Validating,
        ReconciliationResolution::ProvedNotApplied => StepStatus::IntentCommitted,
        ReconciliationResolution::Failed => StepStatus::Failed,
    };
    Ok(())
}

fn record_manual_required(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    reason: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    if !matches!(
        step.status,
        StepStatus::EffectUnknown | StepStatus::Reconciling
    ) {
        return invalid_transition(step, body);
    }
    step.status = StepStatus::ManualRequired;
    step.uncertainty_reason = Some(reason.to_owned());
    Ok(())
}

fn record_effect_observation(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    evidence_digest: &str,
    next_status: StepStatus,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::Executing, body)?;
    step.status = next_status;
    step.effect_evidence_digest = Some(evidence_digest.to_owned());
    Ok(())
}

fn record_verification(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    disposition: VerificationDisposition,
    receipt_id: &str,
    receipt_digest: &str,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::Validating, body)?;
    step.execution_receipt_id = Some(receipt_id.to_owned());
    step.execution_receipt_digest = Some(receipt_digest.to_owned());
    step.status = match disposition {
        VerificationDisposition::Passed => {
            step.uncertainty_reason = None;
            StepStatus::Completed
        }
        VerificationDisposition::Failed => {
            step.uncertainty_reason = None;
            StepStatus::Failed
        }
        VerificationDisposition::Inconclusive => {
            step.uncertainty_reason = Some("verification_inconclusive".to_owned());
            StepStatus::ManualRequired
        }
    };
    Ok(())
}

fn mark_material_unavailable(
    state: &mut RunState,
    step_id: &str,
    effect_id: &str,
    reason: InvocationMaterialUnavailableReason,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    let step = matching_step_mut(state, step_id, effect_id)?;
    require_status(step, StepStatus::IntentCommitted, body)?;
    step.status = StepStatus::ManualRequired;
    step.uncertainty_reason = Some(reason.code().to_owned());
    Ok(())
}

fn commit_effect_intent(
    state: &mut RunState,
    step_id: &str,
    intent: &EffectIntent,
) -> Result<(), TransitionError> {
    match (
        &intent.receipt_provenance,
        &intent.authorization.binding.receipt_provenance_digest,
    ) {
        (Some(provenance), Some(expected)) => {
            let actual = receipt_provenance_digest(provenance)?;
            if &actual != expected {
                return Err(TransitionError::ReceiptProvenanceDigestMismatch {
                    effect_id: intent.effect_id.clone(),
                });
            }
        }
        (None, None) => {}
        _ => {
            return Err(TransitionError::ReceiptProvenanceBindingMismatch {
                effect_id: intent.effect_id.clone(),
            });
        }
    }
    if intent.authorization.max_uses != 1 {
        return Err(TransitionError::InvalidAuthorizationBudget {
            grant_id: intent.authorization.grant_id.clone(),
        });
    }
    if intent.sink_guarantee != SinkGuarantee::None
        && intent.idempotency_key.as_ref().is_none_or(String::is_empty)
    {
        return Err(TransitionError::SinkGuaranteeRequiresIdempotencyKey {
            effect_id: intent.effect_id.clone(),
        });
    }
    if state.steps.values().any(|step| {
        step.intent
            .as_ref()
            .is_some_and(|existing| existing.effect_id == intent.effect_id)
    }) {
        return Err(TransitionError::DuplicateEffect(intent.effect_id.clone()));
    }

    validate_authorization_binding(state, step_id, intent)?;

    let step = state
        .steps
        .get(step_id)
        .ok_or_else(|| TransitionError::UnknownStep(step_id.to_owned()))?;
    require_status(
        step,
        StepStatus::Planned,
        &RunEventBody::EffectIntentCommitted {
            step_id: step_id.to_owned(),
            intent: Box::new(intent.clone()),
        },
    )?;

    let consumption = state
        .authorization_consumption
        .entry(intent.authorization.grant_id.clone())
        .or_insert_with(|| AuthorizationConsumption {
            grant_digest: intent.authorization.grant_digest.clone(),
            max_uses: intent.authorization.max_uses,
            uses: 0,
            effect_ids: BTreeSet::new(),
        });
    if consumption.grant_digest != intent.authorization.grant_digest
        || consumption.max_uses != intent.authorization.max_uses
    {
        return Err(TransitionError::AuthorizationGrantChanged {
            grant_id: intent.authorization.grant_id.clone(),
        });
    }
    if consumption.uses >= consumption.max_uses {
        return Err(TransitionError::AuthorizationBudgetExceeded {
            grant_id: intent.authorization.grant_id.clone(),
            max_uses: consumption.max_uses,
        });
    }
    consumption.uses += 1;
    consumption.effect_ids.insert(intent.effect_id.clone());

    let step = state
        .steps
        .get_mut(step_id)
        .expect("step was checked before authorization mutation");
    step.intent = Some(intent.clone());
    step.status = StepStatus::IntentCommitted;
    Ok(())
}

fn validate_authorization_binding(
    state: &RunState,
    step_id: &str,
    intent: &EffectIntent,
) -> Result<(), TransitionError> {
    let authorization = &intent.authorization;
    let binding = &authorization.binding;
    let invocation = &intent.invocation;

    if binding.run_id != state.run_id {
        return Err(TransitionError::AuthorizationRunMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.step_id != step_id {
        return Err(TransitionError::AuthorizationStepMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.authority != state.authority {
        return Err(TransitionError::AuthorizationAuthorityMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.authority_epoch != state.authority_epoch {
        return Err(TransitionError::AuthorizationEpochMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.issued_at_sequence != state.journal_sequence
        || binding.issued_at_head_digest != state.journal_head_digest
    {
        return Err(TransitionError::AuthorizationHeadMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.action_digest != intent.action_digest {
        return Err(TransitionError::AuthorizationActionMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    if binding.capability_id != invocation.capability_id
        || binding.contract_version != invocation.contract_version
        || binding.definition_digest != invocation.definition_digest
        || binding.instance_id != invocation.instance_id
        || binding.instance_binding_digest != invocation.instance_binding_digest
    {
        return Err(TransitionError::AuthorizationInvocationMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    let expected_id = once_authorization_id(&binding.run_id, &binding.action_digest)?;
    if authorization.grant_id != expected_id {
        return Err(TransitionError::AuthorizationIdMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    let expected = authorization_digest(binding, authorization.max_uses)?;
    if authorization.grant_digest != expected {
        return Err(TransitionError::AuthorizationDigestMismatch {
            grant_id: authorization.grant_id.clone(),
        });
    }
    Ok(())
}

fn step_mut<'a>(
    state: &'a mut RunState,
    step_id: &str,
) -> Result<&'a mut StepState, TransitionError> {
    state
        .steps
        .get_mut(step_id)
        .ok_or_else(|| TransitionError::UnknownStep(step_id.to_owned()))
}

fn matching_step_mut<'a>(
    state: &'a mut RunState,
    step_id: &str,
    effect_id: &str,
) -> Result<&'a mut StepState, TransitionError> {
    let step = step_mut(state, step_id)?;
    let actual = step.intent.as_ref().map(|intent| intent.effect_id.as_str());
    if actual != Some(effect_id) {
        return Err(TransitionError::EffectMismatch {
            step_id: step_id.to_owned(),
            expected: actual.map(str::to_owned),
            actual: effect_id.to_owned(),
        });
    }
    Ok(step)
}

fn require_status(
    step: &StepState,
    required: StepStatus,
    body: &RunEventBody,
) -> Result<(), TransitionError> {
    if step.status != required {
        return invalid_transition(step, body);
    }
    Ok(())
}

fn invalid_transition<T>(step: &StepState, body: &RunEventBody) -> Result<T, TransitionError> {
    Err(TransitionError::InvalidStepTransition {
        step_id: step.step_id.clone(),
        from: step.status,
        event: body.kind(),
    })
}

/// Rebuild a projection from committed records only.
///
/// This function intentionally has no executor or tool port, so replay cannot emit effects.
///
/// # Errors
///
/// Returns an error when the chain or any state transition is invalid.
pub fn replay(records: &[EventRecord]) -> Result<RunState, ReplayError> {
    let mut state = None;
    for record in records {
        state = Some(apply_record(state.as_ref(), record).map_err(replay_transition_error)?);
    }
    state.ok_or(ReplayError::EmptyJournal)
}

fn replay_transition_error(error: TransitionError) -> ReplayError {
    match error {
        TransitionError::UnexpectedSequence { expected, actual } => {
            ReplayError::UnexpectedSequence { expected, actual }
        }
        TransitionError::PreviousDigestMismatch {
            sequence,
            expected,
            actual,
        } => ReplayError::PreviousDigestMismatch {
            sequence,
            expected,
            actual,
        },
        TransitionError::Record(error) => ReplayError::Record(error),
        error => ReplayError::Transition(error),
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RecordError {
    #[error("event canonicalization failed: {0}")]
    Canonicalization(String),
    #[error("event record {sequence} digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        sequence: u64,
        expected: String,
        actual: String,
    },
    #[error("event sequence overflowed u64")]
    SequenceOverflow,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthorizationDigestError {
    #[error("authorization canonicalization failed: {0}")]
    Canonicalization(String),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransitionError {
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error("first event must create the run")]
    FirstEventMustCreateRun,
    #[error("run is already created")]
    RunAlreadyCreated,
    #[error("event sequence mismatch: expected {expected}, got {actual}")]
    UnexpectedSequence { expected: u64, actual: u64 },
    #[error("event {sequence} previous digest mismatch")]
    PreviousDigestMismatch {
        sequence: u64,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error("event run id differs from the run projection")]
    RunIdMismatch,
    #[error("event authority differs from the run projection")]
    AuthorityMismatch,
    #[error("event authority epoch differs from the run projection")]
    AuthorityEpochMismatch,
    #[error("step `{0}` already exists")]
    DuplicateStep(String),
    #[error("unknown step `{0}`")]
    UnknownStep(String),
    #[error("effect `{0}` already has a committed intent")]
    DuplicateEffect(String),
    #[error("step `{step_id}` expected effect {expected:?}, got `{actual}`")]
    EffectMismatch {
        step_id: String,
        expected: Option<String>,
        actual: String,
    },
    #[error("step `{step_id}` cannot apply `{event}` from {from:?}")]
    InvalidStepTransition {
        step_id: String,
        from: StepStatus,
        event: &'static str,
    },
    #[error("authorization `{grant_id}` must allow exactly one use")]
    InvalidAuthorizationBudget { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another Run")]
    AuthorizationRunMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another Step")]
    AuthorizationStepMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another authority")]
    AuthorizationAuthorityMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another authority epoch")]
    AuthorizationEpochMismatch { grant_id: String },
    #[error("authorization `{grant_id}` was issued against another journal head")]
    AuthorizationHeadMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another semantic action")]
    AuthorizationActionMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is bound to another executable invocation")]
    AuthorizationInvocationMismatch { grant_id: String },
    #[error("authorization `{grant_id}` is not the stable ID for its Run/action budget")]
    AuthorizationIdMismatch { grant_id: String },
    #[error("authorization `{grant_id}` digest does not cover its durable binding")]
    AuthorizationDigestMismatch { grant_id: String },
    #[error(transparent)]
    AuthorizationDigest(#[from] AuthorizationDigestError),
    #[error("authorization `{grant_id}` changed after first consumption")]
    AuthorizationGrantChanged { grant_id: String },
    #[error("authorization `{grant_id}` exceeded its {max_uses}-use budget")]
    AuthorizationBudgetExceeded { grant_id: String, max_uses: u32 },
    #[error("effect `{effect_id}` claims a keyed sink guarantee without an idempotency key")]
    SinkGuaranteeRequiresIdempotencyKey { effect_id: String },
    #[error("effect `{effect_id}` receipt provenance and authorization binding differ")]
    ReceiptProvenanceBindingMismatch { effect_id: String },
    #[error("effect `{effect_id}` receipt provenance digest is invalid")]
    ReceiptProvenanceDigestMismatch { effect_id: String },
    #[error("step `{step_id}` attempt counter overflowed")]
    AttemptOverflow { step_id: String },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReplayError {
    #[error("journal has no run-created event")]
    EmptyJournal,
    #[error("event sequence mismatch: expected {expected}, got {actual}")]
    UnexpectedSequence { expected: u64, actual: u64 },
    #[error("event {sequence} previous digest mismatch")]
    PreviousDigestMismatch {
        sequence: u64,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const RUN_ID: &str = "run-1";
    const AUTHORITY: &str = "local:test";
    const AUTHORITY_EPOCH: u64 = 7;

    type IntentMutation = Box<dyn Fn(&mut EffectIntent)>;

    fn event(event_id: &str, body: RunEventBody) -> RunEvent {
        RunEvent {
            event_id: event_id.to_owned(),
            run_id: RUN_ID.to_owned(),
            authority: AUTHORITY.to_owned(),
            authority_epoch: AUTHORITY_EPOCH,
            recorded_at: "2026-08-28T00:00:00Z".to_owned(),
            body,
        }
    }

    fn append(
        records: &mut Vec<EventRecord>,
        state: Option<&RunState>,
        event: RunEvent,
    ) -> RunState {
        let previous = records.last();
        let record = EventRecord::next(previous, event).expect("record should be canonicalizable");
        let state = apply_record(state, &record).expect("transition should be valid");
        records.push(record);
        state
    }

    fn intent(state: &RunState, step_id: &str, effect_id: &str, max_uses: u32) -> RunEventBody {
        let action_digest = format!("sha256:action-{effect_id}");
        let material_digest = invocation_material_digest(&serde_json::json!({
            "effectId": effect_id
        }))
        .expect("material should canonicalize");
        let material_retention_digest =
            invocation_material_retention_digest(&InvocationMaterialRetention::Ephemeral)
                .expect("retention should canonicalize");
        let invocation = InvocationBinding {
            capability_id: "test.effect".to_owned(),
            contract_version: "1.0.0".to_owned(),
            definition_digest: "sha256:definition-1".to_owned(),
            instance_id: "test.instance".to_owned(),
            instance_binding_digest: "sha256:instance-1".to_owned(),
        };
        let binding = AuthorizationBinding {
            run_id: state.run_id.clone(),
            step_id: step_id.to_owned(),
            authority: state.authority.clone(),
            authority_epoch: state.authority_epoch,
            issued_at_sequence: state.journal_sequence,
            issued_at_head_digest: state.journal_head_digest.clone(),
            capability_id: invocation.capability_id.clone(),
            contract_version: invocation.contract_version.clone(),
            definition_digest: invocation.definition_digest.clone(),
            instance_id: invocation.instance_id.clone(),
            instance_binding_digest: invocation.instance_binding_digest.clone(),
            action_digest: action_digest.clone(),
            material_digest,
            material_retention_digest,
            policy_evidence_digest: "sha256:policy-1".to_owned(),
            receipt_provenance_digest: None,
        };
        let grant_digest =
            authorization_digest(&binding, max_uses).expect("authorization should canonicalize");
        let grant_id = once_authorization_id(&binding.run_id, &binding.action_digest)
            .expect("authorization ID should canonicalize");
        RunEventBody::EffectIntentCommitted {
            step_id: step_id.to_owned(),
            intent: Box::new(EffectIntent {
                effect_id: effect_id.to_owned(),
                action_digest,
                invocation,
                effect_class: EffectClass::NonIdempotent,
                idempotency_key: None,
                sink_guarantee: SinkGuarantee::None,
                authorization: AuthorizationUse {
                    grant_id,
                    grant_digest,
                    max_uses,
                    binding,
                },
                receipt_provenance: None,
            }),
        }
    }

    fn planned_state(records: &mut Vec<EventRecord>) -> RunState {
        let created = append(
            records,
            None,
            event(
                "material-event-1",
                RunEventBody::RunCreated {
                    goal: "recover one invocation".to_owned(),
                },
            ),
        );
        append(
            records,
            Some(&created),
            event(
                "material-event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-material".to_owned(),
                    objective: "prepare effect".to_owned(),
                },
            ),
        )
    }

    #[test]
    fn invocation_material_record_detects_cross_effect_and_content_tampering() {
        let mut records = Vec::new();
        let state = planned_state(&mut records);
        let RunEventBody::EffectIntentCommitted { mut intent, .. } =
            intent(&state, "step-material", "effect-material", 1)
        else {
            panic!("helper must create an intent")
        };
        let reference = ReconstructableMaterialReference::new("run-recipe", "recipe-1", "rev-1")
            .expect("reference should validate");
        let retention = InvocationMaterialRetention::ReconstructableReference(reference);
        intent.authorization.binding.material_retention_digest =
            invocation_material_retention_digest(&retention)
                .expect("retention should canonicalize");
        intent.authorization.grant_digest =
            authorization_digest(&intent.authorization.binding, intent.authorization.max_uses)
                .expect("authorization should canonicalize");
        let digest = intent.authorization.binding.material_digest.clone();
        let record =
            InvocationMaterialRecord::new(RUN_ID, "step-material", &intent, digest, retention)
                .expect("record should bind");
        record
            .verify_for(RUN_ID, "step-material", &intent)
            .expect("original record should verify");
        assert!(matches!(
            record.verify_for("run-other", "step-material", &intent),
            Err(InvocationMaterialError::BindingMismatch("run_id"))
        ));
        assert!(matches!(
            record.verify_for(RUN_ID, "step-other", &intent),
            Err(InvocationMaterialError::BindingMismatch("step_id"))
        ));

        let mut swapped_intent = (*intent).clone();
        swapped_intent.effect_id = "effect-other".to_owned();
        assert!(matches!(
            record.verify_for(RUN_ID, "step-material", &swapped_intent),
            Err(InvocationMaterialError::BindingMismatch("effect_id"))
        ));

        let mut tampered_json = serde_json::to_value(&record).expect("record should serialize");
        tampered_json["materialDigest"] = serde_json::json!("sha256:changed");
        let tampered: InvocationMaterialRecord =
            serde_json::from_value(tampered_json).expect("shape should deserialize");
        assert!(matches!(
            tampered.verify_for(RUN_ID, "step-material", &intent),
            Err(InvocationMaterialError::BindingMismatch("material_digest")
                | InvocationMaterialError::MaterialIdMismatch
                | InvocationMaterialError::RecordDigestMismatch)
        ));
        assert!(!format!("{record:?}").contains("recipe-1"));
    }

    #[test]
    fn material_reference_rejects_paths_uris_and_oversized_components() {
        for invalid in ["", ".", "..", "../recipe", "https://recipe", "recipe/value"] {
            assert!(matches!(
                ReconstructableMaterialReference::new("provider", invalid, "rev-1"),
                Err(InvocationMaterialError::InvalidReferenceComponent(
                    "reference_id"
                ))
            ));
        }
        assert!(
            ReconstructableMaterialReference::new(
                "provider",
                "x".repeat(MAX_MATERIAL_REFERENCE_COMPONENT_BYTES + 1),
                "rev-1"
            )
            .is_err()
        );
    }

    #[test]
    fn material_unavailable_moves_only_an_unstarted_intent_to_manual() {
        let mut records = Vec::new();
        let planned = planned_state(&mut records);
        let committed = append(
            &mut records,
            Some(&planned),
            event(
                "material-event-3",
                intent(&planned, "step-material", "effect-material", 1),
            ),
        );
        let manual = append(
            &mut records,
            Some(&committed),
            event(
                "material-event-4",
                RunEventBody::InvocationMaterialUnavailable {
                    step_id: "step-material".to_owned(),
                    effect_id: "effect-material".to_owned(),
                    reason: InvocationMaterialUnavailableReason::EphemeralMaterialLost,
                },
            ),
        );
        assert_eq!(
            manual.steps["step-material"].status,
            StepStatus::ManualRequired
        );
        assert_eq!(
            manual.steps["step-material"].uncertainty_reason.as_deref(),
            Some("ephemeral_material_lost")
        );

        let record = EventRecord::next(
            records.last(),
            event(
                "material-event-5",
                RunEventBody::InvocationMaterialUnavailable {
                    step_id: "step-material".to_owned(),
                    effect_id: "effect-material".to_owned(),
                    reason: InvocationMaterialUnavailableReason::ReferenceUnavailable,
                },
            ),
        )
        .expect("record should build");
        assert!(matches!(
            apply_record(Some(&manual), &record),
            Err(TransitionError::InvalidStepTransition {
                from: StepStatus::ManualRequired,
                ..
            })
        ));
    }

    #[test]
    fn durable_effect_happy_path_is_replayable() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event(
                "event-1",
                RunEventBody::RunCreated {
                    goal: "change one file safely".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".to_owned(),
                    objective: "write file".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-4",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".to_owned(),
                    effect_id: "effect-1".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-5",
                RunEventBody::EffectSucceeded {
                    step_id: "step-1".to_owned(),
                    effect_id: "effect-1".to_owned(),
                    evidence_digest: "sha256:receipt-1".to_owned(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-6",
                RunEventBody::VerificationPassed {
                    step_id: "step-1".to_owned(),
                },
            ),
        );

        assert_eq!(state.steps["step-1"].status, StepStatus::Completed);
        assert_eq!(state.authorization_consumption.len(), 1);
        assert_eq!(
            state
                .authorization_consumption
                .values()
                .next()
                .expect("one authorization should exist")
                .uses,
            1
        );
        assert_eq!(replay(&records).expect("replay should pass"), state);
        assert_eq!(replay(&records).expect("second replay should pass"), state);
    }

    #[test]
    fn unknown_non_idempotent_effect_cannot_be_blindly_retried() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        for (event_id, body) in [
            (
                "event-4",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                },
            ),
            (
                "event-5",
                RunEventBody::EffectBecameUnknown {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                    reason: "ack lost".into(),
                },
            ),
        ] {
            state = append(&mut records, Some(&state), event(event_id, body));
        }

        let retry = EventRecord::next(
            records.last(),
            event(
                "event-6",
                RunEventBody::EffectExecutionStarted {
                    step_id: "step-1".into(),
                    effect_id: "effect-1".into(),
                },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &retry),
            Err(TransitionError::InvalidStepTransition { .. })
        ));
    }

    #[test]
    fn proved_not_applied_allows_same_intent_to_resume() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        state = append(
            &mut records,
            Some(&state),
            event("event-3", intent(&state, "step-1", "effect-1", 1)),
        );
        let bodies = [
            RunEventBody::EffectExecutionStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
            RunEventBody::EffectBecameUnknown {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
                reason: "timeout".into(),
            },
            RunEventBody::ReconciliationStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
            RunEventBody::ReconciliationResolved {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
                resolution: ReconciliationResolution::ProvedNotApplied,
                evidence_digest: "sha256:evidence-1".into(),
            },
            RunEventBody::EffectExecutionStarted {
                step_id: "step-1".into(),
                effect_id: "effect-1".into(),
            },
        ];
        for (index, body) in bodies.into_iter().enumerate() {
            state = append(
                &mut records,
                Some(&state),
                event(&format!("event-{}", index + 4), body),
            );
        }

        assert_eq!(state.steps["step-1"].status, StepStatus::Executing);
        assert_eq!(state.steps["step-1"].attempts, 2);
        assert_eq!(state.authorization_consumption.len(), 1);
        assert_eq!(
            state
                .authorization_consumption
                .values()
                .next()
                .expect("one authorization should exist")
                .uses,
            1
        );
    }

    #[test]
    fn one_shot_authorization_cannot_be_rebound_to_another_step() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        for (event_id, step_id) in [("event-2", "step-1"), ("event-3", "step-2")] {
            state = append(
                &mut records,
                Some(&state),
                event(
                    event_id,
                    RunEventBody::StepPlanned {
                        step_id: step_id.into(),
                        objective: step_id.into(),
                    },
                ),
            );
        }
        state = append(
            &mut records,
            Some(&state),
            event("event-4", intent(&state, "step-1", "effect-1", 1)),
        );
        let RunEventBody::EffectIntentCommitted {
            step_id,
            mut intent,
        } = intent(&state, "step-2", "effect-2", 1)
        else {
            unreachable!("intent helper always creates an effect intent")
        };
        intent.action_digest = "sha256:action-effect-1".to_owned();
        intent.authorization.binding.action_digest = intent.action_digest.clone();
        intent.authorization.grant_id = once_authorization_id(
            &intent.authorization.binding.run_id,
            &intent.authorization.binding.action_digest,
        )
        .expect("authorization ID should canonicalize");
        intent.authorization.grant_digest =
            authorization_digest(&intent.authorization.binding, intent.authorization.max_uses)
                .expect("authorization should canonicalize");
        let rebound = EventRecord::next(
            records.last(),
            event(
                "event-5",
                RunEventBody::EffectIntentCommitted { step_id, intent },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &rebound),
            Err(TransitionError::AuthorizationGrantChanged { .. })
        ));
    }

    #[test]
    fn durable_authorization_binding_mismatches_fail_without_mutating_state() {
        let mut records = Vec::new();
        let created = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let state = append(
            &mut records,
            Some(&created),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        let original = state.clone();

        let mutations: Vec<IntentMutation> = vec![
            Box::new(|intent| intent.authorization.binding.run_id = "other-run".into()),
            Box::new(|intent| intent.authorization.binding.step_id = "other-step".into()),
            Box::new(|intent| intent.authorization.binding.authority = "other:authority".into()),
            Box::new(|intent| intent.authorization.binding.authority_epoch += 1),
            Box::new(|intent| intent.authorization.binding.issued_at_sequence += 1),
            Box::new(|intent| {
                intent.authorization.binding.issued_at_head_digest = "sha256:other-head".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.action_digest = "sha256:other-action".into();
            }),
            Box::new(|intent| intent.invocation.capability_id = "other.capability".into()),
            Box::new(|intent| intent.invocation.contract_version = "2.0.0".into()),
            Box::new(|intent| {
                intent.invocation.definition_digest = "sha256:other-definition".into();
            }),
            Box::new(|intent| intent.invocation.instance_id = "other.instance".into()),
            Box::new(|intent| {
                intent.invocation.instance_binding_digest = "sha256:other-binding".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.policy_evidence_digest = "sha256:other-policy".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.material_digest = "sha256:other-material".into();
            }),
            Box::new(|intent| {
                intent.authorization.binding.material_retention_digest =
                    "sha256:other-retention".into();
            }),
            Box::new(|intent| intent.authorization.grant_id = "authorization-forged".into()),
            Box::new(|intent| {
                intent.authorization.max_uses = 2;
                intent.authorization.grant_digest = authorization_digest(
                    &intent.authorization.binding,
                    intent.authorization.max_uses,
                )
                .expect("authorization should canonicalize");
            }),
            Box::new(|intent| intent.authorization.grant_digest = "sha256:forged".into()),
        ];

        for (index, mutate) in mutations.into_iter().enumerate() {
            let RunEventBody::EffectIntentCommitted {
                step_id,
                mut intent,
            } = intent(&state, "step-1", "effect-1", 1)
            else {
                unreachable!("intent helper always creates an effect intent")
            };
            mutate(&mut intent);
            let record = EventRecord::next(
                records.last(),
                event(
                    &format!("invalid-event-{index}"),
                    RunEventBody::EffectIntentCommitted { step_id, intent },
                ),
            )
            .expect("record should build");

            assert!(apply_record(Some(&state), &record).is_err());
            assert_eq!(state, original);
        }
    }

    #[test]
    fn replay_rejects_a_broken_hash_chain() {
        let mut records = Vec::new();
        let state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let _state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        records[1].previous_digest = Some("sha256:tampered".into());

        assert!(matches!(
            replay(&records),
            Err(ReplayError::PreviousDigestMismatch { sequence: 2, .. })
        ));
    }

    #[test]
    fn keyed_sink_guarantee_requires_an_actual_key() {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        state = append(
            &mut records,
            Some(&state),
            event(
                "event-2",
                RunEventBody::StepPlanned {
                    step_id: "step-1".into(),
                    objective: "o".into(),
                },
            ),
        );
        let RunEventBody::EffectIntentCommitted {
            step_id,
            mut intent,
        } = intent(&state, "step-1", "effect-1", 1)
        else {
            unreachable!("intent helper always returns an intent event");
        };
        intent.sink_guarantee = SinkGuarantee::QueryByKey;
        let invalid = EventRecord::next(
            records.last(),
            event(
                "event-3",
                RunEventBody::EffectIntentCommitted { step_id, intent },
            ),
        )
        .expect("record should build");

        assert!(matches!(
            apply_record(Some(&state), &invalid),
            Err(TransitionError::SinkGuaranteeRequiresIdempotencyKey { .. })
        ));
    }

    #[test]
    fn authority_epoch_change_is_fenced() {
        let mut records = Vec::new();
        let current_projection = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        let mut stale_event = event(
            "event-2",
            RunEventBody::StepPlanned {
                step_id: "step-1".into(),
                objective: "o".into(),
            },
        );
        stale_event.authority_epoch -= 1;
        let stale_record =
            EventRecord::next(records.last(), stale_event).expect("record should build");

        assert_eq!(
            apply_record(Some(&current_projection), &stale_record),
            Err(TransitionError::AuthorityEpochMismatch)
        );
    }

    #[test]
    fn event_sequence_overflow_is_rejected() {
        let previous = EventRecord {
            sequence: u64::MAX,
            previous_digest: None,
            event: event("event-max", RunEventBody::RunCreated { goal: "g".into() }),
            digest: "sha256:max".into(),
        };

        assert_eq!(
            EventRecord::next(
                Some(&previous),
                event(
                    "event-overflow",
                    RunEventBody::StepPlanned {
                        step_id: "step-1".into(),
                        objective: "o".into(),
                    },
                ),
            ),
            Err(RecordError::SequenceOverflow)
        );
    }

    #[test]
    fn legacy_receipt_digest_wire_spelling_is_preserved_for_effect_evidence() {
        let body = RunEventBody::EffectSucceeded {
            step_id: "step-1".to_owned(),
            effect_id: "effect-1".to_owned(),
            evidence_digest: format!("sha256:{}", "a".repeat(64)),
        };
        let value = serde_json::to_value(&body).expect("event body should serialize");
        assert_eq!(
            value.pointer("/receiptDigest"),
            Some(&serde_json::json!(format!("sha256:{}", "a".repeat(64))))
        );
        assert!(value.pointer("/evidenceDigest").is_none());

        let round_trip: RunEventBody =
            serde_json::from_value(value).expect("legacy event should deserialize");
        assert_eq!(round_trip, body);
    }

    fn planned_journal(step_count: u8) -> (Vec<EventRecord>, RunState) {
        let mut records = Vec::new();
        let mut state = append(
            &mut records,
            None,
            event("event-1", RunEventBody::RunCreated { goal: "g".into() }),
        );
        for index in 0..step_count {
            state = append(
                &mut records,
                Some(&state),
                event(
                    &format!("event-{}", u16::from(index) + 2),
                    RunEventBody::StepPlanned {
                        step_id: format!("step-{index}"),
                        objective: format!("objective-{index}"),
                    },
                ),
            );
        }
        (records, state)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn replay_matches_incremental_reduction_for_unique_plans(step_count in 0_u8..32) {
            let (records, incremental) = planned_journal(step_count);
            prop_assert_eq!(replay(&records), Ok(incremental));
        }

        #[test]
        fn mutation_of_any_committed_event_is_detected(
            step_count in 0_u8..32,
            selected in any::<usize>(),
        ) {
            let (mut records, _) = planned_journal(step_count);
            let index = selected % records.len();
            records[index].event.event_id.push_str("-tampered");

            let detected = matches!(
                replay(&records),
                Err(ReplayError::Record(RecordError::DigestMismatch { .. }))
            );
            prop_assert!(detected);
        }
    }
}
