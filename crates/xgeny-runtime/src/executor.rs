use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;
use thiserror::Error;
use xgeny_domain::{
    AuthState, CapabilityInstanceBody, CapabilityRef, HealthStatus, InstanceBinding,
};
use xgeny_local_store::{RunStore, StoreError};
use xgeny_workgraph::{
    EffectIntent, InvocationMaterialError, InvocationMaterialRecord, RunState, SinkGuarantee,
    StepStatus, invocation_material_digest,
};

use crate::admission::{AdmissionError, definition_contract_digest, executable_binding_digest};
use crate::material::InvocationMaterial;
use crate::runtime::{
    DurableEffectRuntime, EffectSink, ExecutionObservation, PreparedEffect, PreparedEffectBinding,
    ReconciliationObservation, RuntimeError,
};
use crate::{CapabilityRegistry, DriveReport, EventFactory, RunLease, RuntimePolicy};

const MAX_BINDING_COMPONENT_BYTES: usize = 2_048;

/// Byte-exact process-local dispatch key derived from a Capability Instance binding.
///
/// `None` and `Some` values are distinct. No URI normalization, default operation, compatible
/// protocol search, or fallback is performed.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AdapterBindingKey {
    binding_ref: String,
    operation_ref: Option<String>,
    protocol_version: Option<String>,
}

impl AdapterBindingKey {
    /// Build an exact key from one schema-validated Instance binding.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, whitespace-padded, or control-character-bearing components.
    pub fn from_binding(binding: &InstanceBinding) -> Result<Self, AdapterRegistryError> {
        validate_binding_component(&binding.binding_ref)?;
        if let Some(operation_ref) = &binding.operation_ref {
            validate_binding_component(operation_ref)?;
        }
        if let Some(protocol_version) = &binding.protocol_version {
            validate_binding_component(protocol_version)?;
        }
        Ok(Self {
            binding_ref: binding.binding_ref.clone(),
            operation_ref: binding.operation_ref.clone(),
            protocol_version: binding.protocol_version.clone(),
        })
    }
}

impl fmt::Debug for AdapterBindingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdapterBindingKey")
            .field("binding_ref", &"<redacted>")
            .field(
                "operation_ref",
                &self.operation_ref.as_ref().map(|_| "<present>"),
            )
            .field(
                "protocol_version",
                &self.protocol_version.as_ref().map(|_| "<present>"),
            )
            .finish()
    }
}

fn validate_binding_component(component: &str) -> Result<(), AdapterRegistryError> {
    if component.is_empty()
        || component.len() > MAX_BINDING_COMPONENT_BYTES
        || component.trim() != component
        || component.chars().any(char::is_control)
    {
        return Err(AdapterRegistryError::InvalidBinding);
    }
    Ok(())
}

/// SHA-256 evidence accepted from a trusted adapter.
///
/// Construction validates canonical lowercase `sha256:<64 hex>` form before a value can reach the
/// durable journal.
#[derive(Clone, PartialEq, Eq)]
pub struct AdapterEvidenceDigest(String);

impl AdapterEvidenceDigest {
    /// Validate one canonical SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Returns a fixed error that does not echo the rejected value.
    pub fn new(value: impl Into<String>) -> Result<Self, AdapterEvidenceDigestError> {
        let value = value.into();
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(AdapterEvidenceDigestError);
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(AdapterEvidenceDigestError);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for AdapterEvidenceDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("AdapterEvidenceDigest")
            .field(&self.0)
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("adapter evidence digest is not canonical SHA-256")]
pub struct AdapterEvidenceDigestError;

/// Fixed, non-sensitive failure classes for side-effect-free adapter preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterPrepareFailure {
    Unavailable,
    UnsupportedProtocol,
    InvalidMaterial,
    ResourceUnavailable,
}

/// Fixed uncertainty classes emitted only after execution may have reached the sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterExecutionUnknownReason {
    TransportOutcomeUnknown,
    AdapterTerminated,
    ResponseUnverifiable,
}

impl AdapterExecutionUnknownReason {
    const fn code(self) -> &'static str {
        match self {
            Self::TransportOutcomeUnknown => "adapter_transport_outcome_unknown",
            Self::AdapterTerminated => "adapter_terminated_after_start",
            Self::ResponseUnverifiable => "adapter_response_unverifiable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterExecutionObservation {
    Succeeded {
        receipt_digest: AdapterEvidenceDigest,
    },
    Failed {
        receipt_digest: AdapterEvidenceDigest,
    },
    Unknown {
        reason: AdapterExecutionUnknownReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterReconciliationInconclusiveReason {
    QueryUnavailable,
    ResponseUnverifiable,
    StableKeyUnsupported,
}

impl AdapterReconciliationInconclusiveReason {
    const fn code(self) -> &'static str {
        match self {
            Self::QueryUnavailable => "adapter_reconciliation_query_unavailable",
            Self::ResponseUnverifiable => "adapter_reconciliation_response_unverifiable",
            Self::StableKeyUnsupported => "adapter_reconciliation_stable_key_unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterReconciliationObservation {
    Applied {
        evidence_digest: AdapterEvidenceDigest,
    },
    NotApplied {
        evidence_digest: AdapterEvidenceDigest,
    },
    Failed {
        evidence_digest: AdapterEvidenceDigest,
    },
    Inconclusive {
        reason: AdapterReconciliationInconclusiveReason,
    },
}

/// Borrowed, core-verified material passed only to the exact registered adapter.
pub struct AdapterPrepareRequest<'a> {
    intent: &'a EffectIntent,
    instance: &'a CapabilityInstanceBody,
    normalized_arguments: &'a Value,
}

impl AdapterPrepareRequest<'_> {
    #[must_use]
    pub const fn intent(&self) -> &EffectIntent {
        self.intent
    }

    #[must_use]
    pub const fn instance(&self) -> &CapabilityInstanceBody {
        self.instance
    }

    #[must_use]
    pub const fn normalized_arguments(&self) -> &Value {
        self.normalized_arguments
    }
}

impl fmt::Debug for AdapterPrepareRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdapterPrepareRequest")
            .field("effect_id", &self.intent.effect_id)
            .field("instance_id", &self.instance.instance_id)
            .field("normalized_arguments", &"<redacted>")
            .finish()
    }
}

/// Borrowed, identity-only request for a read-only reconciliation query.
pub struct AdapterReconcileRequest<'a> {
    intent: &'a EffectIntent,
    instance: &'a CapabilityInstanceBody,
}

impl AdapterReconcileRequest<'_> {
    #[must_use]
    pub const fn intent(&self) -> &EffectIntent {
        self.intent
    }

    #[must_use]
    pub const fn instance(&self) -> &CapabilityInstanceBody {
        self.instance
    }
}

impl fmt::Debug for AdapterReconcileRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdapterReconcileRequest")
            .field("effect_id", &self.intent.effect_id)
            .field("instance_id", &self.instance.instance_id)
            .finish()
    }
}

/// One side-effecting adapter session produced by `prepare` and consumed exactly once.
///
/// The session has no identity getters: Run, Step, effect, material, and adapter identity are
/// supplied and retained by the core-owned wrapper.
pub trait PreparedAdapterInvocation {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation;
}

/// Trusted in-process adapter behavior selected only through [`EffectAdapterRegistry`].
pub trait EffectAdapter {
    /// Prepare an owned, one-shot invocation without performing the external effect.
    ///
    /// # Errors
    ///
    /// Returns only a fixed, non-sensitive preparation failure class.
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure>;

    /// Query one exact durable effect by its committed stable identity without applying it.
    fn reconcile(
        &mut self,
        request: AdapterReconcileRequest<'_>,
    ) -> AdapterReconciliationObservation;
}

/// Trusted process-local behavior registry, separate from the immutable Capability catalog.
#[derive(Default)]
pub struct EffectAdapterRegistry {
    adapters: BTreeMap<AdapterBindingKey, Box<dyn EffectAdapter>>,
}

impl EffectAdapterRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            adapters: BTreeMap::new(),
        }
    }

    /// Register one adapter under an exact full Instance binding.
    ///
    /// # Errors
    ///
    /// Rejects an invalid or duplicate key without replacing an existing adapter.
    pub fn register<A>(
        &mut self,
        binding: &InstanceBinding,
        adapter: A,
    ) -> Result<(), AdapterRegistryError>
    where
        A: EffectAdapter + 'static,
    {
        let key = AdapterBindingKey::from_binding(binding)?;
        if self.adapters.contains_key(&key) {
            return Err(AdapterRegistryError::DuplicateBinding);
        }
        self.adapters.insert(key, Box::new(adapter));
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    fn adapter_mut(&mut self, key: &AdapterBindingKey) -> Option<&mut (dyn EffectAdapter + '_)> {
        match self.adapters.get_mut(key) {
            Some(adapter) => Some(adapter.as_mut()),
            None => None,
        }
    }
}

impl fmt::Debug for EffectAdapterRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectAdapterRegistry")
            .field("adapter_count", &self.adapters.len())
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AdapterRegistryError {
    #[error("adapter binding is invalid")]
    InvalidBinding,
    #[error("adapter binding is already registered")]
    DuplicateBinding,
}

struct CorePreparedEffect {
    binding: PreparedEffectBinding,
    adapter_key: AdapterBindingKey,
    invocation: Box<dyn PreparedAdapterInvocation>,
}

impl PreparedEffect for CorePreparedEffect {
    fn binding(&self) -> &PreparedEffectBinding {
        &self.binding
    }
}

impl fmt::Debug for CorePreparedEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CorePreparedEffect")
            .field("binding", &self.binding)
            .field("adapter_key", &self.adapter_key)
            .field("invocation", &"<opaque/consume-once>")
            .finish()
    }
}

struct DirectSink<'a> {
    adapter: Option<&'a mut dyn EffectAdapter>,
    reconciliation_instance: Option<CapabilityInstanceBody>,
}

impl EffectSink for DirectSink<'_> {
    type Prepared = CorePreparedEffect;

    fn execute(
        &mut self,
        _intent: &EffectIntent,
        prepared: Self::Prepared,
    ) -> ExecutionObservation {
        match prepared.invocation.execute() {
            AdapterExecutionObservation::Succeeded { receipt_digest } => {
                ExecutionObservation::Succeeded {
                    receipt_digest: receipt_digest.into_string(),
                }
            }
            AdapterExecutionObservation::Failed { receipt_digest } => {
                ExecutionObservation::Failed {
                    receipt_digest: receipt_digest.into_string(),
                }
            }
            AdapterExecutionObservation::Unknown { reason } => ExecutionObservation::Unknown {
                reason: reason.code().to_owned(),
            },
        }
    }

    fn reconcile(&mut self, intent: &EffectIntent) -> ReconciliationObservation {
        let adapter = self
            .adapter
            .as_deref_mut()
            .expect("DirectExecutor supplies an adapter before reconciliation");
        let instance = self
            .reconciliation_instance
            .as_ref()
            .expect("DirectExecutor supplies an Instance before reconciliation");
        match adapter.reconcile(AdapterReconcileRequest { intent, instance }) {
            AdapterReconciliationObservation::Applied { evidence_digest } => {
                ReconciliationObservation::Applied {
                    evidence_digest: evidence_digest.into_string(),
                }
            }
            AdapterReconciliationObservation::NotApplied { evidence_digest } => {
                ReconciliationObservation::NotApplied {
                    evidence_digest: evidence_digest.into_string(),
                }
            }
            AdapterReconciliationObservation::Failed { evidence_digest } => {
                ReconciliationObservation::Failed {
                    evidence_digest: evidence_digest.into_string(),
                }
            }
            AdapterReconciliationObservation::Inconclusive { reason } => {
                ReconciliationObservation::Inconclusive {
                    reason: reason.code().to_owned(),
                }
            }
        }
    }
}

/// Core coordinator from verified material to one exact process-local adapter.
#[derive(Debug, Clone, Copy)]
pub struct DirectExecutor {
    policy: RuntimePolicy,
}

impl DirectExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            policy: RuntimePolicy::default(),
        }
    }

    #[must_use]
    pub const fn with_policy(mut self, policy: RuntimePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Advance one Step through exact dispatch and the durable effect state machine.
    ///
    /// `IntentCommitted` requires core-verified material. The adapter's `prepare` is called before
    /// the durable start marker and must be side-effect free. Its one-shot session is consumed only
    /// after `EffectExecutionStarted` commits. Invocation material is borrowed so a pre-start
    /// failure can create a fresh session on retry. Resuming `Executing` never prepares or executes.
    ///
    /// # Errors
    ///
    /// Fails closed for lease/state/material/catalog drift, credential-bearing Instances, unhealthy
    /// Instances, missing exact adapters, fixed preparation failures, or durable runtime errors.
    #[allow(clippy::too_many_arguments)]
    pub fn drive_step<S, F, L>(
        &self,
        store: &mut S,
        events: &mut F,
        lease: &L,
        capabilities: &CapabilityRegistry,
        adapters: &mut EffectAdapterRegistry,
        step_id: &str,
        material: Option<&InvocationMaterial>,
    ) -> Result<DriveReport, DirectExecutorError>
    where
        S: RunStore,
        F: EventFactory,
        L: RunLease,
    {
        let snapshot = store
            .load()?
            .ok_or(DirectExecutorError::RunNotInitialized)?;
        verify_lease(lease, &snapshot.state)?;
        let step = snapshot
            .state
            .steps
            .get(step_id)
            .ok_or_else(|| DirectExecutorError::StepNotFound(step_id.to_owned()))?;

        match step.status {
            StepStatus::IntentCommitted => {
                if step.attempts >= self.policy.max_execution_attempts() {
                    return Err(DirectExecutorError::Runtime(
                        RuntimeError::ExecutionAttemptLimitReached {
                            step_id: step_id.to_owned(),
                            attempts: step.attempts,
                            maximum: self.policy.max_execution_attempts(),
                        },
                    ));
                }
                let material = material.ok_or_else(|| DirectExecutorError::MaterialRequired {
                    step_id: step_id.to_owned(),
                })?;
                let intent = step
                    .intent
                    .as_ref()
                    .ok_or_else(|| DirectExecutorError::IntentMissing(step_id.to_owned()))?;
                let (record, arguments) = material.parts();
                verify_material(store, &snapshot.state, step_id, intent, record, arguments)?;
                let instance = verify_current_instance(capabilities, intent)?;
                verify_dynamic_execution_gate(instance)?;
                let adapter_key = AdapterBindingKey::from_binding(&instance.binding)?;
                let adapter = adapters.adapter_mut(&adapter_key).ok_or_else(|| {
                    DirectExecutorError::AdapterNotRegistered {
                        instance_id: instance.instance_id.clone(),
                    }
                })?;
                let invocation = adapter
                    .prepare(AdapterPrepareRequest {
                        intent,
                        instance,
                        normalized_arguments: arguments,
                    })
                    .map_err(DirectExecutorError::AdapterPrepare)?;
                let prepared = CorePreparedEffect {
                    binding: PreparedEffectBinding::from_verified(
                        &snapshot.state,
                        step_id,
                        intent,
                        record.clone(),
                    ),
                    adapter_key,
                    invocation,
                };
                let mut sink = DirectSink {
                    adapter: None,
                    reconciliation_instance: None,
                };
                DurableEffectRuntime::new(store, &mut sink, events, lease)
                    .with_policy(self.policy)
                    .drive_step(step_id, Some(prepared))
                    .map_err(DirectExecutorError::from)
            }
            StepStatus::EffectUnknown | StepStatus::Reconciling => {
                let intent = step
                    .intent
                    .as_ref()
                    .ok_or_else(|| DirectExecutorError::IntentMissing(step_id.to_owned()))?;
                if !supports_query(intent.sink_guarantee) {
                    return drive_without_adapter(self.policy, store, events, lease, step_id);
                }
                let instance = verify_current_instance(capabilities, intent)?.clone();
                verify_dynamic_execution_gate(&instance)?;
                let key = AdapterBindingKey::from_binding(&instance.binding)?;
                let adapter = adapters.adapter_mut(&key).ok_or_else(|| {
                    DirectExecutorError::AdapterNotRegistered {
                        instance_id: instance.instance_id.clone(),
                    }
                })?;
                let mut sink = DirectSink {
                    adapter: Some(adapter),
                    reconciliation_instance: Some(instance),
                };
                DurableEffectRuntime::new(store, &mut sink, events, lease)
                    .with_policy(self.policy)
                    .drive_step(step_id, None)
                    .map_err(DirectExecutorError::from)
            }
            StepStatus::Executing
            | StepStatus::Planned
            | StepStatus::Validating
            | StepStatus::Completed
            | StepStatus::Failed
            | StepStatus::ManualRequired => {
                drive_without_adapter(self.policy, store, events, lease, step_id)
            }
        }
    }
}

impl Default for DirectExecutor {
    fn default() -> Self {
        Self::new()
    }
}

fn drive_without_adapter<S, F, L>(
    policy: RuntimePolicy,
    store: &mut S,
    events: &mut F,
    lease: &L,
    step_id: &str,
) -> Result<DriveReport, DirectExecutorError>
where
    S: RunStore,
    F: EventFactory,
    L: RunLease,
{
    let mut sink = DirectSink {
        adapter: None,
        reconciliation_instance: None,
    };
    DurableEffectRuntime::new(store, &mut sink, events, lease)
        .with_policy(policy)
        .drive_step(step_id, None)
        .map_err(DirectExecutorError::from)
}

fn verify_material<S: RunStore>(
    store: &S,
    state: &RunState,
    step_id: &str,
    intent: &EffectIntent,
    record: &InvocationMaterialRecord,
    arguments: &Value,
) -> Result<(), DirectExecutorError> {
    record.verify_for(&state.run_id, step_id, intent)?;
    let durable_record = store
        .load_invocation_material(&intent.effect_id)?
        .ok_or_else(|| DirectExecutorError::MaterialRecordMissing {
            effect_id: intent.effect_id.clone(),
        })?;
    if durable_record != *record {
        return Err(DirectExecutorError::MaterialRecordChanged);
    }
    if invocation_material_digest(arguments)? != record.material_digest() {
        return Err(DirectExecutorError::MaterialDigestMismatch);
    }
    Ok(())
}

fn verify_current_instance<'a>(
    registry: &'a CapabilityRegistry,
    intent: &EffectIntent,
) -> Result<&'a CapabilityInstanceBody, DirectExecutorError> {
    let capability = CapabilityRef {
        capability_id: intent.invocation.capability_id.clone(),
        contract_version: intent.invocation.contract_version.clone(),
    };
    let definition = registry.definition(&capability).ok_or_else(|| {
        DirectExecutorError::DefinitionNotFound {
            capability_id: capability.capability_id.clone(),
            contract_version: capability.contract_version.clone(),
        }
    })?;
    if definition_contract_digest(definition)? != intent.invocation.definition_digest {
        return Err(DirectExecutorError::DefinitionChanged);
    }
    let instance = registry
        .instance(&intent.invocation.instance_id)
        .ok_or_else(|| DirectExecutorError::InstanceNotFound {
            instance_id: intent.invocation.instance_id.clone(),
        })?;
    if instance.definition != capability
        || executable_binding_digest(instance)? != intent.invocation.instance_binding_digest
    {
        return Err(DirectExecutorError::InstanceBindingChanged);
    }
    Ok(instance)
}

fn verify_dynamic_execution_gate(
    instance: &CapabilityInstanceBody,
) -> Result<(), DirectExecutorError> {
    if instance.health.status != HealthStatus::Available {
        return Err(DirectExecutorError::InstanceNotAvailable {
            status: instance.health.status,
        });
    }
    if instance.auth.state != AuthState::NotRequired || instance.auth.auth_ref.is_some() {
        return Err(DirectExecutorError::CredentialWitnessUnavailable);
    }
    Ok(())
}

fn verify_lease<L: RunLease>(lease: &L, state: &RunState) -> Result<(), DirectExecutorError> {
    if lease.run_id() != state.run_id {
        return Err(DirectExecutorError::LeaseRunMismatch {
            lease_run_id: lease.run_id().to_owned(),
            state_run_id: state.run_id.clone(),
        });
    }
    Ok(())
}

const fn supports_query(guarantee: SinkGuarantee) -> bool {
    matches!(
        guarantee,
        SinkGuarantee::QueryByKey | SinkGuarantee::DeduplicateAndQuery
    )
}

#[derive(Debug, Error)]
pub enum DirectExecutorError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error(transparent)]
    MaterialRecord(#[from] InvocationMaterialError),
    #[error(transparent)]
    AdapterRegistry(#[from] AdapterRegistryError),
    #[error("durable Run is not initialized")]
    RunNotInitialized,
    #[error("step `{0}` does not exist")]
    StepNotFound(String),
    #[error("step `{0}` has an effect status but no committed intent")]
    IntentMissing(String),
    #[error("step `{step_id}` requires core-verified invocation material")]
    MaterialRequired { step_id: String },
    #[error("effect `{effect_id}` has no invocation material descriptor")]
    MaterialRecordMissing { effect_id: String },
    #[error("invocation material changed after verification")]
    MaterialRecordChanged,
    #[error("invocation material does not match its committed digest")]
    MaterialDigestMismatch,
    #[error("Capability Definition `{capability_id}` version `{contract_version}` is missing")]
    DefinitionNotFound {
        capability_id: String,
        contract_version: String,
    },
    #[error("Capability Definition changed after invocation admission")]
    DefinitionChanged,
    #[error("Capability Instance `{instance_id}` is missing")]
    InstanceNotFound { instance_id: String },
    #[error("Capability Instance binding changed after invocation admission")]
    InstanceBindingChanged,
    #[error("Capability Instance is not currently available ({status:?})")]
    InstanceNotAvailable { status: HealthStatus },
    #[error("credential-bearing Capability Instances require a committed credential witness")]
    CredentialWitnessUnavailable,
    #[error("the exact Capability Instance adapter is not registered")]
    AdapterNotRegistered { instance_id: String },
    #[error("adapter preparation failed with {0:?}")]
    AdapterPrepare(AdapterPrepareFailure),
    #[error("lease is for Run `{lease_run_id}`, but durable state is `{state_run_id}`")]
    LeaseRunMismatch {
        lease_run_id: String,
        state_run_id: String,
    },
}
