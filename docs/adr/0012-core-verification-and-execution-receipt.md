# ADR-0012: Core verification과 Execution Receipt 원자 확정

- 상태: 제안 — 연구 gate 기본형 구현
- 기준일: 2026-08-29
- 적용 범위: effect 성공 또는 applied reconciliation 이후의 검증·종결 경계
- 공개 protocol v0.1 변경: 없음
- local store schema: 3 → 4

> 후속 상태: ADR-0013이 schema와 wire를 바꾸지 않고 one-pass Receipt anchor 검증, connection-local VerifiedRunIndex와 runtime 최소 view를 추가했다. ADR-0014는 Receipt-bound completion만 dependency를 해제하는 Persistent WorkGraph frontier를 추가하고 semantic downgrade 방지를 위해 local store schema를 5로 올렸다. ADR-0018은 v1의 빈 Artifact 의미를 영구 유지하면서 planned ReadOnly 전용 `xgeny.core-receipt/v2`와 bounded Artifact commitment를 추가했다. 아래 schema 4와 후속 범위 설명은 ADR-0012 결정 당시의 기록이다.

## 문맥

ADR-0011까지의 Direct Executor는 외부 effect의 성공 evidence를 기록한 뒤 Step을 `Validating`에 남겼다. 그러나 adapter outcome의 digest가 실제 protocol `ExecutionReceipt`처럼 이름 붙어 있었고, 검증 주체·Receipt 작성 주체·원자 저장 단위가 정해지지 않았다. 이 상태에서는 다음을 보장할 수 없다.

- adapter가 실행 evidence와 최종 Receipt identity를 혼동하지 않는가
- 재시작 뒤 effect를 반복하지 않고 read-only 검증만 재개하는가
- 성공 상태, Receipt와 journal event가 함께 commit되는가
- Receipt가 admission 당시 Capability, Instance, 입력, 정책과 executor 사실에 결합되는가
- 필수 검증의 실패 또는 불확실성이 성공으로 승격되지 않는가

## 결정

### 1. Adapter evidence와 Execution Receipt를 분리한다

Adapter는 외부 effect를 관찰한 canonical evidence digest만 반환한다. Receipt ID, Receipt status, policy·executor provenance, verification summary와 Receipt digest는 Core만 결정한다.

기존 schema 3 journal의 `EffectSucceeded.receiptDigest`와 `EffectFailed.receiptDigest` wire key는 event hash 호환성을 위해 그대로 유지한다. Rust API와 projection에서는 의미를 `evidence_digest`와 `effect_evidence_digest`로 바로잡는다. 과거 event JSON을 새 이름으로 다시 쓰지 않는다.

### 2. Receipt provenance를 admission에서 고정한다

새 `EffectIntent`는 secret-free `ReceiptProvenance`를 포함한다.

- Core Receipt profile version (`xgeny.core-receipt/v1`)
- deterministic invocation ID와 plan ID
- bundled schema·semantic validation을 통과한 canonical `PolicyDecision` document의 decision ID와 digest
- executor ID, placement와 platform
- 고정된 redacted input summary
- Definition에서 복사한 verification strategy와 required flag

Provenance digest를 once authorization binding에 포함해 intent와 함께 hash-chain journal에 commit한다. Adapter와 verifier는 이를 수정할 수 없다.

현재 invocation ID와 plan ID는 durable identity commitment이며 별도의 `InvocationPlan` document를 저장하지 않는다. Policy도 full body가 아니라 decision ID/digest commitment만 보존한다. 문서 조회 가능한 provenance와 서명은 후속 범위다.

이 기본형의 executor provenance는 local Direct Executor만 표현한다. Router가 Device 또는 Remote Instance를 선택하면 admission에서 intent를 발행하지 않는다. 새 실행 또는 `ProvedNotApplied` 재실행 직전에도 durable executor ID·local placement·OS/architecture를 현재 process와 대조한다. 따라서 다른 host에서 effect를 실행하고 이전 host의 platform으로 Receipt를 쓰지 않는다. 이미 effect가 끝난 `Validating`의 read-only 검증은 원래 executor provenance를 보존할 수 있다. 원격 adapter는 실제 executor ID·platform·attestation을 한 권위에서 제공하는 별도 계약 뒤에 추가한다.

### 3. 검증은 exact read-only port로 실행한다

Host는 `EffectVerifierRegistry`에 verifier를 complete Instance binding으로 등록한다. Lookup은 `binding_ref + operation_ref + protocol_version`을 byte-exact로 사용하며 fallback과 duplicate replacement를 금지한다.

Verifier가 받는 값은 Core가 durable state에서 다시 검증한 intent, current Definition, current Instance와 adapter evidence digest다. Verifier는 다음 closed result만 반환한다.

- bounded output digest
- Definition rule 순서와 일치하는 strategy/result/evidence digest

Rule의 `required` 값과 사람이 읽는 summary는 verifier가 아니라 Core가 durable plan에서 생성한다. Rule 수·순서·strategy가 다르거나 passed observation에 evidence digest가 없거나 Definition/Instance가 admission 이후 바뀌면 Receipt를 만들지 않고 `Validating`에 남긴다.

### 4. Verification 결과가 terminal 상태를 결정한다

```text
EffectSucceeded 또는 Reconciliation(ProvedApplied)
                     |
                     v
                Validating
                     |
             exact read-only verifier
                     |
         Core ExecutionReceipt 생성·검증
                     |
       Receipt + event + projection 원자 commit
          /              |               \
     passed           failed        inconclusive
       |                 |                |
   Completed           Failed       ManualRequired
```

판정 규칙은 다음과 같다.

| Core disposition | Receipt status | Step status | 조건 |
|---|---|---|---|
| `passed` | `succeeded` | `Completed` | required fail/inconclusive가 없고 최소 하나가 passed |
| `failed` | `failed` | `Failed` | required rule이 하나 이상 failed |
| `inconclusive` | `unknown` | `ManualRequired` | required inconclusive가 있거나 passed evidence가 없음 |

필수 검증 실패·불확실성을 optional 성공으로 덮어쓰지 않는다. Terminal Step을 다시 drive하면 verifier를 호출하지 않고 no-op 한다.

### 5. Receipt와 terminal event를 한 transaction으로 저장한다

`RunStore::append_with_execution_receipt`는 다음을 하나의 commit으로 묶는다.

```text
VerificationRecorded event
  + complete ProtocolDocument::ExecutionReceipt
  + Receipt index와 previousReceiptDigest chain
  + WorkGraph projection
```

일반 `append`는 새 `VerificationRecorded`와 legacy `VerificationPassed/Failed` 작성을 거부한다. 따라서 새 성공 경로가 Receipt 없이 `Completed`로 우회할 수 없다.

Store load는 protocol schema와 RFC 8785 digest를 다시 검증하고, Receipt row를 journal event·effect intent·admission provenance·검증 plan·시각·이전 Receipt chain과 대조한다. Missing, extra, swapped 또는 변조된 Receipt는 Run corruption으로 닫는다.

저수준 Store API도 Receipt 없는 terminal 전이와 durable fact·profile 불일치를 허용하지 않는다. Deterministic Receipt ID, 빈 extensions·Artifact, 고정 input summary·verification summary·redaction 목록, passed evidence digest와 시작/종료 시각 순서를 다시 검사한다. 이 검사는 Receipt 내부 일관성과 persisted binding을 증명하지만 verifier 함수가 실제로 호출됐다는 actor authentication은 아니다. `RunStore`를 호출하는 composition root는 trusted computing base다.

기존 `export_jsonl`은 호환성을 위해 journal-only 진단 stream으로 유지한다. Complete `ProtocolDocument::ExecutionReceipt`는 `export_execution_receipts_jsonl`로 journal/chain 순서에 export한다. 둘을 한 시점의 portable archive로 묶는 manifest/import 계약은 아직 없으므로 두 stream을 임의로 합쳐 atomic backup이라고 주장하지 않는다.

### 6. SQLite schema 4는 schema 3을 additive migration한다

Schema 4는 `execution_receipts` sidecar table을 추가한다. Schema 3의 event JSON, event digest와 projection 의미는 바꾸지 않는다. Migration은 `BEGIN IMMEDIATE` 안에서 기존 journal, projection, intent, authorization과 material index를 먼저 재검증한 뒤 table 생성과 `user_version = 4`를 commit한다.

- version 0: schema 4 신규 생성
- version 3: 검증 후 schema 4로 원자 migration
- version 4: 전체 derived state와 Receipt 재검증
- version 1, 2 또는 5 이상: mutation 없이 거부

Embedded SQLite는 Rust binary에 link된다. 사용자가 SQLite server, daemon 또는 CLI를 별도로 설치하지 않는다.

Schema 3에서 migration된 pending intent에는 Receipt provenance가 없다. 이를 추측해 backfill하지 않으며 Direct Executor는 Started commit과 adapter prepare/execute 전에 고정 오류로 거부한다. Schema 4 store의 새 `EffectIntentCommitted` append는 supported provenance가 없으면 authorization budget을 소비하기 전에 거부하며, legacy 호환은 load/replay에만 적용한다. 이미 `Validating`인 legacy Step도 Receipt profile과 verification plan을 추측하지 않으며 verifier를 호출하지 않고 `ReceiptProvenanceMissing`으로 닫힌 상태를 유지한다. legacy 재승인·종결 migration workflow는 후속 정책이다. 이미 terminal인 legacy Run은 그대로 replay된다.

## 재시작과 장애 의미

- Effect outcome이 durable한 `Validating`에서는 material provider, adapter prepare, execute와 reconcile을 호출하지 않는다.
- Verifier 또는 Receipt commit 전 process가 종료되면 effect를 다시 실행하지 않고 read-only verification만 재시도할 수 있다.
- Receipt transaction이 commit된 뒤 acknowledgement만 유실되면 reopen 시 terminal state와 Receipt 한 건을 읽으며 verifier와 effect를 다시 호출하지 않는다.
- Verification port failure, malformed report 또는 protocol validation failure는 terminal event를 만들지 않고 `Validating`을 유지한다.
- EventFactory timestamp는 event append 전에 RFC 3339로 검증한다. 특히 invalid Started 시각은 effect 호출 0회인 `IntentCommitted`에서 닫힌다.
- Hash chain과 Receipt digest는 local tamper detection이지 hostile OS에 대한 actor authentication이나 외부 effect의 물리적 exactly-once 증명이 아니다.
- Public Rust `RunStore` trait를 호출할 수 있는 hostile in-process code를 Receipt 작성 권위에서 격리하지 않는다. 제품 plugin 경계가 비신뢰라면 별도 process/IPC 또는 서명된 authority 계약이 필요하다.

## Reference adapter 실증

비제품 `xgeny-adapter-reference`는 실행 때 preopened file에 bounded evidence를 쓰고, 별도 verifier는 같은 handle을 read-only로 다시 읽어 digest를 비교한다. 파일이 effect 이후 검증 전에 바뀌면 failed Receipt가 생기며 effect는 반복하지 않는다. SQLite를 `Validating`에서 닫고 다시 열어도 verifier만 실행한다.

Reference verifier의 `outputDigest`는 현재 typed output이 없음을 나타내는 고정 empty-object commitment다. 일반 tool adapter는 실제 bounded typed output의 digest를 반환해야 하며 raw output을 Receipt에 넣지 않는다.

## 명시적 비목표

- Adapter가 직접 definite failure를 반환한 경로의 Core Receipt
- 해결되지 않은 effect unknown, cancellation, blocked와 not-started Receipt
- Typed tool output body, Artifact content store와 ArtifactRef lifecycle
- InvocationPlan과 PolicyDecision full document 저장·조회
- Receipt 서명, DSSE/in-toto와 원격 attestation
- Hostile in-process verifier/adapter 격리
- Filesystem/process/MCP/Connector/XGEN 제품 adapter
- Device/Remote executor provenance와 실행
- Async verifier, timeout/backoff와 durable blocked scheduling
- Journal과 Receipt를 한 generation으로 묶는 portable archive/import manifest
- 장기 Run용 incremental Receipt 검증 index와 bounded load/append 비용

따라서 이 slice를 “모든 실행 시도에 Receipt가 있다”고 표현하지 않는다. 현재 보장은 성공 또는 applied reconciliation로 `Validating`에 도달한 새 schema 4 실행의 Receipt-bound terminal 확정이다.

## 승인 조건

- Exact verifier lookup과 malformed report가 fail closed한다.
- Required failed/inconclusive가 `Completed`가 되지 않는다.
- 생성 Receipt가 bundled schema와 canonical digest 검증을 통과한다.
- Receipt event, sidecar와 projection의 모든 transaction fault가 전체 rollback된다.
- Receipt row 삽입 직후 process exit가 전체 finalization을 rollback한다.
- Missing/tampered Receipt와 journal·intent·chain binding 불일치를 load에서 탐지한다.
- Schema 3 journal bytes와 digest가 schema 4 migration 뒤 유지된다.
- `Validating` restart와 lost acknowledgement에서 effect·verifier·Receipt 중복이 없다.
- Schema 3 pending intent가 provenance 없이 effect를 시작하지 않는다.
- Schema 4가 provenance 없는 새 intent append를 거부하고 실제 schema 3 pending/Validating fixture는 migration·replay한다.
- Schema 3 legacy `Validating`이 verifier를 호출하거나 Receipt를 추측하지 않는다.
- Unsupported Receipt profile, 다른 executor platform과 Definition verification-plan drift가 adapter/verifier 호출 전에 거부된다.
- Invalid PolicyDecision 또는 event timestamp가 fixed error로 닫히고 candidate 값을 error에 노출하지 않는다.
- Journal JSONL과 complete Receipt JSONL의 Memory/SQLite byte parity가 유지된다.
- Raw argument, marker, target과 OS 오류가 Receipt, Debug, journal, SQLite/WAL/SHM에 없다.
- Memory와 SQLite의 의미가 같고 Linux, macOS, Windows workspace CI가 통과한다.

## 결과

Trusted XGENy composition root의 정상 경로에서 성공은 adapter의 자기보고가 아니라 durable intent에 결합된 Core verification과 protocol Receipt가 함께 있을 때만 dependency를 해제할 수 있다. 이 연구 gate는 hostile Rust caller가 public store API를 호출했다는 사실까지 인증하지 않는다. 동시에 XGENy Core는 XGEN, Connector 또는 외부 DB에 의존하지 않고, 향후 외부 harness에는 versioned Receipt/event contract만 제공할 수 있다.

다음 큰 수직 slice는 이 terminal gate 위에 Tracked/Persistent WorkGraph dependency와 runnable frontier를 연결하는 것이다. Failure/unknown Receipt와 typed output/Artifact는 그와 분리된 후속 연구 gate로 유지한다.
