use std::fmt;
use std::num::NonZeroU32;

use thiserror::Error;
use xgeny_local_store::{RunStore, StoreError};
use xgeny_policy::{PolicyInputs, ResolvedPermissionRequest, ResourceResolver};
use xgeny_runtime::{
    AdmissionError, AdmissionOutcome, AgentLoop, AgentLoopError, AgentLoopQuiescence,
    AgentLoopTick, CapabilityRegistry, DirectExecutor, DirectExecutorError, EffectAdapterRegistry,
    EffectVerifierRegistry, EventFactory, InvocationAdmission, InvocationMaterialRecovery,
    MaterialProviderRegistry, MaterialRecoveryError, PlanMaterializer, PlanMaterializerFailure,
    PlannedAdmissionRequest, PlannerPort, PlannerPortFailure, ProposalRejection, RouteOutcome,
    RouteRequest, RunLease, VerificationRunner, VerificationRunnerError,
};
use xgeny_workgraph::{
    CompletionCandidateState, CompletionOutputRecord, ContinuationAction, FrontierError,
    ModelCallRejectionReason, ModelCallUnknownReason, RunState, StepStatus, derive_frontier,
};

/// Host boundary that turns one durable planned Step into an exact deterministic route request.
pub trait PlannedRoutePort {
    /// Select route requirements without reading invocation arguments or performing an effect.
    ///
    /// # Errors
    ///
    /// Returns only a fixed, non-sensitive failure class.
    fn route_for(
        &mut self,
        state: &RunState,
        step_id: &str,
    ) -> Result<RouteRequest, PlannedRouteFailure>;
}

/// Closed failure taxonomy for host-owned route construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PlannedRouteFailure {
    #[error("planned route configuration is unavailable")]
    Unavailable,
    #[error("planned route configuration rejected the Step")]
    Rejected,
}

/// One host decision over the exact Core-derived permission request.
pub enum ApprovalDecision {
    Approved(Box<PolicyInputs>),
    Denied,
    Pending,
}

impl fmt::Debug for ApprovalDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Approved(_) => formatter.write_str("Approved(<redacted-policy-inputs>)"),
            Self::Denied => formatter.write_str("Denied"),
            Self::Pending => formatter.write_str("Pending"),
        }
    }
}

/// Host policy/UI boundary for one exact resolved permission request.
pub trait ApprovalPort {
    /// Decide without starting an effect. The approved policy inputs remain request-bound and Core
    /// rechecks the Run head before consuming them.
    ///
    /// # Errors
    ///
    /// Returns only a fixed, non-sensitive failure class.
    fn decide(
        &mut self,
        request: &ResolvedPermissionRequest,
    ) -> Result<ApprovalDecision, ApprovalPortFailure>;
}

/// Closed failure taxonomy for host-owned approval handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ApprovalPortFailure {
    #[error("approval service is unavailable")]
    Unavailable,
    #[error("approval response is invalid")]
    InvalidResponse,
}

/// Why a bounded driver invocation yielded control to its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverOutcome {
    CompletionCandidate {
        candidate: CompletionCandidateState,
        /// Exact local summary sidecar. `None` is reserved for replayed schema-7 candidates.
        output: Option<Box<CompletionOutputRecord>>,
    },
    Quiescent(AgentLoopQuiescence),
    ApprovalPending {
        step_id: String,
    },
    ApprovalDenied {
        step_id: String,
    },
    AdmissionNotAuthorized {
        step_id: String,
        outcome: RouteOutcome,
    },
    PlannerUnavailable(PlannerPortFailure),
    ProposalRejected(ProposalRejection),
    MaterializerUnavailable(PlanMaterializerFailure),
    ModelCallRecoveryRequired {
        call_id: String,
        reason: ModelCallUnknownReason,
    },
    ModelCallRejected(ModelCallRejectionReason),
    ModelEgressRequired,
    TickBudgetExhausted,
}

/// Provider-neutral bounded coordinator used by CLI composition roots and hermetic tests.
#[derive(Debug, Clone)]
pub struct RunDriver {
    agent_loop: AgentLoop,
    executor: DirectExecutor,
    verifier: VerificationRunner,
    max_ticks: NonZeroU32,
}

impl RunDriver {
    #[must_use]
    pub fn new(agent_loop: AgentLoop, max_ticks: NonZeroU32) -> Self {
        Self {
            agent_loop,
            executor: DirectExecutor::new(),
            verifier: VerificationRunner::new(),
            max_ticks,
        }
    }

    #[must_use]
    pub const fn with_executor(mut self, executor: DirectExecutor) -> Self {
        self.executor = executor;
        self
    }

    /// Drive durable work until user/external intervention, terminal quiescence, completion, or
    /// the host-selected `AgentLoop` tick bound is reached. Configuration and planning lifecycle
    /// ticks count toward this bound even when they do not drive an external action.
    ///
    /// This method never invents route or approval policy. It also never retries an unresolved
    /// model/effect uncertainty; those conditions are returned to the composition root.
    ///
    /// # Errors
    ///
    /// Fails closed on any Core, store, reconstruction, route, approval, execution, or verification
    /// error. Error variants do not contain raw invocation arguments or tool output.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn drive_until_pause<S, F, L, R, P, M, RP, AP>(
        &self,
        store: &mut S,
        events: &mut F,
        lease: &L,
        capabilities: &CapabilityRegistry,
        resolver: &R,
        planner: &mut P,
        materializer: &mut M,
        providers: &mut MaterialProviderRegistry,
        adapters: &mut EffectAdapterRegistry,
        verifiers: &mut EffectVerifierRegistry,
        routes: &mut RP,
        approvals: &mut AP,
    ) -> Result<DriverOutcome, RunDriverError>
    where
        S: RunStore,
        F: EventFactory,
        L: RunLease,
        R: ResourceResolver,
        P: PlannerPort,
        M: PlanMaterializer,
        RP: PlannedRoutePort,
        AP: ApprovalPort,
    {
        self.drive_until_pause_with_model_egress(
            store,
            events,
            lease,
            capabilities,
            resolver,
            planner,
            materializer,
            providers,
            adapters,
            verifiers,
            routes,
            approvals,
            true,
        )
    }

    /// Drive local frontier actions while stopping before a new model-call reservation when model
    /// egress is not authorized for this invocation.
    ///
    /// # Errors
    ///
    /// Has the same fail-closed behavior as [`Self::drive_until_pause`].
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn drive_until_pause_with_model_egress<S, F, L, R, P, M, RP, AP>(
        &self,
        store: &mut S,
        events: &mut F,
        lease: &L,
        capabilities: &CapabilityRegistry,
        resolver: &R,
        planner: &mut P,
        materializer: &mut M,
        providers: &mut MaterialProviderRegistry,
        adapters: &mut EffectAdapterRegistry,
        verifiers: &mut EffectVerifierRegistry,
        routes: &mut RP,
        approvals: &mut AP,
        model_egress_allowed: bool,
    ) -> Result<DriverOutcome, RunDriverError>
    where
        S: RunStore,
        F: EventFactory,
        L: RunLease,
        R: ResourceResolver,
        P: PlannerPort,
        M: PlanMaterializer,
        RP: PlannedRoutePort,
        AP: ApprovalPort,
    {
        for _ in 0..self.max_ticks.get() {
            if !model_egress_allowed && model_call_may_be_next(store)? {
                return Ok(DriverOutcome::ModelEgressRequired);
            }
            let tick = self.agent_loop.tick(
                store,
                events,
                lease,
                capabilities,
                resolver,
                planner,
                materializer,
            )?;
            match tick {
                AgentLoopTick::Configured { .. }
                | AgentLoopTick::ModelCallLifecycleConfigured { .. }
                | AgentLoopTick::PlanAccepted { .. }
                | AgentLoopTick::ModelCallAbandoned { .. } => {}
                AgentLoopTick::ActionRequired { action, .. } => match action.action {
                    ContinuationAction::Admit => {
                        let state = store
                            .load_current()?
                            .ok_or(RunDriverError::RunNotInitialized)?;
                        let route = routes.route_for(&state, &action.step_id)?;
                        let pending = InvocationMaterialRecovery::new().prepare_planned_admission(
                            store,
                            lease,
                            capabilities,
                            resolver,
                            providers,
                            PlannedAdmissionRequest::new(&action.step_id, route),
                        )?;
                        match approvals.decide(pending.permission_request())? {
                            ApprovalDecision::Pending => {
                                return Ok(DriverOutcome::ApprovalPending {
                                    step_id: action.step_id,
                                });
                            }
                            ApprovalDecision::Denied => {
                                return Ok(DriverOutcome::ApprovalDenied {
                                    step_id: action.step_id,
                                });
                            }
                            ApprovalDecision::Approved(policy_inputs) => {
                                match InvocationAdmission::new().authorize_and_commit(
                                    pending,
                                    &policy_inputs,
                                    capabilities,
                                    store,
                                    events,
                                    lease,
                                )? {
                                    AdmissionOutcome::Authorized(_) => {}
                                    AdmissionOutcome::NotAuthorized(outcome) => {
                                        return Ok(DriverOutcome::AdmissionNotAuthorized {
                                            step_id: action.step_id,
                                            outcome,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    ContinuationAction::DriveEffect => {
                        let state = store
                            .load_current()?
                            .ok_or(RunDriverError::RunNotInitialized)?;
                        let status = state
                            .steps
                            .get(&action.step_id)
                            .ok_or_else(|| RunDriverError::StepNotFound(action.step_id.clone()))?
                            .status;
                        if status == StepStatus::IntentCommitted {
                            let material = InvocationMaterialRecovery::new().recover(
                                store,
                                lease,
                                capabilities,
                                resolver,
                                providers,
                                &action.step_id,
                            )?;
                            self.executor.drive_step(
                                store,
                                events,
                                lease,
                                capabilities,
                                adapters,
                                &action.step_id,
                                Some(&material),
                            )?;
                        } else {
                            self.executor.drive_step(
                                store,
                                events,
                                lease,
                                capabilities,
                                adapters,
                                &action.step_id,
                                None,
                            )?;
                        }
                    }
                    ContinuationAction::Verify => {
                        self.verifier.drive_step(
                            store,
                            events,
                            lease,
                            capabilities,
                            verifiers,
                            &action.step_id,
                        )?;
                    }
                },
                AgentLoopTick::CompletionCandidate {
                    candidate, output, ..
                } => {
                    return Ok(DriverOutcome::CompletionCandidate { candidate, output });
                }
                AgentLoopTick::Quiescent { reason, .. } => {
                    return Ok(DriverOutcome::Quiescent(reason));
                }
                AgentLoopTick::PlannerUnavailable { failure, .. } => {
                    return Ok(DriverOutcome::PlannerUnavailable(failure));
                }
                AgentLoopTick::ProposalRejected { reason, .. } => {
                    return Ok(DriverOutcome::ProposalRejected(reason));
                }
                AgentLoopTick::MaterializerUnavailable { failure, .. } => {
                    return Ok(DriverOutcome::MaterializerUnavailable(failure));
                }
                AgentLoopTick::ModelCallRecoveryRequired {
                    call_id, reason, ..
                } => {
                    return Ok(DriverOutcome::ModelCallRecoveryRequired { call_id, reason });
                }
                AgentLoopTick::ModelCallRejected { reason, .. } => {
                    return Ok(DriverOutcome::ModelCallRejected(reason));
                }
            }
        }
        Ok(DriverOutcome::TickBudgetExhausted)
    }
}

fn model_call_may_be_next<S: RunStore>(store: &S) -> Result<bool, RunDriverError> {
    let Some(state) = store.load_current()? else {
        return Ok(false);
    };
    let Some(agent) = &state.agent_loop else {
        return Ok(false);
    };
    let Some(model_calls) = &agent.model_calls else {
        return Ok(false);
    };
    if model_calls.active_call.is_some() || agent.completion_candidate.is_some() {
        return Ok(false);
    }
    let frontier = derive_frontier(&state)?;
    if frontier.next_action().is_some()
        || !frontier.failed_step_ids.is_empty()
        || !frontier.manual_required_step_ids.is_empty()
        || agent.accepted_model_turns >= agent.budget.max_model_turns
        || model_calls.reserved_calls >= model_calls.budget.max_model_calls()
    {
        return Ok(false);
    }
    Ok(true)
}

#[derive(Debug, Error)]
pub enum RunDriverError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    AgentLoop(#[from] AgentLoopError),
    #[error(transparent)]
    MaterialRecovery(#[from] MaterialRecoveryError),
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error(transparent)]
    Executor(#[from] DirectExecutorError),
    #[error(transparent)]
    Verification(#[from] VerificationRunnerError),
    #[error(transparent)]
    Frontier(#[from] FrontierError),
    #[error(transparent)]
    Route(#[from] PlannedRouteFailure),
    #[error(transparent)]
    Approval(#[from] ApprovalPortFailure),
    #[error("durable Run is not initialized")]
    RunNotInitialized,
    #[error("frontier Step is missing from the durable Run")]
    StepNotFound(String),
}
