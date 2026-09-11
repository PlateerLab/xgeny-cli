# ADR-0036: planner prompt는 고정 부분을 앞에, 가변 식별자를 뒤에 둔다

- 상태: Accepted
- 날짜: 2026-09-05
- 관련: ADR-0017 OpenAI-compatible provider adapter, ADR-0030 chronological planning context v3, ADR-0035 model profile inference limits

## 배경

Planner 호출의 user message는 request envelope(`xgeny.planner-request/v1`)과 그 안의 planning
context(`xgeny.planning-context/v3`)를 `serde_json`으로 직렬화한 것이다. 두 구조체의 필드 순서가
그대로 wire의 키 순서가 되며, 현재는 `callId`, `requestDigest`, `runId`, `authority`,
`journalHeadDigest`처럼 **호출마다 달라지는 값이 맨 앞**에 오고, 호출 사이에 거의 바뀌지 않는
capability catalog가 맨 뒤에 온다.

Qwen3.8 27B(Q4_K_M)를 Ollama로 실행해 측정하면 user message 9,064바이트 중 catalog가 8,304바이트
(92%)이고, planner 호출 하나의 약 60초 중 절반이 이 catalog의 prefill이다. Provider들(Ollama, vLLM,
llama.cpp)은 직전 요청과 **앞에서부터 같은 token**만 재사용하는 prefix cache를 갖는데, 가변 식별자가
첫 50토큰 안에 있으므로 cache는 매 호출 깨진다. 같은 요청에 새 식별자만 바꿔 두 번 보내면 현재
순서는 72.2초/61.7초, 가변 키를 뒤로 보낸 순서는 64.8초/**30.7초**다. 이 이득은 model이 아니라
provider의 캐시 구조에서 오므로 model 선택과 무관하다.

`context_digest`와 `request_digest`는 RFC 8785(JCS)로 정규화해 계산하므로 키 순서에 영향을 받지
않는다. 즉 순서 변경은 journal, receipt, manifest에 기록되는 어떤 digest도 바꾸지 않고, 모델이 보는
bytes만 바꾼다.

## 결정

### 1. Wire 키 순서를 "고정 → 누적 → 가변"으로 고정한다

Envelope: `profileVersion`, `planningContext`, `callId`, `requestDigest`.

Context: `profileVersion`, `capabilities`, `omittedCapabilities`, `catalogDigest`, `goal`,
`planningConstraints`, `steps`, `omittedSteps`, `toolOutputs`, `totalSteps`,
`verifiedCompletedSteps`, `runId`, `authority`, `authorityEpoch`, `journalSequence`,
`journalHeadDigest`.

Catalog와 goal은 Run 안에서 불변이고, `steps`/`toolOutputs`는 턴마다 뒤에 추가되므로 이전 턴의
prefix가 그대로 재사용된다. 가변 식별자는 맨 뒤에 있어 어떤 값이 오든 앞의 prefix를 무효화하지
않는다.

### 2. Envelope profile을 v2로 올린다

키 순서는 이제 계약이므로 `REQUEST_ENVELOPE_PROFILE`을 `xgeny.planner-request/v2`로 올린다.
이 값은 ADR-0017의 `request_profile_digest` 입력이므로 digest가 바뀌고, ADR-0035 §4와 같은 결과가
따른다: 이 ADR 이전에 시작해 완료되지 않은 Run은 resume 시 `configuration_mismatch`로 닫힌다.
Planning context profile은 v3을 유지한다. 내용과 canonical digest가 같고, 순서는 envelope v2가
정의한다.

### 3. 순서는 테스트로 고정한다

직렬화된 JSON에서 `capabilities`가 `runId`보다 앞에, `steps`와 `toolOutputs`가 `capabilities`보다
뒤에, `journalHeadDigest`가 마지막에 오는지, 그리고 같은 context의 `context_digest`가 순서와 무관하게
같은지를 단위 테스트가 검사한다. Provider는 envelope의 `planningContext`가 `callId`보다 앞에 오는지
검사한다.

### 4. Catalog 내용은 바꾸지 않는다

`outputSchema`를 빼면 prompt가 29% 줄지만(3,683→2,621 token, cold 9초) model이 "read-text가
digest를 돌려준다"를 그 schema에서 배우므로 planning 품질 검증 없이는 하지 않는다. 이 ADR은 순서만
바꾼다.

## 결과

- 같은 Run의 두 번째 호출부터, 그리고 같은 catalog를 쓰는 다음 Run의 첫 호출부터 prefill이
  재사용된다. 로컬 27B 기준 호출당 약 30초 절감이 기대된다.
- Journal·Receipt·manifest·protocol fixture는 바뀌지 않는다. 바뀌는 것은 request profile digest
  하나다.
- Prefix cache가 없는 provider에서는 이득도 손해도 없다.

## 대안

- Provider별 prompt caching API(예: 명시적 cache control)를 쓴다: OpenAI-compatible 공통 표면이
  아니므로 채택하지 않는다.
- Catalog를 별도 system message로 옮긴다: 순서를 바꾸는 것과 효과가 같지만 message 구조가 바뀌어
  더 큰 계약 변경이다.

## 검증

- 단위 테스트: 직렬화 순서, 순서 무관 digest, envelope v2.
- 실측: 같은 27B 쓰기 Run의 호출별 지연(journal `model_call_reserved`→`plan_accepted`)을 변경 전
  68/96/107초와 비교하고, 새 식별자로 연속 두 번 보낸 요청의 2회차가 약 30초인지 확인한다.
  Ollama 8B 읽기 Run과 compatibility probe에 회귀가 없어야 한다.
