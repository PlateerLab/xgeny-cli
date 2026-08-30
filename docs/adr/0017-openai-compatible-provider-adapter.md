# ADR-0017: OpenAI-compatible 단일 요청 Provider 경계

- 상태: 제안 — 첫 실제 모델 Provider engineering slice 구현
- 기준일: 2026-08-30
- 적용 범위: PlanningContext production bound, internal planner failure taxonomy, `xgeny-provider-openai`
- 공개 protocol v0.1 schema 변경: 없음
- local store schema: 6 유지

## 문맥

ADR-0016은 provider 호출 전에 possible-send slot을 journal에 예약하고, reservation 하나에 `PlannerPort::plan` 한 번만 결합했다. 실제 HTTP adapter가 retry, redirect, fallback 또는 사전 health check를 숨기면 Core의 durable call budget보다 많은 outbound 요청이 발생한다. 또한 기존 context assembler는 최종 payload byte만 제한하고 전체 Capability schema를 먼저 clone·digest했으므로 실제 network egress 앞의 CPU와 peak-memory 상한이 불완전했다.

첫 engineering target은 `go50902`에서 vLLM 0.27.1로 제공되는 `qwen3.8-27b`다. 이 target은 OpenAI-compatible Chat Completions와 JSON Schema structured output을 제공한다. XGENy Core는 이 서버나 XGEN Model Gateway에 의존하지 않아야 하며, 이후 XGEN gateway도 같은 `PlannerPort`를 구현하는 별도 adapter가 되어야 한다.

## 결정

### 1. Provider는 Core 바깥의 leaf crate다

`xgeny-provider-openai`가 `xgeny-runtime`의 `PlannerPort`를 구현한다. Runtime, WorkGraph, local store는 HTTP, OpenAI, vLLM, Qwen 타입을 import하지 않는다.

```text
xgeny-runtime
  PlannerPort / reserved PlannerCallRequest / PlanProposal
                         ▲
                         │ implements
xgeny-provider-openai
  immutable profile -> one HTTP POST -> strict proposal codec
```

향후 local inference server와 XGEN Model Gateway는 같은 방향으로 별도 leaf adapter를 추가한다. endpoint를 바꾸기 위해 Core를 수정하지 않는다.

### 2. Request profile은 불변 의미만 commit한다

생성 시 다음 non-secret 의미를 RFC 8785/SHA-256으로 한 번 commit한다.

- provider dialect와 served model
- tokenizer identity
- planning-context profile
- system prompt/template revision과 prompt byte digest
- proposal schema revision과 schema digest
- temperature, seed, max output tokens
- timeout, request/response/proposal byte와 JSON depth 상한
- stream, retry와 redirect 정책

SSH tunnel 주소, API URL, bearer credential, raw prompt/context/response는 digest input에 넣지 않는다. Stable `planner_id`가 관리되는 endpoint identity를 표현하고 Core request digest가 이를 profile/context digest와 결합한다. Config는 생성 뒤 setter를 제공하지 않아 digest와 실제 요청 사이 TOCTOU를 막는다.

### 3. Reservation 하나는 HTTP POST 0회 또는 1회다

Adapter는 `/v1/chat/completions`에 non-streaming POST 하나만 보낸다. `plan()` 안에서 `/v1/models`, health check, repair request 또는 fallback provider를 호출하지 않는다.

동기식 `ureq` client는 다음 경계를 고정한다.

- transport: adapter 코드의 명시적 `send` 한 번
- retry: adapter retry loop 없음
- redirect: `max_redirects(0)`
- system proxy: `proxy(None)`으로 자동 사용 금지
- compression feature: 사용하지 않음

Client `send`는 adapter invocation당 코드상 한 번만 존재한다. Request serialization이나 configured byte 상한에서 닫히면 outbound 0회이며 reservation slot은 이미 소비된 closed failure로 남는다.

### 4. Provider 출력은 별도 strict wire DTO다

Core `PlanProposal`에 `Deserialize`를 추가하지 않는다. Provider content는 adapter-private `xgeny.plan-proposal/v1` DTO로 먼저 decode하고 public constructor를 통해 transient Core proposal로 변환한다.

고정 envelope는 `formatVersion`, `kind`, `steps`, `summary`를 모두 요구한다. Plan은 빈 summary, completion candidate는 빈 steps를 요구한다. Dependency도 두 identifier를 모두 요구하되 kind가 선택하지 않은 값은 빈 문자열이어야 한다.

다음 입력은 `InvalidResponse`로 닫는다.

- Markdown fence, 앞뒤 설명 또는 trailing document
- 구조 field 또는 nested arguments의 duplicate key
- unknown proposal field와 지원하지 않는 version
- 과도한 response/proposal byte 또는 JSON depth
- choice 0개 또는 2개 이상
- non-assistant, refusal, tool-call, 예상하지 않은 finish reason
- configured served model과 다른 response model
- plan/completion/dependency 조합 불일치

`finish_reason=length`는 `ProviderLimit`으로 분류한다. Structured decoding은 syntax/shape gate일 뿐이며 Capability availability, graph, input schema, permission과 effect authority는 계속 Core가 검증한다.

### 5. Context assembly 입력도 provider 전에 제한한다

최종 `max_context_bytes` 외에 다음 compile-time hard gate를 적용한다.

- context payload 최대 512 KiB
- catalog Definition 최대 1,024개
- existing Step 후보 최대 4,096개
- Definition 최대 256 KiB, Step summary 최대 64 KiB
- 전체 scan source 최대 8 MiB
- JSON Value 깊이 64, node 32,768, text 256 KiB
- goal을 포함한 top-level header text 합계 최대 256 KiB
- Definition collection item 최대 4,096개

검사는 schema clone과 contract digest 전에 가능한 범위부터 수행한다. Packing은 매 item마다 전체 payload를 다시 canonicalize하지 않고 item canonical size를 한 번 계산한 뒤 보수적인 incremental size로 round-robin 선택한다. 마지막 exact canonical size를 다시 검사한다. 이 상한은 context assembler의 scan/clone 경계이며, 그보다 먼저 수행되는 WorkGraph frontier derivation 전체의 CPU·memory bound를 새로 보장하지 않는다.

Host가 512 KiB보다 큰 기존 `max_context_bytes`를 저장했더라도 effective payload budget만 512 KiB로 clamp해 호환 Run을 무조건 멈추지 않는다. Catalog/source/item hard gate 초과는 `ContextInputLimitExceeded` quiescence이며 reservation과 provider 호출은 모두 0회다. Durable `AgentLoopBudget` wire shape는 바꾸지 않는다.

### 6. 확정적인 provider 거부를 별도 closed 결과로 둔다

`PlannerPortFailure::ProviderRejected`와 `ModelCallRejectionReason::ProviderRejected`를 추가한다.

| 관찰 | 결과 |
|---|---|
| client timeout, HTTP 408/504 | `Timeout` → Unknown |
| transport/read 오류, HTTP 5xx | `Unavailable` → Unknown |
| HTTP 200 외의 unexpected 2xx | `Unavailable` → Unknown |
| HTTP 413/429 | `ProviderLimit` → closed |
| 그 밖의 HTTP 3xx/4xx와 redirect | `ProviderRejected` → closed |
| malformed/oversized HTTP 200 body | `InvalidResponse` → closed |

Raw provider error body는 읽어 durable state나 error text에 넣지 않는다. 새 enum은 internal journal JSON에 표현되지만 물리 table/column은 변하지 않으므로 SQLite schema 6을 유지한다.

### 7. Credential과 raw content는 관찰 경계 밖이다

Bearer token은 16 KiB hard bound의 sensitive `HeaderValue`로만 보관하고 custom `Debug`에서 redaction한다. Base URL은 parsing 전에 8 KiB로 제한한다. Plain HTTP에는 credential을 붙일 수 없고 credential이 없더라도 literal loopback IP endpoint만 허용한다. 원격 local-model server는 SSH tunnel이나 HTTPS를 사용해야 한다.

Adapter 자체는 logging/tracing을 추가하지 않으며 Config/Planner `Debug`, returned error, journal과 projection에 endpoint, schema, request, response, raw error 또는 credential을 넣지 않는다. 다만 `ureq`는 debug/trace에서 transport metadata를 `log` facade로 방출할 수 있으므로, 향후 composition root가 logger를 설치할 때 `ureq` target은 반드시 비활성화한다. 이 filter가 제품 logging test로 고정되기 전에는 endpoint metadata 비노출을 완성 보장으로 주장하지 않는다.

### 8. Live model은 opt-in engineering smoke다

기본 CI는 `127.0.0.1:0`의 hermetic server만 사용한다. `go50902` test는 `#[ignore]`이며 명시적 환경 변수가 없으면 실행할 수 없다. SSH tunnel은 adapter 밖에서 운영한다.

2026-08-30에 다음 경로를 실제 검증했다.

```text
XGENy PlanningContext
  -> durable ModelCallReserved
  -> SSH tunnel
  -> go50902 vLLM 0.27.1 / qwen3.8-27b
  -> strict structured PlanProposal
  -> synthetic idempotent planning fixture의 Core validation/materialization
  -> PlanAccepted
```

이 결과는 engineering connectivity smoke이며 실제 filesystem read/tool execution 검증이 아니다. 당시 Admission 기본형이 `read_only` effect를 지원하지 않아 smoke는 별도 synthetic idempotent planning fixture를 사용했다. ADR-0018 이후 ReadOnly core profile과 bounded driver 기반이 생겼지만 이 live smoke를 실제 tool execution으로 다시 검증한 것은 아니다. 기존 연구 평가 문서의 Qwen3.6 preregistration을 Qwen3.8로 조용히 대체하지 않는다. 주 평가 모델 변경은 pilot 전에 별도 amendment로 기록한다.

## 비목표

- 사용자용 `xgeny run` composition root와 자동 continuation
- streaming/partial response와 hidden reasoning 저장
- provider-side request status, billing 또는 idempotency reconciliation
- retry/backoff/fallback과 multi-provider routing
- tokenizer를 직접 실행한 exact token budget
- SSH process lifecycle 관리
- XGEN Model Gateway adapter
- 사내 CA/custom trust root를 쓰는 HTTPS provider 구성
- filesystem/process 제품 adapter와 실제 tool execution

## 회귀 gate

- profile digest stability와 semantic option change sensitivity
- endpoint/credential/prompt/schema/transport Debug redaction
- strict valid plan/completion decode
- duplicate/unknown/fenced/truncated/deep/oversized response rejection
- exact one choice와 finish-reason 분류
- missing/mismatched response model과 legacy function call 거부
- redirect 미추종과 fixed HTTP status taxonomy
- one reservation → one POST → one accepted Plan
- provider raw response/error body의 journal/projection 비노출
- request 전달 뒤 connection loss에서도 두 번째 connection 0회
- Context hard limit/deep schema에서 reservation과 provider call 0회
- Linux, macOS, Windows hermetic workspace tests
- opt-in `go50902` Qwen3.8 PlanAccepted smoke

## 결과

실제 모델 연결이 Core의 독립성과 durable safety를 우회하지 않는다. Provider가 바뀌어도 WorkGraph, journal, permission과 execution authority는 XGENy에 남고, XGEN은 이후 동일 계약의 호환 adapter로 연결할 수 있다.
