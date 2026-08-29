# Direct Executor 기본형

- 기준일: 2026-08-29
- 상태: ADR-0011 연구 gate용 fake-adapter 수직 슬라이스
- 공개 protocol v0.1 변경: 없음
- local store schema: 3

## 현재 가능한 경로

```text
AdmissionRequest
  -> schema/resource/policy/router
  -> EffectIntent + material sidecar commit
  -> Ephemeral material --------------------+
                                              |
restart -> MaterialProviderRegistry exact ----+-> DirectExecutor
                                                  |
                                                  +-> current Definition/Instance
                                                  +-> health/auth gate
                                                  +-> exact full binding adapter
                                                  +-> side-effect-free prepare
                                                  +-> head/material recheck
                                                  +-> Started commit
                                                  +-> one-shot execute
                                                  +-> durable outcome
```

이 단계는 fake adapter로 실행 순서와 복구 계약을 검증한다. Filesystem이나 process에 실제 변경을 만드는 제품 adapter는 아직 없다.

## Composition root

Host는 문서 catalog와 behavior registry를 별도로 구성한다.

```rust
let mut providers = MaterialProviderRegistry::new();
providers.register("run-recipe", provider)?;

let mut adapters = EffectAdapterRegistry::new();
adapters.register(&instance.binding, adapter)?;
```

Provider ID와 adapter binding은 host가 부여한다. Provider/adapter implementation에는 identity getter가 없다. 같은 key의 두 번째 등록은 기존 entry를 교체하지 않는다.

Adapter dispatch는 `binding_ref + operation_ref + protocol_version` 전체 exact key를 사용한다. `None`과 `Some`을 구분하며 default operation이나 compatible-version fallback을 하지 않는다.

## Adapter 구현 계약

```rust
trait EffectAdapter {
    fn prepare(
        &mut self,
        request: AdapterPrepareRequest<'_>,
    ) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure>;

    fn reconcile(
        &mut self,
        request: AdapterReconcileRequest<'_>,
    ) -> AdapterReconciliationObservation;
}

trait PreparedAdapterInvocation {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation;
}
```

`prepare`는 argument canonicalization을 다시 하지 않고 core가 검증한 exact argument를 사용한다. External effect, irreversible file open mode, child process start, network request send는 `prepare`에서 수행하면 안 된다. 필요한 parsing과 handle description 구성만 하고 side effect는 `execute`에 둔다.

`AdapterPrepareRequest`의 `Debug`는 arguments와 binding detail을 출력하지 않는다. Adapter error는 fixed enum만 core로 반환한다. Provider/OS/library의 arbitrary message는 adapter 내부 진단 경계에서 redaction한 뒤 처리하고 journal outcome으로 전달하지 않는다.

Invocation material 자체는 Direct Executor가 borrow한다. Prepare 실패, stale head 또는 Started commit 실패처럼 외부 effect 전의 transient 오류에서는 같은 material로 새 one-shot session을 다시 prepare할 수 있다. Consume-once 대상은 adapter session이며 material handle이 아니다.

## Dynamic execution gate

현재 실행 허용 조건은 다음뿐이다.

| 항목 | 허용 | 차단 |
|---|---|---|
| Health | `Available` | `Degraded`, `Unavailable`, `Unknown` |
| Auth state | `NotRequired` | `Available`, `Required`, `Expired` |
| Auth reference | `None` | `Some(_)` |

Credential-bearing adapter를 임시로 허용하거나 raw credential을 argument에 넣지 않는다. 후속 credential witness가 추가될 때 이 표와 executable-binding contract를 함께 갱신한다.

## Crash 판정

| 마지막 durable 상태 | 재개 동작 | 외부 호출 |
|---|---|---|
| `IntentCommitted` + material 있음 | exact prepare 후 Started CAS | prepare 1, Started 뒤 execute 최대 1 |
| `IntentCommitted` + material 없음 | 오류 또는 explicit material-unavailable 처리 | execute 0 |
| `Executing` | `EffectUnknown` 기록 | provider/prepare/execute 0 |
| `EffectUnknown`, query 불가 | `ManualRequired` | adapter 0 |
| `EffectUnknown`, query 가능 | exact adapter read-only reconcile | execute 0 |

Started commit 실패 시 prepared session을 폐기하고 execute하지 않는다. Outcome commit이 실패하면 `Executing`을 유지하고 재개 시 blind retry하지 않는다.

## 검증 명령

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo run --locked --quiet -p xgeny-cli -- protocol check
cargo build --workspace --release --locked
```

핵심 회귀 테스트는 다음을 포함한다.

- exact adapter와 provider lookup, duplicate non-replacement
- operation/protocol mismatch의 fallback 0회
- prepare 실패 전후 event/execute counter
- full-head stale token과 cross-Step/effect token 차단
- health와 credential-bearing Instance 차단
- Started commit 뒤 execute 순서
- lost outcome commit과 child-process exit 뒤 duplicate execute 0회
- SQLite restart reconstruct → execute E2E
- Debug, JSONL과 SQLite artifact plaintext sentinel scan

## 다음 수직 슬라이스

다음 구현은 Direct Executor contract를 바꾸지 않고 fake session 하나를 실제 최소 adapter로 교체한다. 첫 후보는 sandbox가 필요 없는 bounded read-only 또는 fixture-only adapter여야 한다. Receipt body와 Artifact 저장, credential resolver, process adapter는 각각 별도 gate로 진행한다.
