# ADR-0019: Durable ToolOutputRecord와 local store schema 7

- 상태: 채택
- 기준일: 2026-08-30
- 적용 범위: WorkGraph tool-output 의미, Direct Executor, verification snapshot, local RunStore
- 공개 protocol v0.1 schema 변경: 없음
- local store schema: 6에서 7로 변경

## 문맥

[ADR-0018](0018-read-only-artifact-and-cli-driver-foundation.md)은 planned `ReadOnly` 실행과 Core Receipt v2를 도입했지만, adapter가 관찰한 typed JSON 본문은 durable state에 남기지 않았다. `EffectSucceeded`와 Receipt에는 digest만 있었고, verifier는 외부 대상을 다시 읽거나 별도 in-memory 값에 의존해야 했다. 프로세스가 종료되면 다음 planning turn에 전달할 정확한 tool output도 복원할 수 없었다.

Raw output을 journal event나 `RunState` projection에 직접 넣으면 다음 문제가 생긴다.

1. journal export, 일반 `Debug`와 projection read에 민감할 수 있는 본문이 확산된다.
2. 큰 output이 모든 replay와 verified-index cache 비용에 포함된다.
3. effect outcome과 별도 파일/blob 저장 사이에 crash window가 생긴다.
4. schema 3~6의 기존 event/projection canonical bytes와 digest를 보존하기 어렵다.

반대로 SQLite row만 추가하고 journal에 commitment를 남기지 않으면, row의 value와 digest를 함께 바꾼 변조를 journal hash chain이 감지하지 못한다. 따라서 typed output은 **journal outcome에 anchor된 원자 sidecar**여야 한다.

## 결정

### 1. 새 ReadOnly 실행은 durable tool-output profile을 명시한다

`ReceiptProvenance`에 다음 optional field를 추가한다.

```text
toolOutputProfile = "xgeny.tool-output/v1"
```

이 field는 `None`일 때 deserialize default를 사용하고 serialize에서 생략한다. schema 3~6의 기존 provenance bytes와 이를 덮는 authorization digest를 바꾸지 않기 위한 조건이다.

Schema 7에서 새 intent를 append할 때 store는 다음 semantic fence를 적용한다.

| effect 의미 | Core Receipt profile | tool output profile |
|---|---|---|
| `ReadOnly` | `xgeny.core-receipt/v2` | `Some("xgeny.tool-output/v1")` |
| effectful 기존 의미 | `xgeny.core-receipt/v1` | `None` |

이 검사는 새 candidate append에 적용한다. Cold replay에 같은 조건을 소급 적용하지 않는다. 그래야 output profile이 없던 schema 3~6 history를 그대로 읽을 수 있으면서, schema 7 writer가 legacy outputless 의미로 새 ReadOnly intent를 만드는 downgrade는 막을 수 있다.

Direct Executor도 실행 전에 같은 fence를 검사한다. 따라서 migration된 pending ReadOnly intent가 profile 없이 남아 있으면 adapter를 호출하지 않고 fail-closed한다. Tool-output profile을 가진 effect에서 `ReconciliationResolved::ProvedApplied`는 output을 복원할 수 없으므로 `Validating`으로 진행하지 않는다.

Schema version 자체도 binary compatibility fence다. Schema 7 DB는 schema 6 binary가 열어 쓰지 못하며, schema 7 writer는 알 수 없는 더 높은 version을 거부한다.

### 2. ToolOutputRecord는 exact 실행 관찰을 self-verify한다

`ToolOutputRecord` v1은 다음 값을 보존한다.

```text
formatVersion
outputId
runId / stepId / effectId
actionDigest
exact InvocationBinding
  capabilityId / contractVersion / definitionDigest
  instanceId / instanceBindingDigest
planId
executionAttempt
adapter evidenceDigest
canonicalSizeBytes
outputDigest
typed JSON output
recordDigest
```

Core는 adapter가 반환한 raw JSON에 durable identity를 맡기지 않는다. Direct Executor가 현재 registry의 Definition digest가 intent와 같은지 먼저 확인하고, 그 exact Definition의 `outputSchema`를 Draft 2020-12, offline mode로 검증한다. 그 뒤 Core가 bounded record를 생성한다. Local store는 registry나 schema document를 소유하지 않고, record와 intent의 exact `definitionDigest` binding을 감사한다.

고정 구조 상한은 다음과 같다.

- RFC 8785 canonical output: 최대 1,048,576 bytes
- JSON nesting depth: 최대 64
- JSON value/node 수: 최대 32,768
- object key와 string value UTF-8 bytes 합계: 최대 1,048,576
- SQLite `record_json`: metadata 여유를 포함해 최대 1,100,000 bytes

Depth, node와 text bound를 canonicalization 전에 iterative traversal로 검사한다. Canonicalization 또는 schema 오류는 raw value를 포함하지 않는 fixed error로 축소한다.

### 3. Digest 의미를 서로 섞지 않는다

| 값 | 의미 |
|---|---|
| adapter `evidenceDigest` | adapter가 실행 관찰에 대해 제출한 evidence commitment |
| `outputDigest` | `SHA-256(JCS(output))` |
| `outputId` | Run/Step/Effect/attempt/output digest를 `xgeny.tool-output.id/v1` domain에서 결합한 ID |
| `recordDigest` | identity, action, invocation, plan, attempt, evidence, size와 `outputDigest`를 `xgeny.tool-output.record/v1` domain에서 결합한 digest |
| event record digest | `outputRecordDigest`를 포함한 기존 journal hash-chain digest |
| Receipt `outputDigest` | 해당 effect의 verified `ToolOutputRecord.outputDigest`와 정확히 같은 값 |
| Artifact digest | 파일 등 artifact 자체의 bytes에 대한 capability별 commitment |

`recordDigest` 입력에 raw output을 중복 직렬화하지는 않지만, load와 cold audit가 raw output의 JCS digest를 다시 계산해 `outputDigest`를 검증하므로 output을 간접적으로 exact bind한다. `EffectSucceeded` event는 `outputRecordDigest = record.recordDigest`를 commit한다.

Receipt output digest와 Artifact digest는 일반적으로 같다고 가정하지 않는다. 예를 들어 typed JSON output 안에 file-byte digest가 들어 있어도, record `outputDigest`는 그 JSON 전체의 JCS digest다.

서로 다른 effect가 같은 JSON을 반환하는 것은 정상이다. 따라서 SQLite의 `output_digest`에는 `UNIQUE`를 걸지 않는다.

### 4. Event, sidecar와 projection을 한 transaction으로 commit한다

`EffectSucceeded`와 `StepState`에 `outputRecordDigest: Option<String>`을 추가한다. `None`은 serialize에서 생략해 legacy wire shape를 유지한다. Raw output은 두 곳에 들어가지 않는다.

Store API는 output success 전용 bundle을 제공한다.

```text
append_with_tool_output(expectedHead, EffectSucceeded, ToolOutputRecord)
load_tool_output(effectId)
load_verification_snapshot(stepId) -> state + receipt head + exact tool output
```

Plain `append`는 output-bound success를 거부한다. Bundle API는 missing/extra record, event digest mismatch, Run/Step/Effect/action/invocation/plan/attempt/evidence mismatch와 duplicate identity를 모두 mutation 전에 거부한다.

SQLite transaction의 의미 순서는 다음과 같다. 중간 derived-index no-op 단계는 생략했다.

```text
BEGIN IMMEDIATE
  verify data_version, journal head and cached commitments
  prepare/reduce EffectSucceeded candidate
  INSERT run_events(outputRecordDigest)
  INSERT tool_outputs(record_json)
  WRITE run_projection(outputRecordDigest only)
COMMIT
```

`tool_outputs` table은 한 effect의 bounded final output 하나를 저장한다.

```sql
CREATE TABLE tool_outputs (
    effect_id TEXT PRIMARY KEY,
    event_sequence INTEGER NOT NULL UNIQUE CHECK (event_sequence >= 1),
    step_id TEXT NOT NULL,
    output_id TEXT NOT NULL UNIQUE,
    capability_id TEXT NOT NULL,
    contract_version TEXT NOT NULL,
    definition_digest TEXT NOT NULL,
    canonical_size_bytes INTEGER NOT NULL
        CHECK (canonical_size_bytes BETWEEN 1 AND 1048576),
    output_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL
        CHECK (length(record_json) BETWEEN 1 AND 1100000),
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence),
    FOREIGN KEY (effect_id) REFERENCES effect_intents(effect_id)
) STRICT;
```

Cold audit는 raw output 전체를 verified cache에 넣지 않는다. Event anchor와 digest commitment만 index에 유지하고, SQLite row를 event order로 한 개씩 읽어 다음을 다시 확인한다.

- row count와 output-bound success event count
- event sequence, effect, Step와 record digest
- exact intent, invocation, plan, attempt와 evidence binding
- indexed columns와 `record_json`의 동일성
- raw output canonical size/digest와 record digest
- output ID, effect ID와 record digest uniqueness
- Receipt output digest와 verified output digest의 동일성

Point load와 verification snapshot도 같은 SQLite read transaction에서 verified state/index와 row를 읽는다. 외부 connection이 output row만 바꿔 journal head가 같더라도 SQLite `data_version` 변화로 cache를 폐기하고 full audit한다.

### 5. Schema 3~6 migration은 backfill하지 않는다

Migration은 `BEGIN IMMEDIATE` 안에서 version을 다시 읽고, 해당 legacy topology를 감사한 뒤 새 table과 `user_version = 7`을 함께 publish한다.

| 시작 schema | legacy에 없는 physical table | schema 7 동작 |
|---|---|---|
| 3 | Receipt, planned invocation, tool output | 기존 journal/projection/intent/material 감사, 빈 missing table 생성 |
| 4 | planned invocation, tool output | Receipt chain 포함 전체 감사, 빈 missing table 생성 |
| 5 | planned invocation, tool output | schema 5 semantic fence 포함 전체 감사, 빈 missing table 생성 |
| 6 | tool output | plan/material/Receipt 포함 전체 감사, 빈 output table 생성 |

기존 event JSON, digest, projection, intent, authorization, material, plan input와 Receipt bytes는 수정하지 않는다. Legacy output을 Artifact, evidence 또는 외부 파일에서 추측해 backfill하지 않는다. 따라서 migration 직후 `tool_outputs`는 비어 있다.

Journal에 output/Receipt sidecar가 필요하다고 주장하는 event가 있는데 해당 legacy schema에 table/row가 없는 hostile fixture는 migration을 거부한다. DDL, audit 또는 foreign-key 검사가 실패하면 새 table과 version update를 모두 rollback한다. 다른 migration writer를 기다리는 동안 관찰 version이 4, 5, 6 또는 7로 바뀐 경우 transaction 안에서 다시 감사하고 수렴한다.

### 6. Raw output 노출은 의도적으로 좁고, 암호화 보장은 아니다

Raw output은 다음 위치에는 존재한다.

- SQLite `tool_outputs.record_json`
- SQLite DB/WAL과 이를 포함한 backup
- trusted execution/verification 중의 bounded in-memory value
- 명시적 `load_tool_output` 또는 verifier request 결과

Raw output은 다음 위치에는 넣지 않는다.

- journal event와 journal JSONL export
- `RunState` projection과 verified index cache
- ExecutionReceipt와 Receipt JSONL export
- `Debug`, fixed error, 일반 log와 telemetry

`ToolOutputRecord`, adapter observation/candidate와 verification request는 redacted `Debug`를 사용한다. 다만 local SQLite는 평문 저장이다. 이 ADR은 at-rest encryption, DLP, secret detection 또는 backup encryption을 제공한다고 주장하지 않는다. SHA-256 digest도 confidentiality 수단이 아니다.

### 7. Crash와 restart는 durable state로만 판단한다

| 중단 지점 | 재시작 시 의미 |
|---|---|
| schema/size/record 검증 실패 | success event와 sidecar를 쓰지 않고 fixed Unknown outcome으로 수렴 |
| output transaction commit 전 crash/rollback | event, sidecar, projection 모두 없음; durable `Executing`은 기존 보수적 Unknown recovery를 따름 |
| commit 후 acknowledgement 유실 | reopen에서 `Validating + output`을 보고 adapter를 다시 실행하지 않음 |
| output commit 후 verifier 전 crash | 같은 durable output으로 verifier만 재개; adapter 호출 없음 |
| Receipt commit 전/후 crash | Receipt chain 규칙으로 재시도 또는 terminal no-op; adapter 호출 없음 |
| output row missing/tampered/orphan/swapped | recoverable tool failure가 아니라 Run corruption으로 fail-closed |

Verifier는 `RunVerificationSnapshot`의 durable output을 받는다. Output-required Receipt를 만들 때 verifier report의 output digest가 durable record와 정확히 같아야 한다. 검증을 위해 원본 파일을 다시 여는 동작은 이 계약의 authority가 아니며, 그런 재열기는 output commit 이후 파일이 바뀌는 TOCTOU를 만든다.

## Legacy compatibility

- Optional provenance/event/projection field의 `None`은 deserialize default이고 serialize에서 생략한다.
- Schema 3~6의 outputless pending, `Validating`과 terminal history는 기존 bytes 그대로 replay할 수 있다.
- Migration은 legacy success에 output을 발명하지 않는다.
- Schema 7의 새 append만 strict output profile을 요구한다.
- Migration된 pending ReadOnly intent에 tool-output profile이 없으면 Direct Executor는 adapter 준비/실행 전에 닫는다.
- Legacy `Validating` history에 durable output이 없으면 새 output-aware planning이나 verification 의미를 소급 주장하지 않는다.
- 새 schema/version/profile을 이해하지 못하는 old binary는 fail-closed한다.

## 회귀 gate

- JCS key order 안정성과 exact max/max+1, depth, node와 text bound
- Run/Step/Effect/action/invocation/plan/attempt/evidence swap 및 body/digest tamper 거부
- output raw sentinel의 journal/projection/Receipt/export/Debug/error 비노출
- plain append 우회 거부와 Memory/SQLite bundle parity
- same `outputDigest`를 가진 서로 다른 effect 허용
- event/output/projection 각 fault stage rollback과 child-process crash rollback
- commit acknowledgement 유실 뒤 adapter 중복 실행 0회
- missing/tampered/orphan/index-mismatch output row의 cold/warm audit 거부
- Receipt output digest mismatch 거부
- warm point load가 historical raw output을 재스캔하지 않고 cold audit가 각 row를 한 번만 방문
- schema 3/4/5/6에서 7로 byte-preserving migration, empty sidecar와 실패 rollback
- legacy pending ReadOnly 실행 0회와 outputless history replay
- Linux, macOS, Windows workspace suite

## 남은 작업

Schema 7 output durability만으로 사용자용 prototype은 완성되지 않는다. 다음 작업은 별도 결정과 회귀 gate가 필요하다.

1. `Completed + passed Receipt + exact output`만 한 generation에서 읽는 `xgeny.planning-context/v2`
2. model call/context/proposal/candidate와 raw final summary를 원자 저장하는 `CompletionOutputRecord`
3. restart 뒤 completion을 모델 재호출 없이 복원하는 public run/resume 표현
4. process restart에도 invocation argument를 복원할 권한 제한 durable recipe provider
5. 실제 bounded filesystem adapter와 descriptor-relative/no-follow confinement
6. filesystem read 승인과 model egress 승인 분리, untrusted output prompt-injection 경계
7. 같은 ReadOnly semantic action을 다시 관찰할 occurrence/generation identity
8. OpenAI-compatible provider의 planning context v2와 prompt revision 지원
9. output commit, Receipt commit, completion commit 뒤 실제 process restart E2E
10. packaged Linux/macOS/Windows binary와 opt-in live provider/tool smoke

현재 table은 effect당 bounded final JSON 하나만 지원한다. Streaming output, 여러 output generation, retention/GC, encryption과 remote synchronization은 이 schema의 암묵적 기능이 아니며 후속 schema 결정이 필요하다.

## 비목표

- Raw output을 public protocol document로 추가
- Journal 또는 projection에 raw output 저장
- Schema migration 중 legacy output 추론/backfill
- At-rest encryption, DLP 또는 secret 분류
- Streaming/chunked output과 effect당 여러 final output
- 동일 ReadOnly action 반복 관찰
- 실제 filesystem confinement와 public `xgeny run`
- planning context v2와 durable completion 본문
- write/process/network/MCP/XGEN adapter

## 결과

XGENy는 adapter가 반환한 bounded typed JSON을 exact Definition, intent와 execution attempt에 결합하고, `EffectSucceeded`, sidecar와 projection을 한 SQLite transaction으로 보존할 수 있다. Journal과 Receipt는 raw body 대신 검증 가능한 commitment만 유지한다. Schema 3~6 history는 재작성하거나 output을 발명하지 않고 schema 7로 이동한다.

이 결정이 닫는 범위는 **durable tool observation과 verification continuity**다. Output을 다음 model turn에 전달하고 최종 사용자 답변을 restart 뒤 복원하는 product continuity는 남은 작업에 명시한 context v2와 completion durability가 완료되어야 성립한다.
