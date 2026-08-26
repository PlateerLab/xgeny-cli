# XGENy CLI

Local-first XGEN coding agent CLI and harness.

> 현재 상태: 구현 전 아키텍처·제품 범위를 정리하는 단계입니다. 아직 설치하거나 실행할 수 있는 CLI는 없습니다.

## 제품 원칙

- 사용자는 `xgeny` 하나만 설치합니다.
- SQLite, PostgreSQL, Qdrant, Docker, Kubernetes, Python, Node.js 또는 별도 데몬을 기본 설치 조건으로 요구하지 않습니다.
- 초기 세션은 JSONL, 사용자·프로젝트 메모리는 Markdown으로 저장합니다.
- XGEN 서버 없이도 기본 로컬 코딩 작업과 세션 재개가 가능해야 합니다.
- XGEN의 기존 에이전트 코어를 정리해 공유하고, 별도의 네 번째 하네스 루프를 만들지 않습니다.
- 서버 연계, 원격 샌드박스, 고급 메모리는 선택형 어댑터로 추가합니다.

## Research

- [XGENy 로컬 CLI 하네스 조사 메모](docs/research/2026-08-26-xgeny-local-cli-harness.md)

조사 메모에는 Qwen Code, Claude Code, Codex, Gemini CLI, Goose, OpenCode, Aider와 XGEN 내부 하네스·런타임·메모리·샌드박스 모듈의 비교 및 MVP 범위가 기록돼 있습니다.
