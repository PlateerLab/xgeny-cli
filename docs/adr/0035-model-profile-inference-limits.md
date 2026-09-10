# ADR-0035: planner inference timeout과 출력 예산을 모델 프로필 설정으로 옮긴다

- 상태: Accepted
- 날짜: 2026-09-05
- 관련: ADR-0016 durable model call lifecycle, ADR-0017 OpenAI-compatible provider adapter, ADR-0032 모델 프로필

## 배경

Public CLI는 planner 호출의 wall-clock 예산과 출력 token 예산을 `xgeny-cli` 상수
`MODEL_TIMEOUT = 60s`, `MAX_OUTPUT_TOKENS = 1024`로 고정한다. Compatibility probe도 같은 값을 쓴다.

Production 기준과 같은 계열인 Qwen3.8 27B(Q4_K_M)를 Ollama로 로컬 실행해 측정한 결과, planner 호출
하나는 prefill 약 34초(planning context 3.1k token)와 생성 약 29초(reasoning 포함 262 token)로 약
60초가 걸린다. Planning context는 매 호출 고유한 run/call 식별자와 digest를 포함하므로 provider의
prefix cache가 적용되지 않아 이 비용은 매 호출 반복된다. 읽기 Run은 호출당 45~55초로 경계에 있고,
쓰기 Run은 첫 호출을 59초에 통과한 뒤 tool output이 더해진 두 번째 호출이 60초에서
`model_call_unknown`으로 닫힌다. 같은 조건에서 model의 제안 자체는 올바르다(파일을 먼저 읽는 step을
계획하고 digest를 지어내지 않는다).

Probe는 planning context가 없는 작은 prompt를 보내므로 12초에 통과하며 이 지연을 예측하지 못한다.
Timeout은 model 크기, quantization, hardware, provider의 prefill 처리량에 따라 달라져 하나의
상수로 모든 환경을 만족시킬 수 없다.

## 결정

### 1. 두 값은 비밀이 아닌 프로필 설정이다

`ModelProfile`에 `inferenceTimeoutSeconds`와 `maxOutputTokens`를 추가한다. 둘 다 credential이
아니며 `model-profiles.json`에 일반 설정으로 저장된다. `xgeny model setup`은
`--inference-timeout <초>`와 `--max-output-tokens <토큰>`으로 받아 프로필에 기록하고,
`xgeny model list`와 setup 결과가 두 값을 표시한다. 해석 순서는 ADR-0032와 같다: setup의 명시적
option, `XGENY_OPENAI_INFERENCE_TIMEOUT`/`XGENY_OPENAI_MAX_OUTPUT_TOKENS` 환경변수, 프로필, 기본값.
`run`/`resume`/`check`는 별도 option 없이 환경변수와 프로필로 해석한다. 범위 밖 값은
`inference_limits_invalid`(exit 64)로 닫는다.

### 2. 기본값은 실측 근거로 정한다

미설정 시 timeout은 300초, 출력 예산은 1024 token이다. 300초는 위 측정에서 두 번째 호출이 넘긴
60초의 5배로, 로컬 27B가 tool output을 포함한 context를 처리할 여유를 두되 죽은 endpoint의
catalog GET은 여전히 `MODEL_CHECK_TIMEOUT`(10초)에서 닫히므로 온보딩 실패는 느려지지 않는다.
1024는 27B의 실제 출력(262 token)에 충분하며 기존 값과 같다. 상한은 provider adapter의 기존
경계(timeout 1시간, 출력 65,536 token)를 그대로 따른다.

### 3. Probe와 production planner는 계속 같은 request profile을 쓴다

`compatibility_probe_config`와 `planner_config`는 둘 다 프로필의 값을 받는다. PR #54가 고정한
"probe의 `request_profile_digest`는 production planner와 같다"는 불변식은 유지된다.

### 4. Digest 결과를 그대로 받아들인다

Timeout과 출력 예산은 ADR-0017의 `request_profile_digest` 입력이므로 값이 바뀌면 digest가 바뀐다.
Run manifest는 digest만 기록하고 resume은 프로필에서 config를 다시 조립하므로:

- 이 ADR 이전에 시작해 아직 완료되지 않은 Run은 기본값 변경(60→300초) 때문에 resume 시
  `configuration_mismatch`로 닫힌다. Developer Preview 단계의 의도된 결과이며 새 Run으로 시작한다.
- Run 시작과 resume 사이에 프로필의 두 값을 편집하면 같은 이유로 `configuration_mismatch`가 된다.
  자동 대체는 하지 않는다.
- Manifest schema는 바꾸지 않는다. 값을 manifest에 기록해 resume이 프로필 대신 manifest를 따르게
  하는 안은 프로필이 단일 진실이라는 ADR-0032의 경계를 흐리므로 채택하지 않는다.

### 5. 저장 형식은 format version 1을 유지한다

새 필드는 `#[serde(default)]`로 읽어 기존 `model-profiles.json`을 그대로 로드한다. 새 필드가
기록된 파일을 이 ADR 이전 binary가 읽으면 `deny_unknown_fields`로 거부된다. RC 채널 간 downgrade는
ADR-0031/getting-started의 rollback 절차대로 별도 install directory를 쓰므로 프로필 파일을 공유하지
않는다.

## 결과

- 로컬 27B에서 쓰기 Run이 두 번째 planner 호출을 통과한다.
- 사용자는 hardware에 맞춰 timeout을 낮추거나 올릴 수 있고 값이 `model list`에 보인다.
- Probe는 여전히 planner 지연을 예측하지 못한다. 이 ADR은 지연을 예측하는 것이 아니라 예산을 환경에
  맞게 두는 것이다.

## 대안

- 상수만 300초로 올린다: 즉시 효과는 같지만 hardware별 조정이 불가능하고, 원격 provider 사용자에게
  불필요하게 긴 timeout을 강제한다.
- Planning context의 가변 식별자를 prompt 끝으로 옮겨 prefix cache를 살린다: 호출당 30초 이상 줄일
  수 있는 유효한 개선이지만 request envelope profile 변경이라 별도 ADR로 다룬다.
- Probe가 production 크기의 planning context를 보낸다: 지연 예측은 가능해지지만 probe가 Run state
  없이 catalog 없이 보내는 원칙(ADR-0032 §4)과 충돌한다.

## 검증

- 프로필 round-trip: 새 필드 저장·로드, 필드 없는 기존 파일 로드 시 기본값, 범위 밖 값 거부.
- `planner_config`/`compatibility_probe_config`가 프로필 값을 쓰고 두 digest가 같다.
- 실측: Qwen3.8 27B(Ollama)에서 쓰기 시나리오가 `XGENY_COMPLETED`, 읽기 시나리오와 8B 회귀 없음,
  probe PASS.
