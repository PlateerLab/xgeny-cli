# XGENy CLI

Local-first general-purpose agent CLI and harness.

> 현재 상태: 프로토콜 v0.1 검증, dependency DAG와 Receipt-gated 재개 frontier를 갖춘 model-free WorkGraph, embedded SQLite schema 8 store, effect 실행·복구 조정기, 결정론적 Capability Registry·Router, I/O 없는 Permission Broker, secret-free invocation material 복구, Direct Executor와 Core-owned Execution Receipt 검증 경계, 장기 Run용 VerifiedRunIndex를 실행할 수 있습니다. Provider-neutral bounded AgentLoop는 비신뢰 `PlanProposal`을 검증해 다중 Step DAG와 reconstructable input sidecar를 원자 commit하고 재시작 뒤 한 frontier action씩 이어갑니다. Model call은 provider 호출 전에 보수적인 possible-send slot을 journal에 예약하며 accepted/rejected/Unknown을 재시작 뒤 복원하고, 불확정 호출을 자동 재시도하지 않습니다. 별도 `xgeny-provider-openai` leaf adapter는 OpenAI-compatible Chat Completions의 단일 요청과 strict structured proposal을 이 lifecycle에 연결합니다. 내부 WorkGraph는 planned `ReadOnly`/`Idempotent`/`NonIdempotent`, Core Receipt v2의 bounded Artifact commitment와 event-anchored typed `ToolOutputRecord`를 지원하며, generation-checked `PlanningContext v3`는 Step의 plan journal 순서와 passed Receipt에 결합된 exact ToolOutput의 관찰 순서를 보존해 다음 model turn에 전달합니다. NonIdempotent durable output은 결과만 보존하고 Started 뒤 불확정 effect의 no-replay 의미는 유지합니다. Public `xgeny run/resume`은 기존 exact `--allow-file` mode를 보존하면서, 명시적 `--allow-dir` mode에서 네 read Capability와 `write-atomic`·`apply-patch`로 workspace를 탐색·수정합니다. Host가 등록한 executable은 별도 승인 뒤 shell 없이 bounded cwd·argv·환경·timeout·출력 제한으로 실행하며 결과를 durable하게 보존하고 불확정 실행을 자동 반복하지 않습니다. Dynamic path/query/write/process material은 Run 전용 embedded `materials.sqlite3`에 digest-bound recipe로 저장되어 approval pause 뒤 다른 process에서도 복원됩니다. 모델 프로필은 일반 설정과 OS 보안 credential 저장소를 분리하고 catalog와 실제 strict-JSON inference를 모두 검증합니다. Bare `xgeny`는 이 경계 위에서 durable progress를 스트리밍하고 model/read/write/execute를 각각 묻는 가벼운 REPL을 시작합니다. Network tool과 XGEN compatibility adapter는 후속 범위입니다.

## 설치와 첫 모델 연결

`v0.1.0-rc.3` release artifact가 게시되면 Linux x86-64/ARM64, macOS Intel/Apple Silicon, Windows
x86-64용 동일 native binary를 GitHub checksum installer와 npm 두 경로로 제공합니다. Native installer는
Rust, Node.js, Python, SQLite 실행 파일이나 XGENy state용 별도 daemon을 요구하지 않습니다. npm 경로만
얇은 launcher를 위해 Node.js 22.14 이상이 필요하며 install script, GitHub 추가 download와 native
compile은 수행하지 않습니다. 사용할 model endpoint는 로컬 또는 원격에 별도로 실행 중이어야 합니다.
후보 version PR의 merge만으로 artifact가 생기지는 않으며 GitHub/npm에 해당 version이 게시된 뒤 설치
명령을 사용해야 합니다.

Node.js가 이미 있다면 exact RC를 npm으로 설치할 수 있습니다. `--omit=optional`은 native package를
제거하므로 사용하지 않습니다.

```bash
npm install --global @xgen/cli@0.1.0-rc.3
xgeny --version
```

Node.js 없는 native 설치와 prerelease exact-tag 검증은 아래와 같습니다.

Linux/macOS:

```bash
installer=$(mktemp "${TMPDIR:-/tmp}/xgeny-installer.XXXXXX")
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo "$installer" \
  https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0-rc.3/xgeny-installer.sh
sh "$installer" --version v0.1.0-rc.3
rm -f "$installer"
export PATH="$HOME/.local/bin:$PATH"
```

Windows PowerShell:

```powershell
$installer = Join-Path ([System.IO.Path]::GetTempPath()) "xgeny-installer-$([Guid]::NewGuid().ToString('N')).ps1"
curl.exe -q --proto "=https" --proto-redir "=https" --tlsv1.2 --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo $installer "https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0-rc.3/xgeny-installer.ps1"
if ($LASTEXITCODE -ne 0) { throw "XGENy installer download failed" }
& $installer -Version v0.1.0-rc.3
Remove-Item -LiteralPath $installer
$env:Path = "$env:LOCALAPPDATA\XGENy\bin;$env:Path"
```

설치 후 `model setup`을 실행하면 URL과 숨김 API key를 입력하고 `/v1/models`에서 model을 선택한다.
저장 전 실제 strict JSON Schema Chat Completions까지 검증한다. API key는 일반 config나 SQLite에 저장하지
않고 macOS Keychain, Windows Credential Manager 또는 Linux Secret Service에만 저장한다.

```bash
xgeny model setup
xgeny model list
xgeny model check --compatibility

# 프로젝트 디렉터리에서 대화형 세션 시작
xgeny

xgeny run \
  --allow-file README.md \
  --allow-remote-model-egress \
  --allow-read \
  'README를 읽고 핵심을 요약해줘.'
```

RC3의 workspace mode는 다음처럼 프로젝트 root와 실행 가능한 개발 도구를 명시적으로 허용한다.

```bash
xgeny run \
  --workspace . \
  --allow-dir . \
  --allow-executable cargo="$(command -v cargo)" \
  --allow-remote-model-egress \
  --allow-read \
  --allow-write \
  --allow-execute \
  '프로젝트 구조를 확인하고 필요한 파일을 수정한 뒤 테스트해줘.'
```

Process는 shell 없이 실행되지만 별도 `--allow-execute` 전에는
`execute_approval_required`로 pause한다. 허용한 executable과 하위 process는 사용자 OS 권한으로 project
code를 실행하므로 이는 sandbox가 아니다. 승인·재개 예시는 [시작하기](docs/getting-started.md)를 따른다.

RC3의 포함 범위와 알려진 한계는 [Developer Preview RC3 후보](docs/development/rc3-release-candidate.md)에
정리되어 있다.

대화형 모드는 현재 디렉터리 전체를 workspace catalog로 열되 실제 read/write/execute와 model egress를
각각 별도로 승인한다. `/model`, `/status`, `/permissions`, `/resume`, `/clear`, `/exit`을 지원하며 줄 끝
`\`로 여러 줄 goal을 입력한다. 직전 durable completion만 다음 goal의 비신뢰 session context로 이어지고
`/clear`가 이를 끊는다. Model의 strict structured proposal을 그대로 화면에 흘리지 않고, 검증된 최종
summary 전에는 model call·plan commit·effect·verification 같은 redacted durable progress만 출력한다.

`model check` 기본형은 선택 profile의 endpoint에 `GET /v1/models` 하나만 보내 기존 자동화 계약을
유지한다. `--compatibility`를 지정하면 strict structured output POST를 한 번 더 보낸다. `model setup`은
두 검증을 모두 통과한 뒤에만 profile을 활성화하며 workspace·Run state는 만들지 않는다.

`--workspace` 기본값은 현재 디렉터리다. Headless/CI에서는 원격 HTTPS key를
`XGENY_OPENAI_API_KEY` 또는 `--token-stdin`으로 현재 invocation에만 전달할 수 있다. OS 보안 저장소가
없는 Linux server에서 평문 파일로 자동 fallback하지 않는다. 현재 호환 경계는 Chat Completions,
strict JSON Schema response와 응답의 exact model ID를 지원하는 서버다. 설치·검증·삭제와 OS별 제약은
[시작하기](docs/getting-started.md)와 [모델 온보딩](docs/development/model-onboarding.md)을 따른다.

Project, Cargo dependency, Rust standard library, Linux musl과 LLVM libunwind의 배포 고지는
binary에 포함되어 있어 network나 별도 파일 없이 `xgeny licenses`로 확인할 수 있다.

## 제품 원칙

- 사용자는 `xgeny` 하나만 설치합니다.
- SQLite server, PostgreSQL, Qdrant, Docker, Kubernetes, Python, Node.js 또는 별도 데몬을 기본 설치 조건으로 요구하지 않습니다.
- 사용자·프로젝트 메모리는 검토 가능한 Markdown으로 유지합니다. Run의 durable state는 embedded SQLite에 저장하며 검토·이관용 JSONL export를 항상 제공합니다.
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
- [ADR-0021: Durable CompletionOutputRecord와 local store schema 8](docs/adr/0021-durable-completion-output-and-schema-8.md)
- [ADR-0022: Capability-confined filesystem read adapter](docs/adr/0022-capability-confined-filesystem-read-adapter.md)
- [ADR-0023: Public local run/resume prototype](docs/adr/0023-public-local-run-resume-prototype.md)
- [ADR-0024: Workspace filesystem discovery와 durable dynamic material](docs/adr/0024-workspace-filesystem-discovery.md)
- [ADR-0025: Capability-confined atomic write와 idempotent durable output](docs/adr/0025-capability-confined-atomic-write.md)
- [ADR-0026: Strict single-file patch와 공유 atomic commit](docs/adr/0026-strict-single-file-atomic-patch.md)
- [ADR-0027: NonIdempotent durable tool output와 no-replay 경계](docs/adr/0027-non-idempotent-durable-tool-output.md)
- [ADR-0028: shell 없는 로컬 process 실행 경계](docs/adr/0028-shell-free-process-execute.md)
- [ADR-0029: Core-derived action occurrence와 반복 coding loop](docs/adr/0029-core-derived-action-occurrence.md)
- [ADR-0030: Journal chronology를 보존하는 PlanningContext v3](docs/adr/0030-chronological-planning-context-v3.md)
- [ADR-0031: npm은 네이티브 XGENy의 무스크립트 배포 계층](docs/adr/0031-npm-native-distribution.md)
- [ADR-0032: 모델 프로필과 OS 보안 저장소 분리](docs/adr/0032-model-profiles-and-secure-credential-boundary.md)
- [ADR-0033: 대화형 REPL과 durable progress/cancellation 경계](docs/adr/0033-interactive-repl-durable-progress.md)

## 개발 및 검증

[rustup](https://rustup.rs/) 설치 후 저장소 루트에서 실행합니다. 저장소가 Rust 1.98.0 toolchain을 자동으로 선택합니다.

```bash
cargo test --workspace --locked
cargo run --locked --quiet -p xgeny-cli -- protocol check
```

상세한 로컬·CI 검증 범위는 [Rust 워크스페이스 개발 환경](docs/development/rust-workspace.md)을 참고합니다. Router의 fail-closed filter, ranking과 권한 경계는 [결정론적 Capability Router 기본형](docs/development/deterministic-router.md), 재시작 전 실행 인자 경계는 [Recoverable Invocation Material 기본형](docs/development/recoverable-invocation-material.md), exact adapter 실행 경계는 [Direct Executor 기본형](docs/development/direct-executor.md), 검증과 Receipt 종결은 [Core Verification과 Execution Receipt 기본형](docs/development/execution-receipt.md), 장기 Run 검증 비용은 [Verified Run Index와 장기 Run 검증](docs/development/verified-run-index.md), dependency와 재개 순서는 [Persistent WorkGraph와 재개 frontier](docs/development/persistent-workgraph.md), 계획 수락·예산·재시작 계약은 [Durable Planner와 bounded AgentLoop](docs/development/durable-planner-loop.md), provider 호출 전 예약·불확정 복구 경계는 [Durable model-call lifecycle](docs/development/durable-model-call-lifecycle.md), 첫 실제 모델 연결은 [OpenAI-compatible Provider Adapter](docs/development/openai-compatible-provider.md), ReadOnly와 CLI 조합 기반은 [ReadOnly와 bounded CLI driver 기반](docs/development/read-only-driver-foundation.md), 제품 workspace read 경계는 [Capability-confined filesystem read adapter](docs/development/filesystem-read-adapter.md), 프로젝트 탐색과 dynamic material 재개는 [Workspace filesystem discovery](docs/development/workspace-filesystem-discovery.md), whole-file mutation은 [Workspace atomic write](docs/development/workspace-atomic-write.md), strict small edit는 [Workspace apply patch](docs/development/workspace-apply-patch.md), 공개 명령과 process 재개 절차는 [Public local run/resume prototype](docs/development/public-local-run-resume.md), 실제 OS I/O를 쓰는 비제품 기준은 [Preopened Reference Adapter Conformance](docs/development/reference-adapter-conformance.md), 기능 개발 순서와 테스트 계층·완료 기준은 [XGENy 개발 방법론과 테스트 전략](docs/development/engineering-method.md)을 따릅니다.

Bare 명령의 입력, 승인, 진행 event와 Ctrl+C 의미는 [대화형 REPL](docs/development/interactive-repl.md)을
따릅니다.

## Research

- [XGENy 로컬 CLI 하네스 조사 메모](docs/research/2026-08-26-xgeny-local-cli-harness.md)
- [Durable Agent Runtime 근거 검토](docs/research/2026-08-28-durable-agent-runtime-evidence.md)
- [Durable Agent Runtime 평가 프로토콜](docs/research/2026-08-28-runtime-evaluation-protocol.md)

조사 문서에는 agent harness와 XGEN 내부 자산 비교, durable execution·memory 관련 논문 및 구현 감사,
사전등록형 평가 계획이 기록돼 있습니다. 2026-08-27 이후의 제품 경계와 XGEN 연계 결정은 위
아키텍처 정본이 우선하며, 현재 구현의 보장 범위와 남은 power-loss·filesystem fault gate는 각 ADR과
개발 문서를 기준으로 판단합니다.
