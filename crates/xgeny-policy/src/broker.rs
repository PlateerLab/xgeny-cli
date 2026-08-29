use std::collections::BTreeSet;

use thiserror::Error;
use xgeny_domain::{CriticalAction, GrantLifetime, PolicySource, PolicySourceKind};

use crate::{ConcreteResource, ResolvedPermissionRequest};

/// Maximum exact authority offered by one trusted policy layer for the current request.
///
/// Lifetimes are an explicit set. Their enum order is never interpreted as an authority order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyAllowance {
    scopes: BTreeSet<String>,
    resources: BTreeSet<ConcreteResource>,
    critical_actions: Vec<CriticalAction>,
    lifetimes: Vec<GrantLifetime>,
}

impl PolicyAllowance {
    #[must_use]
    pub fn from_trusted_evaluation<S, R, C, L>(
        scopes: S,
        resources: R,
        critical_actions: C,
        lifetimes: L,
    ) -> Self
    where
        S: IntoIterator<Item = String>,
        R: IntoIterator<Item = ConcreteResource>,
        C: IntoIterator<Item = CriticalAction>,
        L: IntoIterator<Item = GrantLifetime>,
    {
        let mut critical_actions: Vec<_> = critical_actions.into_iter().collect();
        critical_actions.sort_by_key(|action| critical_action_rank(*action));
        critical_actions.dedup();
        let mut lifetimes: Vec<_> = lifetimes.into_iter().collect();
        lifetimes.sort_by_key(|lifetime| lifetime_rank(*lifetime));
        lifetimes.dedup();
        Self {
            scopes: scopes.into_iter().collect(),
            resources: resources.into_iter().collect(),
            critical_actions,
            lifetimes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContributionVerdict {
    Allow(PolicyAllowance),
    Ask(String),
    Deny(String),
}

/// Pre-evaluated result from one trusted policy layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyContribution {
    source: PolicySource,
    verdict: ContributionVerdict,
}

impl PolicyContribution {
    #[must_use]
    pub fn allow(source: PolicySource, allowance: PolicyAllowance) -> Self {
        Self {
            source,
            verdict: ContributionVerdict::Allow(allowance),
        }
    }

    #[must_use]
    pub fn ask(source: PolicySource, reason_code: impl Into<String>) -> Self {
        Self {
            source,
            verdict: ContributionVerdict::Ask(reason_code.into()),
        }
    }

    #[must_use]
    pub fn deny(source: PolicySource, reason_code: impl Into<String>) -> Self {
        Self {
            source,
            verdict: ContributionVerdict::Deny(reason_code.into()),
        }
    }
}

/// Complete mandatory policy stack. A local stack always contains host and user policy, while a
/// managed stack additionally requires exactly one managed lease contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyInputs {
    Local {
        host: PolicyContribution,
        user_profile: PolicyContribution,
    },
    Managed {
        host: PolicyContribution,
        user_profile: PolicyContribution,
        managed_lease: PolicyContribution,
    },
}

impl PolicyInputs {
    #[must_use]
    pub const fn local(host: PolicyContribution, user_profile: PolicyContribution) -> Self {
        Self::Local { host, user_profile }
    }

    #[must_use]
    pub const fn managed(
        host: PolicyContribution,
        user_profile: PolicyContribution,
        managed_lease: PolicyContribution,
    ) -> Self {
        Self::Managed {
            host,
            user_profile,
            managed_lease,
        }
    }

    fn ordered(&self) -> Vec<(PolicySourceKind, &PolicyContribution)> {
        match self {
            Self::Local { host, user_profile } => vec![
                (PolicySourceKind::Host, host),
                (PolicySourceKind::UserProfile, user_profile),
            ],
            Self::Managed {
                host,
                user_profile,
                managed_lease,
            } => vec![
                (PolicySourceKind::Host, host),
                (PolicySourceKind::UserProfile, user_profile),
                (PolicySourceKind::ManagedLease, managed_lease),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyReason {
    source: Option<PolicySourceKind>,
    code: String,
}

impl PolicyReason {
    #[must_use]
    pub const fn source(&self) -> Option<PolicySourceKind> {
        self.source
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    fn from_source(source: PolicySourceKind, code: impl Into<String>) -> Self {
        Self {
            source: Some(source),
            code: code.into(),
        }
    }

    fn broker(code: impl Into<String>) -> Self {
        Self {
            source: None,
            code: code.into(),
        }
    }
}

/// Exact provisional scope produced by policy composition.
///
/// This type is intentionally not execution authority or a reusable wire `Grant`. It is not
/// bound to a canonical action digest and has no atomic use budget. An Executor must never accept
/// it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionalAuthorization {
    lifetime: GrantLifetime,
    scopes: Vec<String>,
    resources: Vec<ConcreteResource>,
    critical_actions: Vec<CriticalAction>,
}

impl ProvisionalAuthorization {
    #[must_use]
    pub const fn lifetime(&self) -> GrantLifetime {
        self.lifetime
    }

    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    #[must_use]
    pub fn resources(&self) -> &[ConcreteResource] {
        &self.resources
    }

    #[must_use]
    pub fn critical_actions(&self) -> &[CriticalAction] {
        &self.critical_actions
    }
}

/// Provisional policy-composition result. `Allow` is not execution authority until a later issuer
/// binds it to a Run/action identity and an atomically consumable use budget.
#[must_use = "a policy outcome must be handled, and provisional allow must not be executed directly"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerOutcome {
    Allow {
        provisional_authorization: ProvisionalAuthorization,
        sources: Vec<PolicySource>,
    },
    Ask {
        sources: Vec<PolicySource>,
        reasons: Vec<PolicyReason>,
    },
    Deny {
        sources: Vec<PolicySource>,
        reasons: Vec<PolicyReason>,
    },
}

/// Broker-produced request/outcome pair that prevents wiring a detached outcome by mistake.
///
/// Fields are private and this value can only be created by `PermissionBroker::evaluate_bound`.
/// It is still provisional policy evidence, not Executor authority or a reusable grant.
#[must_use = "a bound policy evaluation must be handled and never treated as execution authority"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundPolicyEvaluation {
    request: ResolvedPermissionRequest,
    outcome: BrokerOutcome,
}

impl BoundPolicyEvaluation {
    #[must_use]
    pub const fn request(&self) -> &ResolvedPermissionRequest {
        &self.request
    }

    pub const fn outcome(&self) -> &BrokerOutcome {
        &self.outcome
    }
}

/// I/O-free permission policy composer.
#[derive(Debug, Default, Clone, Copy)]
pub struct PermissionBroker;

impl PermissionBroker {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Evaluate policy and retain the exact resolved request beside its outcome.
    ///
    /// Router and later integration boundaries should consume this opaque pair instead of a bare
    /// `BrokerOutcome`, so the Router can compare the evaluated Capability and static effect
    /// contract before placement. This is not full Run, action, or resource binding.
    /// The result remains provisional and must not be accepted by an Executor.
    ///
    /// # Errors
    ///
    /// Returns the same malformed-evidence errors as `evaluate`.
    pub fn evaluate_bound(
        &self,
        request: &ResolvedPermissionRequest,
        inputs: &PolicyInputs,
    ) -> Result<BoundPolicyEvaluation, BrokerError> {
        let outcome = self.evaluate(request, inputs)?;
        Ok(BoundPolicyEvaluation {
            request: request.clone(),
            outcome,
        })
    }

    /// Compose mandatory policy layers with deterministic deny-over-ask precedence.
    ///
    /// An `Allow` outcome is provisional. This foundation deliberately has no Run-grant issuer or
    /// Executor integration.
    ///
    /// # Errors
    ///
    /// Returns an error rather than a decision when policy evidence is malformed or assigned to
    /// the wrong mandatory layer. Callers must treat errors as non-executable.
    pub fn evaluate(
        &self,
        request: &ResolvedPermissionRequest,
        inputs: &PolicyInputs,
    ) -> Result<BrokerOutcome, BrokerError> {
        let layers = inputs.ordered();
        for (expected, contribution) in &layers {
            validate_contribution(*expected, contribution)?;
        }

        let sources = layers
            .iter()
            .map(|(_, contribution)| contribution.source.clone())
            .collect::<Vec<_>>();
        let mut deny_reasons = Vec::new();
        let mut ask_reasons = Vec::new();

        for (kind, contribution) in layers {
            match &contribution.verdict {
                ContributionVerdict::Deny(code) => {
                    deny_reasons.push(PolicyReason::from_source(kind, code.clone()));
                }
                ContributionVerdict::Ask(code) => {
                    ask_reasons.push(PolicyReason::from_source(kind, code.clone()));
                }
                ContributionVerdict::Allow(allowance) => {
                    append_coverage_denials(request, kind, allowance, &mut deny_reasons);
                }
            }
        }

        if !deny_reasons.is_empty() {
            return Ok(BrokerOutcome::Deny {
                sources,
                reasons: deny_reasons,
            });
        }

        if !request.critical_actions().is_empty() {
            ask_reasons.push(PolicyReason::broker("critical_approval_required"));
        }
        if !ask_reasons.is_empty() {
            return Ok(BrokerOutcome::Ask {
                sources,
                reasons: ask_reasons,
            });
        }

        Ok(BrokerOutcome::Allow {
            provisional_authorization: ProvisionalAuthorization {
                lifetime: request.requested_lifetime(),
                scopes: request.requested_scopes().to_vec(),
                resources: request.resources().to_vec(),
                critical_actions: request.critical_actions().to_vec(),
            },
            sources,
        })
    }
}

fn validate_contribution(
    expected: PolicySourceKind,
    contribution: &PolicyContribution,
) -> Result<(), BrokerError> {
    let actual = contribution.source.kind;
    if actual != expected {
        return Err(BrokerError::SourceKindMismatch { expected, actual });
    }
    if contribution.source.id.is_empty()
        || contribution.source.id.chars().count() > 300
        || contribution.source.id.chars().any(char::is_control)
        || !valid_sha256_digest(&contribution.source.digest)
    {
        return Err(BrokerError::InvalidPolicySource { kind: actual });
    }
    let reason = match &contribution.verdict {
        ContributionVerdict::Ask(reason) | ContributionVerdict::Deny(reason) => Some(reason),
        ContributionVerdict::Allow(_) => None,
    };
    if reason.is_some_and(|reason| !valid_reason_code(reason)) {
        return Err(BrokerError::InvalidReasonCode { kind: actual });
    }
    Ok(())
}

fn append_coverage_denials(
    request: &ResolvedPermissionRequest,
    source: PolicySourceKind,
    allowance: &PolicyAllowance,
    reasons: &mut Vec<PolicyReason>,
) {
    if request
        .requested_scopes()
        .iter()
        .any(|scope| !allowance.scopes.contains(scope))
    {
        reasons.push(PolicyReason::from_source(source, "scope_not_allowed"));
    }
    if request
        .resources()
        .iter()
        .any(|resource| !allowance.resources.contains(resource))
    {
        reasons.push(PolicyReason::from_source(source, "resource_not_allowed"));
    }
    if request
        .critical_actions()
        .iter()
        .any(|action| !allowance.critical_actions.contains(action))
    {
        reasons.push(PolicyReason::from_source(
            source,
            "critical_action_not_allowed",
        ));
    }
    if !allowance.lifetimes.contains(&request.requested_lifetime()) {
        reasons.push(PolicyReason::from_source(source, "lifetime_not_allowed"));
    }
}

fn valid_sha256_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn valid_reason_code(reason: &str) -> bool {
    let mut bytes = reason.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && reason.len() <= 100
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

const fn critical_action_rank(action: CriticalAction) -> u8 {
    match action {
        CriticalAction::IrreversibleDelete => 0,
        CriticalAction::CredentialExport => 1,
        CriticalAction::PaymentOrPurchase => 2,
        CriticalAction::ExternalPublishOrMessage => 3,
        CriticalAction::ProductionDeploy => 4,
        CriticalAction::PrivilegeEscalation => 5,
        CriticalAction::PersistentStartup => 6,
    }
}

const fn lifetime_rank(lifetime: GrantLifetime) -> u8 {
    match lifetime {
        GrantLifetime::Once => 0,
        GrantLifetime::Run => 1,
        GrantLifetime::Session => 2,
        GrantLifetime::Project => 3,
        GrantLifetime::Persistent => 4,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BrokerError {
    #[error("policy layer expects source kind {expected:?}, but received {actual:?}")]
    SourceKindMismatch {
        expected: PolicySourceKind,
        actual: PolicySourceKind,
    },
    #[error("policy source evidence for {kind:?} is malformed")]
    InvalidPolicySource { kind: PolicySourceKind },
    #[error("policy reason code for {kind:?} is malformed")]
    InvalidReasonCode { kind: PolicySourceKind },
}
