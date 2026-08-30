use xgeny_workgraph::{
    AcceptedPlanStep, AgentLoopBudget, AuthorizationBinding, AuthorizationUse, EffectClass,
    EffectIntent, EventRecord, ExpectedPlanningTurn, InvocationBinding,
    InvocationMaterialRetention, PlannedExecutionProfile, PlannedInvocationMaterialRecord,
    PlannedInvocationSpec, PlanningContractError, ReceiptPlacement, ReceiptProvenance,
    ReconstructableMaterialReference, RunEvent, RunEventBody, RunState, SinkGuarantee, StepStatus,
    TransitionError, VerificationDisposition, apply_record, authorization_digest,
    invocation_material_retention_digest, once_authorization_id, receipt_provenance_digest,
};

const RUN_ID: &str = "run-durable-plan";

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn event(event_id: &str, body: RunEventBody) -> RunEvent {
    RunEvent {
        event_id: event_id.to_owned(),
        run_id: RUN_ID.to_owned(),
        authority: "local:test".to_owned(),
        authority_epoch: 1,
        recorded_at: "2026-08-30T00:00:00Z".to_owned(),
        body,
    }
}

fn append(state: Option<&RunState>, event_id: &str, body: RunEventBody) -> RunState {
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
    let record = EventRecord::next(previous.as_ref(), event(event_id, body))
        .expect("event record should be canonical");
    apply_record(state, &record).expect("transition should succeed")
}

fn configured_state() -> RunState {
    let created = append(
        None,
        "run-created",
        RunEventBody::RunCreated {
            goal: "complete a two-step task".to_owned(),
        },
    );
    append(
        Some(&created),
        "loop-configured",
        RunEventBody::AgentLoopConfigured {
            budget: AgentLoopBudget::new(4, 8, 8, 16_384).expect("budget should be valid"),
        },
    )
}

fn accepted_step(
    run_id: &str,
    step_id: &str,
    objective: &str,
    depends_on: Vec<String>,
    proposal_digest: &str,
    marker: char,
) -> (AcceptedPlanStep, PlannedInvocationMaterialRecord) {
    accepted_step_with_profile(
        run_id,
        step_id,
        objective,
        depends_on,
        proposal_digest,
        marker,
        PlannedExecutionProfile::LocalSyncOnceV1,
    )
}

#[allow(clippy::too_many_arguments)]
fn accepted_step_with_profile(
    run_id: &str,
    step_id: &str,
    objective: &str,
    depends_on: Vec<String>,
    proposal_digest: &str,
    marker: char,
    profile: PlannedExecutionProfile,
) -> (AcceptedPlanStep, PlannedInvocationMaterialRecord) {
    let spec = PlannedInvocationSpec::new(
        "local.fs.read_text",
        "1.0.0",
        digest('d'),
        digest(marker),
        digest('e'),
        profile,
        "linux",
        "x86_64",
    )
    .expect("planned invocation should be valid");
    let reference =
        ReconstructableMaterialReference::new("test-recipes", format!("recipe-{marker}"), "rev-1")
            .expect("reference should be valid");
    let (invocation, input) =
        PlannedInvocationMaterialRecord::bind(run_id, step_id, proposal_digest, spec, reference)
            .expect("plan input should bind");
    (
        AcceptedPlanStep {
            step_id: step_id.to_owned(),
            objective: objective.to_owned(),
            depends_on,
            invocation,
        },
        input,
    )
}

#[test]
fn read_only_profile_rejects_effect_downgrade_and_keyed_semantics_before_budget_consumption() {
    let state = configured_state();
    let proposal_digest = digest('f');
    let (step, _) = accepted_step_with_profile(
        RUN_ID,
        "step-read",
        "read the source",
        Vec::new(),
        &proposal_digest,
        'a',
        PlannedExecutionProfile::LocalSyncReadOnlyV1,
    );
    let planned = append(
        Some(&state),
        "read-plan-accepted",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(1, digest('c'), proposal_digest)
                .expect("turn should bind"),
            steps: vec![step],
        },
    );

    let effectful = planned_intent(&planned, "step-read", "effect-read");
    let effectful_record = record_after(
        &planned,
        "read-effect-downgrade",
        RunEventBody::EffectIntentCommitted {
            step_id: "step-read".to_owned(),
            intent: Box::new(effectful),
        },
    );
    assert!(matches!(
        apply_record(Some(&planned), &effectful_record),
        Err(TransitionError::PlannedInvocationMismatch {
            field: "effect_class",
            ..
        })
    ));
    assert!(planned.authorization_consumption.is_empty());

    let mut keyed_read = planned_intent(&planned, "step-read", "effect-read");
    keyed_read.effect_class = EffectClass::ReadOnly;
    let keyed_record = record_after(
        &planned,
        "read-keyed-semantics",
        RunEventBody::EffectIntentCommitted {
            step_id: "step-read".to_owned(),
            intent: Box::new(keyed_read),
        },
    );
    assert!(matches!(
        apply_record(Some(&planned), &keyed_record),
        Err(TransitionError::ReadOnlyEffectSemanticsInvalid { .. })
    ));
    assert!(planned.authorization_consumption.is_empty());

    let mut read_only = planned_intent(&planned, "step-read", "effect-read");
    read_only.effect_class = EffectClass::ReadOnly;
    read_only.idempotency_key = None;
    let committed = apply_record(
        Some(&planned),
        &record_after(
            &planned,
            "read-correct-semantics",
            RunEventBody::EffectIntentCommitted {
                step_id: "step-read".to_owned(),
                intent: Box::new(read_only),
            },
        ),
    )
    .expect("exact read-only semantics should commit");
    assert_eq!(
        committed.steps["step-read"].status,
        StepStatus::IntentCommitted
    );
    assert_eq!(committed.authorization_consumption.len(), 1);
}

fn planned_intent(state: &RunState, step_id: &str, effect_id: &str) -> EffectIntent {
    let planned = state.steps[step_id]
        .planned_invocation
        .as_ref()
        .expect("accepted Step should have a binding");
    let invocation = InvocationBinding {
        capability_id: planned.capability_id().to_owned(),
        contract_version: planned.contract_version().to_owned(),
        definition_digest: planned.definition_digest().to_owned(),
        instance_id: "local.test.instance".to_owned(),
        instance_binding_digest: digest('i'),
    };
    let provenance = ReceiptProvenance {
        profile_version: "xgeny.core-receipt/v1".to_owned(),
        tool_output_profile: None,
        invocation_id: "invocation-durable-plan".to_owned(),
        plan_id: planned.plan_id().to_owned(),
        policy_decision_id: "decision-durable-plan".to_owned(),
        policy_decision_digest: digest('p'),
        executor_id: "xgeny-local".to_owned(),
        executor_placement: ReceiptPlacement::Local,
        executor_platform: "linux-x86_64".to_owned(),
        input_summary: "canonical-arguments-and-resolved-resources".to_owned(),
        verification_plan: Vec::new(),
    };
    let binding = AuthorizationBinding {
        run_id: state.run_id.clone(),
        step_id: step_id.to_owned(),
        authority: state.authority.clone(),
        authority_epoch: state.authority_epoch,
        issued_at_sequence: state.journal_sequence,
        issued_at_head_digest: state.journal_head_digest.clone(),
        capability_id: invocation.capability_id.clone(),
        contract_version: invocation.contract_version.clone(),
        definition_digest: invocation.definition_digest.clone(),
        instance_id: invocation.instance_id.clone(),
        instance_binding_digest: invocation.instance_binding_digest.clone(),
        action_digest: planned.action_digest().to_owned(),
        material_digest: planned.plan_input_digest().to_owned(),
        material_retention_digest: invocation_material_retention_digest(
            &InvocationMaterialRetention::Ephemeral,
        )
        .expect("retention should digest"),
        policy_evidence_digest: digest('q'),
        receipt_provenance_digest: Some(
            receipt_provenance_digest(&provenance).expect("provenance should digest"),
        ),
    };
    let grant_id = once_authorization_id(&binding.run_id, &binding.action_digest)
        .expect("grant ID should derive");
    let grant_digest = authorization_digest(&binding, 1).expect("grant should digest");
    EffectIntent {
        effect_id: effect_id.to_owned(),
        action_digest: binding.action_digest.clone(),
        invocation,
        effect_class: EffectClass::Idempotent,
        idempotency_key: Some("durable-plan-key".to_owned()),
        sink_guarantee: SinkGuarantee::None,
        authorization: AuthorizationUse {
            grant_id,
            grant_digest,
            max_uses: 1,
            binding,
        },
        receipt_provenance: Some(provenance),
    }
}

fn record_after(state: &RunState, event_id: &str, body: RunEventBody) -> EventRecord {
    let previous = EventRecord {
        sequence: state.journal_sequence,
        previous_digest: None,
        event: event(
            "synthetic-previous",
            RunEventBody::RunCreated {
                goal: String::new(),
            },
        ),
        digest: state.journal_head_digest.clone(),
    };
    EventRecord::next(Some(&previous), event(event_id, body)).expect("event should encode")
}

#[test]
fn one_accepted_plan_atomically_projects_a_forward_reference_dag() {
    let state = configured_state();
    let proposal_digest = digest('f');
    let (step_a, input_a) = accepted_step(
        RUN_ID,
        "step-a",
        "read the source",
        Vec::new(),
        &proposal_digest,
        'a',
    );
    let (step_b, input_b) = accepted_step(
        RUN_ID,
        "step-b",
        "summarize the source",
        vec!["step-a".to_owned()],
        &proposal_digest,
        'b',
    );
    let decision = ExpectedPlanningTurn::new(1, digest('c'), proposal_digest.clone())
        .expect("turn binding should be valid");

    let planned = append(
        Some(&state),
        "plan-accepted",
        RunEventBody::PlanAccepted {
            decision,
            steps: vec![step_b, step_a],
        },
    );

    assert_eq!(planned.steps.len(), 2);
    assert_eq!(planned.agent_loop.as_ref().unwrap().accepted_model_turns, 1);
    assert_eq!(planned.steps["step-a"].status, StepStatus::Planned);
    assert_eq!(planned.steps["step-b"].depends_on, ["step-a"]);
    assert_eq!(
        planned.steps["step-a"]
            .planned_invocation
            .as_ref()
            .unwrap()
            .plan_input_record_digest(),
        input_a.record_digest()
    );
    assert_eq!(
        planned.steps["step-b"]
            .planned_invocation
            .as_ref()
            .unwrap()
            .plan_input_record_digest(),
        input_b.record_digest()
    );
}

#[test]
fn cyclic_plan_is_rejected_without_changing_the_previous_projection() {
    let state = configured_state();
    let proposal_digest = digest('f');
    let (step_a, _) = accepted_step(
        RUN_ID,
        "step-a",
        "first",
        vec!["step-b".to_owned()],
        &proposal_digest,
        'a',
    );
    let (step_b, _) = accepted_step(
        RUN_ID,
        "step-b",
        "second",
        vec!["step-a".to_owned()],
        &proposal_digest,
        'b',
    );
    let decision = ExpectedPlanningTurn::new(1, digest('c'), proposal_digest)
        .expect("turn binding should be valid");
    let previous = EventRecord {
        sequence: state.journal_sequence,
        previous_digest: None,
        event: event(
            "synthetic-previous",
            RunEventBody::RunCreated {
                goal: String::new(),
            },
        ),
        digest: state.journal_head_digest.clone(),
    };
    let record = EventRecord::next(
        Some(&previous),
        event(
            "cyclic-plan",
            RunEventBody::PlanAccepted {
                decision,
                steps: vec![step_a, step_b],
            },
        ),
    )
    .expect("event should encode");

    assert!(matches!(
        apply_record(Some(&state), &record),
        Err(TransitionError::DependencyCycle)
    ));
    assert!(state.steps.is_empty());
    assert_eq!(state.agent_loop.as_ref().unwrap().accepted_model_turns, 0);
}

#[test]
fn reducer_rejects_an_effect_detached_from_the_accepted_action_before_budget_consumption() {
    let state = configured_state();
    let proposal_digest = digest('f');
    let (step, _) = accepted_step(
        RUN_ID,
        "step-a",
        "read the source",
        Vec::new(),
        &proposal_digest,
        'a',
    );
    let planned = append(
        Some(&state),
        "plan-accepted",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(1, digest('c'), proposal_digest)
                .expect("turn should bind"),
            steps: vec![step],
        },
    );
    let mut intent = planned_intent(&planned, "step-a", "effect-a");
    intent.action_digest = digest('x');
    intent.authorization.binding.action_digest = intent.action_digest.clone();
    intent.authorization.grant_id = once_authorization_id(
        &intent.authorization.binding.run_id,
        &intent.authorization.binding.action_digest,
    )
    .expect("grant ID should derive");
    intent.authorization.grant_digest =
        authorization_digest(&intent.authorization.binding, 1).expect("grant should digest");
    let record = record_after(
        &planned,
        "detached-intent",
        RunEventBody::EffectIntentCommitted {
            step_id: "step-a".to_owned(),
            intent: Box::new(intent),
        },
    );

    assert!(matches!(
        apply_record(Some(&planned), &record),
        Err(TransitionError::PlannedInvocationMismatch {
            step_id,
            field: "action_digest",
        }) if step_id == "step-a"
    ));
    assert!(planned.authorization_consumption.is_empty());
    assert_eq!(planned.steps["step-a"].status, StepStatus::Planned);
}

#[test]
fn reducer_enforces_the_durable_external_tool_start_budget() {
    let created = append(
        None,
        "run-created",
        RunEventBody::RunCreated {
            goal: "start at most one external effect".to_owned(),
        },
    );
    let configured = append(
        Some(&created),
        "loop-configured",
        RunEventBody::AgentLoopConfigured {
            budget: AgentLoopBudget::new(2, 2, 1, 16_384).expect("budget should be valid"),
        },
    );
    let proposal_digest = digest('f');
    let (step_a, _) = accepted_step(
        RUN_ID,
        "step-a",
        "first effect",
        Vec::new(),
        &proposal_digest,
        'a',
    );
    let (step_b, _) = accepted_step(
        RUN_ID,
        "step-b",
        "second effect",
        Vec::new(),
        &proposal_digest,
        'b',
    );
    let planned = append(
        Some(&configured),
        "plan-accepted",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(1, digest('c'), proposal_digest)
                .expect("turn should bind"),
            steps: vec![step_a, step_b],
        },
    );
    let intent_a = planned_intent(&planned, "step-a", "effect-a");
    let intent_a_committed = append(
        Some(&planned),
        "intent-a",
        RunEventBody::EffectIntentCommitted {
            step_id: "step-a".to_owned(),
            intent: Box::new(intent_a),
        },
    );
    let intent_b = planned_intent(&intent_a_committed, "step-b", "effect-b");
    let intents_committed = append(
        Some(&intent_a_committed),
        "intent-b",
        RunEventBody::EffectIntentCommitted {
            step_id: "step-b".to_owned(),
            intent: Box::new(intent_b),
        },
    );
    let first_started = append(
        Some(&intents_committed),
        "start-a",
        RunEventBody::EffectExecutionStarted {
            step_id: "step-a".to_owned(),
            effect_id: "effect-a".to_owned(),
        },
    );
    let second_start = record_after(
        &first_started,
        "start-b",
        RunEventBody::EffectExecutionStarted {
            step_id: "step-b".to_owned(),
            effect_id: "effect-b".to_owned(),
        },
    );

    assert!(matches!(
        apply_record(Some(&first_started), &second_start),
        Err(TransitionError::ToolCallBudgetExceeded)
    ));
    assert_eq!(first_started.steps["step-a"].attempts, 1);
    assert_eq!(first_started.steps["step-b"].attempts, 0);
    assert_eq!(
        first_started.steps["step-b"].status,
        StepStatus::IntentCommitted
    );
}

#[test]
fn legacy_projection_shape_omits_absent_agent_loop_and_plan_bindings() {
    let created = append(
        None,
        "legacy-created",
        RunEventBody::RunCreated {
            goal: "legacy projection".to_owned(),
        },
    );
    let legacy = append(
        Some(&created),
        "legacy-step",
        RunEventBody::StepPlanned {
            step_id: "legacy-step".to_owned(),
            objective: "remain byte-compatible".to_owned(),
            depends_on: Vec::new(),
        },
    );
    let value = serde_json::to_value(&legacy).expect("projection should serialize");

    assert!(value.get("agentLoop").is_none());
    assert!(
        value["steps"]["legacy-step"]
            .get("plannedInvocation")
            .is_none()
    );
    let decoded: RunState = serde_json::from_value(value).expect("projection should deserialize");
    assert_eq!(decoded, legacy);
}

#[test]
fn unsupported_or_non_concrete_planned_targets_are_rejected() {
    for (target_os, target_arch, field) in [
        ("solaris", "x86_64", "target_os"),
        ("any", "x86_64", "target_os"),
        ("linux", "riscv64", "target_arch"),
        ("linux", "any", "target_arch"),
    ] {
        assert!(matches!(
            PlannedInvocationSpec::new(
                "local.fs.read_text",
                "1.0.0",
                digest('d'),
                digest('a'),
                digest('e'),
                PlannedExecutionProfile::LocalSyncOnceV1,
                target_os,
                target_arch,
            ),
            Err(PlanningContractError::UnsupportedTarget(actual)) if actual == field
        ));
    }
}

#[test]
fn duplicate_semantic_actions_are_rejected_across_batch_and_prior_plans() {
    let state = configured_state();
    let proposal_digest = digest('f');
    let (step_a, _) = accepted_step(
        RUN_ID,
        "step-a",
        "first spelling",
        Vec::new(),
        &proposal_digest,
        'a',
    );
    let (step_b, _) = accepted_step(
        RUN_ID,
        "step-b",
        "same semantic action",
        Vec::new(),
        &proposal_digest,
        'a',
    );
    let duplicate_batch = record_after(
        &state,
        "duplicate-action-batch",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(1, digest('c'), proposal_digest)
                .expect("turn should bind"),
            steps: vec![step_a.clone(), step_b],
        },
    );
    assert!(matches!(
        apply_record(Some(&state), &duplicate_batch),
        Err(TransitionError::DuplicatePlannedAction {
            step_id,
            existing_step_id,
        }) if step_id == "step-b" && existing_step_id == "step-a"
    ));
    assert!(state.steps.is_empty());

    let accepted = append(
        Some(&state),
        "first-action-plan",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(
                1,
                digest('c'),
                step_a.invocation.proposal_digest(),
            )
            .expect("turn should bind"),
            steps: vec![step_a],
        },
    );
    let second_proposal = digest('9');
    let (replanned, _) = accepted_step(
        RUN_ID,
        "step-c",
        "same action in a later turn",
        Vec::new(),
        &second_proposal,
        'a',
    );
    let duplicate_later = record_after(
        &accepted,
        "duplicate-action-later",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(2, digest('8'), second_proposal)
                .expect("turn should bind"),
            steps: vec![replanned],
        },
    );
    assert!(matches!(
        apply_record(Some(&accepted), &duplicate_later),
        Err(TransitionError::DuplicatePlannedAction {
            step_id,
            existing_step_id,
        }) if step_id == "step-c" && existing_step_id == "step-a"
    ));
    assert_eq!(accepted.steps.len(), 1);
    assert_eq!(
        accepted.agent_loop.as_ref().unwrap().accepted_model_turns,
        1
    );
}

#[test]
fn configured_legacy_plans_obey_step_budget() {
    let created = append(
        None,
        "legacy-budget-created",
        RunEventBody::RunCreated {
            goal: "bound legacy Step planning".to_owned(),
        },
    );
    let configured = append(
        Some(&created),
        "legacy-budget-configured",
        RunEventBody::AgentLoopConfigured {
            budget: AgentLoopBudget::new(2, 1, 2, 16_384).expect("budget should validate"),
        },
    );
    let first = append(
        Some(&configured),
        "legacy-budget-first",
        RunEventBody::StepPlanned {
            step_id: "legacy-a".to_owned(),
            objective: "first legacy Step".to_owned(),
            depends_on: Vec::new(),
        },
    );
    let over_budget = record_after(
        &first,
        "legacy-budget-second",
        RunEventBody::StepPlanned {
            step_id: "legacy-b".to_owned(),
            objective: "second legacy Step".to_owned(),
            depends_on: Vec::new(),
        },
    );
    assert!(matches!(
        apply_record(Some(&first), &over_budget),
        Err(TransitionError::PlannedStepBudgetExceeded)
    ));
    assert_eq!(first.steps.len(), 1);
}

#[test]
fn completion_candidate_seals_legacy_planning() {
    let state = configured_state();
    let proposal_digest = digest('f');
    let (step, _) = accepted_step(
        RUN_ID,
        "step-done",
        "finish once",
        Vec::new(),
        &proposal_digest,
        'a',
    );
    let planned = append(
        Some(&state),
        "completion-plan",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(1, digest('c'), proposal_digest)
                .expect("turn should bind"),
            steps: vec![step],
        },
    );
    let intent = planned_intent(&planned, "step-done", "effect-done");
    let committed = append(
        Some(&planned),
        "completion-intent",
        RunEventBody::EffectIntentCommitted {
            step_id: "step-done".to_owned(),
            intent: Box::new(intent),
        },
    );
    let started = append(
        Some(&committed),
        "completion-started",
        RunEventBody::EffectExecutionStarted {
            step_id: "step-done".to_owned(),
            effect_id: "effect-done".to_owned(),
        },
    );
    let observed = append(
        Some(&started),
        "completion-observed",
        RunEventBody::EffectSucceeded {
            step_id: "step-done".to_owned(),
            effect_id: "effect-done".to_owned(),
            evidence_digest: digest('7'),
            output_record_digest: None,
        },
    );
    let verified = append(
        Some(&observed),
        "completion-verified",
        RunEventBody::VerificationRecorded {
            step_id: "step-done".to_owned(),
            effect_id: "effect-done".to_owned(),
            disposition: VerificationDisposition::Passed,
            receipt_id: "receipt-done".to_owned(),
            receipt_digest: digest('6'),
        },
    );
    let completed = append(
        Some(&verified),
        "completion-candidate",
        RunEventBody::CompletionCandidateRecorded {
            decision: ExpectedPlanningTurn::new(2, digest('5'), digest('4'))
                .expect("turn should bind"),
            candidate_id: "completion-done".to_owned(),
            summary_digest: digest('3'),
        },
    );
    let reopened = record_after(
        &completed,
        "legacy-after-completion",
        RunEventBody::StepPlanned {
            step_id: "legacy-after".to_owned(),
            objective: "must remain sealed".to_owned(),
            depends_on: Vec::new(),
        },
    );
    assert!(matches!(
        apply_record(Some(&completed), &reopened),
        Err(TransitionError::CompletionCandidateAlreadyRecorded)
    ));
    assert_eq!(completed.steps.len(), 1);
}

#[test]
fn transitive_blocked_existing_dependency_cannot_consume_another_plan_turn() {
    let state = configured_state();
    let proposal_digest = digest('f');
    let (step_a, _) = accepted_step(
        RUN_ID,
        "step-a",
        "failing parent",
        Vec::new(),
        &proposal_digest,
        'a',
    );
    let (step_b, _) = accepted_step(
        RUN_ID,
        "step-b",
        "blocked child",
        vec!["step-a".to_owned()],
        &proposal_digest,
        'b',
    );
    let planned = append(
        Some(&state),
        "blocked-plan",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(1, digest('c'), proposal_digest)
                .expect("turn should bind"),
            steps: vec![step_a, step_b],
        },
    );
    let committed = append(
        Some(&planned),
        "blocked-intent",
        RunEventBody::EffectIntentCommitted {
            step_id: "step-a".to_owned(),
            intent: Box::new(planned_intent(&planned, "step-a", "effect-a")),
        },
    );
    let started = append(
        Some(&committed),
        "blocked-start",
        RunEventBody::EffectExecutionStarted {
            step_id: "step-a".to_owned(),
            effect_id: "effect-a".to_owned(),
        },
    );
    let failed = append(
        Some(&started),
        "blocked-failed",
        RunEventBody::EffectFailed {
            step_id: "step-a".to_owned(),
            effect_id: "effect-a".to_owned(),
            evidence_digest: digest('7'),
        },
    );
    let second_proposal = digest('9');
    let (step_c, _) = accepted_step(
        RUN_ID,
        "step-c",
        "must not attach to blocked branch",
        vec!["step-b".to_owned()],
        &second_proposal,
        'c',
    );
    let blocked = record_after(
        &failed,
        "blocked-replan",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(2, digest('8'), second_proposal)
                .expect("turn should bind"),
            steps: vec![step_c],
        },
    );
    assert!(matches!(
        apply_record(Some(&failed), &blocked),
        Err(TransitionError::PlanDependencyBlocked {
            step_id,
            dependency_id,
            reason: xgeny_workgraph::DependencyBlockReason::DependencyBlocked,
        }) if step_id == "step-c" && dependency_id == "step-b"
    ));
    assert_eq!(failed.steps.len(), 2);
    assert_eq!(failed.agent_loop.as_ref().unwrap().accepted_model_turns, 1);
}

#[test]
fn planned_profile_rejects_low_level_retry_and_executor_downgrades() {
    let state = configured_state();
    let proposal_digest = digest('f');
    let (step, _) = accepted_step(
        RUN_ID,
        "step-a",
        "execute locally once",
        Vec::new(),
        &proposal_digest,
        'a',
    );
    let planned = append(
        Some(&state),
        "profile-plan",
        RunEventBody::PlanAccepted {
            decision: ExpectedPlanningTurn::new(1, digest('c'), proposal_digest)
                .expect("turn should bind"),
            steps: vec![step],
        },
    );

    let mut missing_key = planned_intent(&planned, "step-a", "effect-missing-key");
    missing_key.idempotency_key = None;
    let record = record_after(
        &planned,
        "profile-missing-key",
        RunEventBody::EffectIntentCommitted {
            step_id: "step-a".to_owned(),
            intent: Box::new(missing_key),
        },
    );
    assert!(matches!(
        apply_record(Some(&planned), &record),
        Err(TransitionError::PlannedInvocationMismatch {
            field: "idempotency_key",
            ..
        })
    ));

    for (event_id, field, mutate) in [
        (
            "profile-remote",
            "receipt_provenance.executor_placement",
            0_u8,
        ),
        (
            "profile-platform",
            "receipt_provenance.executor_platform",
            1_u8,
        ),
    ] {
        let mut intent = planned_intent(&planned, "step-a", event_id);
        let provenance = intent
            .receipt_provenance
            .as_mut()
            .expect("planned intent has provenance");
        if mutate == 0 {
            provenance.executor_placement = ReceiptPlacement::Remote;
        } else {
            provenance.executor_platform = "windows-x86_64".to_owned();
        }
        intent.authorization.binding.receipt_provenance_digest =
            Some(receipt_provenance_digest(provenance).expect("provenance should digest"));
        intent.authorization.grant_digest =
            authorization_digest(&intent.authorization.binding, 1).expect("grant should digest");
        let record = record_after(
            &planned,
            event_id,
            RunEventBody::EffectIntentCommitted {
                step_id: "step-a".to_owned(),
                intent: Box::new(intent),
            },
        );
        assert!(matches!(
            apply_record(Some(&planned), &record),
            Err(TransitionError::PlannedInvocationMismatch {
                field: actual,
                ..
            }) if actual == field
        ));
    }
    assert!(planned.authorization_consumption.is_empty());
    assert_eq!(planned.steps["step-a"].status, StepStatus::Planned);
}
