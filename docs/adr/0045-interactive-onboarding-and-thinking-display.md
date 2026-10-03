# ADR-0045: 제공자 온보딩과 최소 대화 화면

- 상태: Accepted
- 날짜: 2026-10-04
- 보완: ADR-0032, ADR-0033, ADR-0043

## 배경

일반 사용자가 endpoint, structured output transport와 thinking option을 알아야 시작할 수 있고,
REPL 기본 화면에 durable lifecycle 로그가 쌓인다. 사용자에게 필요한 기본 흐름은 요청 입력,
처리 중 표시, 검증된 최종 응답과 후속 요청이다.

## 결정

이 변경은 일반적인 frontend 개선이다. 별도 agent runtime이나 특정 workspace/key file 경로를 추가하지 않는다.

- 첫 TTY 실행 또는 URL이 없는 interactive `model setup`에서 제공자를 선택한다. DeepSeek/OpenAI
  preset은 setup 단계에서 일반 profile 값으로 저장한다. Custom endpoint와 고급 CLI 옵션은 유지한다.
- 명시적 CLI, 환경변수와 기존 profile이 preset보다 우선한다. 실제 catalog와 compatibility probe를
  통과한 profile만 저장한다. Runtime은 hostname/model 이름으로 wire option을 추론하지 않는다.
- 일반 TTY는 요청 처리 중 `Thinking…`과 경과 시간을 표시하고 최종 검증된 summary만 출력한다.
  Thinking은 model reasoning 공개가 아니라 model/tool/verification을 포함하는 처리 중 표시다.
- 표시 갱신 thread는 presentation만 소유한다. Driver는 호출 thread에서 기존 observer, 승인,
  cancellation과 durable recovery 계약을 계속 소유한다. 승인 입력과 결과·오류 출력 전에 표시를 지운다.
- `xgen --debug`와 redirected 입력/출력은 기존 progress와 시작 Run ID 로그를 유지한다. TTY의
  Run ID는 `/status`에서 확인한다. 최종 model text의 terminal control character escape는 유지한다.
- 숨김 입력 credential은 우선 OS 보안 저장소에 저장한다. 저장소가 unavailable이고 사용자가
  `--store-token`을 명시하지 않았을 때만 현재 process의 zeroizing memory로 보관하고 안내한다.
  Profile에는 key reference를 넣지 않는다. 다음 TTY 실행에서 저장 key가 없으면 다시 숨김 입력한다.
- 한 goal의 승인된 permission class는 자동 continuation 동안 누적한다. 다음 goal과 명시적
  `/resume`은 기본 mode로 다시 시작한다. 서로 다른 class의 승인과 `deny`는 유지한다.
- Session key는 입력 당시 base URL과 byte-exact하게 일치할 때만 사용하며 environment/tool process로
  전달하거나 파일로 보관하지 않는다. Headless 입력과 명시적 `--store-token`의 실패 계약은 유지한다.

## 검증과 한계

Preset 두 종류의 실제 mock HTTP wire와 CLI/environment override, 기존 setup/run/resume,
승인·중단·escape와 event가 없는 blocking 구간의 표시 갱신을 검증한다. 별도의 PTY smoke에서
첫 실행과 설정 재사용, Thinking 표시와 최종 응답을 확인한다.

전체 transcript나 장기 memory, assistant token streaming은 이번 변경 범위가 아니다. 후속 요청은
기존 bounded previous durable summary를 사용한다. Secure store가 없는 환경에서는 실행마다 key 입력이
필요하며 이 점을 영구 저장 성공처럼 안내하지 않는다.

실제 DeepSeek 검증에서 새 Run에 receipt-completed Step이 없는 단순 후속 질문은 거절됐다.
이 frontend 변경은 Core의 작업 완료 조건을 완화하지 않는다. 순수 대화 응답을 작업 완료와
구분하는 계약과 검증은 별도 작업으로 남긴다.
