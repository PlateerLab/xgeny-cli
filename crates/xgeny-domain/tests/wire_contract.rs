use serde_json::{Value, json};
use xgeny_domain::ProtocolDocument;

fn minimal_work_graph() -> Value {
    json!({
        "apiVersion": "xgeny.io/v1alpha1",
        "kind": "WorkGraph",
        "runId": "run-test-001",
        "revision": 0,
        "authority": "local:device-test",
        "goal": "Verify the domain wire contract.",
        "executionMode": "persistent",
        "status": "pending",
        "steps": [],
        "journalSequence": 0,
        "journalHeadDigest": null,
        "updatedAt": "2026-08-28T03:00:00Z"
    })
}

#[test]
fn unknown_top_level_field_is_rejected() {
    let mut value = minimal_work_graph();
    value["unexpected"] = json!(true);

    let error = serde_json::from_value::<ProtocolDocument>(value)
        .expect_err("unknown core fields must fail closed");
    assert!(error.to_string().contains("unknown field `unexpected`"));
}

#[test]
fn unknown_nested_field_is_rejected() {
    let mut value = minimal_work_graph();
    value["steps"] = json!([{
        "stepId": "step-test-001",
        "objective": "Exercise nested field rejection.",
        "dependsOn": [],
        "capability": {
            "capabilityId": "xgeny.test/noop",
            "contractVersion": "1.0.0"
        },
        "status": "pending",
        "attempts": 0,
        "verificationStatus": "not_started",
        "unexpected": "must not be ignored"
    }]);

    let error = serde_json::from_value::<ProtocolDocument>(value)
        .expect_err("unknown nested core fields must fail closed");
    assert!(error.to_string().contains("unknown field `unexpected`"));
}

#[test]
fn optional_extension_payload_survives_round_trip() {
    let mut value = minimal_work_graph();
    value["extensions"] = json!({
        "https://xgeny.dev/extensions/test/v1": {
            "enabled": true,
            "nested": {"value": 42}
        }
    });
    value["requiredExtensions"] = json!(["https://xgeny.dev/extensions/required-test/v1"]);

    let document: ProtocolDocument =
        serde_json::from_value(value.clone()).expect("valid document should deserialize");
    let round_trip = serde_json::to_value(document).expect("document should serialize");

    assert_eq!(
        round_trip.pointer("/extensions"),
        value.pointer("/extensions")
    );
    assert_eq!(
        round_trip.pointer("/requiredExtensions"),
        value.pointer("/requiredExtensions")
    );
}
