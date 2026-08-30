# ADR-0016: Durable model-call lifecycle와 possible-send budget

- 상태: 제안 — provider-free lifecycle 연구 gate 기본형 구현
- 기준일: 2026-08-30
- 적용 범위: internal model-call contract, `RunEvent`/`RunState`, bounded `AgentLoop`, built-in `RunStore`
- 공개 protocol v0.1 schema 변경: 없음
- local store schema: 6 유지

## 문맥

ADR-0015는 bounded `PlanningContext`, accepted model decision 예산과 `PlanAccepted` 원자 commit을 도입했다. 그러나 `accepted_model_turns`는 Plan 또는 Completion decision이 수락된 뒤에만 증가했다. Provider 요청을 보내기 전에 durable 사실이 없으므로 다음 장애 창에서는 같은 호출을 다시 보내도 되는지 판단할 수 없었다.

- 요청을 보냈지만 응답 전에 process가 종료됨
- timeout 또는 transport 오류가 실제 전송 전인지 후인지 알 수 없음
- 응답을 받았지만 Plan commit 전에 process가 종료됨
- Plan commit은 성공했지만 caller가 acknowledgement를 받지 못함
- 호출 중 다른 durable event가 head를 전진시켜 응답 context가 stale해짐
- invalid response나 Core rejection이 반복되지만 accepted-turn 예산은 줄지 않음

이 상태에서 blind retry를 허용하면 provider 비용과 rate limit을 무제한 소비할 수 있고, 같은 응답을 다른 WorkGraph head에 적용할 수 있다. 반대로 model call이 불확정이라는 이유로 effect recovery와 Core verification까지 막으면 이미 시작된 외부 effect의 안전한 수습을 방해한다.

이번 결정은 실제 HTTP provider를 붙이지 않는다. Model call의 **전송 가능성**을 요청 전에 durable하게 예약하고, injected provider-neutral port의 한 호출을 하나의 reservation에 결합하며, 재시작과 stale response에서 fail-closed하는 lifecycle만 만든다.

## 결정

### 1. Accepted decision과 possible-send budget을 분리한다

기존 `AgentLoopBudget.max_model_turns`는 계속 **수락돼 commit된 Plan/Completion decision 수**를 제한한다. 별도 `ModelCallBudget`은 provider 호출을 시도할 수 있는 reservation 수를 제한한다.

Reservation commit 시 call slot을 영구 소비한다. 이 수는 실제 provider 청구 건수와 같다고 주장하지 않는다. Reservation 직후, network 전송 전에 crash할 수 있기 때문이다. 따라서 durable counter가 제공하는 보장은 다음과 같다.

```text
historical accepted floor + post-configuration outbound attempts
  <= durable reserved_calls
  <= configured call budget
```

첫 번째 부등식은 `PlannerPort` 구현이 reservation 하나당 outbound request를 최대 한 번만 보내고 hidden retry를 하지 않을 때만 성립한다. 여기서 outbound는 `ModelCallLifecycleConfigured` 이후 호출만 뜻한다. Provider 내부 retry, proxy 재전송과 provider 과금 정책까지 Core가 증명하는 것은 아니다. 그러므로 reservation counter를 Run 전체의 실제 outbound 또는 billed-call 수라고 표시하지 않는다.

Accepted-turn, reserved-call, settled-call과 unknown-call 수는 서로 다른 의미로 projection한다. Rejection 또는 Unknown은 accepted turn을 증가시키지 않지만 이미 소비한 call slot을 돌려주지 않는다. Token, 금액과 wall-clock은 이 budget에 포함하지 않는다.

### 2. Reservation identity는 Core가 현재 head에서 결정한다

한 reservation은 다음 host-owned 사실을 domain-separated RFC 8785/SHA-256 commitment로 결합한다.

- exact Run ID와 authority epoch
- planner identity
- monotonic call index
- 다음 accepted turn index
- planning context를 만든 base journal sequence와 head digest
- `context_digest`
- adapter의 `request_profile_digest`를 포함해 Core가 계산한 `request_digest`
- 위 사실에서 유도한 deterministic model call ID

Model이나 provider는 call ID, call index, turn, Run/authority 또는 base head를 선택하거나 echo해서 권위를 얻지 않는다. Runtime은 verified current state와 request profile로 값을 계산하고 reducer가 shape, 순번, digest와 현재 state를 다시 확인한다.

현재 planner identifier는 1~256 bytes의 `[A-Za-z0-9._-]`만 허용한다. Core가 만드는 call ID wire shape는 `model-call-` 뒤에 lower-case SHA-256 hex 64자를 붙인 형태다. 이 값은 provider가 발급하거나 응답에서 돌려준 correlation ID가 아니다.

이 검사는 길이와 허용 문자만 확인하는 **문법 검사**이며 credential/token/민감 본문을 탐지하는 DLP나 secret detector가 아니다. 허용 문자로만 구성된 secret도 통과할 수 있다. 따라서 trusted composition root가 관리되는 registry에서 stable, non-secret `planner_id`를 공급해야 하며 URL, path, API key, token, raw prompt/response/error 또는 사용자 데이터를 identifier field에 넣지 않는 것이 계약이다.

`request_profile_digest`는 실제 adapter가 사용할 model identity, prompt/template revision, provider options, structured-output schema/dialect처럼 request 의미를 바꾸는 구성을 commit한다. 이 digest는 내용을 숨기는 암호화가 아니며, 원문 prompt나 credential을 대신 저장하는 장소가 아니다. Core의 `request_digest`는 request profile commitment와 exact planning context commitment를 결합한다.

Digest validator도 `sha256:` lower-case wire shape만 검사할 뿐 digest input의 출처나 민감도를 감사하지 않는다. Trusted host는 versioned non-secret profile descriptor와 Core가 정의한 commitment input만 사용해야 한다. Raw prompt/response/error/credential을 digest field에 직접 넣거나, 이를 ad-hoc hash한 값을 비밀 대용으로 저장해서는 안 된다.

### 3. Provider 호출 전에 reservation을 commit한다

호출 순서는 다음과 같다.

```mermaid
sequenceDiagram
  participant Loop as AgentLoop
  participant Store as RunStore
  participant Port as PlannerPort
  participant Core as Core validation

  Loop->>Store: reserve(call identity, base head, request digest)
  Store-->>Loop: reservation commit head
  Loop->>Port: plan(request), at most once
  alt accepted Plan
    Port-->>Loop: transient PlanProposal
    Loop->>Core: validate + materialize
    Core->>Store: PlanAccepted(modelCallId) + all input sidecars
    Store-->>Loop: atomic success settlement
  else accepted Completion candidate
    Port-->>Loop: transient CompletionCandidate
    Core->>Store: CompletionCandidateRecorded(modelCallId)
    Store-->>Loop: atomic success settlement
  else known closed failure
    Port-->>Loop: fixed failure or invalid proposal
    Loop->>Store: closed rejection settlement
  else ambiguous delivery
    Port--xLoop: timeout / unavailable / lost acknowledgement
    Loop->>Store: Unknown
  end
```

Reservation append가 실패하면 `PlannerPort`를 호출하지 않는다. Reservation commit 뒤 정상 control flow는 `PlannerPort`를 한 번 호출하지만, commit 직후 crash까지 포함한 lifecycle 전체 보장은 reservation당 **최대 한 번**이다. Port 구현은 내부 network retry, fallback provider 호출과 같은 숨은 두 번째 outbound request를 수행하면 안 된다.

### 4. 성공 settlement는 Plan/Completion commit 그 자체다

성공 응답에 별도 `ModelCallSettled(success)` event를 먼저 기록하지 않는다.

- Plan 응답은 `ExpectedPlanningTurn.model_call_id`와 exact active reservation을 결합한 `PlanAccepted`가 성공 settlement다.
- Completion 응답은 같은 binding을 가진 `CompletionCandidateRecorded`가 성공 settlement다.
- `PlanAccepted` event, 모든 `planned_invocations` sidecar와 projection은 기존 `append_with_plan_inputs` transaction으로 함께 commit한다.

성공 settlement와 Plan을 두 event로 나누면 첫 event 뒤 crash에서 “성공으로 닫힌 call이지만 Plan은 없는” 복구 불가능한 gap이 생긴다. 반대로 Plan을 nested wrapper event 안에 넣으면 기존 reducer와 sidecar 원자성 계약을 중복하게 된다.

Lifecycle이 configured된 뒤에는 `model_call_id`가 없는 legacy `PlanAccepted`/`CompletionCandidateRecorded`를 허용하지 않는다. Schema 6의 기존 journal을 replay하기 위한 optional wire shape는 유지하지만, 새 configured Run의 append 우회로 사용할 수 없다.

### 5. Closed rejection과 Unknown을 구분한다

Provider raw 오류 대신 bounded closed taxonomy만 durable하게 남긴다.

| 결과 | durable 분류 | 새 reservation 가능 여부 |
|---|---|---:|
| `ProviderLimit`로 분류된 terminal provider limit | closed rejection | budget이 남으면 가능 |
| response decode/shape가 invalid | closed rejection | budget이 남으면 가능 |
| Core proposal/context/graph 검증 실패 | closed rejection | budget이 남으면 가능 |
| recipe materialization 실패 | closed rejection | budget이 남으면 가능 |
| timeout 또는 delivery 여부를 모르는 unavailable | Unknown | 자동으로 불가 |
| process crash 또는 response/commit acknowledgement 유실 | reopen replay에서 unresolved/Unknown 또는 already settled | reopen 확인 전 불가 |
| exact Plan/Completion commit | accepted settlement | 다음 frontier 판단 후 가능 |

`Unavailable`이라는 이름만으로 “전송되지 않았다”고 추정하지 않는다. Port가 delivery 전에 실패했음을 별도 계약으로 증명하지 못하면 보수적으로 Unknown이다. Unknown이나 재시작 뒤 남은 unresolved reservation에는 blind retry를 하지 않는다.

새 호출을 허용하려면 explicit recovery가 기존 call을 discard해야 한다. Discard는 “호출되지 않았다”는 증명이 아니라 해당 응답을 앞으로 수락하지 않겠다는 보수적 운영 결정이다. Consumed slot은 복구되지 않는다. Provider request-status 조회, idempotency reconciliation 또는 과금 조회는 실제 adapter 후속 계약이다.

### 6. Stale response는 어떤 새 head에도 rebase하지 않는다

Reservation은 planning context를 만든 base head와 reservation lifecycle position을 고정한다. Response settlement는 exact active call, context, request와 expected current lifecycle head를 모두 만족해야 한다.

호출 중 다른 event가 journal head를 전진시키면 원래 response를 새 `ExpectedHead`로 다시 제출하지 않는다. Store compare-and-append와 reducer binding 중 어느 한쪽만 통과해도 되는 것이 아니라 둘 다 통과해야 한다. Wrong/missing call ID, duplicate settlement와 과거 call response도 state mutation 없이 거부한다.

Model call이 active/Unknown이라고 WorkGraph 전체를 freeze하지는 않는다. 이미 시작된 effect의 recovery, reconciliation, Core verification과 manual safety transition은 계속 진행할 수 있다. 그런 안전 event가 base head를 바꾸면 model response는 새 head에 적용하지 않고 bounded `StaleHead` rejection으로 call을 정산한다. 그 뒤에만 새 context/reservation을 만들 수 있다. 안전한 외부 effect 수습이 모델 응답 수락보다 우선한다.

### 7. Journal과 projection이 유일한 durable authority다

Model-call lifecycle은 bounded secret-free event와 `RunState` projection으로 표현한다. SQLite table, sidecar 또는 별도 mutable status row를 추가하지 않는다.

- journal은 reservation, settlement, Unknown과 recovery의 전체 causal history를 보존한다.
- projection은 budget/counter와 현재 unresolved call처럼 coordination에 필요한 bounded view만 보존한다.
- deterministic identity와 reducer 순번 검사가 duplicate/stale call을 막으므로 모든 과거 call을 projection map에 복제하지 않는다.
- `VerifiedRunIndex` cold replay와 warm candidate append 계약을 그대로 사용한다.

Memory store는 정상 `Result` 경로에서 event와 projection의 논리적 none-or-all을 제공하지만 process crash durability는 제공하지 않는다. Embedded SQLite는 기존 `BEGIN IMMEDIATE`, `WAL`, `synchronous=FULL` 설정 아래 event와 projection을 한 transaction으로 commit한다. Accepted Plan은 기존 sidecar transaction을 함께 사용한다.

### 8. SQLite schema 6을 유지한다

물리 table/column, foreign key와 sidecar 의미가 바뀌지 않으므로 `user_version`은 6을 유지한다. 새 lifecycle event와 projection은 기존 `run_events.event_json`과 `run_projection.state_json`에 저장된다.

Schema 6 binary가 새 event variant 또는 새 projection 의미를 알지 못하면 decode/replay에서 fail-closed한다. 새 event를 legacy `PlanAccepted`로 해석하거나 조용히 생략할 수 있는 기본값을 제공하지 않는다. 반대로 새 binary는 과거 schema 6 event의 digest를 보존해야 한다. 기존 hash-chained 타입에 추가되는 optional field는 과거 값이 없을 때 serialization에서도 생략돼야 하며, replay가 과거 event canonical bytes를 바꾸면 안 된다.

이번 변경에는 SQLite migration이나 durable row rewrite가 없다. Open/load는 계속 event hash chain, replay-equivalent projection, derived indexes, plan/material sidecar와 Receipt chain을 전체 감사한다.

기존 schema 6 Run은 `ModelCallLifecycleConfigured` 전까지 과거 `ExpectedPlanningTurn`을 그대로 replay할 수 있다. Lifecycle을 처음 configure할 때 이미 존재하는 `accepted_model_turns`를 historical floor로 사용해 reserved/settled counter를 같은 값에서 시작한다. 과거 accepted decision을 call budget 밖의 무료 호출로 다시 세지 않기 위한 보수적 선택이다. 새 `ModelCallBudget`은 이 floor보다 작을 수 없으며 configuration 이후에는 model-call ID 없는 Plan/Completion을 거부한다.

이 floor는 lifecycle 이전 journal에서 관찰할 수 있는 **accepted decision의 하한**일 뿐이다. Reservation event가 없던 시기의 invalid response, timeout, provider failure, crash 또는 기타 미수락 호출은 복원하거나 계상할 수 없다. 따라서 `possible-send reservations <= configured call budget` 상한은 `ModelCallLifecycleConfigured` 이후 reservation에 대해서만 보장하며, 기존 Run 전체의 과거 outbound 수나 billed-call 수에 대한 exact count 또는 upper bound로 해석하면 안 된다.

기본 `AgentLoop` 구성은 model-call 상한을 accepted-turn 상한과 같게 두어 기존 caller가 보수적으로 동작하게 한다. 둘을 다르게 운영하려면 별도 model-call budget을 명시한다. 이 기본값도 실제 provider 청구 횟수를 뜻하지 않는다.

### 9. Raw provider content는 durable state와 일반 log에 넣지 않는다

다음 값은 journal, projection, model-call status, Receipt, SQLite index와 일반 runtime log에 저장하지 않는다.

- raw/system/user prompt와 provider request body
- raw response, partial stream과 provider envelope
- provider raw error body, stack trace와 credential-bearing header
- chain-of-thought, hidden reasoning과 전체 transcript
- API key, bearer token, cookie, presigned URL과 raw tool argument

Durable 값은 opaque ID, bounded closed enum, count, supported identifier와 well-formed digest로 제한한다. Digest는 기밀화가 아니다. Low-entropy secret 또는 민감 본문을 hash해서 저장해도 안전해지는 것이 아니므로 digest input allowlist 자체를 통제한다. Adapter는 request/response logging과 tracing에서도 같은 경계를 지켜야 한다.

### 10. XGEN과 provider에 의존하지 않는다

Core lifecycle에는 XGEN workflow/node/interaction ID, Connector 타입, PostgreSQL/MinIO key, OpenAI/Anthropic request type 또는 특정 model 이름을 넣지 않는다. Planner identity와 request profile은 provider-neutral bounded commitment다.

향후 local model, OpenAI-compatible endpoint 또는 XGEN Model Gateway adapter는 같은 reservation/request 계약을 구현한다. XGEN/Connector가 없어도 Memory/SQLite fake-port tests가 전부 통과해야 한다. XGEN을 연결할 때도 XGENy Core가 XGEN package나 저장소에 의존하지 않으며, adapter가 provider dialect와 request-status reconciliation을 소유한다.

## Failure matrix

| 장애 시점 | durable 관찰 | 자동 재호출 | 올바른 다음 행동 |
|---|---|---:|---|
| reservation commit 전 실패 | reservation 없음 | 가능 | 같은 verified head에서 새 reservation 시도 |
| reservation transaction rollback 또는 commit 전 crash | reservation 없음 | 가능 | reopen full audit 후 재시도 |
| reservation commit 후 send 전 crash | Reserved slot 존재 | 금지 | explicit recovery/discard |
| send 후 response 전 crash/timeout | Reserved 또는 Unknown | 금지 | provider reconciliation 또는 discard |
| response 수신 후 validation 전 crash | unresolved reservation | 금지 | raw response를 저장하지 않으므로 recovery 결정 |
| invalid/provider/Core rejection settlement | closed rejection | 가능 | 남은 call budget으로 새 context/call |
| materializer 실패 | closed rejection, 외부 orphan 가능 | 가능 | orphan GC는 별도; 새 call은 새 reservation |
| 호출 중 safety event가 head 전진 | response stale | 금지 | stale-head rejection 정산 후 새 context 생성 |
| Plan sidecar transaction rollback | reservation은 남고 Plan/sidecar 없음 | 금지 | 동일 응답 자동 rebase 금지, recovery 필요 |
| Plan transaction commit 후 acknowledgement 유실 | accepted Plan/sidecar가 모두 존재 | 금지 | reopen state를 신뢰하고 다음 frontier 수행 |
| duplicate/late response | 이미 settled됐거나 다른 active call | 금지 | mutation 없이 폐기 |

## 기각한 대안

### Reservation 없이 accepted turn만 센다

Timeout, crash와 invalid response가 budget을 소비하지 않아 비용 상한과 중복 호출을 통제할 수 없다.

### 성공 settlement와 Plan을 두 event로 기록한다

두 event 사이 crash gap이 생긴다. 성공은 `PlanAccepted`/`CompletionCandidateRecorded` 자체가 정산해야 한다.

### Plan 전체를 model-call wrapper event 안에 중첩한다

기존 WorkGraph reducer와 `append_with_plan_inputs` 원자성 계약을 이중화하고 replay path를 불필요하게 분기한다.

### Model-call 전용 SQLite table을 둔다

Journal/projection과 별도 mutable authority가 생기며 transaction, replay와 corruption audit가 중복된다. 현재 metadata는 bounded하고 point-query table이 필요하지 않다.

### Timeout이나 restart 뒤 같은 request를 blind retry한다

이전 request의 전송·과금·응답 상태를 모르므로 중복 호출과 stale response를 만들 수 있다. Provider reconciliation 계약 전에는 Unknown으로 닫는다.

### Active model call 동안 WorkGraph를 전역 freeze한다

이미 시작된 non-idempotent effect의 recovery와 verification을 지연시킨다. Safety lifecycle은 계속 허용하되 model response를 stale로 만든다.

### Raw prompt/response transcript를 저장해 복구한다

Credential, 사용자 데이터와 hidden reasoning을 local journal에 영구 보존하고 provider 독립성을 깨뜨린다. 이번 계약은 최소 commitment와 closed status만 저장한다.

## 회귀 gate

- deterministic call/request ID와 Run/epoch/planner/call-index/turn/base-head/context/profile binding
- reservation commit 전 planner 호출 0회와 reservation당 port/outbound 최대 1회
- call budget exhaustion, second active reservation, duplicate/missing/wrong call ID 거부
- lifecycle configured 이후 legacy call-ID 없는 Plan/Completion 우회 거부와 schema 6 legacy replay
- upgrade floor는 historical accepted turn만 reserved/settled에 반영하고 그보다 작은 call budget은 거부하며, lifecycle 이전 실패 호출을 합성하지 않음
- accepted Plan/Completion의 exact active-call settlement와 accepted/reserved/settled/unknown counter 분리
- invalid/provider/Core/materializer closed rejection과 timeout/unavailable Unknown 분류
- restart Reserved/Unknown에서 planner 호출 0회, explicit discard 뒤에만 새 reservation
- stale head/late response mutation 0회와 effect recovery/verification 비차단
- raw prompt/response/error/credential sentinel의 event/projection/export/Debug 비노출
- model-call 전용 Memory/SQLite Unknown reopen parity
- model-call 전용 reservation/Unknown Event·Projection transaction fault none-or-all
- model-call 전용 active reservation 상태의 PlanAccepted Event·sidecar·Projection fault에서 partial Plan 0개
- runtime SQLite reopen simulation에서 committed Plan 또는 interrupted reservation 뒤 planner 재호출 0회
- runtime `CommitAckLossStore` simulation에서 reservation commit acknowledgement 유실 시 provider 호출 0회, accepted Plan commit acknowledgement 유실 시 reopen 뒤 provider 재호출 0회
- 기존 shared store의 process-exit/two-handle stale-writer/cache-invalidation 회귀 유지; 이는 model-call 전용 process-kill/two-handle injector가 아님
- 기존 shared store의 warm append historical scan 0과 cold audit event당 한 번 회귀 유지
- schema version 6 유지와 기존 durable row/blob 무변경
- XGEN 없는 local-only build/test와 public protocol fixture 무변경
- workspace format, clippy, 전체 test, protocol check와 release build

## 명시적 비목표

- OpenAI, Anthropic, Ollama, vLLM 또는 XGEN Model Gateway HTTP adapter
- provider별 tokenizer, prompt/template, structured-output dialect와 stream parser
- tool dispatcher와 자동 `Admit`/`DriveEffect`/`Verify` loop
- blind retry, backoff, fallback model과 provider-side hidden retry
- provider request-status/idempotency/과금 reconciliation
- token, 금액, rate limit과 wall-clock deadline budget
- raw prompt/response/error, transcript와 chain-of-thought 저장
- parallel model call, distributed lease와 multi-orchestrator scheduling
- terminal goal verification과 completion confirmation UI

## 결과

XGENy는 실제 provider가 없어도 model request를 보내기 전에 bounded possible-send slot을 durable하게 소비하고, 성공·closed rejection·Unknown을 재시작 뒤 같은 journal에서 복원할 수 있다. Accepted Plan과 Completion은 기존 원자 commit 경계를 유지하고 stale response는 새 head에 적용되지 않는다. Unknown call은 자동 재호출하지 않지만 effect recovery와 Core verification은 계속 진행할 수 있다.

이 보장은 실제 청구 건수나 exactly-once network delivery가 아니다. 실제 provider adapter는 reservation당 outbound 최대 한 번, raw logging 금지와 provider-specific reconciliation을 별도 integration gate로 증명해야 한다.
