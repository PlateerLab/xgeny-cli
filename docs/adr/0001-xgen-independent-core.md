# ADR-0001: XGEN 비의존 독립 코어

- 상태: Accepted
- 날짜: 2026-08-27

## Context

기존 Connector 로컬 실행과 `xgen-agent-runtime`의 LocalHost는 XGEN 서버의 설정, 메모리, workspace, DB, MinIO, sandbox, 조직 기능을 원격 호출했다. 동일 에이전트를 서버와 PC에서 유지하기 위해 기능을 중복 구현했고, 최신 Connector v3와 Runtime v4에서 이 로컬 경로가 제거됐다.

XGENy는 서버 없이 동작하는 로컬 하네스여야 한다. 동시에 향후 XGEN의 조직 능력·정책·감사·원격 실행을 사용할 수 있어야 한다.

## Decision

XGENy의 domain/runtime/local-store 코어는 XGEN, Connector, XGEN DB/MinIO/Kubernetes 및 XGEN Python 패키지에 의존하지 않는다.

XGEN 연동은 다음 조건의 선택형 `xgen-remote` adapter로만 제공한다.

- 공개 버전 계약과 HTTPS/MCP/A2A 사용
- DB/MinIO 내부 식별자 대신 `ArtifactRef`, `MemoryRecord` 사용
- 서버 미설정·오프라인 시 코어 기동과 로컬 작업에 영향 없음
- legacy XGEN mapping은 XGEN 서버 측 Compatibility Shell 소유

adapter는 Cargo default feature에서 제외한다. 독립 빌드·테스트는 adapter와 네트워크 없이 실행돼야 한다.

기존 XGEN runtime은 코드 의존성이 아니라 의미·회귀 fixture·conformance oracle로 사용한다.

## Consequences

### Positive

- 단일 설치와 오프라인 실행을 보장한다.
- XGEN 인프라 변경이 XGENy 로컬 저장·실행을 깨지 않는다.
- Rust 네이티브 패키징과 명확한 제품 경계를 얻는다.
- XGEN도 새 계약을 단계적으로 수용할 수 있다.

### Negative

- Python runtime 코드를 그대로 재사용할 수 없다.
- provider/tool/event 동등성을 contract fixture로 별도 유지해야 한다.
- XGEN 서버에 Compatibility Shell 작업이 필요하다.

## Rejected alternatives

- Connector의 삭제된 sidecar 복구
- `xgen-agent-runtime` 전체를 XGENy 필수 의존성으로 설치
- XGENy가 XGEN DB와 MinIO에 직접 연결
- XGEN legacy API를 XGENy core에 영구 구현
