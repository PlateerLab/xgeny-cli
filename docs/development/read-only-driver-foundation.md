# ReadOnly와 bounded CLI driver 기반

## 현재 가능한 것

- WorkGraph가 `ReadOnly`를 별도 의미로 보존한다.
- planned ReadOnly invocation은 `LocalSyncReadOnlyV1`이며 idempotency key와 sink guarantee가 없다.
- Accepted planned-invocation Admission은 ReadOnly intent에 Core Receipt v2 provenance를 발행한다. Legacy/unplanned direct ReadOnly는 명시적으로 닫혀 있다.
- Verifier는 bounded artifact descriptor를 제출하고 Core가 exact Run/Step/Receipt provenance를 붙인다.
- Memory/SQLite store가 v1 empty-artifact와 v2 artifact-bearing 의미를 각각 다시 검증한다.
- `xgeny-cli` library의 `RunDriver`가 기존 AgentLoop, admission, executor와 verifier를 bounded 순서로 조합한다.
- test-only fake planner/adapter로 approval pending/deny 무효과, IntentCommitted·Validating SQLite 재개, 완료와 no-call replay를 검증한다.

## 아직 사용자에게 제공하지 않는 것

`xgeny` binary에는 `run` 명령이 없다. 실제 파일을 여는 제품 adapter, raw typed output sidecar, 다음 planning turn의 output context, completion 원문 저장도 없다. 따라서 현재 기반을 “Claude Code/Codex와 같은 파일 읽기”, “모델이 파일 내용을 이어서 사용함” 또는 “실제 workspace sandbox 검증 완료”로 설명하면 안 된다.

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

현재 같은 Run의 동일 Capability+normalized argument는 ReadOnly여도 semantic action 중복으로 거부된다. 변경 후 같은 파일을 다시 관찰하는 generation/occurrence identity는 output durability 단계에서 먼저 결정한다.

## 검증

```bash
cargo test -p xgeny-workgraph --test durable_plan
cargo test -p xgeny-runtime --test durable_agent_loop
cargo test -p xgeny-runtime --test verification_artifacts
cargo test -p xgeny-cli --test durable_driver
cargo test --workspace --locked
```

전체 설계와 다음 제품 slice의 보안 gate는 [ADR-0018](../adr/0018-read-only-artifact-and-cli-driver-foundation.md)을 따른다.
