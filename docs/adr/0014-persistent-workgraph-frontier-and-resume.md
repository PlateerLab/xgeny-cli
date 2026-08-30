# ADR-0014: Persistent WorkGraph dependency와 Receipt-gated frontier

- 상태: 제안 — Tracked/Persistent WorkGraph 연구 gate 기본형 구현
- 기준일: 2026-08-30
- 적용 범위: internal RunEvent/RunState, public WorkGraph 의미 검증, built-in RunStore와 runtime coordination
- 공개 protocol v0.1 schema 변경: 없음 (기존 `dependsOn` 의미 검증 강화)
- local store schema: 4 → 5

## 문맥

ADR-0012는 성공 Step이 Core 검증과 `ExecutionReceipt`를 함께 commit해야 종결되도록 만들었고, ADR-0013은 같은 verified generation의 steady-state runtime이 전체 history를 재물질화하지 않고 current projection을 읽게 했다. 그러나 여러 Step의 의존성을 durable state에 보존하고, 재시작 뒤 무엇을 복구·검증·실행해야 하는지 한 권위에서 결정하는 frontier는 없었다.

`Completed` 문자열만으로 다음 Step을 해제하면 legacy `VerificationPassed`처럼 complete Receipt가 없는 상태도 성공으로 오인할 수 있다. 반대로 실행 가능 여부를 별도 table이나 event로 저장하면 journal projection과 두 번째 mutable authority가 생긴다. 모델의 계획 순서나 요약을 복구 기준으로 삼는 것도 crash 이후 같은 결정을 보장하지 않는다.

## 결정

### 1. Dependency는 Step 계획 사실의 immutable 일부다

Internal `StepPlanned` event와 `StepState`에 `depends_on`을 저장한다. 빈 목록은 serialization에서 생략하고 deserialize 때 기본값을 사용하므로 기존 event/projection byte shape를 새로 쓸 필요가 없다.

Append-only planner는 dependency가 다음 조건을 만족할 때만 Step을 추가한다.

- 자기 자신이 아니다.
- 같은 dependency를 두 번 선언하지 않는다.
- 현재 Run에 이미 계획된 Step이다.

따라서 정상 reducer 경로에서 forward reference와 cycle이 생기지 않는다. 이미 계획된 Step의 dependency를 수정하거나 graph를 재배선하는 event는 제공하지 않는다. 외부에서 들어온 public `WorkGraph` snapshot은 step 순서와 무관하게 전체 ID 집합을 먼저 확인한 뒤 unknown/self/duplicate edge와 cycle을 반복형 알고리즘으로 거부한다.

### 2. Journal과 current projection만 durable authority다

Dependency는 hash-chained `RunEvent`에 기록되고 `RunState` projection으로 replay된다. `Ready`, `Waiting`, `Blocked` 또는 frontier 자체를 event, SQLite row나 별도 cache로 저장하지 않는다. 이 값들은 generation-verified current projection에서 매번 파생한다.

`WorkGraphCoordinator::inspect()`는 전체 history API가 아니라 `RunStore::load_current()`만 호출하고 effect를 실행하지 않는다. Built-in store의 warm verified generation은 journal·Receipt vector를 재물질화하지 않는다. Cold open, 외부 generation 변경과 third-party default 구현은 integrity를 세우기 위해 full audit을 수행할 수 있다. Memory와 SQLite store는 같은 projection에서 동일한 frontier를 만들며 SQLite close/reopen 뒤에도 dependency, journal head와 결과가 같아야 한다.

### 3. Core Receipt가 있는 성공만 dependency를 해제한다

다음 세 조건을 모두 만족할 때만 downstream dependency가 해제된다.

```text
StepStatus::Completed
+ non-empty execution_receipt_id
+ well-formed lowercase SHA-256 execution_receipt_digest
```

`Failed`와 `ManualRequired`는 해당 가지를 막는다. `Completed`지만 Receipt ID/digest가 없거나 shape가 잘못된 legacy/hostile projection도 읽을 수는 있으나 `ReceiptMissing`으로 fail-closed 처리한다. 모델의 완료 주장, adapter evidence digest, legacy `VerificationPassed` 또는 status 하나만으로는 dependency를 해제하지 않는다.

Pure `derive_frontier()`는 Receipt document를 다시 읽지 않고 projection identity shape만 검사한다. Execution-authoritative 보장은 journal replay와 complete Receipt sidecar/chain까지 검증하는 built-in Memory/SQLite `load_current()`에서 나온 state에 한정된다. Third-party `RunStore`가 이 경로에 참여하려면 같은 검증 계약을 구현해야 하며 임의로 deserialize한 `RunState`를 실행 권위로 사용하지 않는다.

Admission의 사전 검사와 reducer의 `EffectIntentCommitted` 전이 모두 같은 gate를 적용한다. 따라서 저수준 caller가 admission을 우회해 unreleased Step의 authorization budget을 소비하거나 intent를 기록할 수 없다. Receipt finalization의 event, complete Receipt sidecar와 projection transaction이 commit된 순간에만 다음 `inspect()`가 child를 actionable로 본다. Transaction의 어느 단계에서든 rollback되면 child는 계속 닫혀 있다.

### 4. Frontier는 결정론적인 단일-orchestrator continuation view다

`derive_frontier()`는 graph를 반복형 Kahn traversal로 검증·분류한다. 결과는 다음을 분리한다.

- `actionable`: 기존 Core operation으로 진행할 수 있는 Step
- `waiting`: 아직 끝나지 않은 dependency를 기다리는 Planned Step
- `blocked`: 실패, manual, Receipt 누락 또는 이미 막힌 dependency를 가진 Planned Step
- Receipt-bound completion, unverified legacy completion, failed와 manual terminal 목록

Action 우선순위는 다음과 같다. 같은 상태에서는 byte-exact Step ID 순서를 사용한다.

1. `Executing` 복구 (`DriveEffect`)
2. `EffectUnknown` 조정 (`DriveEffect`)
3. `Reconciling` 계속 (`DriveEffect`)
4. `Validating` read-only 검증 (`Verify`)
5. `IntentCommitted` 실행 (`DriveEffect`)
6. dependency가 풀린 `Planned` Step admission (`Admit`)

Caller는 `next_action()` 하나만 선택하고 durable commit 뒤 frontier를 다시 계산한다. 이 기본형은 병렬 scheduler, worker ownership, retry timer 또는 model loop를 만들지 않는다. 이미 active/terminal인 Step의 dependency가 해제되지 않았다면 정상 waiting 상태가 아니라 reducer 우회 또는 손상으로 보고 전체 frontier derivation을 거부한다.

### 5. Graph 전체의 Receipt completion은 Run 완료가 아니다

`all_steps_receipt_completed()`는 **현재 계획된 Step이 하나 이상이고 전부 Receipt-bound Completed인지**만 답한다. 사용자의 목표가 충족됐는지, 새 Step을 더 계획해야 하는지 또는 Run을 종결해도 되는지는 별도의 goal/Run lifecycle 계약이다. 이 helper를 Run 완료 event나 모델의 종료 판단으로 사용하지 않는다.

### 6. Schema 5는 의미 downgrade를 막는 fence다

Dependency는 기존 event/projection JSON 안에 있으므로 SQLite table이나 column은 추가하지 않는다. 그래도 schema 4 binary는 새 `depends_on` 의미를 알지 못해 dependent Step을 독립 실행할 수 있으므로 current store version을 5로 올린다.

- version 0: schema 5 신규 생성
- version 3: 기존 schema 3 → Receipt table 추가 및 전체 감사 → schema 5
- version 4: transaction 안에서 event/projection/material/Receipt 전체 감사 → byte rewrite 없이 `user_version = 5`
- version 5: 전체 검증 후 open
- version 1, 2와 6 이상: mutation 없이 fail-closed

Schema 4 → 5 migration은 journal, projection, effect intent, authorization, material과 Receipt row를 바꾸지 않는다. Version을 올리기 전에 JSON event ID와 SQLite index column까지 포함한 전체 audit을 통과해야 하며 오류 시 version 4와 모든 row가 그대로 남는다. Open이 처음 version 3을 본 뒤 writer lock을 기다리는 사이 schema 4 migration이 끝난 mixed-version 경합도 transaction 안에서 4를 재감사해 5로 수렴한다. Receipt finalization event가 있지만 complete Receipt storage가 없는 hostile schema 3도 version을 올리기 전에 transaction 전체가 rollback된다. 반대로 schema 4까지만 이해하는 과거 binary는 version 5를 거부하므로 dependency 의미를 조용히 무시할 수 없다.

## 복잡도와 성능 경계

Frontier derivation은 recursion을 사용하지 않고 validation phase와 classification phase에서 각각 Step과 edge를 한 번 방문한다. Ordered map/set과 deterministic sorting을 포함한 CPU upper bound는 `O((V + E) log V)`, 파생 결과와 graph index memory는 `O(V + E)`다. 10,000-Step chain으로 stack overflow 없이 마지막 Step만 해제되는지와 phase별 exact visit count를 검사한다.

이 결정은 append 전체를 `O(1)`로 만들지 않는다. `load_current()`의 owned `RunState` clone, reducer의 projection clone과 SQLite의 전체 projection JSON rewrite는 여전히 graph 크기에 비례한다. Graph 규모별 wall-clock/memory characterization과 incremental projection은 별도 연구 gate다.

## 호환성과 경계

- Public WorkGraph JSON Schema는 바꾸지 않고 기존 `dependsOn`의 DAG·상태 선행조건을 semantic validator에서 검사한다.
- Public WorkGraph v0.1에는 Core Receipt binding field가 없으므로 현재 문서는 observation/compatibility surface이지 실행-authoritative frontier가 아니다. Cross-document Receipt binding은 후속 protocol 계약이다.
- Internal event의 빈 dependency 목록은 생략되어 기존 단일-Step event/projection의 serialized shape를 유지한다.
- Internal RunState를 public WorkGraph로 투영하는 adapter는 아직 없다. 현재 internal Step에는 public capability, execution mode와 timestamp를 완전하게 재구성할 정보가 없으므로 값을 추측하지 않는다.
- XGEN, Connector, PostgreSQL, MinIO, MCP 또는 외부 harness에 실행 의존성을 추가하지 않는다. 향후 연동은 versioned WorkGraph/Receipt 계약을 통해 이뤄지며 같은 Parent graph를 공동 수정하지 않는다.

## 의도적으로 제외한 범위

- 모델 호출·planning loop·context packing과 사용자용 `resume` CLI
- 자동 병렬 scheduler, distributed worker lease와 preemption
- dependency 추가/삭제, dynamic rewiring, OR·conditional edge와 subgraph
- retry/backoff timer, waiting-input와 approval UX
- failure/unknown/cancelled/not-started Receipt의 일반화
- typed output, Artifact와 output-to-input binding
- internal RunState ↔ public WorkGraph delta projection
- XGEN/Connector/MCP adapter와 원격 authority handoff

`Planned` Step은 objective와 dependency만 보존한다. Capability와 arguments를 durable plan에서 재구성하는 계약이 아직 없으므로 이 slice만으로 autonomous agent loop를 실행할 수 있다고 주장하지 않는다.

## 회귀 gate

- diamond/독립 branch에서 Receipt-bound active frontier만 해제
- recovery·reconciliation·verification이 새 admission보다 먼저 선택됨
- failure/manual/legacy Receipt 누락의 transitive block
- self/duplicate/unknown dependency와 direct cyclic state 거부
- unreleased child의 admission resolver 호출 0회와 reducer authorization mutation 0회
- 10,000-Step iterative traversal과 exact structural visit count
- Memory/SQLite frontier parity와 SQLite reopen continuity
- Receipt finalization transaction fault와 실제 process exit 뒤 child 비해제, retry commit 뒤 원자 해제
- schema 3의 missing Receipt rollback, schema 4 → 5 full-audit 성공/실패 원자성, mixed-version 수렴과 모든 durable row/index 보존
- workspace format, clippy, test, protocol check와 release build

## 결과

XGENy는 이제 모델 context와 분리된 durable dependency DAG를 재시작 뒤 같은 current projection에서 복원하고, 미확정 effect와 검증을 먼저 처리하며, Core Receipt로 증명된 선행 Step만 다음 작업을 해제할 수 있다. 다음 수직 slice는 이 frontier의 `Admit` action이 사용할 durable 계획 입력과 bounded model loop를 정의하는 일이며, 실제 모델 연결은 그 경계와 함께 별도 평가 gate로 검증한다.
