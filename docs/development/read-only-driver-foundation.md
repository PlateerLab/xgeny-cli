# ReadOnly와 bounded CLI driver 기반

## 현재 가능한 것

- WorkGraph가 `ReadOnly`를 별도 의미로 보존한다.
- planned ReadOnly invocation은 `LocalSyncReadOnlyV1`이며 idempotency key와 sink guarantee가 없다.
- Accepted planned-invocation Admission은 ReadOnly intent에 Core Receipt v2 provenance를 발행한다. Legacy/unplanned direct ReadOnly는 명시적으로 닫혀 있다.
- Verifier는 bounded artifact descriptor를 제출하고 Core가 exact Run/Step/Receipt provenance를 붙인다.
- Adapter의 bounded typed JSON은 exact Definition output schema 검증 뒤 event-anchored `ToolOutputRecord`로 schema 7에 원자 저장되며, verifier는 같은-generation snapshot을 받는다.
- PlanningContext v2는 Receipt-completed exact output을 다음 model turn에 전달하고, schema 8 `CompletionOutputRecord`는 그 turn의 exact UTF-8 summary를 별도 process 재시작 뒤 모델 재호출 없이 복원한다.
- Memory/SQLite store가 v1 empty-artifact와 v2 artifact-bearing 의미를 각각 다시 검증한다.
- `xgeny-cli` library의 `RunDriver`가 기존 AgentLoop, admission, executor와 verifier를 bounded 순서로 조합한다.
- test-only fake planner/adapter로 approval pending/deny 무효과, IntentCommitted·Validating SQLite 재개, 완료와 no-call replay를 검증한다.

## 아직 사용자에게 제공하지 않는 것

`xgeny` binary에는 `run/resume` 명령이 없고 실제 사용자 workspace 파일을 여는 제품 adapter도 없다. 승인 UI와 local-output model egress composition도 연결되지 않았다. 따라서 fake adapter로 검증한 durable output/context/completion 기반만을 “Claude Code/Codex와 같은 실제 파일 읽기” 또는 “workspace sandbox 검증 완료”로 설명하면 안 된다.

## Driver 정지 조건

`RunDriver::drive_until_pause`는 다음 조건에서 caller에게 제어를 돌려준다.

- completion candidate
- WorkGraph quiescence 또는 AgentLoop tick hard bound
- approval pending/denied
- policy/router non-selection
- planner/materializer closed failure
- unresolved model call recovery
- Core/store/adapter/verifier fixed error

Driver는 approval을 만들거나 model/effect uncertainty를 자동 retry하지 않는다.

현재 같은 Run의 동일 Capability+normalized argument는 ReadOnly여도 semantic action 중복으로 거부된다. 변경 후 같은 파일을 다시 관찰하는 generation/occurrence identity는 제품 adapter 전 별도 결정이 필요하다.

## 검증

```bash
cargo test -p xgeny-workgraph --test durable_plan
cargo test -p xgeny-runtime --test durable_agent_loop
cargo test -p xgeny-runtime --test verification_artifacts
cargo test -p xgeny-cli --test durable_driver
cargo test --workspace --locked
```

ReadOnly/driver 기반은 [ADR-0018](../adr/0018-read-only-artifact-and-cli-driver-foundation.md), durable tool output은 [ADR-0019](../adr/0019-durable-tool-output-and-schema-7.md), next-turn context는 [ADR-0020](../adr/0020-generation-checked-planning-context-v2.md), durable completion은 [ADR-0021](../adr/0021-durable-completion-output-and-schema-8.md)을 따른다.
