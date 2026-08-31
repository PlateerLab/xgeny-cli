pub(crate) struct Asset {
    pub name: &'static str,
    pub contents: &'static str,
}

macro_rules! asset {
    ($name:literal, $path:literal) => {
        Asset {
            name: $name,
            contents: include_str!($path),
        }
    };
}

pub(crate) const SCHEMAS: &[Asset] = &[
    asset!(
        "capability-definition.schema.json",
        "../../../protocol/schema/v1alpha1/capability-definition.schema.json"
    ),
    asset!(
        "capability-instance.schema.json",
        "../../../protocol/schema/v1alpha1/capability-instance.schema.json"
    ),
    asset!(
        "common.schema.json",
        "../../../protocol/schema/v1alpha1/common.schema.json"
    ),
    asset!(
        "execution-receipt.schema.json",
        "../../../protocol/schema/v1alpha1/execution-receipt.schema.json"
    ),
    asset!(
        "invocation-plan.schema.json",
        "../../../protocol/schema/v1alpha1/invocation-plan.schema.json"
    ),
    asset!(
        "permission-request.schema.json",
        "../../../protocol/schema/v1alpha1/permission-request.schema.json"
    ),
    asset!(
        "policy-decision.schema.json",
        "../../../protocol/schema/v1alpha1/policy-decision.schema.json"
    ),
    asset!(
        "run-journal-event.schema.json",
        "../../../protocol/schema/v1alpha1/run-journal-event.schema.json"
    ),
    asset!(
        "work-graph.schema.json",
        "../../../protocol/schema/v1alpha1/work-graph.schema.json"
    ),
];

pub(crate) const FIXTURE_MANIFEST: &str =
    include_str!("../../../protocol/fixtures/v1alpha1/manifest.json");

pub(crate) const FIXTURES: &[Asset] = &[
    asset!(
        "valid/capability-definition.fs-list-directory.json",
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-list-directory.json"
    ),
    asset!(
        "valid/capability-definition.fs-read-text.json",
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
    ),
    asset!(
        "valid/capability-definition.fs-search-text.json",
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-search-text.json"
    ),
    asset!(
        "valid/capability-definition.fs-stat.json",
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-stat.json"
    ),
    asset!(
        "valid/capability-instance.local-fs.json",
        "../../../protocol/fixtures/v1alpha1/valid/capability-instance.local-fs.json"
    ),
    asset!(
        "valid/execution-receipt.fs-read-success.json",
        "../../../protocol/fixtures/v1alpha1/valid/execution-receipt.fs-read-success.json"
    ),
    asset!(
        "valid/invocation-plan.direct-fs-read.json",
        "../../../protocol/fixtures/v1alpha1/valid/invocation-plan.direct-fs-read.json"
    ),
    asset!(
        "valid/permission-request.fs-read.json",
        "../../../protocol/fixtures/v1alpha1/valid/permission-request.fs-read.json"
    ),
    asset!(
        "valid/policy-decision.allow-once.json",
        "../../../protocol/fixtures/v1alpha1/valid/policy-decision.allow-once.json"
    ),
    asset!(
        "valid/policy-decision.ask.json",
        "../../../protocol/fixtures/v1alpha1/valid/policy-decision.ask.json"
    ),
    asset!(
        "valid/policy-decision.deny.json",
        "../../../protocol/fixtures/v1alpha1/valid/policy-decision.deny.json"
    ),
    asset!(
        "valid/run-journal-event.step-completed.json",
        "../../../protocol/fixtures/v1alpha1/valid/run-journal-event.step-completed.json"
    ),
    asset!(
        "valid/work-graph.direct-completed.json",
        "../../../protocol/fixtures/v1alpha1/valid/work-graph.direct-completed.json"
    ),
    asset!(
        "valid/work-graph.persistent-planning.json",
        "../../../protocol/fixtures/v1alpha1/valid/work-graph.persistent-planning.json"
    ),
    asset!(
        "invalid/capability-definition.unknown-effect.json",
        "../../../protocol/fixtures/v1alpha1/invalid/capability-definition.unknown-effect.json"
    ),
    asset!(
        "invalid/capability-instance.raw-token.json",
        "../../../protocol/fixtures/v1alpha1/invalid/capability-instance.raw-token.json"
    ),
    asset!(
        "invalid/execution-receipt.success-with-required-failure.json",
        "../../../protocol/fixtures/v1alpha1/invalid/execution-receipt.success-with-required-failure.json"
    ),
    asset!(
        "invalid/execution-receipt.success-without-verification.json",
        "../../../protocol/fixtures/v1alpha1/invalid/execution-receipt.success-without-verification.json"
    ),
    asset!(
        "invalid/invocation-plan.effect-without-idempotency-key.json",
        "../../../protocol/fixtures/v1alpha1/invalid/invocation-plan.effect-without-idempotency-key.json"
    ),
    asset!(
        "invalid/permission-request.unnormalized-resource.json",
        "../../../protocol/fixtures/v1alpha1/invalid/permission-request.unnormalized-resource.json"
    ),
    asset!(
        "invalid/policy-decision.allow-without-grant.json",
        "../../../protocol/fixtures/v1alpha1/invalid/policy-decision.allow-without-grant.json"
    ),
    asset!(
        "invalid/policy-decision.contradictory-allow.json",
        "../../../protocol/fixtures/v1alpha1/invalid/policy-decision.contradictory-allow.json"
    ),
    asset!(
        "invalid/work-graph.direct-two-steps.json",
        "../../../protocol/fixtures/v1alpha1/invalid/work-graph.direct-two-steps.json"
    ),
];
