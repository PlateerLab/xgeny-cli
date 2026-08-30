use std::collections::BTreeMap;

use xgeny_workgraph::{
    AuthorizationBinding, AuthorizationUse, ContinuationAction, DependencyBlockReason, EffectClass,
    EffectIntent, EventRecord, FrontierAction, InvocationBinding, InvocationMaterialRetention,
    RunEvent, RunEventBody, RunState, SinkGuarantee, StepState, StepStatus, TransitionError,
    apply_record, authorization_digest, derive_frontier, invocation_material_digest,
    invocation_material_retention_digest, once_authorization_id,
};

const RUN_ID: &str = "run-frontier";
const AUTHORITY: &str = "local:test";
const AUTHORITY_EPOCH: u64 = 7;

fn step(step_id: &str, depends_on: &[&str], status: StepStatus) -> StepState {
    StepState {
        step_id: step_id.to_owned(),
        objective: format!("complete {step_id}"),
        depends_on: depends_on.iter().map(|value| (*value).to_owned()).collect(),
        planned_invocation: None,
        status,
        attempts: 0,
        intent: None,
        effect_evidence_digest: None,
        output_record_digest: None,
        execution_receipt_id: None,
        execution_receipt_digest: None,
        uncertainty_reason: None,
        reconciliation_evidence_digest: None,
    }
}

fn receipt_completed(step_id: &str, depends_on: &[&str]) -> StepState {
    let mut step = step(step_id, depends_on, StepStatus::Completed);
    step.execution_receipt_id = Some(format!("receipt-{step_id}"));
    step.execution_receipt_digest = Some(format!("sha256:{}", "a".repeat(64)));
    step
}

fn state(steps: impl IntoIterator<Item = StepState>) -> RunState {
    RunState {
        run_id: RUN_ID.to_owned(),
        authority: AUTHORITY.to_owned(),
        authority_epoch: AUTHORITY_EPOCH,
        goal: "exercise a persistent dependency graph".to_owned(),
        revision: 10,
        journal_sequence: 10,
        journal_head_digest: "sha256:head-10".to_owned(),
        steps: steps
            .into_iter()
            .map(|step| (step.step_id.clone(), step))
            .collect(),
        authorization_consumption: BTreeMap::new(),
        agent_loop: None,
    }
}

fn action(step_id: &str, action: ContinuationAction) -> FrontierAction {
    FrontierAction {
        step_id: step_id.to_owned(),
        action,
    }
}

#[test]
fn receipt_bound_diamond_releases_only_the_active_frontier() {
    let initial = state([
        step("a", &[], StepStatus::Planned),
        step("b", &["a"], StepStatus::Planned),
        step("c", &["a"], StepStatus::Planned),
        step("d", &["b", "c"], StepStatus::Planned),
        step("e", &[], StepStatus::Planned),
    ]);

    let frontier = derive_frontier(&initial).expect("valid DAG should derive");
    assert_eq!(
        frontier.actionable,
        vec![
            action("a", ContinuationAction::Admit),
            action("e", ContinuationAction::Admit),
        ]
    );
    assert_eq!(frontier.waiting.len(), 3);
    assert!(frontier.blocked.is_empty());

    let after_a = state([
        receipt_completed("a", &[]),
        step("b", &["a"], StepStatus::Planned),
        step("c", &["a"], StepStatus::Planned),
        step("d", &["b", "c"], StepStatus::Planned),
        step("e", &[], StepStatus::Planned),
    ]);
    let frontier = derive_frontier(&after_a).expect("verified completion should release children");
    assert_eq!(
        frontier.actionable,
        vec![
            action("b", ContinuationAction::Admit),
            action("c", ContinuationAction::Admit),
            action("e", ContinuationAction::Admit),
        ]
    );
    assert_eq!(frontier.waiting[0].step_id, "d");
    assert_eq!(frontier.waiting[0].pending_dependencies, ["b", "c"]);
}

#[test]
fn continuation_work_precedes_starting_a_new_effect() {
    let frontier = derive_frontier(&state([
        step("admit", &[], StepStatus::Planned),
        step("execute", &[], StepStatus::IntentCommitted),
        step("recover", &[], StepStatus::Executing),
        step("reconcile-a", &[], StepStatus::EffectUnknown),
        step("reconcile-b", &[], StepStatus::Reconciling),
        step("verify", &[], StepStatus::Validating),
    ]))
    .expect("independent active steps should derive");

    assert_eq!(
        frontier.actionable,
        vec![
            action("recover", ContinuationAction::DriveEffect),
            action("reconcile-a", ContinuationAction::DriveEffect),
            action("reconcile-b", ContinuationAction::DriveEffect),
            action("verify", ContinuationAction::Verify),
            action("execute", ContinuationAction::DriveEffect),
            action("admit", ContinuationAction::Admit),
        ]
    );
    assert_eq!(frontier.next_action(), frontier.actionable.first());
}

#[test]
fn failed_dependency_blocks_descendants_but_not_an_independent_branch() {
    let frontier = derive_frontier(&state([
        step("a", &[], StepStatus::Failed),
        step("b", &["a"], StepStatus::Planned),
        step("c", &["b"], StepStatus::Planned),
        step("independent", &[], StepStatus::Planned),
    ]))
    .expect("failed dependency is a valid terminal graph state");

    assert_eq!(
        frontier.actionable,
        vec![action("independent", ContinuationAction::Admit)]
    );
    assert_eq!(frontier.blocked.len(), 2);
    assert_eq!(frontier.blocked[0].step_id, "b");
    assert_eq!(frontier.blocked[0].blockers[0].step_id, "a");
    assert_eq!(
        frontier.blocked[0].blockers[0].reason,
        DependencyBlockReason::Failed
    );
    assert_eq!(frontier.blocked[1].step_id, "c");
    assert_eq!(frontier.blocked[1].blockers[0].step_id, "b");
    assert_eq!(
        frontier.blocked[1].blockers[0].reason,
        DependencyBlockReason::DependencyBlocked
    );
}

#[test]
fn manual_dependency_blocks_descendants_but_not_an_independent_branch() {
    let frontier = derive_frontier(&state([
        step("manual", &[], StepStatus::ManualRequired),
        step("child", &["manual"], StepStatus::Planned),
        step("grandchild", &["child"], StepStatus::Planned),
        step("independent", &[], StepStatus::Planned),
    ]))
    .expect("manual dependency is a valid terminal graph state");

    assert_eq!(
        frontier.actionable,
        vec![action("independent", ContinuationAction::Admit)]
    );
    assert_eq!(frontier.blocked.len(), 2);
    assert_eq!(frontier.blocked[0].step_id, "child");
    assert_eq!(
        frontier.blocked[0].blockers[0].reason,
        DependencyBlockReason::ManualRequired
    );
    assert_eq!(frontier.blocked[1].step_id, "grandchild");
    assert_eq!(
        frontier.blocked[1].blockers[0].reason,
        DependencyBlockReason::DependencyBlocked
    );
}

#[test]
fn legacy_completion_without_a_receipt_fails_closed() {
    let frontier = derive_frontier(&state([
        step("legacy", &[], StepStatus::Completed),
        step("child", &["legacy"], StepStatus::Planned),
    ]))
    .expect("legacy state remains readable");

    assert!(frontier.actionable.is_empty());
    assert_eq!(frontier.blocked.len(), 1);
    assert_eq!(frontier.blocked[0].step_id, "child");
    assert_eq!(frontier.blocked[0].blockers[0].step_id, "legacy");
    assert_eq!(
        frontier.blocked[0].blockers[0].reason,
        DependencyBlockReason::ReceiptMissing
    );
    assert_eq!(frontier.unverified_completed_step_ids, ["legacy"]);
    assert!(!frontier.all_steps_receipt_completed());

    for (receipt_id, receipt_digest) in [
        (Some("receipt-legacy"), None),
        (None, Some(format!("sha256:{}", "a".repeat(64)))),
        (Some(""), Some(format!("sha256:{}", "a".repeat(64)))),
        (
            Some("receipt-legacy"),
            Some("sha256:not-a-digest".to_owned()),
        ),
    ] {
        let mut incomplete = step("legacy", &[], StepStatus::Completed);
        incomplete.execution_receipt_id = receipt_id.map(str::to_owned);
        incomplete.execution_receipt_digest = receipt_digest;
        let frontier = derive_frontier(&state([
            incomplete,
            step("child", &["legacy"], StepStatus::Planned),
        ]))
        .expect("partial Receipt identity should remain readable");
        assert!(frontier.actionable.is_empty());
        assert_eq!(
            frontier.blocked[0].blockers[0].reason,
            DependencyBlockReason::ReceiptMissing
        );
    }
}

#[test]
fn ten_thousand_step_chain_is_derived_without_recursive_traversal() {
    let mut steps = Vec::with_capacity(10_000);
    for index in 0..9_999 {
        let step_id = format!("step-{index:05}");
        let dependency = (index > 0).then(|| format!("step-{:05}", index - 1));
        let dependencies: Vec<&str> = dependency.iter().map(String::as_str).collect();
        steps.push(receipt_completed(&step_id, &dependencies));
    }
    steps.push(step("step-09999", &["step-09998"], StepStatus::Planned));

    let frontier = derive_frontier(&state(steps)).expect("long chain should derive iteratively");
    assert_eq!(frontier.total_steps, 10_000);
    assert_eq!(
        frontier.actionable,
        vec![action("step-09999", ContinuationAction::Admit)]
    );
    assert_eq!(frontier.verified_completed_step_ids.len(), 9_999);
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

fn append(
    records: &mut Vec<EventRecord>,
    current: Option<&RunState>,
    event: RunEvent,
) -> Result<RunState, TransitionError> {
    let record = EventRecord::next(records.last(), event).expect("event should canonicalize");
    let next = apply_record(current, &record)?;
    records.push(record);
    Ok(next)
}

#[test]
fn planning_rejects_self_duplicate_and_unknown_dependencies() {
    let mut records = Vec::new();
    let created = append(
        &mut records,
        None,
        event(
            "event-1",
            RunEventBody::RunCreated {
                goal: "validate planning".to_owned(),
            },
        ),
    )
    .expect("Run should create");
    let root = append(
        &mut records,
        Some(&created),
        event(
            "event-2",
            RunEventBody::StepPlanned {
                step_id: "root".to_owned(),
                objective: "root".to_owned(),
                depends_on: Vec::new(),
            },
        ),
    )
    .expect("root should plan");

    let self_dependency = append(
        &mut records.clone(),
        Some(&root),
        event(
            "event-self",
            RunEventBody::StepPlanned {
                step_id: "self".to_owned(),
                objective: "self".to_owned(),
                depends_on: vec!["self".to_owned()],
            },
        ),
    );
    assert!(matches!(
        self_dependency,
        Err(TransitionError::SelfDependency { step_id }) if step_id == "self"
    ));

    let duplicate = append(
        &mut records.clone(),
        Some(&root),
        event(
            "event-duplicate",
            RunEventBody::StepPlanned {
                step_id: "duplicate".to_owned(),
                objective: "duplicate".to_owned(),
                depends_on: vec!["root".to_owned(), "root".to_owned()],
            },
        ),
    );
    assert!(matches!(
        duplicate,
        Err(TransitionError::DuplicateDependency { step_id, dependency_id })
            if step_id == "duplicate" && dependency_id == "root"
    ));

    let unknown = append(
        &mut records,
        Some(&root),
        event(
            "event-unknown",
            RunEventBody::StepPlanned {
                step_id: "unknown".to_owned(),
                objective: "unknown".to_owned(),
                depends_on: vec!["missing".to_owned()],
            },
        ),
    );
    assert!(matches!(
        unknown,
        Err(TransitionError::UnknownDependency { step_id, dependency_id })
            if step_id == "unknown" && dependency_id == "missing"
    ));
}

fn intent_for(state: &RunState, step_id: &str) -> EffectIntent {
    let action_digest = format!("sha256:action-{step_id}");
    let material_digest = invocation_material_digest(&serde_json::json!({"step": step_id}))
        .expect("material should canonicalize");
    let material_retention_digest =
        invocation_material_retention_digest(&InvocationMaterialRetention::Ephemeral)
            .expect("retention should canonicalize");
    let invocation = InvocationBinding {
        capability_id: "test.effect".to_owned(),
        contract_version: "1.0.0".to_owned(),
        definition_digest: "sha256:definition".to_owned(),
        instance_id: "test.instance".to_owned(),
        instance_binding_digest: "sha256:instance".to_owned(),
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
        action_digest: action_digest.clone(),
        material_digest,
        material_retention_digest,
        policy_evidence_digest: "sha256:policy".to_owned(),
        receipt_provenance_digest: None,
    };
    EffectIntent {
        effect_id: format!("effect-{step_id}"),
        action_digest,
        invocation,
        effect_class: EffectClass::Idempotent,
        idempotency_key: None,
        sink_guarantee: SinkGuarantee::None,
        authorization: AuthorizationUse {
            grant_id: once_authorization_id(&state.run_id, &binding.action_digest)
                .expect("authorization ID should derive"),
            grant_digest: authorization_digest(&binding, 1)
                .expect("authorization should canonicalize"),
            max_uses: 1,
            binding,
        },
        receipt_provenance: None,
    }
}

fn previous_record(state: &RunState) -> EventRecord {
    EventRecord {
        sequence: state.journal_sequence,
        previous_digest: None,
        event: event(
            "previous-placeholder",
            RunEventBody::RunCreated {
                goal: "placeholder".to_owned(),
            },
        ),
        digest: state.journal_head_digest.clone(),
    }
}

fn commit_intent(state: &RunState, step_id: &str) -> Result<RunState, TransitionError> {
    let record = EventRecord::next(
        Some(&previous_record(state)),
        event(
            "candidate-intent",
            RunEventBody::EffectIntentCommitted {
                step_id: step_id.to_owned(),
                intent: Box::new(intent_for(state, step_id)),
            },
        ),
    )
    .expect("candidate should canonicalize");
    apply_record(Some(state), &record)
}

#[test]
fn reducer_prevents_dependency_bypass_and_requires_receipt_identity() {
    let pending_parent = state([
        step("parent", &[], StepStatus::Planned),
        step("child", &["parent"], StepStatus::Planned),
    ]);
    let blocked = commit_intent(&pending_parent, "child");
    assert!(matches!(
        blocked,
        Err(TransitionError::DependencyNotReleased { reason, .. })
            if reason == DependencyBlockReason::NotCompleted
    ));
    assert!(pending_parent.authorization_consumption.is_empty());

    let legacy_parent = state([
        step("parent", &[], StepStatus::Completed),
        step("child", &["parent"], StepStatus::Planned),
    ]);
    let blocked = commit_intent(&legacy_parent, "child");
    assert!(matches!(
        blocked,
        Err(TransitionError::DependencyNotReleased { reason, .. })
            if reason == DependencyBlockReason::ReceiptMissing
    ));

    let verified_parent = state([
        receipt_completed("parent", &[]),
        step("child", &["parent"], StepStatus::Planned),
    ]);
    let committed = commit_intent(&verified_parent, "child")
        .expect("Receipt-bound completion should release the child");
    assert_eq!(committed.steps["child"].status, StepStatus::IntentCommitted);
    assert_eq!(committed.authorization_consumption.len(), 1);
}

#[test]
fn reducer_rejects_a_corrupt_unknown_dependency_without_panicking_or_mutating_budget() {
    let corrupt = state([step("child", &["missing"], StepStatus::Planned)]);

    let result = commit_intent(&corrupt, "child");

    assert!(matches!(
        result,
        Err(TransitionError::UnknownDependency {
            step_id,
            dependency_id,
        }) if step_id == "child" && dependency_id == "missing"
    ));
    assert!(corrupt.authorization_consumption.is_empty());
}

#[test]
fn direct_state_with_a_cycle_fails_closed() {
    let cyclic = state([
        step("a", &["b"], StepStatus::Planned),
        step("b", &["a"], StepStatus::Planned),
    ]);
    assert!(derive_frontier(&cyclic).is_err());
}
