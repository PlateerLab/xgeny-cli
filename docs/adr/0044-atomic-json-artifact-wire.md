# ADR-0044: JSON 산출물의 객체 전송과 명시적 chat-template 옵션

- 상태: opt-in 구현·통합 검사 완료, 운영 반영 전 PR 검증
- 날짜: 2026-09-28
- 범위: OpenAI-compatible adapter와 CLI request profile

## 9/28 후속: 내부 도메인 schema

호스트는 `XGEN_OPENAI_ARTIFACT_SCHEMA`에 요청별 JSON Schema **본문**을 선택적으로 전달한다.
help의 `XGEN_OPENAI_ARTIFACT_SCHEMA=atomic-json-schema-v1`은 offline 지원 탐지 marker이며,
환경 변수에 `atomic-json-schema-v1` 문자열을 그대로 넣는 설정이 아니다.
이 옵션은 `json_schema_atomic_json`에서만 허용한다. 모델 ID/profile의 영구 기본값으로 저장하지 않는다.

- root object, 최대32768bytes·깊이32의 schema를 strict JSON으로 읽는다.
- 지원 vocabulary: type/enum/const/properties/required/additionalProperties(false)/items/anyOf,
  minItems/maxItems/minLength/maxLength/minimum/maximum/title/description.
- 참조·외부 리소스·미지원 키워드는 거부한다. 표준 `jsonschema` meta 검사와 Draft202012 offline validator를 쓴다.
- 동일 schema를 provider response_format의 `jsonContent`에 넣고, 돌아온 도메인 객체를 계획 수락 전에 검사한다.
  전체 envelope/model identity/출력 절단 검사가 먼저다. 파일 쓰기·영수증 권위는 변하지 않는다.
- schema가 request profile digest에 포함되므로 resume에도 동일 schema가 필요하다.
  새 schema로 바뀌거나 환경에서 빠지면 기존 실행을 조용히 다른 계약으로 재개하지 않는다.
- 프로세스 환경은 CLI composition이 읽고 provider builder는 명시적인 문자열을 받는다. provider가 환경을 읽지 않는다.

플랫폼은 하나의 Pydantic 구조에서 schema를 생성하고 strict host 검사도 수행한다. 내용·출처·요구사항 간
관계 검사는 별도로 유지한다. 범용 provider에 ML 도메인의 필드나 판단 규칙을 넣지 않는다.

실제 모델 후속 소규모 검사3/3은 필드·타입뿐 아니라 값까지 일치했다(15.007/20.762/11.847초).
하지만 실제 데이터 대화는0/3이었다. 두 timeout과 한 도구 오판/관계 제약 반복 실패로 운영 채택을 보류한다.
이후 아래 초기 검증 실패 기록은 삭제하거나 성공으로 덮어쓰지 않는다.

## 문제

파일 쓰기 계획의 바깥 JSON이 유효해도 `arguments.content`에 모델이 직접 작성한 JSON 문자열은
깨질 수 있다. ML 플랫폼의 센서 계획과 배송 답변에서 서로 다른 내부 JSON 형식 실패가 관찰됐다.
단순 괄호 보정이나 모델 재요청은 원본 판단을 바꾸거나 미확정 요청을 중복 실행할 수 있다.

또한 기존 `thinking=disabled`의 `thinking.type=disabled`와 vLLM의
`chat_template_kwargs.enable_thinking=false`는 다른 옵션이다. 모델 ID나 endpoint로 이를 추측하지 않는다.

## 결정

`--response-format json_schema_atomic_json`을 명시적으로 선택하면 다음 wire를 사용한다.

```json
{
  "path": "DECISION.json",
  "jsonContent": {"answer": "도메인 판단 결과"},
  "expectedDigest": null
}
```

- `xgeny.fs/write-atomic@1.0.0`만 허용한다. 다른 capability가 섞이면 계획 전체를 거부한다.
- `jsonContent`는 객체이며 문자열·배열을 받지 않는다. 인자는 위 세 필드로 한정한다.
- 표준 `serde_json::to_string`으로 객체를 native `content` 문자열로 변환한다. JSON 복구나 의미 보정이 아니다.
- 기존 native capability schema, 경로/권한/CAS 검사, 원자 쓰기, 검증과 영수증을 그대로 거친다.
- 파일 쓰기 계획과 완료 판단은 여전히 각각 실제 모델 호출이다. 완료 호출을 가짜 영수증으로 대체하지 않는다.
- 정수 정밀도, Unicode, 인용·역슬래시·개행, 중첩 구조를 모델이 반환한 값 그대로 직렬화한다.
- 기존 envelope/proposal 바이트·깊이 제한, 중복 필드·truncation 거부를 유지한다.

`--thinking chat_template_disabled`는 `chat_template_kwargs.enable_thinking=false`만 보낸다.
기존 `default`, `disabled`, `enabled`의 wire 의미는 바꾸지 않는다. 이 옵션을 지원하는 provider에서만 쓴다.

두 옵션은 profile 저장/복원·request digest에 포함한다. 변경된 profile로 기존 run을 몰래 재개하지 않는다.
기본값은 `json_schema/default`이며 거절·timeout에서 다른 모드로 자동 fallback하지 않는다.

## 한계

`jsonContent.additionalProperties=true`이므로 **도메인 필드의 완전성이나 의미를 보장하지 않는다**.
모든 strict-schema provider가 이 열린 객체 schema를 지원하는 것도 아니다. 호환성 확인을 따로 해야 한다.
CLI model check는 작은 completion probe이며, 실제 artifact 작성이나 의미 정확성 검증이 아니다.

2026-09-28 실제 Qwen endpoint의 호환성 probe는 16.95초에 통과했다. 작은 객체 복사 두 건은
실제 쓰기·검증·완료와 JSON parsing까지 통과했지만, 추가 필드/변경된 중첩 구조로 내용 일치 0/2였다.
브라우저 대화 세 건도 전체 CLI 90초 안에 끝나지 못했다. 당시 공유 서버는 GPU 두 장 100%,
실행 4~6건·대기 1~3건이었다. 이는 병목 관찰이지 모든 실패가 부하 때문이라는 인과 증명이 아니다.
따라서 성능 개선·사용자 대화 완주·운영 준비 완료라고 주장하지 않는다.

## 검증

```bash
cargo test -p xgen-provider-openai -p xgen-cli --all-targets --locked -j 2
```

offline 검사는 기존 profile digest, 새 옵션 저장/복원, native codec, 혼합 capability 거부,
중첩 중복 키·깊이·크기·출력 절단을 확인한다. 플랫폼의 loopback integration은 실제 CLI를 통해
두 객체의 값 보존·파일·영수증·정확히 두 번의 HTTP 호출을 확인한다. fixture는 모델 품질 증거가 아니다.

후속은 요청별 도메인 output schema를 wire schema와 host 검증의 단일 소스로 연결하는 설계다.
ML 필드나 특정 데이터셋 용어는 provider 안에 넣지 않는다. 추가적인 native 완료 호출 축소도
별도 execution contract 없이 이 codec에서 수행하지 않는다.

공식 chat-template 옵션 근거: [Qwen 모델 카드](https://huggingface.co/Qwen/Qwen3.8-27B-FP8).

## 9/29 후속: 호출 실패 원인 분리

기존 `provider_limit`은 호환 기록으로 유지한다. HTTP413/로컬 요청 크기 초과는
`request_too_large`, HTTP429는 `rate_limited`, finish_reason=length는 `output_truncated`로
provider→runtime→영속 ModelCallRejectionReason→CLI verdict에 전달한다. 원본 HTTP 오류 본문,
모델 partial output, credential은 진단으로 출력하지 않는다. timeout/unavailable은 계속 unknown이다.

원인 분류 자체는 자동 재시도 권한이 아니다. 플랫폼은 정확한 첫 호출/goal digest/거부 원장과
부작용0을 확인한 output_truncated에 한해서만 변경된 간결화 요청을 별도 원장으로 보낼 수 있다.
완료 단계의 잘림은 이미 도구가 실행됐을 수 있으므로 같은 경로로 재실행하지 않는다.
기존 model compatibility probe의 외부 enum은 유지하고 실제 실행 원장만 세분화했다.

provider/CLI/runtime/workgraph393개 통과, 외부 환경 검사4개 ignored. 실제 CLI/로컬 HTTP fixture는
두 객체 구조에서413/429/잘림/완료 단계 잘림/504의 원인·부작용·unknown을 검증했다.
구 binary는 새 reason을 읽지 못할 수 있으므로 배포 시 digest별 binary와 원장 호환성을 보존해야 한다.
운영에는 아직 반영하지 않았다. 플랫폼 상세는 `docs/NATIVE_FAILURE_RECOVERY_20260929.md`에 있다.

## 9/29 릴리스 준비

플랫폼의 단일 행동·관찰 루프와 결합한 실제 모델 브라우저 검사에서 서로 다른 4과제의
첫 질문·후속 질문·새로고침이 통과했다. 50 HTTP 호출 중 출력 절단0, HTTP 실패0이며
최대 completion652 tokens였다. 이는 해당 데이터 대화의 표본 결과이며 학습 전체의 성공 보장이 아니다.
native 전체 workspace test, clippy `-D warnings`, fmt, third-party license 검사도 통과했다.
기본 request profile은 변경하지 않는다. 운영 변경은 플랫폼 통합·필수 CI 이후 versioned binary로 진행한다.
