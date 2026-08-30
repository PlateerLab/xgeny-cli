use xgeny_workgraph::{
    AcceptedPlanStep, AgentLoopBudget, EventRecord, ExpectedPlanningTurn,
    ModelCallAbandonmentReason, ModelCallBudget, ModelCallRejectionReason, ModelCallReservation,
    ModelCallSettlement, ModelCallStatus, ModelCallUnknownReason, PlannedExecutionProfile,
    PlannedInvocationMaterialRecord, PlannedInvocationSpec, PlanningContractError,
    ReconstructableMaterialReference, RunEvent, RunEventBody, RunState, StepStatus,
    TransitionError, apply_record, replay,
};

const RUN_ID: &str = "run-durable-model-call";
const AUTHORITY: &str = "local:test";
const AUTHORITY_EPOCH: u64 = 7;

fn digest(byte: char) -> String {
    let hexadecimal = format!("{:x}", u32::from(byte));
    let encoded = hexadecimal.repeat(64).chars().take(64).collect::<String>();
    format!("sha256:{encoded}")
}

fn digest_text(value: &str) -> String {
    let marker = value.bytes().fold(0_u8, |digest, byte| {
        digest.wrapping_mul(31).wrapping_add(byte)
    });
    format!("sha256:{}", format!("{marker:02x}").repeat(32))
}

fn event(event_id: &str, body: RunEventBody) -> RunEvent {
    RunEvent {
        event_id: event_id.to_owned(),
        run_id: RUN_ID.to_owned(),
        authority: AUTHORITY.to_owned(),
        authority_epoch: AUTHORITY_EPOCH,
        recorded_at: "2026-08-30T00:00:00Z".to_owned(),
        body,
    }
}

fn record_after(state: Option<&RunState>, event_id: &str, body: RunEventBody) -> EventRecord {
    let previous = state.map(|state| EventRecord {
        sequence: state.journal_sequence,
        previous_digest: None,
        event: event(
            "synthetic-previous",
            RunEventBody::RunCreated {
                goal: String::new(),
            },
        ),
        digest: state.journal_head_digest.clone(),
    });
    EventRecord::next(previous.as_ref(), event(event_id, body))
        .expect("event record should canonicalize")
}

fn try_append(
    state: Option<&RunState>,
    event_id: &str,
    body: RunEventBody,
) -> Result<RunState, TransitionError> {
    apply_record(state, &record_after(state, event_id, body))
}

fn append(state: Option<&RunState>, event_id: &str, body: RunEventBody) -> RunState {
    try_append(state, event_id, body).expect("transition should succeed")
}

fn configured_state(model_turns: u32) -> RunState {
    let created = append(
        None,
        "run-created",
        RunEventBody::RunCreated {
            goal: "finish a durable model-owned task".to_owned(),
        },
    );
    append(
        Some(&created),
        "loop-configured",
        RunEventBody::AgentLoopConfigured {
            budget: AgentLoopBudget::new(model_turns, 16, 16, 262_144)
                .expect("loop budget should validate"),
        },
    )
}

fn configure_calls(state: &RunState, max_model_calls: u32) -> RunState {
    append(
        Some(state),
        "model-calls-configured",
        RunEventBody::ModelCallLifecycleConfigured {
            budget: ModelCallBudget::new(max_model_calls)
                .expect("model-call budget should validate"),
        },
    )
}

fn reservation(
    state: &RunState,
    call_index: u32,
    turn_index: u32,
    context_marker: char,
) -> ModelCallReservation {
    ModelCallReservation::new(
        &state.run_id,
        state.authority_epoch,
        "planner.test.v1",
        call_index,
        turn_index,
        state.journal_sequence,
        &state.journal_head_digest,
        digest(context_marker),
        digest('r'),
    )
    .expect("reservation should validate")
}

fn reserve(state: &RunState, reservation: &ModelCallReservation, event_id: &str) -> RunState {
    append(
        Some(state),
        event_id,
        RunEventBody::ModelCallReserved {
            reservation: reservation.clone(),
        },
    )
}

fn accepted_step(
    step_id: &str,
    proposal_digest: &str,
    depends_on: Vec<String>,
) -> AcceptedPlanStep {
    let spec = PlannedInvocationSpec::new(
        "local.fs.read_text",
        "1.0.0",
        digest('d'),
        digest_text(step_id),
        digest('i'),
        PlannedExecutionProfile::LocalSyncOnceV1,
        "linux",
        "x86_64",
    )
    .expect("planned invocation should validate");
    let reference =
        ReconstructableMaterialReference::new("test-recipes", format!("recipe-{step_id}"), "rev-1")
            .expect("recipe reference should validate");
    let (invocation, _) =
        PlannedInvocationMaterialRecord::bind(RUN_ID, step_id, proposal_digest, spec, reference)
            .expect("planned invocation should bind");
    AcceptedPlanStep {
        step_id: step_id.to_owned(),
        objective: format!("execute {step_id}"),
        depends_on,
        invocation,
    }
}

fn model_decision(
    reservation: &ModelCallReservation,
    proposal_digest: &str,
) -> ExpectedPlanningTurn {
    ExpectedPlanningTurn::for_model_call(
        reservation.turn_index(),
        reservation.call_id(),
        reservation.context_digest(),
        proposal_digest,
    )
    .expect("model-call decision should validate")
}

#[test]
fn legacy_turn_and_event_digest_round_trip_without_model_call_field() {
    let turn = ExpectedPlanningTurn::new(1, digest('c'), digest('p'))
        .expect("legacy turn should validate");
    assert_eq!(turn.model_call_id(), None);
    let turn_json = serde_json::to_value(&turn).expect("turn should serialize");
    assert!(turn_json.get("modelCallId").is_none());
    let legacy_raw = format!(
        r#"{{"turnIndex":1,"contextDigest":"{}","proposalDigest":"{}"}}"#,
        digest('c'),
        digest('p')
    );
    let legacy_decoded: ExpectedPlanningTurn =
        serde_json::from_str(&legacy_raw).expect("historical JSON should deserialize");
    assert_eq!(
        serde_json::to_string(&legacy_decoded).expect("historical JSON should reserialize"),
        legacy_raw
    );

    let configured = configured_state(2);
    let step = accepted_step("legacy-step", turn.proposal_digest(), Vec::new());
    let record = record_after(
        Some(&configured),
        "legacy-plan",
        RunEventBody::PlanAccepted {
            decision: turn,
            steps: vec![step],
        },
    );
    let json = serde_json::to_vec(&record).expect("legacy record should serialize");
    assert!(
        !json
            .windows(b"modelCallId".len())
            .any(|bytes| bytes == b"modelCallId")
    );
    let decoded: EventRecord = serde_json::from_slice(&json).expect("legacy record should load");
    decoded
        .verify_digest()
        .expect("legacy digest should remain valid");
    assert_eq!(decoded, record);

    let created = record_after(
        None,
        "legacy-run-created",
        RunEventBody::RunCreated {
            goal: "legacy replay".to_owned(),
        },
    );
    let configured_record = EventRecord::next(
        Some(&created),
        event(
            "legacy-loop-configured",
            RunEventBody::AgentLoopConfigured {
                budget: AgentLoopBudget::new(2, 16, 16, 262_144).unwrap(),
            },
        ),
    )
    .unwrap();
    replay(&[created, configured_record]).expect("legacy journal should replay");
}

#[test]
#[allow(clippy::too_many_lines)] // Covers legacy upgrade floor and both direct-decision fences.
fn lifecycle_configuration_uses_historical_accepted_turn_floor_and_blocks_direct_bypass() {
    assert!(matches!(
        ModelCallBudget::new(0),
        Err(PlanningContractError::ZeroBudget("max_model_calls"))
    ));

    let configured = configured_state(3);
    let legacy_decision = ExpectedPlanningTurn::new(1, digest('c'), digest('p')).unwrap();
    let legacy = append(
        Some(&configured),
        "legacy-accepted-plan",
        RunEventBody::PlanAccepted {
            steps: vec![accepted_step(
                "legacy-step",
                legacy_decision.proposal_digest(),
                Vec::new(),
            )],
            decision: legacy_decision,
        },
    );
    let second_decision = ExpectedPlanningTurn::new(2, digest('e'), digest('q')).unwrap();
    let legacy = append(
        Some(&legacy),
        "second-legacy-accepted-plan",
        RunEventBody::PlanAccepted {
            steps: vec![accepted_step(
                "legacy-step-two",
                second_decision.proposal_digest(),
                Vec::new(),
            )],
            decision: second_decision,
        },
    );
    assert!(matches!(
        try_append(
            Some(&legacy),
            "below-historical-floor",
            RunEventBody::ModelCallLifecycleConfigured {
                budget: ModelCallBudget::new(1).unwrap(),
            },
        ),
        Err(
            TransitionError::ModelCallBudgetBelowHistoricalAcceptedTurns {
                max_model_calls: 1,
                accepted_model_turns: 2,
            }
        )
    ));
    let lifecycle = configure_calls(&legacy, 3);
    let calls = lifecycle
        .agent_loop
        .as_ref()
        .and_then(|state| state.model_calls.as_ref())
        .expect("model-call lifecycle should project");
    assert_eq!(calls.reserved_calls, 2);
    assert_eq!(calls.settled_calls, 2);
    assert_eq!(calls.unknown_calls, 0);
    assert!(calls.active_call.is_none());

    let equal_floor = try_append(
        Some(&legacy),
        "equal-historical-floor",
        RunEventBody::ModelCallLifecycleConfigured {
            budget: ModelCallBudget::new(2).unwrap(),
        },
    );
    assert!(
        equal_floor.is_ok(),
        "budget equal to the historical floor is valid"
    );
    let zero_remaining = equal_floor.unwrap();
    assert!(matches!(
        try_append(
            Some(&zero_remaining),
            "direct-plan-bypass",
            RunEventBody::PlanAccepted {
                decision: ExpectedPlanningTurn::new(3, digest('e'), digest('f')).unwrap(),
                steps: vec![accepted_step("bypass", &digest('f'), Vec::new())],
            },
        ),
        Err(TransitionError::ModelCallDecisionRequired)
    ));

    let fresh = configured_state(2);
    let fake_call_decision = ExpectedPlanningTurn::for_model_call(
        1,
        format!("model-call-{}", "0".repeat(64)),
        digest('c'),
        digest('p'),
    )
    .unwrap();
    assert!(matches!(
        try_append(
            Some(&fresh),
            "unexpected-call-decision",
            RunEventBody::PlanAccepted {
                steps: vec![accepted_step(
                    "unexpected",
                    fake_call_decision.proposal_digest(),
                    Vec::new(),
                )],
                decision: fake_call_decision,
            },
        ),
        Err(TransitionError::ModelCallLifecycleNotConfigured)
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // Audits every immutable reservation binding in one fixture.
fn reservation_rejects_tampered_identity_head_index_and_turn_without_mutation() {
    let configured = configure_calls(&configured_state(2), 3);
    assert!(matches!(
        ModelCallReservation::new(
            RUN_ID,
            AUTHORITY_EPOCH,
            "planner/contains-slash",
            1,
            1,
            configured.journal_sequence,
            &configured.journal_head_digest,
            digest('c'),
            digest('r'),
        ),
        Err(PlanningContractError::InvalidIdentifier("planner_id"))
    ));
    assert!(matches!(
        ModelCallReservation::new(
            RUN_ID,
            AUTHORITY_EPOCH,
            "planner.test.v1",
            1,
            1,
            configured.journal_sequence,
            &configured.journal_head_digest,
            "sha256:not-a-context",
            digest('r'),
        ),
        Err(PlanningContractError::InvalidDigest("context_digest"))
    ));
    assert!(matches!(
        ModelCallReservation::new(
            RUN_ID,
            AUTHORITY_EPOCH,
            "planner.test.v1",
            0,
            1,
            configured.journal_sequence,
            &configured.journal_head_digest,
            digest('c'),
            digest('r'),
        ),
        Err(PlanningContractError::ModelCallIndexZero)
    ));

    let valid = reservation(&configured, 1, 1, 'c');
    let mut tampered_json = serde_json::to_value(&valid).expect("reservation should serialize");
    tampered_json["callId"] = serde_json::Value::String(format!("model-call-{}", "b".repeat(64)));
    let tampered: ModelCallReservation =
        serde_json::from_value(tampered_json).expect("wire shape should deserialize");
    assert!(matches!(
        try_append(
            Some(&configured),
            "tampered-call-id",
            RunEventBody::ModelCallReserved {
                reservation: tampered,
            },
        ),
        Err(TransitionError::PlanningContract(
            PlanningContractError::ModelCallIdMismatch
        ))
    ));

    let wrong_head = ModelCallReservation::new(
        RUN_ID,
        AUTHORITY_EPOCH,
        "planner.test.v1",
        1,
        1,
        configured.journal_sequence,
        digest('h'),
        digest('c'),
        digest('r'),
    )
    .unwrap();
    assert!(matches!(
        try_append(
            Some(&configured),
            "wrong-base-head",
            RunEventBody::ModelCallReserved {
                reservation: wrong_head,
            },
        ),
        Err(TransitionError::ModelCallBaseHeadMismatch)
    ));

    for (call_index, turn_index, expected) in [
        (
            2,
            1,
            TransitionError::UnexpectedModelCallIndex {
                expected: 1,
                actual: 2,
            },
        ),
        (
            1,
            2,
            TransitionError::UnexpectedModelCallTurn {
                expected: 1,
                actual: 2,
            },
        ),
    ] {
        let candidate = ModelCallReservation::new(
            RUN_ID,
            AUTHORITY_EPOCH,
            "planner.test.v1",
            call_index,
            turn_index,
            configured.journal_sequence,
            &configured.journal_head_digest,
            digest('c'),
            digest('r'),
        )
        .unwrap();
        let actual = try_append(
            Some(&configured),
            "wrong-counter",
            RunEventBody::ModelCallReserved {
                reservation: candidate,
            },
        )
        .expect_err("counter mismatch must fail");
        assert_eq!(actual, expected);
    }
}

#[test]
fn rejected_calls_consume_call_budget_but_not_accepted_turn_budget() {
    let configured = configure_calls(&configured_state(2), 1);
    let call = reservation(&configured, 1, 1, 'c');
    let reserved = reserve(&configured, &call, "call-reserved");
    let projected = reserved.agent_loop.as_ref().unwrap();
    assert_eq!(projected.accepted_model_turns, 0);
    let calls = projected.model_calls.as_ref().unwrap();
    assert_eq!(calls.reserved_calls, 1);
    assert_eq!(calls.settled_calls, 0);
    assert!(matches!(
        calls.active_call.as_ref().map(|call| call.status),
        Some(ModelCallStatus::Reserved)
    ));

    let settled = append(
        Some(&reserved),
        "call-rejected",
        RunEventBody::ModelCallSettled {
            call_id: call.call_id().to_owned(),
            settlement: ModelCallSettlement::Rejected {
                reason: ModelCallRejectionReason::PlannerInvalidResponse,
            },
        },
    );
    let loop_state = settled.agent_loop.as_ref().unwrap();
    assert_eq!(loop_state.accepted_model_turns, 0);
    let calls = loop_state.model_calls.as_ref().unwrap();
    assert_eq!(
        (
            calls.reserved_calls,
            calls.settled_calls,
            calls.unknown_calls
        ),
        (1, 1, 0)
    );
    assert!(calls.active_call.is_none());

    let next = ModelCallReservation::new(
        RUN_ID,
        AUTHORITY_EPOCH,
        "planner.test.v1",
        2,
        1,
        settled.journal_sequence,
        &settled.journal_head_digest,
        digest('n'),
        digest('r'),
    )
    .unwrap();
    assert!(matches!(
        try_append(
            Some(&settled),
            "budget-exhausted",
            RunEventBody::ModelCallReserved { reservation: next },
        ),
        Err(TransitionError::ModelCallBudgetExceeded)
    ));
}

#[test]
fn accepted_plan_atomically_settles_exact_reserved_call() {
    let configured = configure_calls(&configured_state(2), 3);
    let call = reservation(&configured, 1, 1, 'c');
    let reserved = reserve(&configured, &call, "call-reserved");
    let proposal_digest = digest('p');
    let accepted = append(
        Some(&reserved),
        "plan-accepted",
        RunEventBody::PlanAccepted {
            decision: model_decision(&call, &proposal_digest),
            steps: vec![accepted_step("step-a", &proposal_digest, Vec::new())],
        },
    );
    let loop_state = accepted.agent_loop.as_ref().unwrap();
    assert_eq!(loop_state.accepted_model_turns, 1);
    let calls = loop_state.model_calls.as_ref().unwrap();
    assert_eq!(
        (
            calls.reserved_calls,
            calls.settled_calls,
            calls.unknown_calls
        ),
        (1, 1, 0)
    );
    assert!(calls.active_call.is_none());
    assert!(accepted.steps.contains_key("step-a"));

    let configured = configure_calls(&configured_state(2), 3);
    let call = reservation(&configured, 1, 1, 'd');
    let reserved = reserve(&configured, &call, "invalid-call-reserved");
    let proposal_digest = digest('q');
    let result = try_append(
        Some(&reserved),
        "invalid-plan",
        RunEventBody::PlanAccepted {
            decision: model_decision(&call, &proposal_digest),
            steps: vec![accepted_step(
                "cyclic-step",
                &proposal_digest,
                vec!["cyclic-step".to_owned()],
            )],
        },
    );
    assert!(matches!(
        result,
        Err(TransitionError::SelfDependency { .. })
    ));
    let calls = reserved
        .agent_loop
        .as_ref()
        .unwrap()
        .model_calls
        .as_ref()
        .unwrap();
    assert_eq!(calls.settled_calls, 0);
    assert!(calls.active_call.is_some());
    assert!(reserved.steps.is_empty());
}

#[test]
fn completion_candidate_atomically_settles_exact_reserved_call() {
    let configured = configure_calls(&configured_state(2), 3);
    let planned = append(
        Some(&configured),
        "legacy-step",
        RunEventBody::StepPlanned {
            step_id: "completed-step".to_owned(),
            objective: "provide a receipt-completed graph".to_owned(),
            depends_on: Vec::new(),
        },
    );
    let mut completed = planned;
    let step = completed.steps.get_mut("completed-step").unwrap();
    step.status = StepStatus::Completed;
    step.execution_receipt_id = Some("receipt-completed-step".to_owned());
    step.execution_receipt_digest = Some(digest('e'));

    let call = reservation(&completed, 1, 1, 'c');
    let reserved = reserve(&completed, &call, "completion-call-reserved");
    let proposal_digest = digest('p');
    let completed = append(
        Some(&reserved),
        "completion-candidate",
        RunEventBody::CompletionCandidateRecorded {
            decision: model_decision(&call, &proposal_digest),
            candidate_id: "completion-candidate-1".to_owned(),
            summary_digest: digest('s'),
        },
    );
    let loop_state = completed.agent_loop.as_ref().unwrap();
    assert_eq!(loop_state.accepted_model_turns, 1);
    assert!(loop_state.completion_candidate.is_some());
    let calls = loop_state.model_calls.as_ref().unwrap();
    assert_eq!(
        (
            calls.reserved_calls,
            calls.settled_calls,
            calls.unknown_calls
        ),
        (1, 1, 0)
    );
    assert!(calls.active_call.is_none());
}

#[test]
fn success_requires_exact_call_context_turn_and_immediate_reservation_head() {
    let configured = configure_calls(&configured_state(3), 3);
    let call = reservation(&configured, 1, 1, 'c');
    let reserved = reserve(&configured, &call, "call-reserved");
    let proposal_digest = digest('p');
    let step = || accepted_step("step-a", &proposal_digest, Vec::new());

    for (decision, expected) in [
        (
            ExpectedPlanningTurn::for_model_call(
                1,
                format!("model-call-{}", "c".repeat(64)),
                call.context_digest(),
                &proposal_digest,
            )
            .unwrap(),
            TransitionError::ModelCallIdMismatch,
        ),
        (
            ExpectedPlanningTurn::for_model_call(1, call.call_id(), digest('x'), &proposal_digest)
                .unwrap(),
            TransitionError::ModelCallContextMismatch,
        ),
        (
            ExpectedPlanningTurn::for_model_call(
                2,
                call.call_id(),
                call.context_digest(),
                &proposal_digest,
            )
            .unwrap(),
            TransitionError::UnexpectedModelCallTurn {
                expected: 1,
                actual: 2,
            },
        ),
    ] {
        let actual = try_append(
            Some(&reserved),
            "mismatched-success",
            RunEventBody::PlanAccepted {
                decision,
                steps: vec![step()],
            },
        )
        .expect_err("mismatched call binding must fail");
        assert_eq!(actual, expected);
    }

    let advanced = append(
        Some(&reserved),
        "unrelated-safety-event",
        RunEventBody::StepPlanned {
            step_id: "legacy-safety-step".to_owned(),
            objective: "allow safety work to advance the head".to_owned(),
            depends_on: Vec::new(),
        },
    );
    assert!(matches!(
        try_append(
            Some(&advanced),
            "stale-success",
            RunEventBody::PlanAccepted {
                decision: model_decision(&call, &proposal_digest),
                steps: vec![step()],
            },
        ),
        Err(TransitionError::StaleModelCallResponse)
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // Keeps the complete Unknown-to-explicit-abandon recovery chain.
fn unknown_call_blocks_late_response_until_explicit_abandonment() {
    let configured = configure_calls(&configured_state(3), 3);
    let call = reservation(&configured, 1, 1, 'c');
    let reserved = reserve(&configured, &call, "call-reserved");
    let unknown = append(
        Some(&reserved),
        "call-unknown",
        RunEventBody::ModelCallBecameUnknown {
            call_id: call.call_id().to_owned(),
            reason: ModelCallUnknownReason::Timeout,
        },
    );
    let calls = unknown
        .agent_loop
        .as_ref()
        .unwrap()
        .model_calls
        .as_ref()
        .unwrap();
    assert_eq!(
        (
            calls.reserved_calls,
            calls.settled_calls,
            calls.unknown_calls
        ),
        (1, 0, 1)
    );
    assert!(matches!(
        calls.active_call.as_ref().map(|call| call.status),
        Some(ModelCallStatus::Unknown {
            reason: ModelCallUnknownReason::Timeout
        })
    ));

    let proposal_digest = digest('p');
    assert!(matches!(
        try_append(
            Some(&unknown),
            "late-plan",
            RunEventBody::PlanAccepted {
                decision: model_decision(&call, &proposal_digest),
                steps: vec![accepted_step("late-step", &proposal_digest, Vec::new())],
            },
        ),
        Err(TransitionError::ModelCallNotReserved)
    ));
    let blocked_retry = ModelCallReservation::new(
        RUN_ID,
        AUTHORITY_EPOCH,
        "planner.test.v1",
        2,
        1,
        unknown.journal_sequence,
        &unknown.journal_head_digest,
        digest('n'),
        digest('r'),
    )
    .unwrap();
    assert!(matches!(
        try_append(
            Some(&unknown),
            "retry-while-unknown",
            RunEventBody::ModelCallReserved {
                reservation: blocked_retry,
            },
        ),
        Err(TransitionError::ModelCallActive)
    ));
    assert!(matches!(
        try_append(
            Some(&unknown),
            "reject-unknown",
            RunEventBody::ModelCallSettled {
                call_id: call.call_id().to_owned(),
                settlement: ModelCallSettlement::Rejected {
                    reason: ModelCallRejectionReason::ProposalRejected,
                },
            },
        ),
        Err(TransitionError::ModelCallNotReserved)
    ));

    let abandoned = append(
        Some(&unknown),
        "abandon-unknown",
        RunEventBody::ModelCallSettled {
            call_id: call.call_id().to_owned(),
            settlement: ModelCallSettlement::Abandoned {
                reason: ModelCallAbandonmentReason::RecoveryDiscarded,
            },
        },
    );
    let calls = abandoned
        .agent_loop
        .as_ref()
        .unwrap()
        .model_calls
        .as_ref()
        .unwrap();
    assert_eq!(
        (
            calls.reserved_calls,
            calls.settled_calls,
            calls.unknown_calls
        ),
        (1, 1, 1)
    );
    assert!(calls.active_call.is_none());
    let retry = ModelCallReservation::new(
        RUN_ID,
        AUTHORITY_EPOCH,
        "planner.test.v1",
        2,
        1,
        abandoned.journal_sequence,
        &abandoned.journal_head_digest,
        digest('n'),
        digest('r'),
    )
    .unwrap();
    reserve(&abandoned, &retry, "retry-after-abandon");
}

#[test]
fn stale_head_requires_stale_rejection_and_other_rejections_require_immediate_head() {
    let configured = configure_calls(&configured_state(3), 3);
    let call = reservation(&configured, 1, 1, 'c');
    let reserved = reserve(&configured, &call, "call-reserved");
    let advanced = append(
        Some(&reserved),
        "head-advanced",
        RunEventBody::StepPlanned {
            step_id: "safety-step".to_owned(),
            objective: "record unrelated safety progress".to_owned(),
            depends_on: Vec::new(),
        },
    );

    assert!(matches!(
        try_append(
            Some(&advanced),
            "non-stale-reason-at-stale-head",
            RunEventBody::ModelCallSettled {
                call_id: call.call_id().to_owned(),
                settlement: ModelCallSettlement::Rejected {
                    reason: ModelCallRejectionReason::ProposalRejected,
                },
            },
        ),
        Err(TransitionError::ModelCallSettlementHeadMismatch)
    ));
    let settled = append(
        Some(&advanced),
        "stale-rejected",
        RunEventBody::ModelCallSettled {
            call_id: call.call_id().to_owned(),
            settlement: ModelCallSettlement::Rejected {
                reason: ModelCallRejectionReason::StaleHead,
            },
        },
    );
    let calls = settled
        .agent_loop
        .as_ref()
        .unwrap()
        .model_calls
        .as_ref()
        .unwrap();
    assert_eq!(
        (
            calls.reserved_calls,
            calls.settled_calls,
            calls.unknown_calls
        ),
        (1, 1, 0)
    );
    assert!(calls.active_call.is_none());

    let immediate = configure_calls(&configured_state(3), 3);
    let immediate_call = reservation(&immediate, 1, 1, 'd');
    let immediate = reserve(&immediate, &immediate_call, "immediate-call-reserved");
    assert!(matches!(
        try_append(
            Some(&immediate),
            "false-stale-reason",
            RunEventBody::ModelCallSettled {
                call_id: immediate_call.call_id().to_owned(),
                settlement: ModelCallSettlement::Rejected {
                    reason: ModelCallRejectionReason::StaleHead,
                },
            },
        ),
        Err(TransitionError::ModelCallSettlementHeadMismatch)
    ));
}
