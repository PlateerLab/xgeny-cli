# OpenAI-compatible Provider Adapter

## 현재 제공 범위

`xgeny-provider-openai`는 synchronous `PlannerPort`를 OpenAI-compatible Chat Completions에 연결한다.

- immutable request profile과 digest
- non-streaming JSON Schema request
- retry/redirect/proxy/fallback 없는 HTTP POST 최대 1회
- request, response, proposal와 JSON depth hard bound
- duplicate/unknown field를 거부하는 strict proposal codec
- configured served model과 response model exact match
- HTTP/transport 결과의 closed/Unknown 분류
- credential, endpoint와 raw content Debug redaction
- Core model-call reservation 및 Plan settlement 통합 테스트

Core, public protocol v0.1과 SQLite schema 6은 OpenAI/vLLM/Qwen에 의존하지 않는다.
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
- deterministic 4xx rejection의 closed settlement
- raw provider response sentinel 비영속화
- request 전달 뒤 connection loss에서 transport retry 0회

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

Test는 health/model-list preflight를 하지 않는다. 한 durable reservation에 inference POST 외의 요청을 더하지 않기 위해서다. Endpoint나 model이 잘못됐으면 고정 failure로 종료하고 raw response를 출력하지 않는다.

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
- API key, raw prompt/context/response/error를 log, journal, Receipt 또는 digest input에 넣지 않는다.
- 향후 CLI가 logger를 설치할 때 `ureq` target을 비활성화한다. Transport metadata filter가 제품 logging test로 고정되기 전에는 endpoint 비노출을 완성 보장으로 간주하지 않는다.
- Timeout/transport/5xx는 delivery ambiguity 때문에 Unknown이며 자동 retry하지 않는다.
- HTTP 200 외 unexpected 2xx도 provider 수락 가능성이 있으므로 Unknown으로 둔다.
- Unknown call은 explicit recovery 전까지 새 model call을 막는다.
- HTTP 4xx의 확정적 거부는 `ProviderRejected` closed settlement다.
- Model proposal은 permission, selected Instance, tool execution 또는 completion 권위를 얻지 않는다.

## 아직 연결하지 않은 것

사용자용 `xgeny run`, 실제 filesystem/process Capability와 XGEN Model Gateway는 아직 없다. ADR-0018에서 generic bounded driver와 ReadOnly/Artifact Receipt 기반은 fake port로 검증했다. 다음 vertical slice는 actual filesystem adapter, typed output sidecar, planning context v2와 completion output을 먼저 닫고, 그 뒤 동일 driver의 planner 구성을 이 adapter로 교체한다. 현재 rustls trust는 public web PKI 기준이므로 사내 CA/custom trust root가 필요한 HTTPS provider 구성도 후속 설계 범위다.

설계 근거와 failure matrix는 [ADR-0017](../adr/0017-openai-compatible-provider-adapter.md)을 따른다.
