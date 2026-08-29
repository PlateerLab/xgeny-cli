use std::cmp::Ordering;

use thiserror::Error;
use xgeny_domain::{
    Architecture, AuthState, CapabilityDefinitionBody, CapabilityInstanceBody, CapabilityRef,
    DataBoundary, EffectClass, ExecutionStyle, HealthStatus, OperatingSystem, Platform, TrustLevel,
};
use xgeny_policy::{BoundPolicyEvaluation, BrokerOutcome, PolicyReason};

use crate::CapabilityRegistry;

/// Runtime requirements used to remove incapable providers before ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredRouteFeatures {
    pub execution_style: ExecutionStyle,
    pub cancellation: bool,
    pub idempotency_key: bool,
    pub idempotency_query: bool,
}

/// Exact, deterministic placement request.
///
/// Trust and data-boundary values are explicit sets. The Router never interprets their enum order
/// as an implicit security policy. Preference vectors are optional and ordered from most to least
/// preferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRequest {
    pub capability: CapabilityRef,
    pub target_platform: Platform,
    pub required_features: RequiredRouteFeatures,
    pub allowed_trust_levels: Vec<TrustLevel>,
    pub allowed_data_boundaries: Vec<DataBoundary>,
    pub trust_preference: Vec<TrustLevel>,
    pub data_boundary_preference: Vec<DataBoundary>,
    pub preferred_instance_ids: Vec<String>,
    pub pinned_instance_id: Option<String>,
}

/// Whether a candidate passed candidate-local placement filters.
///
/// Request-wide policy is evaluated after this classification, so placement eligibility never
/// implies execution permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateEligibility {
    PlacementEligible,
    Rejected,
}

/// Stable candidate audit record. Candidate order is always Instance-ID byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCandidateEvaluation {
    pub instance_id: String,
    pub eligibility: CandidateEligibility,
    pub reasons: Vec<RouteReason>,
}

/// Stable hard-filter and informational reason taxonomy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteReason {
    PlatformOsMismatch,
    PlatformArchitectureMismatch,
    HealthDegraded,
    HealthUnavailable,
    HealthUnknown,
    AuthRequired,
    AuthExpired,
    TrustNotAllowed,
    DataBoundaryNotAllowed,
    DefinitionExtensionUnsupported { extension: String },
    InstanceExtensionUnsupported { extension: String },
    ExecutionStyleUnsupported { style: ExecutionStyle },
    CancellationUnsupported,
    IdempotencyKeyUnsupported,
    IdempotencyQueryUnsupported,
    InvalidCostHint,
    InvalidReliabilityHint,
}

impl RouteReason {
    /// Machine-stable code for journals, logs, and golden tests.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PlatformOsMismatch => "platform_os_mismatch",
            Self::PlatformArchitectureMismatch => "platform_arch_mismatch",
            Self::HealthDegraded => "health_degraded",
            Self::HealthUnavailable => "health_unavailable",
            Self::HealthUnknown => "health_unknown",
            Self::AuthRequired => "auth_required",
            Self::AuthExpired => "auth_expired",
            Self::TrustNotAllowed => "trust_not_allowed",
            Self::DataBoundaryNotAllowed => "data_boundary_not_allowed",
            Self::DefinitionExtensionUnsupported { .. } => "definition_extension_unsupported",
            Self::InstanceExtensionUnsupported { .. } => "instance_extension_unsupported",
            Self::ExecutionStyleUnsupported { .. } => "execution_style_unsupported",
            Self::CancellationUnsupported => "cancellation_unsupported",
            Self::IdempotencyKeyUnsupported => "idempotency_key_unsupported",
            Self::IdempotencyQueryUnsupported => "idempotency_query_unsupported",
            Self::InvalidCostHint => "invalid_cost_hint",
            Self::InvalidReliabilityHint => "invalid_reliability_hint",
        }
    }

    const fn rejects_candidate(&self) -> bool {
        !matches!(self, Self::HealthDegraded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSelectionReason {
    Pinned,
    OnlyPlacementEligibleCandidate,
    LexicographicRanking,
}

impl RouteSelectionReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Pinned => "selected_by_pin",
            Self::OnlyPlacementEligibleCandidate => "selected_only_eligible_candidate",
            Self::LexicographicRanking => "selected_lexicographically",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteInteractionReason {
    PolicyApprovalRequired,
    CriticalApprovalRequired,
}

impl RouteInteractionReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::PolicyApprovalRequired => "policy_approval_required",
            Self::CriticalApprovalRequired => "critical_approval_required",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteBlockReason {
    NoCandidates,
    NoPlacementEligibleCandidates,
    PolicyMissing,
    PolicyDenied,
    PinnedInstanceNotFound,
    PinnedInstanceCapabilityMismatch,
    PinnedInstanceIneligible,
}

impl RouteBlockReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoCandidates => "no_candidates",
            Self::NoPlacementEligibleCandidates => "no_placement_eligible_candidates",
            Self::PolicyMissing => "policy_missing",
            Self::PolicyDenied => "policy_denied",
            Self::PinnedInstanceNotFound => "pinned_instance_not_found",
            Self::PinnedInstanceCapabilityMismatch => "pinned_instance_capability_mismatch",
            Self::PinnedInstanceIneligible => "pinned_instance_ineligible",
        }
    }
}

/// Deterministic placement result. `Selected` is not an execution grant or `InvocationPlan`.
#[must_use = "a route outcome must be handled before any execution is attempted"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    Selected {
        selected_instance_id: String,
        ranked_instance_ids: Vec<String>,
        candidates: Vec<RouteCandidateEvaluation>,
        reason: RouteSelectionReason,
    },
    InteractionRequired {
        ranked_instance_ids: Vec<String>,
        candidates: Vec<RouteCandidateEvaluation>,
        reasons: Vec<RouteInteractionReason>,
        policy_reasons: Vec<PolicyReason>,
    },
    Blocked {
        ranked_instance_ids: Vec<String>,
        candidates: Vec<RouteCandidateEvaluation>,
        reasons: Vec<RouteBlockReason>,
        policy_reasons: Vec<PolicyReason>,
    },
}

/// Fail-closed request-shape errors. These are not candidate rejections.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RouteInputError {
    #[error("target platform must use a concrete OS and architecture")]
    TargetPlatformMustBeConcrete,
    #[error("allowed trust-level set must not be empty")]
    AllowedTrustLevelsEmpty,
    #[error("allowed data-boundary set must not be empty")]
    AllowedDataBoundariesEmpty,
    #[error("allowed trust level `{trust:?}` is duplicated")]
    DuplicateAllowedTrustLevel { trust: TrustLevel },
    #[error("allowed data boundary `{boundary:?}` is duplicated")]
    DuplicateAllowedDataBoundary { boundary: DataBoundary },
    #[error("trust preference `{trust:?}` is not in the allowed set")]
    TrustPreferenceNotAllowed { trust: TrustLevel },
    #[error("trust preference `{trust:?}` is duplicated")]
    DuplicateTrustPreference { trust: TrustLevel },
    #[error("data-boundary preference `{boundary:?}` is not in the allowed set")]
    DataBoundaryPreferenceNotAllowed { boundary: DataBoundary },
    #[error("data-boundary preference `{boundary:?}` is duplicated")]
    DuplicateDataBoundaryPreference { boundary: DataBoundary },
    #[error("preferred Instance ID must not be empty")]
    EmptyPreferredInstanceId,
    #[error("preferred Instance ID `{instance_id}` is duplicated")]
    DuplicatePreferredInstanceId { instance_id: String },
    #[error("pinned Instance ID must not be empty")]
    EmptyPinnedInstanceId,
    #[error(
        "Capability Definition `{capability_id}` contract version `{contract_version}` was not found"
    )]
    DefinitionNotFound {
        capability_id: String,
        contract_version: String,
    },
    #[error("bound policy Capability does not match the route Capability")]
    PolicyCapabilityMismatch,
    #[error(
        "bound policy effect class `{actual:?}` does not match Definition effect class `{expected:?}`"
    )]
    PolicyEffectClassMismatch {
        expected: EffectClass,
        actual: EffectClass,
    },
    #[error("bound policy critical actions do not match the Definition")]
    PolicyCriticalActionsMismatch,
    #[error("bound policy scopes do not match the Definition resource selectors")]
    PolicyScopesMismatch,
}

/// I/O-free deterministic Capability placement engine.
#[derive(Debug, Default, Clone, Copy)]
pub struct CapabilityRouter;

impl CapabilityRouter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Hard-filter exact-version candidates, rank survivors, then apply pin and policy gates.
    ///
    /// A provisional policy allow only permits placement. The returned Instance must still cross
    /// the later Run-bound grant issuer and Executor enforcement boundary.
    ///
    /// # Errors
    ///
    /// Returns a request-shape error for non-concrete platforms, ambiguous preferences, or a
    /// missing exact Definition. Candidate failures are represented in `RouteOutcome::Blocked`.
    pub fn route(
        &self,
        registry: &CapabilityRegistry,
        request: &RouteRequest,
        policy: Option<&BoundPolicyEvaluation>,
    ) -> Result<RouteOutcome, RouteInputError> {
        validate_request(request)?;
        let definition = registry.definition(&request.capability).ok_or_else(|| {
            RouteInputError::DefinitionNotFound {
                capability_id: request.capability.capability_id.clone(),
                contract_version: request.capability.contract_version.clone(),
            }
        })?;
        validate_policy_binding(definition, request, policy)?;

        let instances = registry
            .instances_for(&request.capability)
            .collect::<Vec<_>>();
        let candidates = instances
            .iter()
            .map(|instance| evaluate_candidate(instance, definition, request))
            .collect::<Vec<_>>();

        if let Some(reason) = pin_block_reason(registry, request, &candidates) {
            return Ok(blocked(candidates, reason));
        }
        if request.pinned_instance_id.is_none() && candidates.is_empty() {
            return Ok(blocked(candidates, RouteBlockReason::NoCandidates));
        }

        let ranked_instance_ids = rank_candidates(instances, &candidates, request);
        if ranked_instance_ids.is_empty() {
            let reason = if request.pinned_instance_id.is_some() {
                RouteBlockReason::PinnedInstanceIneligible
            } else {
                RouteBlockReason::NoPlacementEligibleCandidates
            };
            return Ok(blocked(candidates, reason));
        }
        Ok(apply_policy(
            definition,
            request,
            policy,
            candidates,
            ranked_instance_ids,
        ))
    }
}

fn validate_policy_binding(
    definition: &CapabilityDefinitionBody,
    request: &RouteRequest,
    policy: Option<&BoundPolicyEvaluation>,
) -> Result<(), RouteInputError> {
    let Some(policy) = policy else {
        return Ok(());
    };
    let bound_request = policy.request();
    if bound_request.capability() != &request.capability {
        return Err(RouteInputError::PolicyCapabilityMismatch);
    }
    if bound_request.effect_class() != definition.spec.effect.class {
        return Err(RouteInputError::PolicyEffectClassMismatch {
            expected: definition.spec.effect.class,
            actual: bound_request.effect_class(),
        });
    }
    if !same_members(
        bound_request.critical_actions(),
        &definition.spec.effect.critical_actions,
    ) {
        return Err(RouteInputError::PolicyCriticalActionsMismatch);
    }

    let mut expected_scopes = definition
        .spec
        .effect
        .resource_selectors
        .iter()
        .map(|selector| selector.scope.clone())
        .collect::<Vec<_>>();
    expected_scopes.sort();
    expected_scopes.dedup();
    let mut actual_scopes = bound_request.requested_scopes().to_vec();
    actual_scopes.sort();
    actual_scopes.dedup();
    if expected_scopes != actual_scopes {
        return Err(RouteInputError::PolicyScopesMismatch);
    }
    Ok(())
}

fn same_members<T: PartialEq>(left: &[T], right: &[T]) -> bool {
    left.len() == right.len() && left.iter().all(|item| right.contains(item))
}

fn pin_block_reason(
    registry: &CapabilityRegistry,
    request: &RouteRequest,
    candidates: &[RouteCandidateEvaluation],
) -> Option<RouteBlockReason> {
    let pin = request.pinned_instance_id.as_deref()?;
    let Some(instance) = registry.instance(pin) else {
        return Some(RouteBlockReason::PinnedInstanceNotFound);
    };
    if instance.definition != request.capability {
        return Some(RouteBlockReason::PinnedInstanceCapabilityMismatch);
    }
    candidates
        .iter()
        .find(|candidate| candidate.instance_id == pin)
        .is_none_or(|candidate| candidate.eligibility != CandidateEligibility::PlacementEligible)
        .then_some(RouteBlockReason::PinnedInstanceIneligible)
}

fn rank_candidates(
    instances: Vec<&CapabilityInstanceBody>,
    candidates: &[RouteCandidateEvaluation],
    request: &RouteRequest,
) -> Vec<String> {
    let mut ranked = instances
        .into_iter()
        .zip(candidates)
        .filter_map(|(instance, candidate)| {
            (candidate.eligibility == CandidateEligibility::PlacementEligible).then_some(instance)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| compare_candidates(left, right, request));
    if let Some(pin) = request.pinned_instance_id.as_deref() {
        let Some(pin_index) = ranked
            .iter()
            .position(|instance| instance.instance_id == pin)
        else {
            return Vec::new();
        };
        let pinned = ranked.remove(pin_index);
        ranked.insert(0, pinned);
    }
    ranked
        .into_iter()
        .map(|instance| instance.instance_id.clone())
        .collect()
}

fn apply_policy(
    definition: &CapabilityDefinitionBody,
    request: &RouteRequest,
    policy: Option<&BoundPolicyEvaluation>,
    candidates: Vec<RouteCandidateEvaluation>,
    ranked_instance_ids: Vec<String>,
) -> RouteOutcome {
    match policy.map(BoundPolicyEvaluation::outcome) {
        None => blocked_with_policy(
            candidates,
            ranked_instance_ids,
            RouteBlockReason::PolicyMissing,
            Vec::new(),
        ),
        Some(BrokerOutcome::Deny { reasons, .. }) => blocked_with_policy(
            candidates,
            ranked_instance_ids,
            RouteBlockReason::PolicyDenied,
            reasons.clone(),
        ),
        Some(BrokerOutcome::Ask { reasons, .. }) => RouteOutcome::InteractionRequired {
            ranked_instance_ids,
            candidates,
            reasons: interaction_reasons(definition),
            policy_reasons: reasons.clone(),
        },
        Some(BrokerOutcome::Allow { .. })
            if !definition.spec.effect.critical_actions.is_empty() =>
        {
            RouteOutcome::InteractionRequired {
                ranked_instance_ids,
                candidates,
                reasons: vec![RouteInteractionReason::CriticalApprovalRequired],
                policy_reasons: Vec::new(),
            }
        }
        Some(BrokerOutcome::Allow { .. }) => {
            let Some(selected_instance_id) = ranked_instance_ids.first().cloned() else {
                return blocked(candidates, RouteBlockReason::NoPlacementEligibleCandidates);
            };
            RouteOutcome::Selected {
                selected_instance_id,
                reason: selection_reason(request, ranked_instance_ids.len()),
                ranked_instance_ids,
                candidates,
            }
        }
    }
}

const fn selection_reason(request: &RouteRequest, candidate_count: usize) -> RouteSelectionReason {
    if request.pinned_instance_id.is_some() {
        RouteSelectionReason::Pinned
    } else if candidate_count == 1 {
        RouteSelectionReason::OnlyPlacementEligibleCandidate
    } else {
        RouteSelectionReason::LexicographicRanking
    }
}

fn validate_request(request: &RouteRequest) -> Result<(), RouteInputError> {
    if request.target_platform.os == OperatingSystem::Any
        || request.target_platform.arch == Architecture::Any
    {
        return Err(RouteInputError::TargetPlatformMustBeConcrete);
    }
    if request.allowed_trust_levels.is_empty() {
        return Err(RouteInputError::AllowedTrustLevelsEmpty);
    }
    if request.allowed_data_boundaries.is_empty() {
        return Err(RouteInputError::AllowedDataBoundariesEmpty);
    }
    if let Some(trust) = first_duplicate(&request.allowed_trust_levels) {
        return Err(RouteInputError::DuplicateAllowedTrustLevel { trust });
    }
    if let Some(boundary) = first_duplicate(&request.allowed_data_boundaries) {
        return Err(RouteInputError::DuplicateAllowedDataBoundary { boundary });
    }
    if let Some(trust) = request
        .trust_preference
        .iter()
        .copied()
        .find(|trust| !request.allowed_trust_levels.contains(trust))
    {
        return Err(RouteInputError::TrustPreferenceNotAllowed { trust });
    }
    if let Some(trust) = first_duplicate(&request.trust_preference) {
        return Err(RouteInputError::DuplicateTrustPreference { trust });
    }
    if let Some(boundary) = request
        .data_boundary_preference
        .iter()
        .copied()
        .find(|boundary| !request.allowed_data_boundaries.contains(boundary))
    {
        return Err(RouteInputError::DataBoundaryPreferenceNotAllowed { boundary });
    }
    if let Some(boundary) = first_duplicate(&request.data_boundary_preference) {
        return Err(RouteInputError::DuplicateDataBoundaryPreference { boundary });
    }
    if request.preferred_instance_ids.iter().any(String::is_empty) {
        return Err(RouteInputError::EmptyPreferredInstanceId);
    }
    if let Some(instance_id) = first_duplicate_ref(&request.preferred_instance_ids) {
        return Err(RouteInputError::DuplicatePreferredInstanceId {
            instance_id: instance_id.clone(),
        });
    }
    if request
        .pinned_instance_id
        .as_ref()
        .is_some_and(String::is_empty)
    {
        return Err(RouteInputError::EmptyPinnedInstanceId);
    }
    Ok(())
}

fn first_duplicate<T: Copy + PartialEq>(values: &[T]) -> Option<T> {
    values
        .iter()
        .enumerate()
        .find_map(|(index, value)| values[..index].contains(value).then_some(*value))
}

fn first_duplicate_ref<T: PartialEq>(values: &[T]) -> Option<&T> {
    values
        .iter()
        .enumerate()
        .find_map(|(index, value)| values[..index].contains(value).then_some(value))
}

fn evaluate_candidate(
    instance: &CapabilityInstanceBody,
    definition: &CapabilityDefinitionBody,
    request: &RouteRequest,
) -> RouteCandidateEvaluation {
    let mut reasons = Vec::new();
    if instance.platform.os != OperatingSystem::Any
        && instance.platform.os != request.target_platform.os
    {
        reasons.push(RouteReason::PlatformOsMismatch);
    }
    if instance.platform.arch != Architecture::Any
        && instance.platform.arch != request.target_platform.arch
    {
        reasons.push(RouteReason::PlatformArchitectureMismatch);
    }
    match instance.health.status {
        HealthStatus::Available => {}
        HealthStatus::Degraded => reasons.push(RouteReason::HealthDegraded),
        HealthStatus::Unavailable => reasons.push(RouteReason::HealthUnavailable),
        HealthStatus::Unknown => reasons.push(RouteReason::HealthUnknown),
    }
    match instance.auth.state {
        AuthState::NotRequired | AuthState::Available => {}
        AuthState::Required => reasons.push(RouteReason::AuthRequired),
        AuthState::Expired => reasons.push(RouteReason::AuthExpired),
    }
    if !request.allowed_trust_levels.contains(&instance.trust) {
        reasons.push(RouteReason::TrustNotAllowed);
    }
    if !request
        .allowed_data_boundaries
        .contains(&instance.data_boundary)
    {
        reasons.push(RouteReason::DataBoundaryNotAllowed);
    }

    for extension in required_extensions(&definition.required_extensions) {
        reasons.push(RouteReason::DefinitionExtensionUnsupported { extension });
    }
    for extension in required_extensions(&instance.required_extensions) {
        reasons.push(RouteReason::InstanceExtensionUnsupported { extension });
    }

    let supports_style = match request.required_features.execution_style {
        ExecutionStyle::Sync => instance.features.sync,
        ExecutionStyle::Task => instance.features.task,
    };
    if !supports_style {
        reasons.push(RouteReason::ExecutionStyleUnsupported {
            style: request.required_features.execution_style,
        });
    }
    if request.required_features.cancellation && !instance.features.cancellable {
        reasons.push(RouteReason::CancellationUnsupported);
    }
    if request.required_features.idempotency_key
        && !definition.spec.execution.idempotency_key_supported
    {
        reasons.push(RouteReason::IdempotencyKeyUnsupported);
    }
    if request.required_features.idempotency_query && !instance.features.idempotency_query {
        reasons.push(RouteReason::IdempotencyQueryUnsupported);
    }
    if instance
        .hints
        .as_ref()
        .and_then(|hints| hints.monetary_cost)
        .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
    {
        reasons.push(RouteReason::InvalidCostHint);
    }
    if instance
        .hints
        .as_ref()
        .and_then(|hints| hints.reliability)
        .is_some_and(|reliability| !reliability.is_finite() || !(0.0..=1.0).contains(&reliability))
    {
        reasons.push(RouteReason::InvalidReliabilityHint);
    }

    let eligibility = if reasons.iter().any(RouteReason::rejects_candidate) {
        CandidateEligibility::Rejected
    } else {
        CandidateEligibility::PlacementEligible
    };
    RouteCandidateEvaluation {
        instance_id: instance.instance_id.clone(),
        eligibility,
        reasons,
    }
}

fn required_extensions(required: &[String]) -> Vec<String> {
    let mut required = required.to_vec();
    required.sort();
    required.dedup();
    required
}

fn compare_candidates(
    left: &CapabilityInstanceBody,
    right: &CapabilityInstanceBody,
    request: &RouteRequest,
) -> Ordering {
    health_rank(left.health.status)
        .cmp(&health_rank(right.health.status))
        .then_with(|| compare_optional_f64_desc(reliability(left), reliability(right)))
        .then_with(|| {
            preference_rank(&request.trust_preference, &left.trust)
                .cmp(&preference_rank(&request.trust_preference, &right.trust))
        })
        .then_with(|| {
            preference_rank(&request.data_boundary_preference, &left.data_boundary).cmp(
                &preference_rank(&request.data_boundary_preference, &right.data_boundary),
            )
        })
        .then_with(|| compare_optional_ord_asc(latency(left), latency(right)))
        .then_with(|| compare_optional_f64_asc(cost(left), cost(right)))
        .then_with(|| {
            preference_rank(&request.preferred_instance_ids, &left.instance_id).cmp(
                &preference_rank(&request.preferred_instance_ids, &right.instance_id),
            )
        })
        .then_with(|| left.instance_id.cmp(&right.instance_id))
}

const fn health_rank(status: HealthStatus) -> u8 {
    match status {
        HealthStatus::Available => 0,
        HealthStatus::Degraded => 1,
        HealthStatus::Unavailable | HealthStatus::Unknown => 2,
    }
}

fn preference_rank<T: PartialEq>(preference: &[T], value: &T) -> usize {
    preference
        .iter()
        .position(|candidate| candidate == value)
        .unwrap_or(usize::MAX)
}

fn reliability(instance: &CapabilityInstanceBody) -> Option<f64> {
    instance.hints.as_ref().and_then(|hints| hints.reliability)
}

fn latency(instance: &CapabilityInstanceBody) -> Option<u64> {
    instance.hints.as_ref().and_then(|hints| hints.latency_ms)
}

fn cost(instance: &CapabilityInstanceBody) -> Option<f64> {
    instance
        .hints
        .as_ref()
        .and_then(|hints| hints.monetary_cost)
}

fn compare_optional_ord_asc<T: Ord>(left: Option<T>, right: Option<T>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_optional_f64_asc(left: Option<f64>, right: Option<f64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => normalize_zero(left).total_cmp(&normalize_zero(right)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_optional_f64_desc(left: Option<f64>, right: Option<f64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => normalize_zero(right).total_cmp(&normalize_zero(left)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn normalize_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

fn interaction_reasons(definition: &CapabilityDefinitionBody) -> Vec<RouteInteractionReason> {
    let mut reasons = Vec::new();
    if !definition.spec.effect.critical_actions.is_empty() {
        reasons.push(RouteInteractionReason::CriticalApprovalRequired);
    }
    reasons.push(RouteInteractionReason::PolicyApprovalRequired);
    reasons
}

fn blocked(candidates: Vec<RouteCandidateEvaluation>, reason: RouteBlockReason) -> RouteOutcome {
    blocked_with_policy(candidates, Vec::new(), reason, Vec::new())
}

fn blocked_with_policy(
    candidates: Vec<RouteCandidateEvaluation>,
    ranked_instance_ids: Vec<String>,
    reason: RouteBlockReason,
    policy_reasons: Vec<PolicyReason>,
) -> RouteOutcome {
    RouteOutcome::Blocked {
        ranked_instance_ids,
        candidates,
        reasons: vec![reason],
        policy_reasons,
    }
}
