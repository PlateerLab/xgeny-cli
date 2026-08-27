# ADR-0003: 표준 프로토콜 투영과 XGEN 호환 셸

- 상태: Accepted
- 날짜: 2026-08-27

## Context

XGEN은 기존 workflow/SSE/interaction/execution_io 계약과 광범위한 웹·Connector 소비자를 갖는다. 새 XGENy 의미 모델을 기존 응답에 직접 덮으면 회귀 위험이 크다. 반대로 XGENy가 모든 legacy 구조를 흡수하면 미래 구조가 과거 XGEN에 고정된다.

MCP와 A2A는 각각 도구/능력과 장기 agent task의 넓은 생태계 호환성을 제공한다. XGEN 고유 차별 기능은 WorkGraph, 조직 정책, 실행 증거, edge placement다.

## Decision

- canonical domain contract를 먼저 정의한다.
- MCP에는 capability 검색·호출을 작은 generic surface로 투영한다.
- A2A에는 remote Task, status, interaction, artifact, streaming을 투영한다.
- WorkGraph delta, PolicyLease, ExecutionReceipt, local capability tunnel만 XGEN extension으로 정의한다.
- legacy mapping은 XGEN 서버 측 Compatibility Shell이 소유한다.
- 기존 API는 유지하고 새 prefix/service를 추가한다.
- shadow, canary, feature flag, N/N-1 또는 명시된 지원 기간, rollback을 적용한다.

## Consequences

- OpenClaw, Claude, Codex, LangGraph 같은 외부 agent가 XGEN을 표준 표면으로 사용할 수 있다.
- 기존 웹과 Connector는 즉시 변경할 필요가 없다.
- MCP 2025/2026과 A2A 버전 협상 및 conformance 부담이 생긴다.
- XGEN 내부에 legacy↔canonical projection을 유지해야 한다.

## Rejected alternatives

- workflow마다 MCP 도구 하나씩 영구 노출
- 기존 SSE payload를 즉시 canonical event로 교체
- UI `ChatEvent`를 canonical protocol로 채택
- XGENy 내부에 XGEN legacy adapter를 기본 내장
