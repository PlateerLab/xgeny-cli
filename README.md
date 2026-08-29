# XGENy CLI

Local-first general-purpose agent CLI and harness.

> 현재 상태: 프로토콜 v0.1 검증, model-free WorkGraph·로컬 저장소 후보, effect 실행·복구 조정기를 실행할 수 있습니다. 모델 루프, 실제 filesystem/process adapter와 CLI 연결은 아직 구현하지 않았습니다.

## 제품 원칙

- 사용자는 `xgeny` 하나만 설치합니다.
- SQLite server, PostgreSQL, Qdrant, Docker, Kubernetes, Python, Node.js 또는 별도 데몬을 기본 설치 조건으로 요구하지 않습니다.
- 사용자·프로젝트 메모리는 검토 가능한 Markdown으로 유지합니다. Run의 물리 저장 형식은 ADR-0008의 3개 OS fault-injection 결과로 확정하며 JSONL export를 항상 제공합니다.
- XGEN 서버 없이도 코딩을 포함한 범용 로컬 작업과 세션 재개가 가능해야 합니다.
- XGENy 코어는 XGEN, Connector, PostgreSQL, MinIO, Kubernetes 또는 XGEN Python 패키지에 의존하지 않습니다.
- 기존 XGEN 런타임은 실행 의존성이 아니라 의미 계약·회귀 테스트·검증 자산으로 활용합니다.
- XGEN 연동은 코어 밖의 버전 계약과 선택형 원격 어댑터로만 수행합니다.
- 서버 연결은 실행 시 선택이지만, XGEN 호환성과 통합 E2E는 제품 출시 필수 조건입니다.
- 새 의미 계약은 XGENy의 미래 구조를 기준으로 정의하고, XGEN은 기존 경로를 보존하는 호환 계층을 통해 단계적으로 수용합니다.

## Architecture

- [XGENy 독립 코어와 XGEN 무중단 진화 정본](docs/architecture/2026-08-27-xgeny-xgen-evolution.md)
- [XGENy Capability Runtime v0.1](docs/architecture/2026-08-28-xgeny-capability-runtime-v01.md)
- [ADR-0001: XGEN 비의존 독립 코어](docs/adr/0001-xgen-independent-core.md)
- [ADR-0002: WorkGraph 권위와 동기화](docs/adr/0002-workgraph-authority-and-sync.md)
- [ADR-0003: 표준 프로토콜 투영과 XGEN 호환 셸](docs/adr/0003-protocol-projections-and-compatibility-shell.md)
- [ADR-0004: Capability Definition과 Instance](docs/adr/0004-capability-definition-and-instance.md)
- [ADR-0005: Permission broker와 critical action](docs/adr/0005-permission-broker-and-critical-actions.md)
- [ADR-0006: 결정론적 router와 실행 mode](docs/adr/0006-deterministic-router-and-execution-modes.md)
- [ADR-0007: Run Journal과 Execution Receipt](docs/adr/0007-run-journal-and-execution-receipt.md)
- [ADR-0008: Durable Run Store와 외부 effect 복구](docs/adr/0008-durable-run-store-and-effect-recovery.md)

## 개발 및 검증

[rustup](https://rustup.rs/) 설치 후 저장소 루트에서 실행합니다. 저장소가 Rust 1.98.0 toolchain을 자동으로 선택합니다.

```bash
cargo test --workspace --locked
cargo run --locked --quiet -p xgeny-cli -- protocol check
```

상세한 로컬·CI 검증 범위는 [Rust 워크스페이스 개발 환경](docs/development/rust-workspace.md)을 참고합니다. 기능 개발 순서, 테스트 계층과 완료 기준은 [XGENy 개발 방법론과 테스트 전략](docs/development/engineering-method.md)을 따릅니다.

## Research

- [XGENy 로컬 CLI 하네스 조사 메모](docs/research/2026-08-26-xgeny-local-cli-harness.md)
- [Durable Agent Runtime 근거 검토](docs/research/2026-08-28-durable-agent-runtime-evidence.md)
- [Durable Agent Runtime 평가 프로토콜](docs/research/2026-08-28-runtime-evaluation-protocol.md)

조사 문서에는 agent harness와 XGEN 내부 자산 비교, durable execution·memory 관련 논문 및 구현 감사, 사전등록형 평가 계획이 기록돼 있습니다. 2026-08-27 이후의 제품 경계와 XGEN 연계 결정은 위 아키텍처 정본이 우선하며, 물리 저장소와 effect 복구는 ADR-0008의 연구 gate를 통과하기 전까지 제안 상태입니다.
