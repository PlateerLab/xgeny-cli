# ADR-0027: NonIdempotent durable tool output와 no-replay 경계

- 상태: 채택
- 기준일: 2026-09-01
- 적용 범위: Capability Definition, Admission, WorkGraph, Direct Executor, Core Verification, local RunStore
- 공개 protocol v0.1 schema 변경: `durableToolOutput=true` 허용 effect에 `non_idempotent` 추가
- local store schema 변경: 없음(schema 8 유지)

## 문맥

후속 `xgeny.process/execute` Capability는 test, lint, build처럼 로컬 process를 한 번 실행하고 bounded stdout, stderr, exit status를 다음 planning turn에 전달해야 한다. Process 실행은 파일 생성, cache 변경, network 접근 또는 임의 project code 실행을 포함할 수 있으므로 일반적으로 `NonIdempotent`로 분류해야 한다.

기존 계약은 두 사실을 잘못 결합하고 있었다.

1. `ToolOutputRecord`를 durable하게 보존할 수 있는가.
2. 같은 effect를 안전하게 재실행할 수 있는가.

ADR-0019와 ADR-0025의 구현은 `ReadOnly` 또는 opt-in `Idempotent`에만 Core Receipt v2와 `xgeny.tool-output/v1`을 허용했다. 따라서 `NonIdempotent` 실행이 성공해도 typed output은 sidecar에 남길 수 없고 다음 model turn에 전달되지 않았다. 그렇다고 process를 `Idempotent`로 낮춰 분류하면 실행 시작 뒤 장애 시 중복 실행 위험을 숨기게 된다.

Durable output은 **이미 관찰된 결과의 보존 의미**이고 idempotency는 **같은 effect의 반복 실행 의미**다. 둘은 독립적으로 표현해야 한다.

## 결정

### 1. NonIdempotent는 명시적 opt-in일 때만 durable output을 사용한다

Capability Definition의 `execution.durableToolOutput=true`는 다음 effect class에 허용한다.

| effect class | `durableToolOutput` | Receipt/output profile |
|---|---:|---|
| `ReadOnly` | 생략 또는 어떤 값이든 현재 read route에서 필수 output | `xgeny.core-receipt/v2` + `xgeny.tool-output/v1` |
| `Idempotent` | `true` | v2 + tool output v1 |
| `Idempotent` | 생략/`false` | legacy v1, output sidecar 없음 |
| `NonIdempotent` | `true` | v2 + tool output v1 |
| `NonIdempotent` | 생략/`false` | legacy v1, output sidecar 없음 |
| `Compensatable`, `Unknown` | `true` | 지원하지 않음, Definition 등록 거부 |

`NonIdempotent + durableToolOutput=true`는 effect class, one-shot authorization, idempotency key와 `SinkGuarantee::None`을 바꾸지 않는다. Key는 durable identity와 future provider query를 위한 값이며 deduplication 또는 retry 허가의 증명이 아니다.

Schema fixture는 NonIdempotent opt-in을 valid contract로 고정하고, 아직 runtime profile이 없는 Compensatable opt-in은 invalid contract로 유지한다.

### 2. Receipt v2는 output evidence profile이지 replay profile이 아니다

Admission은 opt-in NonIdempotent intent에 다음 provenance를 authorization digest 안에 고정한다.

```text
profileVersion = xgeny.core-receipt/v2
toolOutputProfile = xgeny.tool-output/v1
```

WorkGraph, Direct Executor, Core Verification과 local store는 같은 effect/profile 조합을 독립적으로 재검사한다. 한 계층만 새로운 조합을 허용하고 다른 계층이 legacy 의미로 해석하는 downgrade를 허용하지 않는다.

성공 경로는 기존 bounded output 계약을 그대로 사용한다.

```text
Started durable commit
  -> adapter execute exactly once
  -> output schema + size/depth/node bound 검증
  -> EffectSucceeded + ToolOutputRecord atomic commit
  -> read-only verifier
  -> Receipt v2 + terminal event atomic commit
  -> generation-checked PlanningContext v2
```

`ToolOutputRecord`는 Run, Step, effect, action, exact Definition/Instance, plan, execution attempt, evidence digest와 typed output digest를 결합한다. Raw output은 journal, projection, Receipt와 `Debug`에 복제하지 않는다. Receipt에는 bounded artifact metadata와 output digest만 남고 exact typed body는 event-anchored SQLite sidecar에 남는다.

### 3. 실행 시작 뒤에는 output을 얻기 위해 effect를 다시 호출하지 않는다

Recovery 의미는 Receipt profile과 무관하게 effect class를 따른다.

| 마지막 durable 상태 | 재개 동작 | adapter `execute` |
|---|---|---:|
| `IntentCommitted` | exact material/Definition/Instance/head를 재검사하고 최초 실행 가능 | 최대 1회 |
| `Executing` | outcome 미확정으로 `EffectUnknown` 기록 | 0회 |
| `EffectUnknown` + query 없음 | `ManualRequired` | 0회 |
| `EffectUnknown` + query 있음 | read-only reconciliation만 수행 | 0회 |
| output bundle commit 완료, acknowledgement 유실 | cold reopen에서 `Validating` 복원 | 0회 |
| Receipt commit 완료, acknowledgement 유실 | cold reopen에서 terminal 복원 | 0회 |

Output profile을 가진 effect에서 reconciliation이 applied 사실만 증명하고 exact output을 복원하지 못하면 `Validating`으로 승격하지 않는다. 성공한 effect를 output 수집 목적으로 다시 실행하지도 않는다. 결과는 계속 Unknown/manual로 닫힌다.

### 4. 기존 DB와 Receipt v1 history는 그대로 읽는다

새 journal event, sidecar column 또는 table이 필요하지 않으므로 local store는 schema 8을 유지한다. 기존 NonIdempotent Receipt v1 intent와 output 없는 성공 history는 canonical bytes를 바꾸지 않고 계속 replay한다.

새 writer만 Definition opt-in에 따라 v2 provenance를 발행한다. Store는 새 intent append 시 profile/effect 조합을 엄격히 검사하지만, 과거 v1 NonIdempotent intent를 v2로 추측하거나 backfill하지 않는다.

## 검증

다음 회귀를 자동화한다.

- public schema valid NonIdempotent opt-in과 invalid Compensatable opt-in
- registry가 ReadOnly/Idempotent/NonIdempotent만 durable output으로 수락
- Admission이 NonIdempotent effect, non-empty key, `SinkGuarantee::None`을 유지하며 v2 provenance 발행
- WorkGraph가 NonIdempotent `ToolOutputRecord`와 output-bound success를 수락
- Direct Executor가 exact output을 한 번 commit하고 output commit acknowledgement 유실 뒤 재실행하지 않음
- verifier가 exact sidecar digest를 Receipt v2에 결합
- Receipt acknowledgement 유실과 SQLite cold reopen 뒤 effect/verifier 중복 호출 0회
- completed NonIdempotent output이 `RunPlanningSnapshot`과 다음 planning context에 복원
- missing/tampered output이 warm/cold audit에서 fail-closed
- 기존 NonIdempotent v1 lost-ack no-blind-retry 회귀 유지

## 포함하지 않는 범위

이번 결정은 OS process를 생성하지 않는다. 다음 수직 슬라이스가 별도 `xgeny.process/execute@1.0.0` Capability, host-owned executable resolution, shell 없는 argv 실행, cwd/env/timeout/output bound, process-tree 종료와 별도 사용자 승인 경계를 정의한다.

또한 durable output은 child process의 ambient OS 권한을 제한하는 sandbox가 아니다. Project의 test/build script는 임의 code 실행으로 취급해야 하며 filesystem allow-list만으로 격리됐다고 주장하지 않는다.

## 결과

XGENy는 NonIdempotent라는 정직한 effect 분류를 유지하면서도 실행 결과를 장기 WorkGraph에 전달할 수 있다. 작은 모델이 여러 turn에 걸쳐 test 결과를 읽고 다음 행동을 계획하는 기반이 생기지만, 결과 연속성이 중복 실행 안전성으로 오해되지 않도록 no-replay 경계는 그대로 유지된다.
