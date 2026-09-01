# OpenAI-compatible Provider Adapter

## 현재 제공 범위

`xgeny-provider-openai`는 synchronous `PlannerPort`를 OpenAI-compatible Chat Completions에 연결한다.

- immutable request profile과 digest
- non-streaming JSON Schema request
- retry/redirect/proxy/fallback 없는 HTTP POST 최대 1회
- user-invoked `GET /v1/models` 1회의 non-durable model catalog checker
- bounded model 목록 decode와 strict JSON Schema Chat Completions compatibility checker
- request, response, proposal와 JSON depth hard bound
- duplicate/unknown field를 거부하는 strict proposal codec
- configured served model과 response model exact match
- HTTP/transport 결과의 closed/Unknown 분류
- credential, endpoint와 raw content Debug redaction
- Core model-call reservation 및 Plan settlement 통합 테스트

Core, public protocol v0.1과 SQLite schema 8은 OpenAI/vLLM/Qwen에 의존하지 않는다.
Configured model은 provider response의 `model`과 exact match해야 한다. Alias가 다른 문자열을 반환하는 provider는 현재 별도 response identity 설정이 없으므로 fail-closed한다.

## 기본 검증

```bash
cargo test -p xgeny-provider-openai --all-targets
cargo test --workspace --locked
```

기본 suite는 외부 network를 사용하지 않는다. 실제 HTTP request를 loopback ephemeral port에서 받아 다음을 검사한다.

- exact `POST /v1/chat/completions`
- model, messages, deterministic options와 strict response schema
- durable reservation 뒤 outbound 한 번
- accepted Plan의 atomic settlement
- exact completion summary의 atomic settlement와 durable in-memory replay
- deterministic 4xx rejection의 closed settlement
- raw provider response sentinel 비영속화
- request 전달 뒤 connection loss에서 transport retry 0회
- loopback server 종료 뒤 completion replay의 HTTP 재호출 0회
- catalog check의 exact GET·model ID, inference/state 0과 loopback credential 미전송
- onboarding의 catalog GET → strict compatibility POST → active profile `run` 수직 연결

HTTPS bearer가 sensitive header로 catalog GET과 compatibility POST에만 결합되는지는 credential을
노출하지 않는 transport test double 단위 테스트로 별도 검증한다. 실제 HTTPS endpoint와 OS 보안
저장소는 배포 환경의 첫 연결 검증 범위다.

Ignored live test도 모든 OS에서 compile되지만 일반 CI에서는 실행하지 않는다.

## go50902 engineering smoke

먼저 별도 터미널에서 SSH tunnel을 연다.

```bash
ssh -N -L 18000:127.0.0.1:8000 go50902
```

그다음 repository root에서 opt-in test를 실행한다.

```bash
XGENY_LIVE_OPENAI_BASE_URL=http://127.0.0.1:18000/v1 \
XGENY_LIVE_OPENAI_MODEL=qwen3.8-27b \
XGENY_LIVE_OPENAI_TOKENIZER=Qwen/Qwen3.8-27B-FP8 \
cargo test -p xgeny-provider-openai \
  --test http_contract live_go50902_plan_smoke \
  -- --ignored --exact
```

Live test와 `run`은 health/model-list preflight를 자동 호출하지 않는다. 한 durable reservation에
inference POST 외의 요청을 더하지 않기 위해서다. 사용자는 그 전에 `xgeny model setup` 또는
`xgeny model check --compatibility`를 실행할 수 있다. Setup은 catalog와 별도 probe를 확인하지만 실제
planner prompt와 coding loop 품질은 첫 Run 및 live E2E가 증명한다. Endpoint나 model이 잘못됐으면 고정
failure로 종료하고 raw response를 출력하지 않는다.

2026-08-30 검증 환경:

- server: `go50902`
- runtime: vLLM `0.27.1`
- served model: `qwen3.8-27b`
- artifact/tokenizer identity: `Qwen/Qwen3.8-27B-FP8`
- advertised maximum model length: 524,288
- result: structured proposal decode와 synthetic idempotent planning fixture의 Core `PlanAccepted` 성공

이 smoke는 모델 연결과 계획 수락만 확인하며 실제 filesystem read/tool execution 검증이 아니다. 당시 smoke는 ReadOnly core profile 도입 전이어서 별도 synthetic idempotent fixture를 썼다. 이후 ADR-0018에서 ReadOnly와 bounded CLI driver 기반을 추가했지만 live smoke 자체를 다시 tool execution까지 확장한 것은 아니다. Advertised model length는 Run 연속성이나 실제 usable context를 보장하는 값이 아니다. XGENy는 bounded PlanningContext와 WorkGraph/journal resume를 사용하며, 장기 작업 성능은 별도 평가 프로토콜로 검증한다.

## 안전 경계

- SSH tunnel 생성·종료는 adapter 책임이 아니다.
- Bearer credential은 HTTPS에서만 허용한다.
- Credential 없는 plain HTTP도 literal loopback IP endpoint만 허용한다. 원격 local-model server는 SSH tunnel이나 HTTPS를 사용한다.
- API key와 raw provider response/error는 log, journal, Receipt 또는 digest input에 넣지 않는다. Provider-bound PlanningContext payload는 exact tool output을 포함해 context digest에 commitment되고, system prompt는 template digest를 거쳐 request-profile digest에 commitment된다. Digest는 기밀화가 아니다.
- Local filesystem read 권한과 remote model egress 권한은 별개다. 이 adapter는 전달만 수행하며 local output의 원격 반출을 승인하지 않는다. Public composition root가 provider 위치와 데이터 민감도를 기준으로 별도 egress 결정을 내려야 한다.
- 향후 CLI가 logger를 설치할 때 `ureq` target을 비활성화한다. Transport metadata filter가 제품 logging test로 고정되기 전에는 endpoint 비노출을 완성 보장으로 간주하지 않는다.
- Timeout/transport/5xx는 delivery ambiguity 때문에 Unknown이며 자동 retry하지 않는다.
- HTTP 200 외 unexpected 2xx도 provider 수락 가능성이 있으므로 Unknown으로 둔다.
- Unknown call은 explicit recovery 전까지 새 model call을 막는다.
- HTTP 4xx의 확정적 거부는 `ProviderRejected` closed settlement다.
- Model proposal은 permission, selected Instance, tool execution 또는 completion 권위를 얻지 않는다.

## 이후 연결 범위

ADR-0022/0023에서 사용자용 `xgeny run/resume`, capability-confined filesystem read와 별도
model-egress/read 동의가 이 adapter에 연결됐다. 기본 CI는 loopback provider를 사용해 첫 model Plan,
별도 process의 local read, durable ToolOutput의 두 번째 model turn 전달과 offline completion replay를
검증한다. 실제 go50902/Qwen을 이용한 같은 2-turn public CLI 증명, process/patch/network Capability와
XGEN Model Gateway는 별도 live/후속 slice다. 현재 rustls trust는 public web PKI 기준이므로 사내
CA/custom trust root가 필요한 HTTPS provider 구성도 후속 설계 범위다.

Public 2-turn live gate의 redacted 실행 절차와 인수 조건은
[public local run/resume](public-local-run-resume.md#go50902-public-live-gate)에 둔다. 이 gate는 기존
synthetic `PlanAccepted` smoke를 대체하지 않고, 같은 provider adapter를 public CLI, 실제 filesystem
effect, SQLite restart와 offline completion replay까지 수직으로 검증한다. 통과 결과는 실행한 exact
commit과 함께 별도로 기록하며, 실행 전에는 완료됐다고 주장하지 않는다.

Provider failure matrix는 [ADR-0017](../adr/0017-openai-compatible-provider-adapter.md), profile과 credential
경계는 [ADR-0032](../adr/0032-model-profiles-and-secure-credential-boundary.md)를 따른다.
