# Direct Executor 기본형

- 기준일: 2026-08-29
- 상태: ADR-0011 실행 경계 + ADR-0012 Core verification 수직 슬라이스
- 공개 protocol v0.1 변경: 없음
- Receipt 도입 schema: 4; current local store schema: 5

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
                                                  +-> durable outcome evidence
                                                  +-> Validating
                                                       |
                                                       +-> exact read-only verifier
                                                       +-> Core ExecutionReceipt
                                                       +-> Receipt + terminal event atomic commit
```

이 단계는 runtime fake와 별도 workspace crate의 preopened reference adapter로 실행 순서와 복구 계약을 검증한다. 참조 adapter는 임시 일반 파일에 실제 I/O를 수행하지만 제품용 filesystem/process adapter는 아니다.

## Composition root

Host는 문서 catalog와 behavior registry를 별도로 구성한다.

```rust
let mut providers = MaterialProviderRegistry::new();
providers.register("run-recipe", provider)?;

let mut adapters = EffectAdapterRegistry::new();
adapters.register(&instance.binding, adapter)?;

let mut verifiers = EffectVerifierRegistry::new();
verifiers.register(&instance.binding, verifier)?;
```

Provider ID와 adapter/verifier binding은 host가 부여한다. Provider/adapter/verifier implementation에는 identity getter가 없다. 같은 key의 두 번째 등록은 기존 entry를 교체하지 않는다.

Adapter와 verifier dispatch는 `binding_ref + operation_ref + protocol_version` 전체 exact key를 사용한다. `None`과 `Some`을 구분하며 default operation이나 compatible-version fallback을 하지 않는다.

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
| Placement | `Local` | `Device`, `Remote` |
| Receipt provenance | admission에서 commit됨 | schema 3 legacy `None` |

Credential-bearing adapter를 임시로 허용하거나 raw credential을 argument에 넣지 않는다. 후속 credential witness가 추가될 때 이 표와 executable-binding contract를 함께 갱신한다.

Device/Remote는 실제 executor identity·platform·attestation을 아직 한 권위에서 만들 수 없어 admission에서 차단한다. Schema 3에서 migration된 provenance 없는 pending intent도 Started·prepare·execute 전에 거부하며 사실을 추측해 backfill하지 않는다.

## Crash 판정

| 마지막 durable 상태 | 재개 동작 | 외부 호출 |
|---|---|---|
| `IntentCommitted` + material 있음 | exact prepare 후 Started CAS | prepare 1, Started 뒤 execute 최대 1 |
| `IntentCommitted` + material 없음 | 오류 또는 explicit material-unavailable 처리 | execute 0 |
| `Executing` | `EffectUnknown` 기록 | provider/prepare/execute 0 |
| `EffectUnknown`, query 불가 | `ManualRequired` | adapter 0 |
| `EffectUnknown`, query 가능 | exact adapter read-only reconcile | execute 0 |
| `Validating` | exact verifier read-only verify 후 Receipt commit | provider/prepare/execute/reconcile 0 |
| Receipt-bound terminal | no-op | verifier/adapter 0 |

Started commit 실패 시 prepared session을 폐기하고 execute하지 않는다. Outcome commit이 실패하면 `Executing`을 유지하고 재개 시 blind retry하지 않는다.

## Public-port reference adapter

`xgeny-adapter-reference`는 `publish = false`인 비제품 crate다. Runtime private API 없이 `EffectAdapter`와 `PreparedAdapterInvocation`을 구현하고, trusted composition root가 미리 연 격리 파일 handle만 받는다. Invocation에는 OS path 대신 exact opaque target reference가 들어간다.

`prepare`는 target을 변경하지 않는다. Started commit 뒤 `execute`가 canonical evidence marker를 seek·truncate·write·sync하고 bounded read-back으로 확인한다. Raw marker와 target reference는 기록하지 않으며 partial write·sync를 포함한 I/O 오류는 OS 문자열을 내보내지 않고 `ResponseUnverifiable` unknown으로 처리한다. 별도 process를 write·sync·read-back 뒤 outcome commit 전에 종료하는 SQLite test는 재시작 때 adapter/provider 없이 `EffectUnknown`으로 수렴하고 adapter execute가 0회인지 확인한다.

Adapter outcome의 `evidence_digest`는 물리 evidence byte의 digest이며 protocol Receipt가 아니다. 성공 outcome 뒤 별도 `PreopenedMarkerVerifier`가 파일을 read-only로 다시 읽고, Core가 protocol `ExecutionReceipt`를 생성해 terminal event와 원자 commit한다. 상세 범위와 비보장은 [Preopened Reference Adapter Conformance](reference-adapter-conformance.md)와 [Core Verification과 Execution Receipt](execution-receipt.md)를 따른다.

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
- 외부 crate의 preopened 일반 파일 actual I/O와 exact binding conformance
- post-write/pre-outcome process exit + SQLite restart의 adapter execute 0회
- `Validating` SQLite restart의 adapter execute 0회와 verifier-only 재개
- 성공/검증 실패 Receipt, schema/digest와 journal·intent binding
- Receipt commit acknowledgement 유실 뒤 effect·verifier 중복 0회
- Debug, JSONL과 SQLite artifact plaintext sentinel scan. POSIX에서는 열린 DB에서 현재 존재하는 WAL/SHM까지 검사하고, Windows에서는 store를 clean close해 SQLite byte-range lock을 해제한 뒤 남아 있는 DB/WAL/SHM artifact를 검사한다.

## 후속 수직 슬라이스

ADR-0014는 Receipt-bound terminal gate를 dependency release 조건으로 사용하는 Tracked/Persistent WorkGraph와 restart frontier를 추가했다. 다음 실행 slice는 actionable `Planned` Step을 만들 durable plan input과 bounded model loop다. 제품용 read-only filesystem adapter는 참조 adapter의 단순 확장이 아니다. WorkGraph `ReadOnly`, bounded typed output와 Artifact, root directory capability와 symlink/junction/TOCTOU 규칙을 별도 ADR·gate에서 먼저 확정한다. Adapter definite failure·unknown Receipt, credential resolver와 process adapter도 각각 분리한다.
