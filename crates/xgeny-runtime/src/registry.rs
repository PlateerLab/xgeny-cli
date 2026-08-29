use std::collections::BTreeMap;

use thiserror::Error;
use xgeny_domain::{
    API_VERSION_V1ALPHA1, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef,
    ExecutionStyle,
};

/// Deterministic in-memory catalog of schema-conformant Capability documents.
///
/// The Registry preserves all platform, health, auth, trust, and placement states. It does not
/// rank or filter executable candidates; those decisions belong to the Router and Permission
/// Broker. Documents arriving from wire or configuration boundaries must pass protocol schema
/// validation before registration.
#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    definitions: BTreeMap<DefinitionKey, CapabilityDefinitionBody>,
    instances: BTreeMap<String, CapabilityInstanceBody>,
}

impl CapabilityRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            definitions: BTreeMap::new(),
            instances: BTreeMap::new(),
        }
    }

    /// Register one schema-validated immutable semantic Capability contract.
    ///
    /// This method intentionally does not compile or apply the JSON Schema. Callers may use it
    /// only after the protocol ingress boundary has validated the document.
    ///
    /// # Errors
    ///
    /// Rejects an unsupported API version, invalid timeout range, or duplicate exact
    /// `(capability ID, contract version)` key. An error never replaces an existing entry.
    pub fn register_schema_validated_definition(
        &mut self,
        definition: CapabilityDefinitionBody,
    ) -> Result<(), RegistryError> {
        validate_definition(&definition)?;
        let key = DefinitionKey::from_definition(&definition);
        if self.definitions.contains_key(&key) {
            return Err(RegistryError::DuplicateDefinition {
                capability_id: key.capability_id,
                contract_version: key.contract_version,
            });
        }
        self.definitions.insert(key, definition);
        Ok(())
    }

    /// Bind one schema-validated executable Instance to an exact registered Definition version.
    ///
    /// This method intentionally does not apply the JSON Schema or decide whether required
    /// extensions are supported. Those checks belong to protocol ingress and Router filtering.
    ///
    /// # Errors
    ///
    /// Rejects an unsupported API version, duplicate global Instance ID, missing exact
    /// Definition, or an Instance feature claim stronger than its Definition contract. An error
    /// never mutates the catalog.
    pub fn register_schema_validated_instance(
        &mut self,
        instance: CapabilityInstanceBody,
    ) -> Result<(), RegistryError> {
        if instance.api_version != API_VERSION_V1ALPHA1 {
            return Err(RegistryError::UnsupportedInstanceApiVersion {
                instance_id: instance.instance_id,
                actual: instance.api_version,
            });
        }
        if self.instances.contains_key(&instance.instance_id) {
            return Err(RegistryError::DuplicateInstance {
                instance_id: instance.instance_id,
            });
        }
        let key = DefinitionKey::from_reference(&instance.definition);
        let definition =
            self.definitions
                .get(&key)
                .ok_or_else(|| RegistryError::DefinitionNotFound {
                    instance_id: instance.instance_id.clone(),
                    capability_id: key.capability_id.clone(),
                    contract_version: key.contract_version.clone(),
                })?;
        validate_instance_features(&instance, definition)?;
        self.instances
            .insert(instance.instance_id.clone(), instance);
        Ok(())
    }

    #[must_use]
    pub fn definition(&self, reference: &CapabilityRef) -> Option<&CapabilityDefinitionBody> {
        self.definitions
            .get(&DefinitionKey::from_reference(reference))
    }

    #[must_use]
    pub fn instance(&self, instance_id: &str) -> Option<&CapabilityInstanceBody> {
        self.instances.get(instance_id)
    }

    /// Iterate Definitions by capability ID and then exact contract-version string.
    pub fn definitions(&self) -> impl Iterator<Item = &CapabilityDefinitionBody> {
        self.definitions.values()
    }

    /// Iterate Instances in global Instance-ID order.
    pub fn instances(&self) -> impl Iterator<Item = &CapabilityInstanceBody> {
        self.instances.values()
    }

    /// Iterate Instances bound to one exact Definition version in Instance-ID order.
    pub fn instances_for(
        &self,
        reference: &CapabilityRef,
    ) -> impl Iterator<Item = &CapabilityInstanceBody> {
        let key = DefinitionKey::from_reference(reference);
        self.instances
            .values()
            .filter(move |instance| DefinitionKey::from_reference(&instance.definition) == key)
    }

    #[must_use]
    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    #[must_use]
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }
}

fn validate_definition(definition: &CapabilityDefinitionBody) -> Result<(), RegistryError> {
    if definition.api_version != API_VERSION_V1ALPHA1 {
        return Err(RegistryError::UnsupportedDefinitionApiVersion {
            capability_id: definition.metadata.id.clone(),
            actual: definition.api_version.clone(),
        });
    }
    let execution = &definition.spec.execution;
    if execution.default_timeout_ms == 0
        || execution.max_timeout_ms == 0
        || execution.default_timeout_ms > execution.max_timeout_ms
    {
        return Err(RegistryError::InvalidTimeoutRange {
            capability_id: definition.metadata.id.clone(),
            default_timeout_ms: execution.default_timeout_ms,
            max_timeout_ms: execution.max_timeout_ms,
        });
    }
    Ok(())
}

fn validate_instance_features(
    instance: &CapabilityInstanceBody,
    definition: &CapabilityDefinitionBody,
) -> Result<(), RegistryError> {
    if instance.features.sync
        && !definition
            .spec
            .execution
            .styles
            .contains(&ExecutionStyle::Sync)
    {
        return Err(unsupported_style(instance, ExecutionStyle::Sync));
    }
    if instance.features.task
        && !definition
            .spec
            .execution
            .styles
            .contains(&ExecutionStyle::Task)
    {
        return Err(unsupported_style(instance, ExecutionStyle::Task));
    }
    if instance.features.cancellable && !definition.spec.execution.cancellable {
        return Err(RegistryError::UnsupportedCancellation {
            instance_id: instance.instance_id.clone(),
            capability_id: definition.metadata.id.clone(),
            contract_version: definition.metadata.contract_version.clone(),
        });
    }
    Ok(())
}

fn unsupported_style(instance: &CapabilityInstanceBody, style: ExecutionStyle) -> RegistryError {
    RegistryError::UnsupportedExecutionStyle {
        instance_id: instance.instance_id.clone(),
        capability_id: instance.definition.capability_id.clone(),
        contract_version: instance.definition.contract_version.clone(),
        style,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DefinitionKey {
    capability_id: String,
    contract_version: String,
}

impl DefinitionKey {
    fn from_definition(definition: &CapabilityDefinitionBody) -> Self {
        Self {
            capability_id: definition.metadata.id.clone(),
            contract_version: definition.metadata.contract_version.clone(),
        }
    }

    fn from_reference(reference: &CapabilityRef) -> Self {
        Self {
            capability_id: reference.capability_id.clone(),
            contract_version: reference.contract_version.clone(),
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RegistryError {
    #[error("Capability Definition `{capability_id}` uses unsupported API version `{actual}`")]
    UnsupportedDefinitionApiVersion {
        capability_id: String,
        actual: String,
    },
    #[error("Capability Instance `{instance_id}` uses unsupported API version `{actual}`")]
    UnsupportedInstanceApiVersion { instance_id: String, actual: String },
    #[error(
        "Capability Definition `{capability_id}` has invalid timeouts: default {default_timeout_ms}ms, maximum {max_timeout_ms}ms"
    )]
    InvalidTimeoutRange {
        capability_id: String,
        default_timeout_ms: u64,
        max_timeout_ms: u64,
    },
    #[error(
        "Capability Definition `{capability_id}` contract version `{contract_version}` is already registered"
    )]
    DuplicateDefinition {
        capability_id: String,
        contract_version: String,
    },
    #[error("Capability Instance `{instance_id}` is already registered")]
    DuplicateInstance { instance_id: String },
    #[error(
        "Capability Instance `{instance_id}` references missing Definition `{capability_id}` contract version `{contract_version}`"
    )]
    DefinitionNotFound {
        instance_id: String,
        capability_id: String,
        contract_version: String,
    },
    #[error(
        "Capability Instance `{instance_id}` claims {style:?}, but Definition `{capability_id}` contract version `{contract_version}` does not"
    )]
    UnsupportedExecutionStyle {
        instance_id: String,
        capability_id: String,
        contract_version: String,
        style: ExecutionStyle,
    },
    #[error(
        "Capability Instance `{instance_id}` claims cancellation, but Definition `{capability_id}` contract version `{contract_version}` does not"
    )]
    UnsupportedCancellation {
        instance_id: String,
        capability_id: String,
        contract_version: String,
    },
}
