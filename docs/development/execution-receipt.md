# Core Verification과 Execution Receipt 기본형

- 기준일: 2026-08-29
- 상태: ADR-0012 연구 gate용 기본형
- 공개 protocol v0.1 변경: 없음
- local store schema: 4

## 구현된 사용자 시나리오

> 외부 effect가 성공한 뒤 process가 중단되더라도 effect를 반복하지 않고 read-only 검증을 재개하며, 검증 결과와 tamper-evident Execution Receipt를 한 번만 확정한다.

## Runtime 구조

```text
Admission
  |
  | EffectIntent
  | + authorization-bound ReceiptProvenance
  v
DirectExecutor -- exact EffectAdapter
  |
  | durable Started -> one-shot effect -> evidence digest
  v
Validating
  |
  +-- current Definition/Instance/durable plan 재검증
  +-- exact EffectVerifier lookup
  +-- read-only verification
  +-- Core-owned disposition/summary/Receipt/digest
  v
RunStore.append_with_execution_receipt
  |
  +-- VerificationRecorded event
  +-- complete ExecutionReceipt document
  +-- Receipt chain/index
  +-- WorkGraph projection
  v
Completed | Failed | ManualRequired
```

`DirectExecutor`와 `VerificationRunner`를 분리한 이유는 `Validating` recovery에서 material reconstruction과 external effect 코드를 호출하지 않기 위해서다. Host loop는 실행 결과가 `Validating`이면 verifier registry와 함께 `VerificationRunner::drive_step`을 호출한다.

| Receipt 정보 | 정본과 결정 주체 |
|---|---|
| Run/Step, 시작·종료 시각 | journal |
| Capability/Instance/input/effect | durable `EffectIntent` |
| profile version/policy/executor/input summary/verification plan | authorization-bound `ReceiptProvenance` |
| output/evidence observation | exact verifier |
| required, summary, status, Receipt ID/digest/chain | Core |
| persistence와 원자성 | `RunStore` |

## Composition root

문서 catalog, effect behavior와 verifier behavior는 서로 다른 registry다.

```rust
let mut adapters = EffectAdapterRegistry::new();
let verifier = adapter.verifier();
adapters.register(&instance.binding, adapter)?;

let mut verifiers = EffectVerifierRegistry::new();
verifiers.register(&instance.binding, verifier)?;
```

두 registry 모두 complete Instance binding exact match만 허용한다. Operation 또는 protocol version이 다르면 fallback하지 않으며, duplicate registration은 기존 entry를 바꾸지 않는다.

## Verifier port

```rust
trait EffectVerifier {
    fn verify(
        &mut self,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationReport, VerificationPortFailure>;
}
```

Request는 getter-only이며 다음 Core-verified 값만 제공한다.

- committed `EffectIntent`
- admission 당시 digest와 일치하는 current Capability Definition
- executable binding digest가 일치하는 current Capability Instance
- effect success 또는 applied reconciliation의 canonical evidence digest

Report는 bounded output digest와 positional rule observation을 반환한다. Report가 free-form summary, `required` flag, Receipt ID/status 또는 executor/policy identity를 만들 수 없다. Core가 durable verification plan과 다음 항목을 대조한다.

- rule 개수
- rule 순서
- strategy exact match
- digest 형식
- passed observation의 evidence digest 존재

Verifier 자체의 unavailable/unsupported/evidence-unavailable/unverifiable 오류는 fixed enum이다. Raw OS/library 오류를 journal이나 Receipt에 연결하지 않는다.

## Receipt 생성과 판정

Core는 admission provenance, durable intent, start event, verifier report와 event factory 시각으로 `ExecutionReceiptBody`를 만든다. 현재 profile은 `xgeny.core-receipt/v1`이며 ID·summary·redaction·판정 함수는 `xgeny-protocol`의 한 구현을 runtime과 store가 공유한다. `receiptDigest`는 `kind: ExecutionReceipt`를 포함한 전체 protocol document에서 top-level `receiptDigest`만 제외해 RFC 8785 canonical JSON/SHA-256으로 계산한다.

```text
required failed                           -> failed / Failed
required inconclusive                     -> unknown / ManualRequired
passed observation 없음                   -> unknown / ManualRequired
그 외 최소 하나 passed                    -> succeeded / Completed
```

성공 Receipt에는 protocol schema 규칙에 따라 최소 하나의 passed verification evidence가 있고 required failed/inconclusive가 없다.

현재 Receipt는 raw input과 raw output 대신 digest와 다음 고정 redaction 설명만 보존한다.

- `raw invocation arguments omitted`
- `raw tool output omitted`

## Store 불변 조건

Memory와 SQLite store는 같은 bundle 검사를 수행한다.

1. `VerificationRecorded`는 Receipt 없이 일반 append할 수 없다.
2. 새 `EffectIntentCommitted`는 supported Receipt provenance가 필수며 legacy absence는 migration load/replay에서만 허용한다.
3. Legacy `VerificationPassed/Failed`는 replay만 가능하고 새 append는 거부한다.
4. Receipt가 다른 event, Run, Step, effect 또는 intent에 붙을 수 없다.
5. Receipt protocol schema와 canonical digest를 cold open, 명시적 full audit 또는 외부 SQLite generation 변경 때 다시 검증한다. 같은 generation의 runtime hot path는 ADR-0013의 verified index를 사용한다.
6. Capability, Instance, input, policy, executor, effect와 verification plan이 admission provenance와 같아야 한다.
7. Receipt ID, extensions, Artifact, summary, redaction은 현재 Core builder의 deterministic shape와 같아야 한다.
8. Receipt started/ended 시각이 journal의 start/final event 시각과 같고 RFC 3339 시간 순서가 역전되지 않아야 한다.
9. Receipt 수와 finalization event 수가 같고 `previousReceiptDigest`가 journal 순서의 직전 Receipt를 가리켜야 한다.
10. Persisted projection은 journal replay 결과와 같아야 한다.

Admission은 Receipt가 참조하는 transient `PolicyDecisionBody`를 bundled schema와 semantic rule로 검증한 뒤에만 digest를 commit한다. Candidate schema 오류는 fixed `PolicyDecisionInvalid`로 축소해 resource나 timestamp를 error에 echo하지 않는다. EventFactory timestamp도 append 전에 RFC 3339로 검증하며 invalid Started metadata에서는 effect를 호출하지 않는다.

`RunStore::export_jsonl`은 기존 journal-only canonical stream이다. Complete Receipt는 `export_execution_receipts_jsonl`에서 `kind: ExecutionReceipt`를 포함한 protocol document로 chain 순서에 export한다. 현재 두 export는 하나의 atomic archive/import format이 아니며, SQLite built-in의 `load_with_execution_receipts`만 한 read transaction에서 Run과 Receipt chain을 읽는다. Runtime의 Receipt finalization은 전체 vector 대신 같은 verified generation에서 state, start 시각과 이전 Receipt digest만 반환하는 `load_verification_snapshot`을 사용한다.

SQLite schema 4 transaction은 다음 순서로 수행한다.

```text
BEGIN IMMEDIATE
  current journal/projection/sidecar 검증
  candidate event + Receipt 검증
  insert event
  insert Receipt
  write projection
COMMIT
```

Receipt insert 또는 projection write에서 실패하면 event를 포함한 전체 transaction이 rollback된다.

## Schema 3 호환성

과거 journal event의 Rust 필드명은 evidence 의미로 정리했지만 serialized JSON key `receiptDigest`는 유지한다. Optional provenance와 execution Receipt projection field는 없는 경우 serialize하지 않아 schema 3 event의 canonical bytes와 digest를 바꾸지 않는다.

Schema 3 open은 기존 journal과 sidecar를 검증한 뒤 빈 `execution_receipts` table을 원자적으로 추가한다. 과거 terminal event에 Receipt를 조작해 backfill하지 않는다. 실제 provenance 없는 pending/Validating schema 3 fixture로 migration과 journal byte 보존을 검증한다. Legacy Run은 그대로 replay할 수 있지만 schema 4에서 provenance 없는 intent를 새 append하는 것은 금지한다.

Schema 3의 `IntentCommitted`, `EffectUnknown` 또는 `Reconciling` intent에는 provenance가 없으므로 자동 실행·reconciliation을 금지한다. Direct Executor는 material/adapter 접근과 Started event 전에 `ReceiptProvenanceUnavailable`로 닫는다. Schema 3의 `Validating`도 profile과 verification plan을 추측하지 않아 `ReceiptProvenanceMissing`으로 닫고 verifier 0회·상태 보존을 보장한다. 사용자가 legacy effect를 새 schema 4 의미로 재승인·대체하거나 종결하는 workflow는 후속 migration 정책이다.

## 재시작 판정표

| durable 상태 | VerificationRunner 동작 | effect adapter 호출 |
|---|---|---|
| schema 4 provenance가 있는 `Validating` | exact verifier로 read-only 검증 후 Receipt commit | 0 |
| provenance가 없는 legacy `Validating` | fixed error, 상태 보존 | 0 |
| `Completed`, `Failed`, `ManualRequired` | no-op, store integrity만 load에서 검증 | 0 |
| 그 외 | 잘못된 호출로 거부 | 0 |

Receipt commit 전 장애에서는 `Validating`이 유지되어 verifier가 다시 호출될 수 있다. Commit 후 acknowledgement 유실에서는 reopen한 store가 Receipt와 terminal event를 발견하므로 verifier를 다시 호출하지 않는다. Verifier는 read-only·deterministic/idempotent contract를 지켜야 하지만 hostile implementation을 process 안에서 격리하지는 않는다.

Store의 public Rust API는 trusted composition root가 호출한다. Store 검사는 Receipt의 durable binding과 deterministic profile을 검증하지만 verifier 호출 자체를 인증하지 않는다. 비신뢰 plugin이 같은 process에서 `RunStore`를 직접 호출할 수 있는 배치는 이 gate의 위협 모델 밖이며 process/IPC 또는 서명 권위가 추가로 필요하다.

Malformed verifier report는 Receipt 0건인 `Validating`으로 남는다. Event와 Receipt의 누락·변조는 재시도 가능한 verification 오류가 아니라 store corruption이며, 손상된 Run에는 새 event를 추가하지 않는다.

## Reference verifier 실증

`PreopenedMarkerVerifier`는 effect adapter와 같은 preopened file handle을 공유하되 bounded read만 수행한다.

- 실행 evidence digest와 현재 파일 byte digest가 같으면 passed
- effect 이후 파일이 바뀌었으면 failed Receipt
- `Validating` SQLite reopen 뒤 파일 재작성 없이 verifier만 호출
- Receipt, journal, DB/WAL/SHM에 raw marker·opaque target·OS 오류 비노출

Reference adapter는 typed output이 없으므로 `outputDigest`로 canonical empty object digest를 사용한다. 이는 범용 output 설계가 아니라 verification/store 수직 슬라이스를 닫기 위한 고정 fixture다.

## 실행 가능한 검증

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo run --locked --quiet -p xgeny-cli -- protocol check
cargo build --workspace --release --locked
```

핵심 회귀 범위는 다음과 같다.

- success, required failure와 required inconclusive 상태 매핑
- malformed rule coverage의 fail-closed `Validating`
- Receipt schema/digest, event/intent/provenance/timestamp/chain binding
- 두 effect Receipt의 journal-order chain과 잘못된 두 번째 link 거부
- Memory/SQLite 의미 동등성
- Receipt transaction 단계별 rollback
- Receipt row insert 직후 child-process exit rollback
- missing/tampered Receipt 탐지
- schema 3 → 4 journal byte 보존 migration
- Receipt acknowledgement 유실 뒤 verifier·effect 중복 0회
- 실제 SQLite reopen 뒤 lost-ack terminal no-op
- legacy pending intent의 Started/prepare/execute 0회
- legacy `Validating`의 verifier/Receipt 0회와 상태 보존
- unsupported profile, 다른 executor platform과 Definition verification-plan drift의 pre-effect/pre-verifier 차단
- invalid PolicyDecision·Started timestamp의 fixed-error/0-effect 차단
- complete Receipt JSONL 2건의 Memory/SQLite byte parity·kind·chain order
- verifier duplicate non-replacement와 nearby binding fallback 0회
- applied reconciliation evidence에서 Receipt 종결
- Reference I/O 성공, tamper 실패와 `Validating` restart

## 남은 범위

이 기본형은 `EffectSucceeded`와 `ReconciliationResolution::ProvedApplied`가 도달하는 `Validating` 경로를 닫는다. 다음은 별도 수직 slice다.

- Adapter definite failure·effect unknown·cancelled/blocked/not-started Receipt
- 실제 typed output과 Artifact store
- PolicyDecision/InvocationPlan document 조회와 Receipt signing
- verifier timeout/backoff/cancellation
- 제품 filesystem/process/MCP/Connector/XGEN adapter
- Tracked/Persistent WorkGraph dependency와 runnable frontier
- atomic Run archive/import manifest
- 장기 Run의 incremental Receipt 검증 index와 성능 budget

현재 load/append는 tamper 탐지를 우선해 committed Receipt chain을 다시 검증한다. Schema validator compile은 process-wide cache하지만 Receipt와 event가 큰 장기 Run에서는 검증 비용이 누적된다. 따라서 이 기본형을 unbounded production workload에 적합하다고 주장하지 않으며 Tracked/Persistent 단계 전에 one-pass index와 reopen/append 성능 budget을 별도 gate로 둔다.

설계 결정은 [ADR-0012](../adr/0012-core-verification-and-execution-receipt.md)를 따른다.
