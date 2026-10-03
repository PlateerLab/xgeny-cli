# ADR-0047: 대화 응답과 제한된 세션 문맥

- 상태: Accepted
- 날짜: 2026-10-04
- 보완: ADR-0033, ADR-0045

## 배경

실제 DeepSeek 검증에서 할인 계산과 온도 변환 프로젝트 모두 설명 뒤 함수 이름만 묻는 질문이
`completion_without_receipt_completed_plan`으로 거절됐다. 새 Run에는 실행 Step이 없는데 모든 최종
응답을 작업 완료로 취급했기 때문이다. 답변을 위해 불필요한 파일 읽기를 만들거나 기존 작업 완료
검증을 완화하는 대신, 대화 응답을 별도 계약으로 구분한다. 특정 질문·함수·모델 이름을 감지하는
분기는 사용하지 않는 일반적인 변경이다.

## 결정

- bare `xgen`는 별도 사용자 설정 없이 `response_candidate`를 제공자 schema와 system contract에
  추가한다. `formatVersion=1`, 빈 `steps`, 비어 있지 않은 bounded `summary`를 요구한다. 질문의
  자연어를 host가 분류하지 않고, model proposal을 Core가 검증한다.
- `response_candidate`는 **Run 전체의 Step 수가 0일 때만** 허용한다. 계획을 시작한 Run은 완료·실패·
  승인 대기 여부에 관계없이 이 응답으로 바꿀 수 없다. `completion_candidate`는 기존대로 실제
  receipt-completed plan을 요구한다. 대화 응답은 도구 실행이나 작업 완료의 증명이 아니다.
- 저장되는 종류는 `ResponseKind::Conversation`이다. 기존 원자 journal/sidecar 저장 경로를 재사용하며
  `CompletionCandidateRecorded` event, projected candidate와 `CompletionOutputRecord`에 종류를 결합한다.
  종류는 proposal digest와 record digest에 묶이고, append와 reopen에서 event/record/state가 일치해야 한다.
  새로운 SQLite table이나 별도 runtime은 만들지 않는다.
- 기존 완료 record는 종류를 생략하면 `TaskCompletion`으로 읽는다. 기본 종류와 manifest의 false flag는
  직렬화에서 생략해 이전 event·sidecar·manifest bytes와 digest를 유지한다. 새 대화 proposal의 digest
  domain은 `xgen.conversation-proposal/v1`이며 기존 완료 domain은 변경하지 않는다.
- REPL은 최종 답변을 같은 화면에 출력하되 `/status`에서는 `responded`, debug progress에서는
  `response_committed`로 구분한다. 다른 process의 `xgen resume`은 `XGEN_RESPONDED`를 출력한다.
  응답 재생은 workspace 도구나 provider 호출 없이 저장된 exact text를 사용한다.
- 새 대화 request profile의 schema·prompt digest와 활성화 여부를 Run manifest에 결합한다. 재개는
  manifest에 기록된 계약을 선택한다. 기존 Run과 headless `xgen run`의 provider 계약은 그대로 유지한다.
  atomic artifact schema와 대화 계약은 함께 사용하지 않는다. 불확정 호출의 자동 재시도는 추가하지 않는다.
- 세션은 최근 요청과 최종 응답을 최대 8 turn, JSON 직렬화 기준 12 KiB로 보관한다. 요청 문맥은 최대
  4 KiB, 응답 문맥은 최대 5,000 byte의 JSON text로 제한한다. UTF-8 경계에서 자르고 오래된 turn부터
  버리며 현재 요청은 자르지 않는다. goal 전체 한도 16 KiB에 문맥이 들어가지 않으면 오래된 문맥을
  생략한다. 내용은 JSON data로 제공하며 permission이나 이번 Run의 실행 증거로 쓰지 않는다.
- 세션 문맥은 process 내에 유지한다. 다음 Run에 전달된 bounded 문맥은 그 Run의 goal에 포함돼 기존
  durable 저장을 따른다. 별도 장기 transcript는 만들지 않는다. `/clear`는 문맥과 pointer만 초기화하고
  저장된 Run은 삭제하지 않는다. 다른 Run을 명시적으로 재개하면 이전 paused 요청을 새 응답에 붙이지 않는다.

## 검증

일반 지식 답변과 프로젝트 설명 뒤 후속 질문을 설계 사례로 삼고, 별도 프로젝트와 실제 코드 수정은
검증 사례로 사용한다. 자동 검증은 종류 변조·완료와 응답의 교차 결합 거절, Step이 있는 Run의 응답
거절, SQLite 재시작 후 exact offline replay, 다중 turn·JSON escape·UTF-8·크기 제한·clear와 permission
경계를 확인한다. 실제 API 검증은 별도 기록에 남긴다.

## 한계

자연어 답변의 모든 주장이 사실인지 자동 보장하지 않는다. 도구가 필요했던 요청에 모델이 대화
응답을 고르는 품질 문제와 대화 응답 자체의 사실성은 별도 평가 대상이다. 대화가 끝나도 새로운
작업 권한을 부여하지 않으며, session 밖의 전체 대화 복원이나 token streaming은 이번 변경에 없다.
