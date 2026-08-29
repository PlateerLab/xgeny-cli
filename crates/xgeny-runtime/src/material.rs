use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;
use thiserror::Error;
use xgeny_domain::{CapabilityRef, GrantLifetime};
use xgeny_local_store::{Commit, ExpectedHead, RunStore, StoreError};
use xgeny_policy::{InvocationResolutionError, PermissionRequestResolver, ResourceResolver};
use xgeny_workgraph::{
    InvocationMaterialRecord, InvocationMaterialRetention, InvocationMaterialUnavailableReason,
    RunEvent, RunEventBody, StepStatus, invocation_material_digest,
};

use crate::admission::{
    AdmissionError, definition_contract_digest, executable_binding_digest, semantic_action_digest,
    validate_arguments,
};
use crate::{CapabilityRegistry, EventFactory, EventFactoryError, RunLease};

/// Fixed, non-sensitive failure classes returned by a trusted material provider.
///
/// Provider-specific messages are intentionally excluded so paths, arguments, or credentials
/// cannot flow into runtime errors or journal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterialProviderFailure {
    Unavailable,
    NotFound,
    RevisionChanged,
    UnsupportedVersion,
}

/// Trusted source for a secret-free, version-pinned reconstruction recipe.
///
/// `reconstruct` must not perform the external effect. It only rebuilds invocation arguments for
/// core validation. Raw credentials remain behind their typed references and are resolved later by
/// the selected adapter.
pub trait InvocationMaterialProvider {
    /// Rebuild candidate invocation arguments for one exact recipe revision.
    ///
    /// # Errors
    ///
    /// Returns only a fixed non-sensitive failure class. Provider-specific error text must remain
    /// inside the trusted provider boundary.
    fn reconstruct(
        &mut self,
        reference_id: &str,
        revision: &str,
    ) -> Result<Value, MaterialProviderFailure>;
}

/// Trusted process-local catalog for restart reconstruction providers.
///
/// Provider identity belongs to the composition root, not to provider implementations. Recovery
/// performs one byte-exact lookup using the identifier committed in the material record and never
/// falls back to another provider.
#[derive(Default)]
pub struct MaterialProviderRegistry {
    providers: BTreeMap<String, Box<dyn InvocationMaterialProvider>>,
}

impl MaterialProviderRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            providers: BTreeMap::new(),
        }
    }

    /// Register a trusted provider under one stable identifier.
    ///
    /// # Errors
    ///
    /// Rejects an invalid or duplicate identifier without replacing the existing provider.
    pub fn register<P>(
        &mut self,
        provider_id: impl Into<String>,
        provider: P,
    ) -> Result<(), MaterialProviderRegistryError>
    where
        P: InvocationMaterialProvider + 'static,
    {
        let provider_id = provider_id.into();
        validate_provider_id(&provider_id)?;
        if self.providers.contains_key(&provider_id) {
            return Err(MaterialProviderRegistryError::DuplicateProvider);
        }
        self.providers.insert(provider_id, Box::new(provider));
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    fn provider_mut(
        &mut self,
        provider_id: &str,
    ) -> Result<&mut (dyn InvocationMaterialProvider + '_), MaterialRecoveryError> {
        match self.providers.get_mut(provider_id) {
            Some(provider) => Ok(provider.as_mut()),
            None => Err(MaterialRecoveryError::ProviderNotRegistered),
        }
    }
}

impl fmt::Debug for MaterialProviderRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterialProviderRegistry")
            .field("provider_count", &self.providers.len())
            .finish()
    }
}

fn validate_provider_id(provider_id: &str) -> Result<(), MaterialProviderRegistryError> {
    const MAX_PROVIDER_ID_BYTES: usize = 128;
    if provider_id.is_empty()
        || matches!(provider_id, "." | "..")
        || provider_id.len() > MAX_PROVIDER_ID_BYTES
        || !provider_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(MaterialProviderRegistryError::InvalidProviderId);
    }
    Ok(())
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum MaterialProviderRegistryError {
    #[error("material provider identifier is invalid")]
    InvalidProviderId,
    #[error("material provider identifier is already registered")]
    DuplicateProvider,
}

/// Typed core-verified invocation arguments bound to one committed effect intent.
///
/// This value cannot be deserialized or cloned as a whole. Direct Executor borrows its arguments
/// only inside the crate, while Debug output redacts arguments and the opaque reference. External
/// adapters receive only a borrowed, redacted prepare request.
#[must_use = "verified invocation material must be prepared by the selected adapter or discarded"]
pub struct InvocationMaterial {
    record: InvocationMaterialRecord,
    normalized_arguments: Value,
}

impl InvocationMaterial {
    #[must_use]
    pub const fn record(&self) -> &InvocationMaterialRecord {
        &self.record
    }

    pub(crate) const fn parts(&self) -> (&InvocationMaterialRecord, &Value) {
        (&self.record, &self.normalized_arguments)
    }
}

impl fmt::Debug for InvocationMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationMaterial")
            .field("record", &self.record)
            .field("normalized_arguments", &"<redacted>")
            .finish()
    }
}

impl crate::AdmittedEffect {
    /// Consume same-process admitted arguments as a core-bound ephemeral material value.
    ///
    /// # Errors
    ///
    /// Returns an error if the admitted value does not carry the expected ephemeral record.
    pub fn into_ephemeral_material(self) -> Result<InvocationMaterial, MaterialRecoveryError> {
        if !matches!(
            self.material_record().retention(),
            InvocationMaterialRetention::Ephemeral
        ) {
            return Err(MaterialRecoveryError::NotEphemeral);
        }
        let actual = invocation_material_digest(self.normalized_arguments())?;
        if actual != self.material_record().material_digest() {
            return Err(MaterialRecoveryError::MaterialDigestMismatch);
        }
        Ok(InvocationMaterial {
            record: self.material_record().clone(),
            normalized_arguments: self.normalized_arguments().clone(),
        })
    }
}

/// Reconstruct and revalidate invocation material after process restart.
#[derive(Debug, Default, Clone, Copy)]
pub struct InvocationMaterialRecovery;

impl InvocationMaterialRecovery {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Recover exact canonical arguments for an `IntentCommitted` Step.
    ///
    /// Definition, Instance, schema, size, resource resolution, payload digest, and semantic action
    /// identity are all rechecked before material is returned. No effect or adapter preparation is
    /// performed by this method.
    ///
    /// # Errors
    ///
    /// Fails closed for a missing/stale Run, wrong lease, non-reconstructable material, wrong
    /// provider, provider failure, Definition/Instance drift, invalid arguments, or digest drift.
    pub fn recover<S, R, L>(
        &self,
        store: &S,
        lease: &L,
        registry: &CapabilityRegistry,
        resolver: &R,
        providers: &mut MaterialProviderRegistry,
        step_id: &str,
    ) -> Result<InvocationMaterial, MaterialRecoveryError>
    where
        S: RunStore,
        R: ResourceResolver,
        L: RunLease,
    {
        let snapshot = store
            .load()?
            .ok_or(MaterialRecoveryError::RunNotInitialized)?;
        verify_lease(lease, &snapshot.state.run_id)?;
        let step = snapshot
            .state
            .steps
            .get(step_id)
            .ok_or_else(|| MaterialRecoveryError::StepNotFound(step_id.to_owned()))?;
        if step.status != StepStatus::IntentCommitted {
            return Err(MaterialRecoveryError::StepNotIntentCommitted {
                step_id: step_id.to_owned(),
                actual: step.status,
            });
        }
        let intent = step
            .intent
            .as_ref()
            .ok_or_else(|| MaterialRecoveryError::IntentMissing(step_id.to_owned()))?;
        let record = store
            .load_invocation_material(&intent.effect_id)?
            .ok_or_else(|| MaterialRecoveryError::MaterialRecordMissing {
                effect_id: intent.effect_id.clone(),
            })?;
        record.verify_for(&snapshot.state.run_id, step_id, intent)?;

        let InvocationMaterialRetention::ReconstructableReference(reference) = record.retention()
        else {
            return Err(MaterialRecoveryError::EphemeralMaterialUnavailable);
        };
        let capability = CapabilityRef {
            capability_id: intent.invocation.capability_id.clone(),
            contract_version: intent.invocation.contract_version.clone(),
        };
        let definition = registry.definition(&capability).ok_or_else(|| {
            MaterialRecoveryError::DefinitionNotFound {
                capability_id: capability.capability_id.clone(),
                contract_version: capability.contract_version.clone(),
            }
        })?;
        if definition_contract_digest(definition)? != intent.invocation.definition_digest {
            return Err(MaterialRecoveryError::DefinitionChanged);
        }
        let instance = registry
            .instance(&intent.invocation.instance_id)
            .ok_or_else(|| MaterialRecoveryError::InstanceNotFound {
                instance_id: intent.invocation.instance_id.clone(),
            })?;
        if instance.definition != capability
            || executable_binding_digest(instance)? != intent.invocation.instance_binding_digest
        {
            return Err(MaterialRecoveryError::InstanceBindingChanged);
        }

        let reconstructed = providers
            .provider_mut(reference.provider_id())?
            .reconstruct(reference.reference_id(), reference.revision())
            .map_err(MaterialRecoveryError::Provider)?;
        validate_arguments(definition, &reconstructed)?;
        let request_id = format!("material-recovery-{}", record.material_id());
        let recovered_request = PermissionRequestResolver::new(resolver).resolve_invocation(
            &request_id,
            &snapshot.state.run_id,
            step_id,
            definition,
            &reconstructed,
            GrantLifetime::Once,
        )?;
        validate_arguments(definition, recovered_request.normalized_arguments())?;
        if invocation_material_digest(recovered_request.normalized_arguments())?
            != record.material_digest()
        {
            return Err(MaterialRecoveryError::MaterialDigestMismatch);
        }
        let action_digest = semantic_action_digest(
            &capability,
            &intent.invocation.definition_digest,
            definition.spec.effect.class,
            recovered_request.normalized_arguments(),
            recovered_request.permission_request(),
        )?;
        if action_digest != intent.action_digest || action_digest != record.action_digest() {
            return Err(MaterialRecoveryError::ActionDigestMismatch);
        }

        Ok(InvocationMaterial {
            record,
            normalized_arguments: recovered_request.normalized_arguments().clone(),
        })
    }

    /// Durably close an intent whose material cannot be recovered.
    ///
    /// This is explicit because a temporarily unavailable provider must not be confused with
    /// permanent loss. The fixed reason enum prevents provider/OS error text from entering the
    /// journal.
    ///
    /// # Errors
    ///
    /// Returns an error for missing Run/Step/intent, wrong lease, invalid lifecycle transition,
    /// event creation, or storage failure.
    pub fn mark_unavailable<S, F, L>(
        &self,
        store: &mut S,
        events: &mut F,
        lease: &L,
        step_id: &str,
        reason: InvocationMaterialUnavailableReason,
    ) -> Result<Commit, MaterialRecoveryError>
    where
        S: RunStore,
        F: EventFactory,
        L: RunLease,
    {
        let snapshot = store
            .load()?
            .ok_or(MaterialRecoveryError::RunNotInitialized)?;
        verify_lease(lease, &snapshot.state.run_id)?;
        let step = snapshot
            .state
            .steps
            .get(step_id)
            .ok_or_else(|| MaterialRecoveryError::StepNotFound(step_id.to_owned()))?;
        let intent = step
            .intent
            .as_ref()
            .ok_or_else(|| MaterialRecoveryError::IntentMissing(step_id.to_owned()))?;
        let metadata = events.create_metadata(&snapshot.state)?;
        store
            .append(
                ExpectedHead::from_state(&snapshot.state),
                RunEvent {
                    event_id: metadata.event_id,
                    run_id: snapshot.state.run_id.clone(),
                    authority: snapshot.state.authority.clone(),
                    authority_epoch: snapshot.state.authority_epoch,
                    recorded_at: metadata.recorded_at,
                    body: RunEventBody::InvocationMaterialUnavailable {
                        step_id: step_id.to_owned(),
                        effect_id: intent.effect_id.clone(),
                        reason,
                    },
                },
            )
            .map_err(MaterialRecoveryError::from)
    }
}

fn verify_lease<L: RunLease>(lease: &L, run_id: &str) -> Result<(), MaterialRecoveryError> {
    if lease.run_id() != run_id {
        return Err(MaterialRecoveryError::LeaseRunMismatch {
            lease_run_id: lease.run_id().to_owned(),
            state_run_id: run_id.to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum MaterialRecoveryError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    EventFactory(#[from] EventFactoryError),
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error(transparent)]
    InvocationResolution(#[from] InvocationResolutionError),
    #[error(transparent)]
    MaterialRecord(#[from] xgeny_workgraph::InvocationMaterialError),
    #[error("durable Run is not initialized")]
    RunNotInitialized,
    #[error("step `{0}` does not exist")]
    StepNotFound(String),
    #[error("step `{0}` has no committed effect intent")]
    IntentMissing(String),
    #[error("step `{step_id}` cannot recover material from status {actual:?}")]
    StepNotIntentCommitted { step_id: String, actual: StepStatus },
    #[error("effect `{effect_id}` has no invocation material descriptor")]
    MaterialRecordMissing { effect_id: String },
    #[error("ephemeral invocation material is unavailable after process loss")]
    EphemeralMaterialUnavailable,
    #[error("admitted invocation material is not ephemeral")]
    NotEphemeral,
    #[error("the committed invocation material provider is not registered")]
    ProviderNotRegistered,
    #[error("invocation material provider failed with {0:?}")]
    Provider(MaterialProviderFailure),
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
    #[error("reconstructed invocation material does not match its committed digest")]
    MaterialDigestMismatch,
    #[error("reconstructed invocation material does not match the committed semantic action")]
    ActionDigestMismatch,
    #[error("lease is for Run `{lease_run_id}`, but durable state is `{state_run_id}`")]
    LeaseRunMismatch {
        lease_run_id: String,
        state_run_id: String,
    },
}
