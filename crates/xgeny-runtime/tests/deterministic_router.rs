use xgeny_domain::{
    Architecture, AuthState, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef,
    CriticalAction, DataBoundary, EffectClass, ExecutionStyle, HealthStatus, OperatingSystem,
    PermissionRequestBody, Platform, PolicySource, PolicySourceKind, ProtocolDocument, TrustLevel,
};
use xgeny_policy::{
    BoundPolicyEvaluation, PermissionBroker, PermissionRequestResolver, PolicyAllowance,
    PolicyContribution, PolicyInputs, ResourceResolutionFailure, ResourceResolver,
};
use xgeny_runtime::{
    CandidateEligibility, CapabilityRegistry, CapabilityRouter, RequiredRouteFeatures,
    RouteBlockReason, RouteCandidateEvaluation, RouteInputError, RouteInteractionReason,
    RouteOutcome, RouteReason, RouteRequest, RouteSelectionReason,
};

#[derive(Debug, Clone, Copy)]
struct IdentityResolver;

impl ResourceResolver for IdentityResolver {
    fn resolve(&self, _scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        Ok(resource.to_owned())
    }
}

#[derive(Debug, Clone, Copy)]
enum PolicyVerdict {
    Allow,
    Ask,
    Deny,
}

type RejectionCase = (&'static str, fn(&mut CapabilityInstanceBody), RouteReason);

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

fn permission_request() -> PermissionRequestBody {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/permission-request.fs-read.json"
    ))
    .expect("bundled permission request should deserialize");
    match document {
        ProtocolDocument::PermissionRequest(body) => *body,
        other => panic!("expected a permission request, got {other:?}"),
    }
}

fn capability_ref(definition: &CapabilityDefinitionBody) -> CapabilityRef {
    CapabilityRef {
        capability_id: definition.metadata.id.clone(),
        contract_version: definition.metadata.contract_version.clone(),
    }
}

fn named_instance(
    instance_id: &str,
    definition: &CapabilityDefinitionBody,
) -> CapabilityInstanceBody {
    let mut instance = instance();
    instance_id.clone_into(&mut instance.instance_id);
    instance.definition = capability_ref(definition);
    instance.binding.binding_ref = format!("test://{instance_id}");
    instance
}

fn build_registry(
    definition: &CapabilityDefinitionBody,
    instances: impl IntoIterator<Item = CapabilityInstanceBody>,
) -> CapabilityRegistry {
    let mut registry = CapabilityRegistry::new();
    registry
        .register_schema_validated_definition(definition.clone())
        .expect("definition should register");
    for instance in instances {
        registry
            .register_schema_validated_instance(instance)
            .expect("instance should register");
    }
    registry
}

fn request(definition: &CapabilityDefinitionBody) -> RouteRequest {
    RouteRequest {
        capability: capability_ref(definition),
        target_platform: Platform {
            os: OperatingSystem::Linux,
            arch: Architecture::X86_64,
        },
        required_features: RequiredRouteFeatures {
            execution_style: ExecutionStyle::Sync,
            cancellation: false,
            idempotency_key: false,
            idempotency_query: false,
        },
        allowed_trust_levels: vec![TrustLevel::Verified],
        allowed_data_boundaries: vec![DataBoundary::Local],
        trust_preference: Vec::new(),
        data_boundary_preference: Vec::new(),
        preferred_instance_ids: Vec::new(),
        pinned_instance_id: None,
    }
}

fn ranking_request(definition: &CapabilityDefinitionBody) -> RouteRequest {
    let mut request = request(definition);
    request.allowed_trust_levels = vec![TrustLevel::Configured, TrustLevel::Verified];
    request.allowed_data_boundaries = vec![DataBoundary::Local, DataBoundary::Device];
    request.trust_preference = vec![TrustLevel::Configured, TrustLevel::Verified];
    request.data_boundary_preference = vec![DataBoundary::Device, DataBoundary::Local];
    request.preferred_instance_ids = vec!["user.preferred".to_owned()];
    request
}

fn source(kind: PolicySourceKind, id: &str, digest_byte: char) -> PolicySource {
    PolicySource {
        kind,
        id: id.to_owned(),
        digest: format!("sha256:{}", digest_byte.to_string().repeat(64)),
    }
}

fn policy_evaluation(verdict: PolicyVerdict) -> BoundPolicyEvaluation {
    policy_evaluation_for(&permission_request(), verdict)
}

fn policy_evaluation_for(
    request: &PermissionRequestBody,
    verdict: PolicyVerdict,
) -> BoundPolicyEvaluation {
    let resolved = PermissionRequestResolver::new(IdentityResolver)
        .resolve_schema_validated(request)
        .expect("permission fixture should resolve");
    let allowance = || {
        PolicyAllowance::from_trusted_evaluation(
            resolved.requested_scopes().iter().cloned(),
            resolved.resources().iter().cloned(),
            resolved.critical_actions().iter().copied(),
            [resolved.requested_lifetime()],
        )
    };
    let host = match verdict {
        PolicyVerdict::Deny => PolicyContribution::deny(
            source(PolicySourceKind::Host, "host.local", '1'),
            "host_denied",
        ),
        PolicyVerdict::Allow | PolicyVerdict::Ask => PolicyContribution::allow(
            source(PolicySourceKind::Host, "host.local", '1'),
            allowance(),
        ),
    };
    let user = match verdict {
        PolicyVerdict::Ask => PolicyContribution::ask(
            source(PolicySourceKind::UserProfile, "profile.default", '2'),
            "approval_required",
        ),
        PolicyVerdict::Allow | PolicyVerdict::Deny => PolicyContribution::allow(
            source(PolicySourceKind::UserProfile, "profile.default", '2'),
            allowance(),
        ),
    };

    PermissionBroker::new()
        .evaluate_bound(&resolved, &PolicyInputs::local(&resolved, host, user))
        .expect("policy inputs should be valid")
}

fn route(
    registry: &CapabilityRegistry,
    request: &RouteRequest,
    policy: Option<&BoundPolicyEvaluation>,
) -> RouteOutcome {
    CapabilityRouter::new()
        .route(registry, request, policy)
        .expect("route input should be valid")
}

fn selected_id(outcome: &RouteOutcome) -> &str {
    match outcome {
        RouteOutcome::Selected {
            selected_instance_id,
            ..
        } => selected_instance_id,
        other => panic!("expected selected outcome, got {other:?}"),
    }
}

fn candidate<'a>(outcome: &'a RouteOutcome, instance_id: &str) -> &'a RouteCandidateEvaluation {
    let candidates = match outcome {
        RouteOutcome::Selected { candidates, .. }
        | RouteOutcome::InteractionRequired { candidates, .. }
        | RouteOutcome::Blocked { candidates, .. } => candidates,
    };
    candidates
        .iter()
        .find(|candidate| candidate.instance_id == instance_id)
        .expect("candidate should be present")
}

#[test]
fn exact_definition_is_required_and_nearby_versions_never_fallback() {
    let definition = definition();
    let registry = build_registry(&definition, [named_instance("exact.instance", &definition)]);
    let mut missing = request(&definition);
    missing.capability.contract_version = "1.0.1".to_owned();

    assert_eq!(
        CapabilityRouter::new().route(
            &registry,
            &missing,
            Some(&policy_evaluation(PolicyVerdict::Allow))
        ),
        Err(RouteInputError::DefinitionNotFound {
            capability_id: definition.metadata.id,
            contract_version: "1.0.1".to_owned(),
        })
    );
}

#[test]
fn empty_catalog_and_hard_rejection_block_before_policy_interaction() {
    let definition = definition();
    let ask = policy_evaluation(PolicyVerdict::Ask);
    let empty = build_registry(&definition, []);
    assert!(matches!(
        route(&empty, &request(&definition), Some(&ask)),
        RouteOutcome::Blocked {
            ranked_instance_ids,
            candidates,
            reasons,
            policy_reasons,
        } if ranked_instance_ids.is_empty()
            && candidates.is_empty()
            && reasons == [RouteBlockReason::NoCandidates]
            && policy_reasons.is_empty()
    ));

    let mut unknown = named_instance("unknown.instance", &definition);
    unknown.health.status = HealthStatus::Unknown;
    let blocked = build_registry(&definition, [unknown]);
    assert!(matches!(
        route(&blocked, &request(&definition), Some(&ask)),
        RouteOutcome::Blocked {
            ranked_instance_ids,
            reasons,
            policy_reasons,
            ..
        } if ranked_instance_ids.is_empty()
            && reasons == [RouteBlockReason::NoPlacementEligibleCandidates]
            && policy_reasons.is_empty()
    ));
}

#[test]
fn target_platform_must_be_concrete_and_instance_any_is_a_wildcard() {
    let definition = definition();
    let mut wildcard = named_instance("wildcard.instance", &definition);
    wildcard.platform.os = OperatingSystem::Any;
    wildcard.platform.arch = Architecture::Any;
    let registry = build_registry(&definition, [wildcard]);
    let allow = policy_evaluation(PolicyVerdict::Allow);

    assert_eq!(
        selected_id(&route(&registry, &request(&definition), Some(&allow))),
        "wildcard.instance"
    );

    let mut invalid = request(&definition);
    invalid.target_platform.os = OperatingSystem::Any;
    assert_eq!(
        CapabilityRouter::new().route(&registry, &invalid, Some(&allow)),
        Err(RouteInputError::TargetPlatformMustBeConcrete)
    );
}

#[test]
fn platform_health_and_auth_filters_are_fail_closed() {
    let definition = definition();
    let allow = policy_evaluation(PolicyVerdict::Allow);
    let cases: [RejectionCase; 6] = [
        (
            "os",
            |candidate: &mut CapabilityInstanceBody| {
                candidate.platform.os = OperatingSystem::Macos;
            },
            RouteReason::PlatformOsMismatch,
        ),
        (
            "arch",
            |candidate: &mut CapabilityInstanceBody| {
                candidate.platform.arch = Architecture::Aarch64;
            },
            RouteReason::PlatformArchitectureMismatch,
        ),
        (
            "unavailable",
            |candidate: &mut CapabilityInstanceBody| {
                candidate.health.status = HealthStatus::Unavailable;
            },
            RouteReason::HealthUnavailable,
        ),
        (
            "unknown",
            |candidate: &mut CapabilityInstanceBody| {
                candidate.health.status = HealthStatus::Unknown;
            },
            RouteReason::HealthUnknown,
        ),
        (
            "auth-required",
            |candidate: &mut CapabilityInstanceBody| {
                candidate.auth.state = AuthState::Required;
            },
            RouteReason::AuthRequired,
        ),
        (
            "auth-expired",
            |candidate: &mut CapabilityInstanceBody| {
                candidate.auth.state = AuthState::Expired;
            },
            RouteReason::AuthExpired,
        ),
    ];

    for (name, mutate, expected_reason) in cases {
        let mut rejected = named_instance(name, &definition);
        mutate(&mut rejected);
        let registry = build_registry(&definition, [rejected]);
        let outcome = route(&registry, &request(&definition), Some(&allow));
        assert_eq!(
            candidate(&outcome, name),
            &RouteCandidateEvaluation {
                instance_id: name.to_owned(),
                eligibility: CandidateEligibility::Rejected,
                reasons: vec![expected_reason],
            }
        );
        assert!(matches!(
            outcome,
            RouteOutcome::Blocked {
                reasons,
                ..
            } if reasons == [RouteBlockReason::NoPlacementEligibleCandidates]
        ));
    }
}

#[test]
fn degraded_is_eligible_but_available_always_ranks_first() {
    let definition = definition();
    let mut degraded = named_instance("a.degraded", &definition);
    degraded.health.status = HealthStatus::Degraded;
    degraded
        .hints
        .as_mut()
        .expect("fixture has hints")
        .reliability = Some(1.0);
    let mut available = named_instance("z.available", &definition);
    available
        .hints
        .as_mut()
        .expect("fixture has hints")
        .reliability = Some(0.0);
    let registry = build_registry(&definition, [degraded, available]);

    let outcome = route(
        &registry,
        &request(&definition),
        Some(&policy_evaluation(PolicyVerdict::Allow)),
    );

    assert_eq!(selected_id(&outcome), "z.available");
    assert_eq!(
        candidate(&outcome, "a.degraded"),
        &RouteCandidateEvaluation {
            instance_id: "a.degraded".to_owned(),
            eligibility: CandidateEligibility::PlacementEligible,
            reasons: vec![RouteReason::HealthDegraded],
        }
    );
}

#[test]
fn trust_and_data_boundary_are_explicit_allowed_sets_not_inferred_orderings() {
    let definition = definition();
    let mut managed_external = named_instance("managed.external", &definition);
    managed_external.trust = TrustLevel::Managed;
    managed_external.data_boundary = DataBoundary::External;
    let registry = build_registry(&definition, [managed_external]);
    let allow = policy_evaluation(PolicyVerdict::Allow);

    let outcome = route(&registry, &request(&definition), Some(&allow));
    assert_eq!(
        candidate(&outcome, "managed.external").reasons,
        [
            RouteReason::TrustNotAllowed,
            RouteReason::DataBoundaryNotAllowed,
        ]
    );

    let mut explicitly_allowed = request(&definition);
    explicitly_allowed.allowed_trust_levels = vec![TrustLevel::Managed];
    explicitly_allowed.allowed_data_boundaries = vec![DataBoundary::External];
    assert_eq!(
        selected_id(&route(&registry, &explicitly_allowed, Some(&allow))),
        "managed.external"
    );
}

#[test]
fn required_features_and_extensions_are_hard_filters() {
    let mut definition = definition();
    definition.spec.execution.styles.push(ExecutionStyle::Task);
    definition.required_extensions = vec!["urn:xgeny:test:def".to_owned()];
    let mut limited = named_instance("limited.instance", &definition);
    limited.features.sync = false;
    limited.features.task = false;
    limited.features.cancellable = false;
    limited.features.idempotency_query = false;
    limited.required_extensions = vec!["urn:xgeny:test:instance".to_owned()];
    let registry = build_registry(&definition, [limited]);
    let mut route_request = request(&definition);
    route_request.required_features = RequiredRouteFeatures {
        execution_style: ExecutionStyle::Task,
        cancellation: true,
        idempotency_key: true,
        idempotency_query: true,
    };

    let outcome = route(
        &registry,
        &route_request,
        Some(&policy_evaluation(PolicyVerdict::Allow)),
    );
    assert_eq!(
        candidate(&outcome, "limited.instance").reasons,
        [
            RouteReason::DefinitionExtensionUnsupported {
                extension: "urn:xgeny:test:def".to_owned(),
            },
            RouteReason::InstanceExtensionUnsupported {
                extension: "urn:xgeny:test:instance".to_owned(),
            },
            RouteReason::ExecutionStyleUnsupported {
                style: ExecutionStyle::Task,
            },
            RouteReason::CancellationUnsupported,
            RouteReason::IdempotencyQueryUnsupported,
        ]
    );

    let mut supported_definition = definition.clone();
    supported_definition.required_extensions.clear();
    let mut supported = named_instance("supported.instance", &supported_definition);
    supported.features.task = true;
    supported.features.idempotency_query = true;
    let registry = build_registry(&supported_definition, [supported]);
    let mut route_request = request(&supported_definition);
    route_request.required_features = RequiredRouteFeatures {
        execution_style: ExecutionStyle::Task,
        cancellation: true,
        idempotency_key: true,
        idempotency_query: true,
    };
    assert_eq!(
        selected_id(&route(
            &registry,
            &route_request,
            Some(&policy_evaluation(PolicyVerdict::Allow))
        )),
        "supported.instance"
    );

    supported_definition
        .spec
        .execution
        .idempotency_key_supported = false;
    let registry = build_registry(
        &supported_definition,
        [named_instance("no.idempotency-key", &supported_definition)],
    );
    let outcome = route(
        &registry,
        &route_request,
        Some(&policy_evaluation(PolicyVerdict::Allow)),
    );
    assert!(
        candidate(&outcome, "no.idempotency-key")
            .reasons
            .contains(&RouteReason::IdempotencyKeyUnsupported)
    );
}

#[test]
fn policy_allow_ask_deny_and_missing_have_distinct_closed_outcomes() {
    let definition = definition();
    let registry = build_registry(
        &definition,
        [named_instance("policy.instance", &definition)],
    );
    let request = request(&definition);

    assert!(matches!(
        route(
            &registry,
            &request,
            Some(&policy_evaluation(PolicyVerdict::Allow))
        ),
        RouteOutcome::Selected { .. }
    ));

    assert!(matches!(
        route(
            &registry,
            &request,
            Some(&policy_evaluation(PolicyVerdict::Ask))
        ),
        RouteOutcome::InteractionRequired {
            reasons,
            policy_reasons,
            ..
        } if reasons == [RouteInteractionReason::PolicyApprovalRequired]
            && policy_reasons.iter().any(|reason| reason.code() == "approval_required")
    ));

    assert!(matches!(
        route(
            &registry,
            &request,
            Some(&policy_evaluation(PolicyVerdict::Deny))
        ),
        RouteOutcome::Blocked {
            ranked_instance_ids,
            reasons,
            policy_reasons,
            ..
        } if reasons == [RouteBlockReason::PolicyDenied]
            && ranked_instance_ids == ["policy.instance"]
            && policy_reasons.iter().any(|reason| reason.code() == "host_denied")
    ));

    assert!(matches!(
        route(&registry, &request, None),
        RouteOutcome::Blocked {
            ranked_instance_ids,
            reasons,
            policy_reasons,
            ..
        } if reasons == [RouteBlockReason::PolicyMissing]
            && ranked_instance_ids == ["policy.instance"]
            && policy_reasons.is_empty()
    ));
}

#[test]
fn bound_policy_must_match_critical_actions_before_routing() {
    let mut definition = definition();
    definition.spec.effect.class = EffectClass::NonIdempotent;
    definition.spec.effect.critical_actions = vec![CriticalAction::ProductionDeploy];
    let registry = build_registry(
        &definition,
        [named_instance("critical.instance", &definition)],
    );

    let mut route_request = request(&definition);
    route_request.pinned_instance_id = Some("critical.instance".to_owned());
    let mut unrelated_permission = permission_request();
    unrelated_permission.effect_class = EffectClass::NonIdempotent;
    let unrelated_evaluation = policy_evaluation_for(&unrelated_permission, PolicyVerdict::Allow);
    assert_eq!(
        CapabilityRouter::new().route(&registry, &route_request, Some(&unrelated_evaluation),),
        Err(RouteInputError::PolicyCriticalActionsMismatch)
    );

    let mut permission = permission_request();
    permission.effect_class = EffectClass::NonIdempotent;
    permission.critical_actions = vec![CriticalAction::ProductionDeploy];
    let critical_evaluation = policy_evaluation_for(&permission, PolicyVerdict::Allow);
    let outcome = route(&registry, &route_request, Some(&critical_evaluation));

    assert!(matches!(
        outcome,
        RouteOutcome::InteractionRequired {
            reasons,
            policy_reasons,
            ..
        } if reasons == [
                RouteInteractionReason::CriticalApprovalRequired,
                RouteInteractionReason::PolicyApprovalRequired,
            ] && policy_reasons
                .iter()
                .any(|reason| reason.code() == "critical_approval_required")
    ));
}

#[test]
fn bound_policy_capability_effect_and_scopes_are_exact() {
    let definition = definition();
    let registry = build_registry(&definition, [named_instance("bound.instance", &definition)]);
    let route_request = request(&definition);

    let mut wrong_capability = permission_request();
    wrong_capability.capability.contract_version = "9.9.9".to_owned();
    let evaluation = policy_evaluation_for(&wrong_capability, PolicyVerdict::Allow);
    assert_eq!(
        CapabilityRouter::new().route(&registry, &route_request, Some(&evaluation)),
        Err(RouteInputError::PolicyCapabilityMismatch)
    );

    let mut wrong_effect = permission_request();
    wrong_effect.effect_class = EffectClass::Idempotent;
    let evaluation = policy_evaluation_for(&wrong_effect, PolicyVerdict::Allow);
    assert_eq!(
        CapabilityRouter::new().route(&registry, &route_request, Some(&evaluation)),
        Err(RouteInputError::PolicyEffectClassMismatch {
            expected: EffectClass::ReadOnly,
            actual: EffectClass::Idempotent,
        })
    );

    let mut wrong_scope = permission_request();
    wrong_scope.requested_scopes[0] = "filesystem.write".to_owned();
    wrong_scope.resolved_resources[0].scope = "filesystem.write".to_owned();
    let evaluation = policy_evaluation_for(&wrong_scope, PolicyVerdict::Allow);
    assert_eq!(
        CapabilityRouter::new().route(&registry, &route_request, Some(&evaluation)),
        Err(RouteInputError::PolicyScopesMismatch)
    );
}

#[test]
fn pin_overrides_ranking_but_never_safety_or_silently_falls_back() {
    let definition = definition();
    let mut fast = named_instance("fast.instance", &definition);
    fast.hints.as_mut().expect("fixture has hints").latency_ms = Some(1);
    let mut pinned = named_instance("pinned.instance", &definition);
    pinned.hints.as_mut().expect("fixture has hints").latency_ms = Some(100);
    let mut middle = named_instance("middle.instance", &definition);
    middle.hints.as_mut().expect("fixture has hints").latency_ms = Some(50);
    let registry = build_registry(&definition, [fast, pinned, middle]);
    let allow = policy_evaluation(PolicyVerdict::Allow);
    let mut route_request = request(&definition);
    route_request.pinned_instance_id = Some("pinned.instance".to_owned());

    assert!(matches!(
        route(&registry, &route_request, Some(&allow)),
        RouteOutcome::Selected {
            selected_instance_id,
            reason: RouteSelectionReason::Pinned,
            ranked_instance_ids,
            ..
        } if selected_instance_id == "pinned.instance"
            && ranked_instance_ids == ["pinned.instance", "fast.instance", "middle.instance"]
    ));

    assert!(matches!(
        route(
            &registry,
            &route_request,
            Some(&policy_evaluation(PolicyVerdict::Ask))
        ),
        RouteOutcome::InteractionRequired {
            ranked_instance_ids,
            reasons,
            ..
        } if ranked_instance_ids.first().is_some_and(|id| id == "pinned.instance")
            && reasons == [RouteInteractionReason::PolicyApprovalRequired]
    ));

    assert!(matches!(
        route(
            &registry,
            &route_request,
            Some(&policy_evaluation(PolicyVerdict::Deny))
        ),
        RouteOutcome::Blocked {
            ranked_instance_ids,
            reasons,
            ..
        } if ranked_instance_ids.first().is_some_and(|id| id == "pinned.instance")
            && reasons == [RouteBlockReason::PolicyDenied]
    ));

    let mut unavailable = named_instance("pinned.instance", &definition);
    unavailable.health.status = HealthStatus::Unavailable;
    let registry = build_registry(
        &definition,
        [named_instance("fast.instance", &definition), unavailable],
    );
    assert!(matches!(
        route(&registry, &route_request, Some(&allow)),
        RouteOutcome::Blocked { reasons, .. }
            if reasons == [RouteBlockReason::PinnedInstanceIneligible]
    ));

    route_request.pinned_instance_id = Some("missing.instance".to_owned());
    assert!(matches!(
        route(&registry, &route_request, Some(&allow)),
        RouteOutcome::Blocked { reasons, .. }
            if reasons == [RouteBlockReason::PinnedInstanceNotFound]
    ));

    let mut other_definition = definition.clone();
    other_definition.metadata.id = "example/other".to_owned();
    let other_instance = named_instance("other.instance", &other_definition);
    let mut cross_capability_registry = CapabilityRegistry::new();
    for definition in [&definition, &other_definition] {
        cross_capability_registry
            .register_schema_validated_definition(definition.clone())
            .expect("definitions should register");
    }
    for instance in [named_instance("fast.instance", &definition), other_instance] {
        cross_capability_registry
            .register_schema_validated_instance(instance)
            .expect("instances should register");
    }
    let mut cross_request = request(&definition);
    cross_request.pinned_instance_id = Some("other.instance".to_owned());
    assert!(matches!(
        route(&cross_capability_registry, &cross_request, Some(&allow)),
        RouteOutcome::Blocked { reasons, .. }
            if reasons == [RouteBlockReason::PinnedInstanceCapabilityMismatch]
    ));
}

#[test]
fn lexicographic_ranking_uses_only_explicit_and_represented_dimensions() {
    let definition = definition();
    let allow = policy_evaluation(PolicyVerdict::Allow);
    let request = ranking_request(&definition);

    let base = named_instance("base", &definition);

    let mut reliable = base.clone();
    reliable.instance_id = "reliable".to_owned();
    reliable
        .hints
        .as_mut()
        .expect("fixture has hints")
        .reliability = Some(1.0);
    let mut preferred_trust = base.clone();
    preferred_trust.instance_id = "preferred.trust".to_owned();
    preferred_trust.trust = TrustLevel::Configured;
    preferred_trust
        .hints
        .as_mut()
        .expect("fixture has hints")
        .reliability = Some(0.5);
    let registry = build_registry(&definition, [preferred_trust, reliable]);
    assert_eq!(
        selected_id(&route(&registry, &request, Some(&allow))),
        "reliable",
        "reliability precedes explicit trust preference"
    );

    let mut preferred_boundary = base.clone();
    preferred_boundary.instance_id = "preferred.boundary".to_owned();
    preferred_boundary.data_boundary = DataBoundary::Device;
    let mut preferred_trust = base.clone();
    preferred_trust.instance_id = "preferred.trust".to_owned();
    preferred_trust.trust = TrustLevel::Configured;
    let registry = build_registry(&definition, [preferred_boundary, preferred_trust]);
    assert_eq!(
        selected_id(&route(&registry, &request, Some(&allow))),
        "preferred.trust",
        "explicit trust preference precedes boundary preference"
    );

    let mut low_latency = base.clone();
    low_latency.instance_id = "low.latency".to_owned();
    low_latency
        .hints
        .as_mut()
        .expect("fixture has hints")
        .latency_ms = Some(1);
    let mut preferred_boundary = base.clone();
    preferred_boundary.instance_id = "preferred.boundary".to_owned();
    preferred_boundary.data_boundary = DataBoundary::Device;
    preferred_boundary
        .hints
        .as_mut()
        .expect("fixture has hints")
        .latency_ms = Some(100);
    let registry = build_registry(&definition, [low_latency, preferred_boundary]);
    assert_eq!(
        selected_id(&route(&registry, &request, Some(&allow))),
        "preferred.boundary",
        "explicit boundary preference precedes latency"
    );
}

#[test]
fn lexicographic_ranking_orders_latency_cost_and_user_preference() {
    let definition = definition();
    let allow = policy_evaluation(PolicyVerdict::Allow);
    let request = ranking_request(&definition);
    let base = named_instance("base", &definition);

    let mut cheap = base.clone();
    cheap.instance_id = "cheap".to_owned();
    cheap.hints.as_mut().expect("fixture has hints").latency_ms = Some(100);
    cheap
        .hints
        .as_mut()
        .expect("fixture has hints")
        .monetary_cost = Some(0.0);
    let mut fast = base.clone();
    fast.instance_id = "fast".to_owned();
    fast.hints.as_mut().expect("fixture has hints").latency_ms = Some(1);
    fast.hints
        .as_mut()
        .expect("fixture has hints")
        .monetary_cost = Some(100.0);
    let registry = build_registry(&definition, [cheap, fast]);
    assert_eq!(
        selected_id(&route(&registry, &request, Some(&allow))),
        "fast",
        "latency precedes cost"
    );

    let mut cheap = base.clone();
    cheap.instance_id = "cheap".to_owned();
    cheap
        .hints
        .as_mut()
        .expect("fixture has hints")
        .monetary_cost = Some(0.0);
    let mut user = base;
    user.instance_id = "user.preferred".to_owned();
    user.hints
        .as_mut()
        .expect("fixture has hints")
        .monetary_cost = Some(1.0);
    let registry = build_registry(&definition, [cheap, user]);
    assert_eq!(
        selected_id(&route(&registry, &request, Some(&allow))),
        "cheap",
        "cost precedes user instance preference"
    );

    let mut explicit_id_request = request.clone();
    explicit_id_request.preferred_instance_ids = vec!["z.instance".to_owned()];
    let alpha = named_instance("a.instance", &definition);
    let zeta = named_instance("z.instance", &definition);
    let registry = build_registry(&definition, [alpha, zeta]);
    assert_eq!(
        selected_id(&route(&registry, &explicit_id_request, Some(&allow))),
        "z.instance",
        "explicit Instance preference precedes the final ID tie-break"
    );
}

#[test]
fn missing_hints_rank_last_invalid_hints_reject_and_signed_zero_ties() {
    let definition = definition();
    let allow = policy_evaluation(PolicyVerdict::Allow);
    let request = request(&definition);
    let mut known = named_instance("z.known", &definition);
    known.hints.as_mut().expect("fixture has hints").reliability = Some(0.0);
    let mut missing = named_instance("a.missing", &definition);
    missing.hints = None;
    let registry = build_registry(&definition, [missing, known]);
    assert_eq!(
        selected_id(&route(&registry, &request, Some(&allow))),
        "z.known"
    );

    let invalid_hint_cases: [RejectionCase; 4] = [
        (
            "nan-cost",
            |candidate: &mut CapabilityInstanceBody| {
                candidate
                    .hints
                    .as_mut()
                    .expect("fixture has hints")
                    .monetary_cost = Some(f64::NAN);
            },
            RouteReason::InvalidCostHint,
        ),
        (
            "negative-cost",
            |candidate: &mut CapabilityInstanceBody| {
                candidate
                    .hints
                    .as_mut()
                    .expect("fixture has hints")
                    .monetary_cost = Some(-1.0);
            },
            RouteReason::InvalidCostHint,
        ),
        (
            "infinite-reliability",
            |candidate: &mut CapabilityInstanceBody| {
                candidate
                    .hints
                    .as_mut()
                    .expect("fixture has hints")
                    .reliability = Some(f64::INFINITY);
            },
            RouteReason::InvalidReliabilityHint,
        ),
        (
            "oversized-reliability",
            |candidate: &mut CapabilityInstanceBody| {
                candidate
                    .hints
                    .as_mut()
                    .expect("fixture has hints")
                    .reliability = Some(1.1);
            },
            RouteReason::InvalidReliabilityHint,
        ),
    ];
    for (name, mutate, expected) in invalid_hint_cases {
        let mut invalid = named_instance(name, &definition);
        mutate(&mut invalid);
        let registry = build_registry(&definition, [invalid]);
        let outcome = route(&registry, &request, Some(&allow));
        assert_eq!(candidate(&outcome, name).reasons, [expected]);
    }

    let mut negative_zero = named_instance("a.zero", &definition);
    negative_zero
        .hints
        .as_mut()
        .expect("fixture has hints")
        .monetary_cost = Some(-0.0);
    let mut positive_zero = named_instance("z.zero", &definition);
    positive_zero
        .hints
        .as_mut()
        .expect("fixture has hints")
        .monetary_cost = Some(0.0);
    let registry = build_registry(&definition, [positive_zero, negative_zero]);
    assert_eq!(
        selected_id(&route(&registry, &request, Some(&allow))),
        "a.zero",
        "signed zero must tie and fall through to instance ID"
    );
}

#[test]
fn typed_route_golden_is_stable_across_registration_permutations() {
    let definition = definition();
    let allow = policy_evaluation(PolicyVerdict::Allow);
    let request = request(&definition);
    let mut alpha = named_instance("alpha", &definition);
    alpha.platform.os = OperatingSystem::Macos;
    alpha.auth.state = AuthState::Expired;
    alpha.trust = TrustLevel::Managed;
    alpha.data_boundary = DataBoundary::External;
    let beta = named_instance("beta", &definition);
    let gamma = named_instance("gamma", &definition);

    let golden = RouteOutcome::Selected {
        selected_instance_id: "beta".to_owned(),
        ranked_instance_ids: vec!["beta".to_owned(), "gamma".to_owned()],
        candidates: vec![
            RouteCandidateEvaluation {
                instance_id: "alpha".to_owned(),
                eligibility: CandidateEligibility::Rejected,
                reasons: vec![
                    RouteReason::PlatformOsMismatch,
                    RouteReason::AuthExpired,
                    RouteReason::TrustNotAllowed,
                    RouteReason::DataBoundaryNotAllowed,
                ],
            },
            RouteCandidateEvaluation {
                instance_id: "beta".to_owned(),
                eligibility: CandidateEligibility::PlacementEligible,
                reasons: Vec::new(),
            },
            RouteCandidateEvaluation {
                instance_id: "gamma".to_owned(),
                eligibility: CandidateEligibility::PlacementEligible,
                reasons: Vec::new(),
            },
        ],
        reason: RouteSelectionReason::LexicographicRanking,
    };
    let permutations = [
        [alpha.clone(), beta.clone(), gamma.clone()],
        [alpha.clone(), gamma.clone(), beta.clone()],
        [beta.clone(), alpha.clone(), gamma.clone()],
        [beta.clone(), gamma.clone(), alpha.clone()],
        [gamma.clone(), alpha.clone(), beta.clone()],
        [gamma, beta, alpha],
    ];

    for instances in permutations {
        let registry = build_registry(&definition, instances);
        assert_eq!(route(&registry, &request, Some(&allow)), golden);
    }
}

#[test]
fn adding_an_ineligible_candidate_never_changes_the_selected_route() {
    let definition = definition();
    let allow = policy_evaluation(PolicyVerdict::Allow);
    let request = request(&definition);
    let selected = named_instance("selected", &definition);
    let baseline = build_registry(&definition, [selected.clone()]);
    let baseline_id = selected_id(&route(&baseline, &request, Some(&allow))).to_owned();

    let mut blocked = named_instance("a.blocked", &definition);
    blocked.health.status = HealthStatus::Unknown;
    let expanded = build_registry(&definition, [selected, blocked]);

    assert_eq!(
        selected_id(&route(&expanded, &request, Some(&allow))),
        baseline_id
    );
}

#[test]
fn malformed_route_preferences_are_rejected_before_candidate_selection() {
    let definition = definition();
    let registry = build_registry(&definition, [named_instance("valid.instance", &definition)]);
    let allow = policy_evaluation(PolicyVerdict::Allow);

    let mut empty_trust = request(&definition);
    empty_trust.allowed_trust_levels.clear();
    assert_eq!(
        CapabilityRouter::new().route(&registry, &empty_trust, Some(&allow)),
        Err(RouteInputError::AllowedTrustLevelsEmpty)
    );

    let mut duplicate_boundary = request(&definition);
    duplicate_boundary
        .allowed_data_boundaries
        .push(DataBoundary::Local);
    assert_eq!(
        CapabilityRouter::new().route(&registry, &duplicate_boundary, Some(&allow)),
        Err(RouteInputError::DuplicateAllowedDataBoundary {
            boundary: DataBoundary::Local,
        })
    );

    let mut invalid_preference = request(&definition);
    invalid_preference.trust_preference = vec![TrustLevel::Managed];
    assert_eq!(
        CapabilityRouter::new().route(&registry, &invalid_preference, Some(&allow)),
        Err(RouteInputError::TrustPreferenceNotAllowed {
            trust: TrustLevel::Managed,
        })
    );
}
