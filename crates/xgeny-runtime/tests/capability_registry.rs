use xgeny_domain::{
    API_VERSION_V1ALPHA1, Architecture, AuthState, CapabilityDefinitionBody,
    CapabilityInstanceBody, CapabilityRef, CapabilitySource, ExecutionStyle, HealthStatus,
    OperatingSystem, Placement, ProtocolDocument,
};
use xgeny_runtime::{CapabilityRegistry, RegistryError};

fn definition() -> CapabilityDefinitionBody {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
    ))
    .expect("bundled definition fixture should deserialize");
    match document {
        ProtocolDocument::CapabilityDefinition(body) => *body,
        other => panic!("expected a capability definition, got {other:?}"),
    }
}

fn instance() -> CapabilityInstanceBody {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-instance.local-fs.json"
    ))
    .expect("bundled instance fixture should deserialize");
    match document {
        ProtocolDocument::CapabilityInstance(body) => *body,
        other => panic!("expected a capability instance, got {other:?}"),
    }
}

fn capability_ref(definition: &CapabilityDefinitionBody) -> CapabilityRef {
    CapabilityRef {
        capability_id: definition.metadata.id.clone(),
        contract_version: definition.metadata.contract_version.clone(),
    }
}

fn renamed_definition(id: &str, contract_version: &str) -> CapabilityDefinitionBody {
    let mut definition = definition();
    id.clone_into(&mut definition.metadata.id);
    contract_version.clone_into(&mut definition.metadata.contract_version);
    definition
}

fn bound_instance(
    instance_id: &str,
    definition: &CapabilityDefinitionBody,
) -> CapabilityInstanceBody {
    let mut instance = instance();
    instance_id.clone_into(&mut instance.instance_id);
    instance.definition = capability_ref(definition);
    instance.binding.binding_ref = format!("registry://test/{instance_id}");
    instance
}

#[test]
fn fake_instance_resolves_only_its_exact_definition() {
    let mut registry = CapabilityRegistry::new();
    let definition = definition();
    let reference = capability_ref(&definition);
    let instance = bound_instance("fake.fs.read.v1", &definition);

    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    registry
        .register_schema_validated_instance(instance.clone())
        .expect("fake instance should bind");

    assert_eq!(registry.definition(&reference), Some(&definition));
    assert_eq!(registry.instance(&instance.instance_id), Some(&instance));
    assert_eq!(
        registry.instances_for(&reference).collect::<Vec<_>>(),
        vec![&instance]
    );
    assert_eq!(registry.definition_count(), 1);
    assert_eq!(registry.instance_count(), 1);
}

#[test]
fn catalog_iteration_is_deterministic_regardless_of_registration_order() {
    let alpha = renamed_definition("example/alpha", "2.0.0");
    let alpha_old = renamed_definition("example/alpha", "1.0.0");
    let zeta = renamed_definition("example/zeta", "1.0.0");
    let instance_z = bound_instance("z.instance", &alpha);
    let instance_a = bound_instance("a.instance", &alpha);
    let mut registry = CapabilityRegistry::new();

    for definition in [zeta, alpha.clone(), alpha_old] {
        registry
            .register_schema_validated_definition(definition)
            .expect("unique definition should register");
    }
    for instance in [instance_z, instance_a] {
        registry
            .register_schema_validated_instance(instance)
            .expect("unique instance should register");
    }

    let definitions: Vec<_> = registry
        .definitions()
        .map(|definition| {
            (
                definition.metadata.id.as_str(),
                definition.metadata.contract_version.as_str(),
            )
        })
        .collect();
    assert_eq!(
        definitions,
        [
            ("example/alpha", "1.0.0"),
            ("example/alpha", "2.0.0"),
            ("example/zeta", "1.0.0")
        ]
    );
    assert_eq!(
        registry
            .instances_for(&capability_ref(&alpha))
            .map(|instance| instance.instance_id.as_str())
            .collect::<Vec<_>>(),
        ["a.instance", "z.instance"]
    );
}

#[test]
fn instances_are_partitioned_by_exact_contract_version() {
    let mut registry = CapabilityRegistry::new();
    let version_one = renamed_definition("example/versioned", "1.0.0");
    let version_two = renamed_definition("example/versioned", "2.0.0");
    let instance_one = bound_instance("version.one", &version_one);
    let instance_two = bound_instance("version.two", &version_two);

    for definition in [version_two.clone(), version_one.clone()] {
        registry
            .register_schema_validated_definition(definition)
            .expect("exact Definition versions should coexist");
    }
    for instance in [instance_two.clone(), instance_one.clone()] {
        registry
            .register_schema_validated_instance(instance)
            .expect("Instance should bind to its exact Definition version");
    }

    assert_eq!(
        registry
            .instances_for(&capability_ref(&version_one))
            .collect::<Vec<_>>(),
        vec![&instance_one]
    );
    assert_eq!(
        registry
            .instances_for(&capability_ref(&version_two))
            .collect::<Vec<_>>(),
        vec![&instance_two]
    );
}

#[test]
fn duplicate_definition_is_rejected_without_replacing_the_original() {
    let mut registry = CapabilityRegistry::new();
    let original = definition();
    let reference = capability_ref(&original);
    let mut replacement = original.clone();
    replacement.metadata.display_name = "replacement must not win".to_owned();
    registry
        .register_schema_validated_definition(original.clone())
        .expect("first definition should register");

    let result = registry.register_schema_validated_definition(replacement);

    assert!(matches!(
        result,
        Err(RegistryError::DuplicateDefinition { .. })
    ));
    assert_eq!(registry.definition(&reference), Some(&original));
    assert_eq!(registry.definition_count(), 1);
}

#[test]
fn duplicate_instance_id_is_rejected_without_replacing_the_original() {
    let mut registry = CapabilityRegistry::new();
    let definition = definition();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    let original = bound_instance("shared.instance", &definition);
    let mut replacement = original.clone();
    replacement.binding.binding_ref = "registry://test/replacement".to_owned();
    registry
        .register_schema_validated_instance(original.clone())
        .expect("first instance should register");

    let result = registry.register_schema_validated_instance(replacement);

    assert!(matches!(
        result,
        Err(RegistryError::DuplicateInstance { .. })
    ));
    assert_eq!(registry.instance("shared.instance"), Some(&original));
    assert_eq!(registry.instance_count(), 1);
}

#[test]
fn orphan_or_nearby_version_instance_is_rejected_without_mutation() {
    let mut registry = CapabilityRegistry::new();
    let definition = definition();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    let mut instance = bound_instance("wrong.version", &definition);
    instance.definition.contract_version = "1.0.1".to_owned();

    let result = registry.register_schema_validated_instance(instance);

    assert!(matches!(
        result,
        Err(RegistryError::DefinitionNotFound {
            contract_version,
            ..
        }) if contract_version == "1.0.1"
    ));
    assert_eq!(registry.instance_count(), 0);
}

#[test]
fn instance_may_only_claim_execution_styles_declared_by_definition() {
    let mut registry = CapabilityRegistry::new();
    let definition = definition();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    let mut instance = bound_instance("unsupported.task", &definition);
    instance.features.task = true;

    let result = registry.register_schema_validated_instance(instance);

    assert!(matches!(
        result,
        Err(RegistryError::UnsupportedExecutionStyle {
            style: ExecutionStyle::Task,
            ..
        })
    ));
    assert_eq!(registry.instance_count(), 0);
}

#[test]
fn instance_may_expose_a_supported_subset_of_definition_styles() {
    let mut registry = CapabilityRegistry::new();
    let mut definition = definition();
    definition.spec.execution.styles = vec![ExecutionStyle::Sync, ExecutionStyle::Task];
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    let mut task_only = bound_instance("task.only", &definition);
    task_only.features.sync = false;
    task_only.features.task = true;

    registry
        .register_schema_validated_instance(task_only)
        .expect("supported subset should register");

    assert_eq!(registry.instance_count(), 1);
}

#[test]
fn instance_without_an_execution_style_is_preserved_for_router_filtering() {
    let mut registry = CapabilityRegistry::new();
    let definition = definition();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    let mut instance = bound_instance("no.style", &definition);
    instance.features.sync = false;
    instance.features.task = false;

    registry
        .register_schema_validated_instance(instance)
        .expect("missing required runtime features are a Router concern");

    assert_eq!(registry.instance_count(), 1);
}

#[test]
fn instance_cannot_strengthen_the_cancellation_contract() {
    let mut registry = CapabilityRegistry::new();
    let mut definition = definition();
    definition.spec.execution.cancellable = false;
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");

    let cancellable = bound_instance("claims.cancellation", &definition);
    assert!(matches!(
        registry.register_schema_validated_instance(cancellable),
        Err(RegistryError::UnsupportedCancellation { .. })
    ));
    assert_eq!(registry.instance_count(), 0);
}

#[test]
fn unsupported_api_versions_and_invalid_timeout_ranges_are_rejected() {
    let mut registry = CapabilityRegistry::new();
    let mut future = definition();
    future.api_version = "xgeny.io/v2".to_owned();
    assert!(matches!(
        registry.register_schema_validated_definition(future),
        Err(RegistryError::UnsupportedDefinitionApiVersion { .. })
    ));

    let mut invalid_timeout = definition();
    invalid_timeout.spec.execution.default_timeout_ms = 2_000;
    invalid_timeout.spec.execution.max_timeout_ms = 1_000;
    assert!(matches!(
        registry.register_schema_validated_definition(invalid_timeout),
        Err(RegistryError::InvalidTimeoutRange { .. })
    ));

    let definition = definition();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("valid definition should register");
    let mut future_instance = bound_instance("future.instance", &definition);
    future_instance.api_version = "xgeny.io/v2".to_owned();
    assert!(matches!(
        registry.register_schema_validated_instance(future_instance),
        Err(RegistryError::UnsupportedInstanceApiVersion { .. })
    ));
    assert_eq!(registry.definition_count(), 1);
    assert_eq!(registry.instance_count(), 0);
    assert_eq!(definition.api_version, API_VERSION_V1ALPHA1);
}

#[test]
fn registry_preserves_source_platform_health_and_auth_states_for_the_router() {
    let mut registry = CapabilityRegistry::new();
    let definition = definition();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    let cases = [
        (
            "builtin.instance",
            CapabilitySource::Builtin,
            Placement::Local,
            OperatingSystem::Linux,
            HealthStatus::Available,
            AuthState::NotRequired,
        ),
        (
            "local-cli.instance",
            CapabilitySource::LocalCli,
            Placement::Local,
            OperatingSystem::Windows,
            HealthStatus::Degraded,
            AuthState::Available,
        ),
        (
            "mcp.instance",
            CapabilitySource::Mcp,
            Placement::Remote,
            OperatingSystem::Any,
            HealthStatus::Unknown,
            AuthState::Required,
        ),
        (
            "connector.instance",
            CapabilitySource::Connector,
            Placement::Device,
            OperatingSystem::Macos,
            HealthStatus::Unavailable,
            AuthState::Expired,
        ),
        (
            "xgen.instance",
            CapabilitySource::Xgen,
            Placement::Remote,
            OperatingSystem::Any,
            HealthStatus::Available,
            AuthState::Available,
        ),
    ];

    for (instance_id, source, placement, os, health, auth) in cases {
        let mut instance = bound_instance(instance_id, &definition);
        instance.source = source;
        instance.placement = placement;
        instance.platform.os = os;
        instance.platform.arch = Architecture::Aarch64;
        instance.health.status = health;
        instance.health.reason =
            (health != HealthStatus::Available).then(|| "dynamic discovery state".to_owned());
        instance.auth.state = auth;
        instance.auth.auth_ref = matches!(auth, AuthState::Available | AuthState::Expired)
            .then(|| "credential://test/account".to_owned());
        registry
            .register_schema_validated_instance(instance)
            .expect("Registry must preserve Router input states");
    }

    assert_eq!(registry.instance_count(), 5);
    assert_eq!(
        registry
            .instances()
            .map(|instance| instance.instance_id.as_str())
            .collect::<Vec<_>>(),
        [
            "builtin.instance",
            "connector.instance",
            "local-cli.instance",
            "mcp.instance",
            "xgen.instance"
        ]
    );
    assert_eq!(
        registry.instance("connector.instance").map(|instance| (
            instance.health.status,
            instance.auth.state,
            instance.platform.os
        )),
        Some((
            HealthStatus::Unavailable,
            AuthState::Expired,
            OperatingSystem::Macos
        ))
    );
}

#[test]
fn required_extensions_are_preserved_for_router_fail_closed_filtering() {
    let mut registry = CapabilityRegistry::new();
    let mut definition = definition();
    definition
        .required_extensions
        .push("https://example.test/xgeny/required/v1".to_owned());
    let mut instance = bound_instance("requires.extension", &definition);
    instance.required_extensions = definition.required_extensions.clone();

    registry
        .register_schema_validated_definition(definition.clone())
        .expect("Registry does not negotiate extension support");
    registry
        .register_schema_validated_instance(instance.clone())
        .expect("Registry does not negotiate extension support");

    assert_eq!(
        registry
            .definition(&capability_ref(&definition))
            .map(|definition| definition.required_extensions.as_slice()),
        Some(definition.required_extensions.as_slice())
    );
    assert_eq!(
        registry
            .instance(&instance.instance_id)
            .map(|instance| instance.required_extensions.as_slice()),
        Some(instance.required_extensions.as_slice())
    );
}
