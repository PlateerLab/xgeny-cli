# XGEN 대화형 실사용 검증 — 2026-10-04

이 기록은 일반적인 온보딩·표시·승인 흐름을 검증한다. 특정 예제에 맞춘 engine 분기나 prompt 변경은 추가하지 않았다.
실제 DeepSeek API(`deepseek-flash`, `json_object`, `thinking disabled`)와 설치된 release binary를 PTY에서 실행했다.
고객 코드 대신 서로 다른 계산 예제를 사용했으며, 모델이 수정한 코드의 테스트는 별도 process에서 다시 실행했다.
API key는 숨김 입력했고 OS credential store에 저장했다. 이 문서에는 credential이나 원본 사용자 데이터가 없다.

## 온보딩과 표시

- profile이 없는 첫 실행에서 DeepSeek 선택, 숨김 key 입력, model 선택과 실제 compatibility probe를 완료했다.
- 다음 실행은 제공자·key 입력 없이 저장된 profile과 credential을 사용했다.
- 프로젝트 설명과 두 코드 수정 작업 모두 `Thinking…` 표시와 최종 응답을 확인했다.
- 도구 진행 event가 없는 blocking 구간의 경과 시간 갱신은 별도 deterministic test로 검증했다.

## 반복 승인의 수정

이전 구현은 approval pause를 재개할 때 방금 승인한 class만 남겨 앞서 승인한 model/read 권한을 버렸다.
현재 구현은 한 goal의 자동 continuation 안에서 승인한 class를 누적한다. 다음 goal이나 명시적 `/resume`은
기본 mode에서 시작하며, 종류별 승인과 `deny`는 유지한다.

| 실제 요청 | 수정 전 승인 횟수 | 수정 후 승인 횟수 | 결과 |
| --- | --- | --- | --- |
| README와 할인 함수를 읽고 설명 | model 3, read 2 | model 1, read 1 | 설명 출력, 파일 변경 없음 |
| 할인 계산 실패를 수정하고 테스트 재실행 | model 5, read 1, write 1, execute 2 | 각 class 1회 | 실제 코드 수정, 독립 테스트 3개 통과 |
| 별도 온도 변환 예제 수정 | 변경 설계에 사용하지 않음 | 각 class 1회 | 실제 코드 수정, 독립 테스트 3개 통과 |

이 수치는 각각 한 번의 실행에서 관찰한 값이며 latency나 모델 품질의 통계적 개선으로 해석하지 않는다.
회귀 테스트는 goal 내 누적, 다음 goal 초기화, 거절 뒤 명시적 resume 초기화와 permission deny를 확인한다.

## 확인된 한계와 다음 작업

1. **일반: 순수 대화 응답 계약.** 할인 함수와 온도 변환 함수 모두 설명 직후 함수 이름만 묻는 요청이
   `completion_without_receipt_completed_plan`으로 거절됐다. 직전 summary가 다음 goal로 전달돼도 새 Run에는
   receipt-completed Step이 없기 때문이다. 작업 완료 조건을 단순 완화하거나 불필요한 파일 읽기로 우회하지 않고,
   도구 작업 완료와 대화 응답을 구분하는 ADR·응답 계약·durable 저장 검증을 설계해야 한다.
2. **일반: 최종 설명과 실제 변경의 일치 평가.** 할인 코드의 실제 변경은 `sum(prices) * (1 - discount)`였지만,
   최종 summary에는 `sum(prices) - discount`라고 잘못 적혔다. 독립 테스트는 통과했고 테스트 파일은 그대로였다.
   Receipt와 completion binding 검증은 설명 문장 전체의 사실성을 보장하지 않는다. 이는 한 사례에서 관찰한
   가설이며, 다른 작업과 설계에 사용하지 않은 입력으로 재현하기 전에는 engine이나 prompt를 변경하지 않는다.

## 자동 검증

- Rust workspace: 611 passed, 0 failed, 4 ignored.
- `cargo fmt --all -- --check`, workspace 전체 target의 Clippy `-D warnings` 통과.
- 이름 변경에 대한 npm 테스트 14개, 5개 platform distribution 검사, native installer와 npm 설치·재설치·제거 smoke 통과.
- 기존 binary가 만든 profile과 Run을 새 binary가 읽고 완료 replay·approval resume하는 검증은
  `json_object`, `json_schema` 두 형식에서 통과했다. 내부 durable namespace와 credential service ID는 유지했다.
