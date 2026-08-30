# XGENy CLI

Local-first general-purpose agent CLI and harness.

> 현재 상태: 프로토콜 v0.1 검증, dependency DAG와 Receipt-gated 재개 frontier를 갖춘 model-free WorkGraph, embedded SQLite schema 7 store, effect 실행·복구 조정기, 결정론적 Capability Registry·Router, I/O 없는 Permission Broker, secret-free invocation material 복구, Direct Executor와 Core-owned Execution Receipt 검증 경계, 장기 Run용 VerifiedRunIndex를 실행할 수 있습니다. Provider-neutral bounded AgentLoop는 비신뢰 `PlanProposal`을 검증해 다중 Step DAG와 reconstructable input sidecar를 원자 commit하고 재시작 뒤 한 frontier action씩 이어갑니다. Model call은 provider 호출 전에 보수적인 possible-send slot을 journal에 예약하며 accepted/rejected/Unknown을 재시작 뒤 복원하고, 불확정 호출을 자동 재시도하지 않습니다. 별도 `xgeny-provider-openai` leaf adapter는 OpenAI-compatible Chat Completions의 단일 요청과 strict structured proposal을 이 lifecycle에 연결하며, `go50902`의 Qwen3.8-27B engineering smoke로 Plan 수락까지 검증했습니다. 내부 WorkGraph는 planned `ReadOnly`, Core Receipt v2의 bounded Artifact commitment와 event-anchored typed `ToolOutputRecord`를 지원하며, `xgeny-cli` library의 bounded driver가 계획·승인·실행·검증을 조합합니다. Generation-checked `PlanningContext v2`는 SQLite 재시작 뒤 passed Receipt에 결합된 exact tool output을 다음 model turn과 OpenAI-compatible 요청에 전달합니다. Legacy/unplanned direct ReadOnly, 실제 filesystem/process 제품 adapter, durable completion summary 복원, 승인 UI와 사용자용 CLI `run` 연결은 아직 구현하지 않았습니다.

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
- [ADR-0009: Single Orchestrator와 외부 Harness 통합](docs/adr/0009-single-orchestrator-and-external-harness-integration.md)
- [ADR-0010: 복구 가능한 Invocation Material](docs/adr/0010-recoverable-invocation-material.md)
- [ADR-0011: Direct Executor와 exact adapter dispatch](docs/adr/0011-direct-executor-and-exact-adapter-dispatch.md)
- [ADR-0012: Core verification과 Execution Receipt 원자 확정](docs/adr/0012-core-verification-and-execution-receipt.md)
- [ADR-0013: Verified Run Index와 bounded history 검증](docs/adr/0013-verified-run-index-and-bounded-history-validation.md)
- [ADR-0014: Persistent WorkGraph dependency와 Receipt-gated frontier](docs/adr/0014-persistent-workgraph-frontier-and-resume.md)
- [ADR-0015: Durable planner 계약과 bounded AgentLoop](docs/adr/0015-durable-planner-contract-and-bounded-agent-loop.md)
- [ADR-0016: Durable model-call lifecycle와 possible-send budget](docs/adr/0016-durable-model-call-lifecycle.md)
- [ADR-0017: OpenAI-compatible 단일 요청 Provider 경계](docs/adr/0017-openai-compatible-provider-adapter.md)
- [ADR-0018: ReadOnly 의미, Artifact Receipt와 bounded CLI driver 기반](docs/adr/0018-read-only-artifact-and-cli-driver-foundation.md)
- [ADR-0019: Durable ToolOutputRecord와 local store schema 7](docs/adr/0019-durable-tool-output-and-schema-7.md)
- [ADR-0020: Generation-checked PlanningContext v2](docs/adr/0020-generation-checked-planning-context-v2.md)

## 개발 및 검증

[rustup](https://rustup.rs/) 설치 후 저장소 루트에서 실행합니다. 저장소가 Rust 1.98.0 toolchain을 자동으로 선택합니다.

```bash
cargo test --workspace --locked
cargo run --locked --quiet -p xgeny-cli -- protocol check
```

상세한 로컬·CI 검증 범위는 [Rust 워크스페이스 개발 환경](docs/development/rust-workspace.md)을 참고합니다. Router의 fail-closed filter, ranking과 권한 경계는 [결정론적 Capability Router 기본형](docs/development/deterministic-router.md), 재시작 전 실행 인자 경계는 [Recoverable Invocation Material 기본형](docs/development/recoverable-invocation-material.md), exact adapter 실행 경계는 [Direct Executor 기본형](docs/development/direct-executor.md), 검증과 Receipt 종결은 [Core Verification과 Execution Receipt 기본형](docs/development/execution-receipt.md), 장기 Run 검증 비용은 [Verified Run Index와 장기 Run 검증](docs/development/verified-run-index.md), dependency와 재개 순서는 [Persistent WorkGraph와 재개 frontier](docs/development/persistent-workgraph.md), 계획 수락·예산·재시작 계약은 [Durable Planner와 bounded AgentLoop](docs/development/durable-planner-loop.md), provider 호출 전 예약·불확정 복구 경계는 [Durable model-call lifecycle](docs/development/durable-model-call-lifecycle.md), 첫 실제 모델 연결은 [OpenAI-compatible Provider Adapter](docs/development/openai-compatible-provider.md), ReadOnly와 CLI 조합 기반은 [ReadOnly와 bounded CLI driver 기반](docs/development/read-only-driver-foundation.md), 실제 OS I/O를 쓰는 비제품 기준은 [Preopened Reference Adapter Conformance](docs/development/reference-adapter-conformance.md), 기능 개발 순서와 테스트 계층·완료 기준은 [XGENy 개발 방법론과 테스트 전략](docs/development/engineering-method.md)을 따릅니다.

## Research

- [XGENy 로컬 CLI 하네스 조사 메모](docs/research/2026-08-26-xgeny-local-cli-harness.md)
- [Durable Agent Runtime 근거 검토](docs/research/2026-08-28-durable-agent-runtime-evidence.md)
- [Durable Agent Runtime 평가 프로토콜](docs/research/2026-08-28-runtime-evaluation-protocol.md)

조사 문서에는 agent harness와 XGEN 내부 자산 비교, durable execution·memory 관련 논문 및 구현 감사, 사전등록형 평가 계획이 기록돼 있습니다. 2026-08-27 이후의 제품 경계와 XGEN 연계 결정은 위 아키텍처 정본이 우선하며, 물리 저장소와 effect 복구는 ADR-0008의 연구 gate를 통과하기 전까지 제안 상태입니다.
