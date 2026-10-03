# 대화 응답 실사용 검증 — 2026-10-04

실제 DeepSeek API(`deepseek-flash`, `json_object`, `thinking disabled`)와 release binary를 PTY에서 실행했다.
저장된 OS credential을 사용했고 제공자 선택이나 key 재입력은 없었다. 고객 코드 대신 작은 독립 예제를
사용했다. 아래 결과는 각 요청 한 번의 관찰이며 모델 품질이나 latency의 통계적 개선을 뜻하지 않는다.

## 대화와 후속 질문

| 입력 사례 | 변경 전 | 변경 후 | 승인 |
| --- | --- | --- | --- |
| 할인 함수 설명 뒤 이름과 인자만 질문 | Receipt 없는 완료로 거절 | `discounted_total(prices, discount)` 응답 | model 1회 |
| 별도 온도 변환 함수 설명 뒤 이름만 질문 | Receipt 없는 완료로 거절 | `celsius_to_fahrenheit` 응답 | model 1회 |
| 큐와 스택의 차이 설명 | 이번 변경의 일반 질문 검증 | 두 개념 설명 | model 1회 |
| 방금 설명한 것 중 FIFO 질문 | 후속 문맥 검증 | Queue 응답 | model 1회 |
| 세 번째 요청에서 첫 질문의 두 개념 질문 | 다중 turn 검증 | Queue and Stack 응답 | model 1회 |

후속 질문과 일반 질문에서 추가 파일 읽기·수정·process 실행 승인은 없었고, Thinking과 최종 응답을 확인했다.
두 프로젝트의 함수명은 실제 파일과 비교했다. 별도의 deterministic CLI integration에서 Step과 실행 Receipt가
없음, `responded` 상태, `/clear`의 문맥 초기화와 별도 process의 exact offline replay를 확인했다.

## 작업 기능의 회귀 확인

- 할인 예제의 실패를 수정하는 요청은 model/read/write/execute를 각각 한 번 승인했다. 실제 코드는
  `sum(prices) * (1 - discount)`로 변경됐고, 모델과 별도로 실행한 테스트 3개가 통과했다.
- 별도 온도 변환 예제 역시 각 class를 한 번 승인했다. 실제 코드는 `value * 9/5 + 32`로 변경됐고,
  독립 테스트 3개가 통과했다. 두 예제의 테스트 파일은 변경되지 않았다.
- 이전 binary가 만든 profile·완료 Run·approval-paused Run을 새 binary로 읽고 replay/resume하는
  smoke는 `json_object`, `json_schema` 두 형식에서 통과했다.

이 실행의 수식 설명은 실제 변경과 일치했지만, 자연어 사실성이 해결됐다고 주장하지 않는다.
[이전 검증](interactive-acceptance-2026-10-04.md)에서 관찰한 불일치는 별도 평가 대상으로 남아 있다.

## 자동 검증의 범위

종류를 바꾼 sidecar와 event의 결합 거절, 도구 Step이 있는 Run의 대화 응답 거절, output commitment가
없는 응답 거절, SQLite reopen 뒤 모델·도구를 다시 부르지 않는 replay, 기존 완료의 Receipt 조건,
legacy 기본 필드 생략, builder 순서에 따른 schema·digest 보존, UTF-8·JSON 크기 한도, 다중 turn과 clear,
다른 Run 재개 때 paused 요청을 잘못 연결하지 않는지를 확인한다. 전체 테스트 결과는 PR에 기록한다.

최종 자동 검증: Rust workspace 620 passed, 0 failed, 4 ignored. 이후 추가한 headless 계약 회귀
테스트를 포함한 CLI integration 5개도 통과해 고유 테스트 총 621개를 확인했다. 전체 target의 Clippy
`-D warnings`, fmt와 public documentation 계약 검사도 통과했다.
