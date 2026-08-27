# XGENy CLI

Local-first XGEN coding agent CLI and harness.

> 현재 상태: 구현 전 아키텍처·제품 범위를 정리하는 단계입니다. 아직 설치하거나 실행할 수 있는 CLI는 없습니다.

## 제품 원칙

- 사용자는 `xgeny` 하나만 설치합니다.
- SQLite, PostgreSQL, Qdrant, Docker, Kubernetes, Python, Node.js 또는 별도 데몬을 기본 설치 조건으로 요구하지 않습니다.
- 초기 세션은 JSONL, 사용자·프로젝트 메모리는 Markdown으로 저장합니다.
- XGEN 서버 없이도 기본 로컬 코딩 작업과 세션 재개가 가능해야 합니다.
- XGENy 코어는 XGEN, Connector, PostgreSQL, MinIO, Kubernetes 또는 XGEN Python 패키지에 의존하지 않습니다.
- 기존 XGEN 런타임은 실행 의존성이 아니라 의미 계약·회귀 테스트·검증 자산으로 활용합니다.
- XGEN 연동은 코어 밖의 버전 계약과 선택형 원격 어댑터로만 수행합니다.
- 서버 연결은 실행 시 선택이지만, XGEN 호환성과 통합 E2E는 제품 출시 필수 조건입니다.
- 새 의미 계약은 XGENy의 미래 구조를 기준으로 정의하고, XGEN은 기존 경로를 보존하는 호환 계층을 통해 단계적으로 수용합니다.

## Architecture

- [XGENy 독립 코어와 XGEN 무중단 진화 정본](docs/architecture/2026-08-27-xgeny-xgen-evolution.md)
- [ADR-0001: XGEN 비의존 독립 코어](docs/adr/0001-xgen-independent-core.md)
- [ADR-0002: WorkGraph 권위와 동기화](docs/adr/0002-workgraph-authority-and-sync.md)
- [ADR-0003: 표준 프로토콜 투영과 XGEN 호환 셸](docs/adr/0003-protocol-projections-and-compatibility-shell.md)

## Research

- [XGENy 로컬 CLI 하네스 조사 메모](docs/research/2026-08-26-xgeny-local-cli-harness.md)

조사 메모에는 Qwen Code, Claude Code, Codex, Gemini CLI, Goose, OpenCode, Aider와 XGEN 내부 하네스·런타임·메모리·샌드박스 모듈의 비교 및 MVP 범위가 기록돼 있습니다. 2026-08-27 이후의 제품 경계와 XGEN 연계 결정은 위 아키텍처 정본이 우선합니다.
