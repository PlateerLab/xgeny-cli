# ADR-0004: CapabilityDefinition과 CapabilityInstance 분리

- 상태: 승인
- 기준일: 2026-08-28

## 문맥

XGENy는 내장 기능, 로컬 CLI, MCP, Connector, XGEN에서 같은 의미의 기능을 실행할 수 있다. 의미 계약과 현재 실행 상태를 하나의 manifest에 섞으면 credential·health·placement가 정본 계약에 침투하고, 같은 기능을 실행 위치별로 중복 정의하게 된다.

## 결정

- `CapabilityDefinition`은 ID, contract version, input/output schema, effect, permission resource selector, prerequisite, 실행·검증 계약만 가진다.
- `CapabilityInstance`는 source, placement, platform, trust, health, auth reference, latency/cost hint, transport binding을 가진다.
- 입력·출력·effect·검증 의미가 같을 때만 Instance가 같은 Capability ID를 공유한다.
- XGEN `workflow_id`와 MCP tool name은 binding metadata이며 canonical Capability ID가 아니다.
- PATH executable은 자동으로 typed Capability가 되지 않는다. 범용 실행은 `process.execute`, 안정적인 CLI는 curated adapter로 제공한다.
- JSON Schema 2020-12를 정본 schema 형식으로 사용한다.

## 결과

- XGENy core는 XGEN과 Connector에 의존하지 않고도 같은 계약으로 실행 위치를 선택할 수 있다.
- runtime availability가 변해도 Definition과 WorkGraph 의미가 바뀌지 않는다.
- adapter별 schema mapping과 conformance fixture가 필요하다.

## 폐기안

- XGEN workflow마다 별도 고정 MCP tool 생성
- source별로 서로 다른 core Capability 타입 사용
- static manifest에 credential과 health 포함
- PATH 전체 자동 노출
