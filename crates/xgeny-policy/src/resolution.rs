use std::collections::BTreeSet;

use thiserror::Error;
use xgeny_domain::{
    API_VERSION_V1ALPHA1, CapabilityRef, CriticalAction, EffectClass, GrantLifetime,
    PermissionRequestBody,
};

/// Trusted, scope-specific canonicalization boundary used before policy evaluation.
///
/// Implementations belong to host adapters. Returning a string asserts that the adapter has
/// applied the canonicalization and containment rules required by that scope. The broker never
/// treats the wire-level `normalized` boolean as proof and never performs path-prefix matching.
pub trait ResourceResolver {
    /// Return the canonical identity used for exact authorization comparison.
    ///
    /// # Errors
    ///
    /// Returns a stable, non-sensitive rejection code when the resource cannot be safely
    /// resolved or falls outside the host boundary.
    fn resolve(&self, scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure>;
}

impl<T: ResourceResolver + ?Sized> ResourceResolver for &T {
    fn resolve(&self, scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        (**self).resolve(scope, resource)
    }
}

/// Closed, non-sensitive rejection taxonomy returned by a trusted resource resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ResourceResolutionFailure {
    #[error("resource is invalid")]
    InvalidResource,
    #[error("resource is outside the host boundary")]
    OutsideHostBoundary,
    #[error("resource scope is unsupported")]
    UnsupportedScope,
    #[error("resource resolver is unavailable")]
    ResolverUnavailable,
}

impl ResourceResolutionFailure {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidResource => "invalid_resource",
            Self::OutsideHostBoundary => "outside_host_boundary",
            Self::UnsupportedScope => "unsupported_scope",
            Self::ResolverUnavailable => "resolver_unavailable",
        }
    }
}

/// Exact canonical resource identity. Fields are intentionally private so raw wire values cannot
/// be passed to the broker without crossing a trusted resolver.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConcreteResource {
    scope: String,
    canonical_resource: String,
}

impl ConcreteResource {
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub fn canonical_resource(&self) -> &str {
        &self.canonical_resource
    }
}

/// Permission request whose concrete resources have crossed a trusted resolver boundary.
///
/// Free-form reason, resource metadata, timestamps and extension payloads are deliberately not
/// retained as authority-bearing inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPermissionRequest {
    api_version: String,
    request_id: String,
    run_id: String,
    step_id: String,
    capability: CapabilityRef,
    effect_class: EffectClass,
    requested_scopes: Vec<String>,
    resources: Vec<ConcreteResource>,
    critical_actions: Vec<CriticalAction>,
    requested_lifetime: GrantLifetime,
}

impl ResolvedPermissionRequest {
    #[must_use]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
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
    pub const fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    #[must_use]
    pub const fn effect_class(&self) -> EffectClass {
        self.effect_class
    }

    #[must_use]
    pub fn requested_scopes(&self) -> &[String] {
        &self.requested_scopes
    }

    #[must_use]
    pub fn resources(&self) -> &[ConcreteResource] {
        &self.resources
    }

    #[must_use]
    pub fn critical_actions(&self) -> &[CriticalAction] {
        &self.critical_actions
    }

    #[must_use]
    pub const fn requested_lifetime(&self) -> GrantLifetime {
        self.requested_lifetime
    }
}

/// Converts schema-validated wire requests into broker-only resolved requests.
#[derive(Debug)]
pub struct PermissionRequestResolver<R> {
    resolver: R,
}

impl<R> PermissionRequestResolver<R> {
    #[must_use]
    pub fn new(resolver: R) -> Self {
        Self { resolver }
    }
}

impl<R: ResourceResolver> PermissionRequestResolver<R> {
    /// Resolve a request that has already passed the protocol JSON Schema boundary.
    ///
    /// This method still checks security-relevant cross-field invariants that JSON Schema cannot
    /// express and invokes the trusted resolver for every resource.
    ///
    /// # Errors
    ///
    /// Fails closed for unsupported protocol features, inconsistent scope/resource sets,
    /// unnormalized wire assertions, resolver rejection, empty canonical identities or aliases
    /// that collapse to the same concrete resource.
    pub fn resolve_schema_validated(
        &self,
        request: &PermissionRequestBody,
    ) -> Result<ResolvedPermissionRequest, RequestResolutionError> {
        if request.api_version != API_VERSION_V1ALPHA1 {
            return Err(RequestResolutionError::UnsupportedApiVersion {
                actual: request.api_version.clone(),
            });
        }

        let unsupported_extensions: Vec<_> = request
            .required_extensions
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !unsupported_extensions.is_empty() {
            return Err(RequestResolutionError::UnsupportedRequiredExtensions {
                extensions: unsupported_extensions,
            });
        }

        if request.requested_scopes.is_empty() {
            return Err(RequestResolutionError::EmptyRequestedScopes);
        }
        let mut requested_scopes = BTreeSet::new();
        for scope in &request.requested_scopes {
            if scope.is_empty() {
                return Err(RequestResolutionError::EmptyRequestedScope);
            }
            if !requested_scopes.insert(scope.clone()) {
                return Err(RequestResolutionError::DuplicateRequestedScope {
                    scope: scope.clone(),
                });
            }
        }

        if request.resolved_resources.is_empty() {
            return Err(RequestResolutionError::EmptyResolvedResources);
        }
        let mut concrete_resources = BTreeSet::new();
        let mut represented_scopes = BTreeSet::new();
        for resource in &request.resolved_resources {
            if !resource.normalized {
                return Err(RequestResolutionError::UnnormalizedResource {
                    scope: resource.scope.clone(),
                });
            }
            if !requested_scopes.contains(&resource.scope) {
                return Err(RequestResolutionError::ResourceScopeNotRequested {
                    scope: resource.scope.clone(),
                });
            }
            if resource.resource.is_empty() {
                return Err(RequestResolutionError::EmptyResource {
                    scope: resource.scope.clone(),
                });
            }
            let canonical_resource = self
                .resolver
                .resolve(&resource.scope, &resource.resource)
                .map_err(|error| RequestResolutionError::ResolverRejected {
                    scope: resource.scope.clone(),
                    code: error.code().to_owned(),
                })?;
            if canonical_resource.is_empty() {
                return Err(RequestResolutionError::EmptyCanonicalResource {
                    scope: resource.scope.clone(),
                });
            }
            represented_scopes.insert(resource.scope.clone());
            if !concrete_resources.insert(ConcreteResource {
                scope: resource.scope.clone(),
                canonical_resource,
            }) {
                return Err(RequestResolutionError::DuplicateConcreteResource {
                    scope: resource.scope.clone(),
                });
            }
        }

        if let Some(scope) = requested_scopes
            .iter()
            .find(|scope| !represented_scopes.contains(*scope))
        {
            return Err(RequestResolutionError::MissingResourceForScope {
                scope: scope.clone(),
            });
        }

        let mut critical_actions = request.critical_actions.clone();
        critical_actions.sort_by_key(|action| critical_action_rank(*action));
        if critical_actions.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(RequestResolutionError::DuplicateCriticalAction);
        }

        Ok(ResolvedPermissionRequest {
            api_version: request.api_version.clone(),
            request_id: request.request_id.clone(),
            run_id: request.run_id.clone(),
            step_id: request.step_id.clone(),
            capability: request.capability.clone(),
            effect_class: request.effect_class,
            requested_scopes: requested_scopes.into_iter().collect(),
            resources: concrete_resources.into_iter().collect(),
            critical_actions,
            requested_lifetime: request.requested_lifetime,
        })
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RequestResolutionError {
    #[error("PermissionRequest uses unsupported API version `{actual}`")]
    UnsupportedApiVersion { actual: String },
    #[error("PermissionRequest requires unsupported extensions: {extensions:?}")]
    UnsupportedRequiredExtensions { extensions: Vec<String> },
    #[error("PermissionRequest has no requested scope")]
    EmptyRequestedScopes,
    #[error("PermissionRequest contains an empty requested scope")]
    EmptyRequestedScope,
    #[error("PermissionRequest repeats requested scope `{scope}`")]
    DuplicateRequestedScope { scope: String },
    #[error("PermissionRequest has no resolved resource")]
    EmptyResolvedResources,
    #[error("PermissionRequest resource for scope `{scope}` is not marked normalized")]
    UnnormalizedResource { scope: String },
    #[error("PermissionRequest resource scope `{scope}` was not requested")]
    ResourceScopeNotRequested { scope: String },
    #[error("PermissionRequest has no resource for requested scope `{scope}`")]
    MissingResourceForScope { scope: String },
    #[error("PermissionRequest contains an empty resource for scope `{scope}`")]
    EmptyResource { scope: String },
    #[error("trusted resolver rejected a resource in scope `{scope}` with code `{code}`")]
    ResolverRejected { scope: String, code: String },
    #[error("trusted resolver produced an empty canonical resource for scope `{scope}`")]
    EmptyCanonicalResource { scope: String },
    #[error("multiple resources resolve to the same concrete identity in scope `{scope}`")]
    DuplicateConcreteResource { scope: String },
    #[error("PermissionRequest repeats a critical action")]
    DuplicateCriticalAction,
}
