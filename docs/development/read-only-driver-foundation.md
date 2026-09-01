# ReadOnly와 bounded CLI driver 기반

## 현재 가능한 것

- WorkGraph가 `ReadOnly`를 별도 의미로 보존한다.
- 새 planned ReadOnly invocation은 `LocalSyncReadOnlyOccurrenceV1`이며 idempotency key와 sink guarantee가
  없다. 기존 저장 Run의 `LocalSyncReadOnlyV1`은 legacy semantic identity로 계속 읽는다.
- Accepted planned-invocation Admission은 ReadOnly intent에 Core Receipt v2 provenance를 발행한다. Legacy/unplanned direct ReadOnly는 명시적으로 닫혀 있다.
- Verifier는 bounded artifact descriptor를 제출하고 Core가 exact Run/Step/Receipt provenance를 붙인다.
- Adapter의 bounded typed JSON은 exact Definition output schema 검증 뒤 event-anchored `ToolOutputRecord`로 schema 7에 원자 저장되며, verifier는 같은-generation snapshot을 받는다.
- PlanningContext v3는 Step의 plan journal 순서와 Receipt-completed exact output의 관찰 순서를 보존해 다음 model turn에 전달하고, schema 8 `CompletionOutputRecord`는 그 turn의 exact UTF-8 summary를 별도 process 재시작 뒤 모델 재호출 없이 복원한다.
- Memory/SQLite store가 v1 empty-artifact와 v2 artifact-bearing 의미를 각각 다시 검증한다.
- `xgeny-cli` library의 `RunDriver`가 기존 AgentLoop, admission, executor와 verifier를 bounded 순서로 조합한다.
- test-only fake planner/adapter로 approval pending/deny 무효과, IntentCommitted·Validating SQLite 재개, 완료와 no-call replay를 검증한다.
- `xgeny-adapter-filesystem`이 root-bound resolver, component별 no-follow, 64 KiB strict UTF-8 read와 durable-output-only verifier를 제공한다. 실제 SQLite driver 회귀는 파일을 한 번 읽어 다음 planning turn에 전달하고 원본 삭제·재open 뒤 추가 file/model/verifier 호출 없이 completion을 복원한다.

## 현재 공개 범위 밖

Public `xgeny run/resume`은 실제 model provider와 제품 filesystem `read-text` adapter를 한 bounded
composition으로 연결한다. 다만 승인은 invocation flag이고 interactive UI, 일반 process/write/network
도구와 plugin sandbox는 아직 없다. 따라서 이 수직 slice를 “Claude Code/Codex와 같은 사용자용 CLI
완성” 또는 “untrusted plugin/host 전체 sandbox”로 설명하면 안 된다.

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

같은 Proposal 안의 동일 Capability+normalized argument는 semantic duplicate로 거부된다. 이전 Step이
Core Receipt-completed된 뒤에는 ADR-0029의 Core-derived Run/Step occurrence로 같은 파일을 다시 관찰할
수 있다. 각 observation은 별도 approval/action/Receipt identity를 사용하고 unresolved read lifecycle을
자동 replay하지 않는다.

## 검증

```bash
cargo test -p xgeny-workgraph --test durable_plan
cargo test -p xgeny-runtime --test durable_agent_loop
cargo test -p xgeny-runtime --test verification_artifacts
cargo test -p xgeny-cli --test durable_driver
cargo test --workspace --locked
```

ReadOnly/driver 기반은 [ADR-0018](../adr/0018-read-only-artifact-and-cli-driver-foundation.md), durable tool output은 [ADR-0019](../adr/0019-durable-tool-output-and-schema-7.md), next-turn context는 [ADR-0020](../adr/0020-generation-checked-planning-context-v2.md), durable completion은 [ADR-0021](../adr/0021-durable-completion-output-and-schema-8.md), 제품 filesystem read 경계는 [ADR-0022](../adr/0022-capability-confined-filesystem-read-adapter.md), 반복 action identity는 [ADR-0029](../adr/0029-core-derived-action-occurrence.md)를 따른다.
