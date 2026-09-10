# ADR-0037: 모델용 proposal 스키마를 이식 가능하게 줄이고 compact 출력을 요구한다

- 상태: Accepted
- 날짜: 2026-09-06
- 관련: ADR-0015 durable planner contract, ADR-0017 OpenAI-compatible provider adapter, ADR-0036 planner prompt prefix order

## 배경

Planner에 보내는 strict JSON Schema(`xgeny.plan-proposal/v1`)는 Core의 상한을 그대로 옮긴
`maxLength: 5000`(objective, summary), `maxLength: 128/256`, `maxItems: 32/128`을 포함한다. Core는
같은 상한을 `MAX_ACCEPTED_OBJECTIVE_BYTES`, `MAX_COMPLETION_SUMMARY_BYTES`, `MAX_PROPOSAL_KEY_BYTES`,
`MAX_ACCEPTED_PLAN_STEPS`, `MAX_ACCEPTED_PLAN_EDGES`로 독립 검증하므로 스키마의 상한은 안전 장치가 아니라
model에 대한 안내다.

같은 GGUF를 두 provider로 실행해 측정한 결과다.

- llama.cpp(`llama-server`)는 JSON Schema를 GBNF 문법으로 컴파일하는데 `maxLength: 5000`이 반복 상한을
  넘겨 컴파일에 실패하고, **HTTP 200으로 제약 없는 출력**을 반환한다(서버 로그 `failed to parse
  grammar`). 상한을 제거하거나 2000 이하로 두면 정상 강제된다. PR #56의 production 스키마 프로브가 이
  provider를 `chat_completions_incompatible`로 닫는 이유가 이것이다.
- Ollama는 상한이 있든 없든 같은 결과를 낸다(8B, 3회씩 A/B 동일).
- `pattern` 키워드는 이식 가능하지 않다. Ollama 0.33은 anchored/unanchored, bounded/unbounded 어떤
  형태든 `400 failed to parse grammar`로 거부하고, llama.cpp는 anchored 형태만 강제한다.
- 작은 model은 pretty-print JSON을 낸다. 8B no-think model에 쓰기 카탈로그 prompt를 보내면 1024 token
  출력 예산 안에서 100% 잘렸고(2/2, 3/3), system prompt에 "한 줄 minified JSON" 문장 하나를 더하면
  100% 순응했다(767/854 token, 2/2). 27B는 이미 compact라 205→206 token으로 변화가 없다.

## 결정

### 1. 모델용 스키마에서 `maxLength`와 `maxItems`를 제거한다

Core 검증은 그대로다. 스키마는 구조(`required`, `additionalProperties: false`, `enum`, `const`)만
강제한다. `PROPOSAL_SCHEMA_REVISION`을 `xgeny.plan-proposal/v2`로 올린다.

### 2. `pattern`은 넣지 않는다

Step key 형식(`[A-Za-z0-9._-]{1,128}`)은 Core가 검증한다. 스키마 `pattern`은 Ollama 사용자 전부를
`model setup`에서 막으므로 채택하지 않는다.

### 3. System prompt가 compact 출력과 step key 규칙을 말한다

`SYSTEM_PROMPT`와 `CONSTRAINED_SYSTEM_PROMPT` 끝에 두 문장을 더한다: 한 줄 minified JSON, 그리고
Core의 step key 규칙(letters, digits, `.`, `_`, `-`만; `/`와 공백 금지). 스키마 `pattern`이 이식 가능하지
않으므로(§2) 규칙은 prompt로 전달하고 Core가 검증한다. 8B 측정에서 compact 문장만으로도 key가 3/3
유효했고, key 문장을 더하면 출력이 767~854에서 577 token으로 줄며 key는 계속 유효했다(3/3).
`PROMPT_TEMPLATE_REVISION`과 `CONSTRAINED_PROMPT_TEMPLATE_REVISION`을 v4로 올린다.

### 4. Digest 결과를 그대로 받아들인다

스키마 revision과 prompt template은 ADR-0017의 `request_profile_digest` 입력이다. 이 ADR 이전에
시작해 완료되지 않은 Run은 resume 시 `configuration_mismatch`로 닫힌다(ADR-0035 §4, ADR-0036 §2와
같은 결과). ADR-0036과 함께 배포해 resume 단절을 한 번으로 줄인다. Journal·Receipt·manifest schema는
바뀌지 않는다.

## 결과

- llama.cpp 계열 provider(LM Studio 포함 가능성)가 `model setup`을 통과하고 Run을 완료한다.
- 작은 model의 출력 잘림이 줄고, 큰 model에는 비용이 없다.
- Core의 상한 검증과 `invalid_step_key` 진단은 그대로 남는다.

## 대안

- 상한을 2000으로 낮춘다: llama.cpp의 정확한 임계는 버전 상수라 깨지기 쉽다. 제거가 단순하고 Core를
  단일 진실로 둔다.
- Provider별 스키마를 보낸다: request profile이 provider마다 갈라져 digest·resume 의미가 복잡해진다.
- `outputSchema`를 빼서 prompt를 29% 줄인다: model이 "read-text가 digest를 돌려준다"를 그 schema에서
  배우므로 planning 품질 검증 없이는 하지 않는다.

## 검증

- 단위 테스트: 스키마에 `maxLength`/`maxItems`/`pattern`이 없고, 두 system prompt에 compact 문장과
  key 규칙 문장이 있으며, revision 문자열이 올라갔다. Request profile golden은 의도적으로 재채취한다.
- 실측: llama.cpp(27B GGUF) `model setup` PASS와 읽기 Run `XGENY_COMPLETED`(이전: setup
  `chat_completions_incompatible`), Ollama 27B 쓰기 Run 회귀 없음, 8B 쓰기 카탈로그 prompt 잘림
  0/3.
