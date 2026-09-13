# ADR-0038: planner 출력 잘림을 provider limit과 다른 rejection class로 기록한다

- 상태: Accepted
- 날짜: 2026-09-14
- 관련: ADR-0016 durable model call lifecycle, ADR-0017 OpenAI-compatible provider adapter, ADR-0035 model profile inference limits, ADR-0037 portable proposal schema and compact output

## 배경

Planner 호출이 `finish_reason: "length"`로 끝나면 provider adapter는 `PlannerPortFailure::ProviderLimit`을
돌려주고, Core는 journal에 `ModelCallRejectionReason::ProviderLimit`(`provider_limit`)으로 settlement를
기록하며, CLI는 `XGENY_REJECTED … reason=model_rejected.provider_limit`을 출력한다. HTTP 413과 429도
같은 class다. 즉 사용자는 세 가지 다른 원인을 하나의 문자열로 본다.

| 원인 | 사용자가 취해야 할 조치 |
| --- | --- |
| 출력 token 예산에서 잘림 | `--max-output-tokens`를 올리거나 model의 thinking을 줄인다 |
| HTTP 429 rate limit | 기다리거나 동시 요청을 줄인다 |
| HTTP 413 요청 크기 초과 | workspace/goal을 줄인다 |

Compatibility probe(PR #53, ADR-0032 §4)는 이미 `provider_output_truncated`와 `rate_limited`를 구분한다.
Production planner 경로만 구분하지 않아 진단이 비대칭이다.

실측:

- llama.cpp(Ollama 번들 `llama-server`, Qwen3.8 27B GGUF, `--jinja`)에서 쓰기 Run이 출력 예산 4096
  token에서 `model_rejected.provider_limit`으로 닫혔다. 응답의 `reasoning_content`가 10,148자였고
  `finish_reason`은 `length`였다. Rate limit이 아니었지만 결과 코드로는 알 수 없었다.
- 2026-09-14 재현: Ollama 0.33 Qwen3.8 27B 읽기 Run에 `XGENY_OPENAI_MAX_OUTPUT_TOKENS=64`를 주면 25초
  만에 `model_rejected.provider_limit`으로 닫히고 journal에 `"reason":"provider_limit"`이 남는다.

## 결정

### 1. Provider port에 `OutputTruncated` failure를 추가한다

`xgeny_runtime::PlannerPortFailure`에 `OutputTruncated` variant를 더한다. OpenAI-compatible adapter는
choice의 `finish_reason == "length"`일 때 이 값을 돌려준다. HTTP 413/429와 요청 크기 상한 초과는 그대로
`ProviderLimit`이다.

### 2. Journal에 `output_truncated` rejection class를 추가한다

`xgeny_workgraph::ModelCallRejectionReason`에 `OutputTruncated`를 더한다. serde 표현은 다른 variant와
같은 규칙으로 `output_truncated`다. AgentLoop는 `PlannerPortFailure::OutputTruncated`를 이 class로
settle하고 conflict intent는 `ProviderLimit`과 같은 `RejectStale`이다. 재시도하지 않는다는 ADR-0016의
의미는 그대로다.

### 3. 공개 결과 코드 `model_rejected.output_truncated`를 추가한다

CLI는 `model_rejected.output_truncated`를 출력한다. `model_rejected.provider_limit`의 의미는 "출력
예산·요청 크기 초과"에서 "요청 크기 초과와 429"로 좁아진다. Getting-started troubleshooting 표에 새
코드와 조치(`--max-output-tokens`, `XGENY_OPENAI_MAX_OUTPUT_TOKENS`, thinking 설정)를 적는다.

### 4. 호환성 경계를 그대로 받아들인다

- Request profile digest(ADR-0017)의 입력이 아니다. 진행 중 Run의 resume은 이 변경으로 깨지지 않는다.
- Journal event JSON에 새 enum 값이 생긴다. 이 변경 이후 binary가 기록한 `output_truncated` settlement는
  이전 binary가 역직렬화하지 못한다. RC3 Run을 RC2로 여는 것은 이미 비지원이고 `v0.1.0-rc.3` tag 전이므로
  store schema version은 올리지 않는다.
- 이전 binary가 기록한 journal은 새 binary가 그대로 읽는다. `provider_limit`은 계속 유효한 값이다.

## 결과

- 사용자가 `output_truncated`를 보면 예산을 올리면 되고, `provider_limit`을 보면 기다리거나 요청을 줄이면
  된다. 두 조치를 섞어 시도할 필요가 없다.
- Probe와 planner의 진단 어휘가 같은 사실을 같은 방식으로 말한다.
- Journal settlement가 provider 응답의 `finish_reason` 사실을 보존하므로 이후 분석에서 잘림 빈도를 셀 수
  있다.

## 대안

- `provider_limit`을 유지하고 settlement event에 부가 필드를 넣는다: journal schema 변경은 어차피
  발생하고, 사용자가 grep하는 것은 class 문자열이라 부가 필드는 진단에 쓰이지 않는다.
- `planner_invalid_response`로 분류한다: 잘림은 형식 위반이 아니라 예산 문제다. 같은 class에 넣으면 strict
  schema 미준수 provider와 구분할 수 없어 ADR-0037의 진단이 흐려진다.
- 잘리면 예산을 올려 자동 재시도한다: planner 호출당 HTTP POST 1회와 불확정 호출 무재시도(ADR-0016)
  원칙을 깬다. 예산은 request profile digest 입력이라 호출 중간에 바꿀 수도 없다.

## 검증

- 단위 테스트: `decode_chat_response`가 `finish_reason: "length"`에 `OutputTruncated`를 돌려주고
  413/429는 `ProviderLimit`으로 남는다. `RejectionReason::ModelRejected(OutputTruncated).code()`가
  `model_rejected.output_truncated`다.
- 계약 테스트: HTTP 200에 `finish_reason: "length"`를 돌려주는 서버로 AgentLoop tick을 돌리면 journal
  마지막 event가 `ModelCallSettled { Rejected { OutputTruncated } }`이고 raw body는 저장되지 않는다.
- 실측: 위 재현 절차(27B, 예산 64)가 `XGENY_REJECTED … reason=model_rejected.output_truncated`를 내고,
  같은 입력에 예산 1024를 주면 `XGENY_COMPLETED`다. 닫힌 포트와 429 응답은 계속 `provider_limit`이 아닌
  각자의 class(`model_call_unknown.transport_unavailable`, `model_rejected.provider_limit`)를 낸다.
