# ADR-0006: 결정론적 Router와 단일 Run 엔진

- 상태: 승인
- 기준일: 2026-08-28

## 문맥

같은 Capability를 로컬, MCP, Connector, XGEN이 제공할 수 있다. 모든 후보와 schema를 모델에 노출하고 실행 위치 선택까지 맡기면 작은 모델의 context를 낭비하고 정책 우회와 비결정적 실행을 만든다.

## 결정

- catalog가 크면 `capability.search`와 `capability.describe`로 progressive disclosure한다.
- Router는 contract, platform, availability, auth, trust, policy, required feature를 hard filter한다.
- 통과 후보는 의미 적합성, 검증 강도, 추가 권한, reversibility, data locality, 단계 수, latency/cost, 사용자 선호 순으로 lexicographic ranking한다.
- 모델은 후보와 이유를 제안할 수 있으나 runtime이 실행 직전에 재검증한다.
- effect 시작 후 자동 placement fallback을 금지한다. idempotency query로 미실행이 증명된 경우만 resume한다.
- `Direct`, `Tracked`, `Persistent`는 같은 Run/WorkGraph 엔진의 모드다. 모든 모드는 Journal과 Receipt를 남긴다.
- runtime은 위험에 따라 mode를 escalate할 수 있고 모델은 안전 하한 아래로 downgrade할 수 없다.

## 결과

- 모델과 provider가 바뀌어도 routing·권한 결과를 재현하고 회귀 테스트할 수 있다.
- Direct 요청도 기록과 검증을 잃지 않으면서 사용자 UX는 가볍게 유지된다.
- Router golden fixture와 후보 탈락 이유 기록이 필요하다.

## 폐기안

- 전체 Capability schema를 매 턴 prompt에 삽입
- 모델의 자유 형식 판단만으로 placement 선택
- Direct/Tracked/Persistent를 별도 실행 엔진으로 구현
- effect 시작 후 다른 위치에서 무조건 재시도
