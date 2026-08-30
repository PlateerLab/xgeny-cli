# ADR-0018: ReadOnly 의미, Artifact Receipt와 bounded CLI driver 기반

- 상태: 제안 — filesystem 제품 adapter 전의 core/driver 기반 구현
- 기준일: 2026-08-30
- 적용 범위: WorkGraph ReadOnly 의미, Core Receipt v2, `xgeny-cli` library driver
- 공개 protocol v0.1 schema 변경: 없음
- local store schema: 6 유지

## 문맥

ADR-0017은 실제 OpenAI-compatible 모델이 durable reservation 하나에 proposal 요청을 한 번만 보내고 `PlanAccepted`까지 도달하는 경계를 닫았다. 그 다음 단계에서 곧바로 사용자용 `xgeny run`과 파일 읽기를 노출하면 다음 세 가지를 사실과 다르게 주장하게 된다.

1. 공개 protocol의 `read_only`가 내부 WorkGraph에서 별도 의미가 아니라 거부되고 있었다.
2. Adapter와 verifier는 digest만 전달하므로 파일 내용이 다음 planning turn으로 전달되지 않는다.
3. Completion candidate도 summary 원문이 아니라 digest만 durable state에 남으므로 재시작 뒤 CLI가 최종 답변을 복원할 수 없다.

따라서 이번 slice는 제품 filesystem adapter가 아니라, 의미 축약 없이 그 adapter를 수용할 core 계약과 기존 실행 구성요소를 한 방향으로 조합하는 driver를 먼저 만든다.

## 결정

### 1. ReadOnly는 Idempotent의 별칭이 아니다

내부 `xgeny-workgraph::EffectClass`에 `ReadOnly`를 추가하고 wire spelling을 `read_only`로 고정한다. 계획 binding은 effectful local sync profile과 별도로 `LocalSyncReadOnlyV1`을 사용한다.

| 계획 profile | 허용 effect | idempotency key | sink guarantee |
|---|---|---|---|
| `LocalSyncOnceV1` | 기존 effectful 의미 | non-empty | 기존 규칙 |
| `LocalSyncReadOnlyV1` | `ReadOnly`만 | `None` | `None` |

Reducer는 profile/effect/key/sink/local placement/platform을 authorization budget 소비 전에 교차검증한다. Accepted planned-invocation Admission은 Definition `ReadOnly`를 같은 내부 variant로 보존하고 key를 발행하지 않는다. ReadOnly Definition은 `idempotencyKeySupported`를 요구하지 않는다. Legacy `StepPlanned`의 unplanned/direct ReadOnly는 이 PR에서 열지 않고 resolver 전에 고정 오류로 닫는다. 읽기도 accepted tool-call budget에는 한 번으로 계산된다.

현재 semantic action과 effect/grant identity가 Run+normalized action에 결합되므로 같은 Capability와 normalized argument의 ReadOnly 작업은 한 Run에서 한 번만 수락된다. 파일 변경 후 같은 경로를 재조회하려면 occurrence/generation identity가 필요하다. 이를 path나 Step ID를 임의로 섞어 우회하지 않고 output durability 설계에서 먼저 결정한다.

ReadOnly라고 해서 crash 뒤 무조건 안전한 자동 재실행을 뜻하지 않는다. 현재 Direct Executor의 `Executing` 복구는 기존처럼 Unknown/manual 규칙을 유지한다. 실제 filesystem adapter에서 handle-relative 재읽기와 output sidecar 원자성을 설계할 때 ReadOnly 전용 recovery event를 별도 결정한다.

### 2. Core Receipt v1 의미를 바꾸지 않는다

`xgeny.core-receipt/v1`은 Artifact가 비어 있는 기존 의미로 영구 유지한다. ReadOnly Admission은 별도 `xgeny.core-receipt/v2` provenance를 발행한다.

v2 verifier report는 raw output 대신 다음 bounded descriptor만 제출할 수 있다.

- schema-compatible artifact identifier
- optional bounded name
- media type
- byte size
- canonical SHA-256 digest

고정 상한은 artifact 8개, 개별 1 MiB, 합계 4 MiB다. Identifier 중복과 size overflow는 fixed error로 닫는다. Verifier는 Run ID, Step ID, Receipt ID, extensions 또는 required extensions를 제출할 수 없다. Core가 Receipt ID를 계산한 뒤 exact provenance와 빈 extension을 붙인다.

Descriptor의 identifier, name, media type, size와 digest는 Receipt/SQLite에 감사 가능한 metadata로 영속된다. 따라서 trusted verifier는 host-fixed logical 값만 사용하고 raw path, 파일 내용, credential 또는 secret을 넣지 않는다. SHA-256 digest도 confidentiality를 제공하지 않는다.

```text
trusted verifier
  output digest + bounded artifact descriptors
                     |
                     v
XGENy Core
  receipt ID + Run/Step/Receipt provenance
                     |
                     v
RunStore atomic Receipt commit
```

Store는 cold audit와 append에서 다음을 다시 검사한다.

- v1은 Artifact 0개이며 ReadOnly가 아니다.
- v2는 ReadOnly이고 Artifact가 1개 이상이다.
- count/individual/aggregate bound와 artifact ID uniqueness
- artifact provenance의 exact Run/Step/Receipt binding
- artifact extension과 required extension이 비어 있음

Receipt `outputDigest`와 각 Artifact digest는 서로 다른 commitment가 될 수 있다. 이번 기반은 둘을 같다고 일반화하지 않는다. Capability별 output 계약이 관계를 정의하고 향후 ToolOutputRecord가 이를 검증해야 한다.

### 3. Driver는 기존 권위를 조합할 뿐 새 권위를 만들지 않는다

`xgeny-cli`에 library target과 bounded `RunDriver::drive_until_pause`를 추가하되 binary command로 노출하지 않는다.

```text
RunDriver
  |
  +-- AgentLoop::tick
        |
        +-- Admit
        |    PlannedRoutePort
        |      -> InvocationMaterialRecovery
        |      -> ApprovalPort
        |      -> InvocationAdmission
        |
        +-- DriveEffect
        |    InvocationMaterialRecovery
        |      -> DirectExecutor
        |
        +-- Verify
             VerificationRunner
```

`PlannedRoutePort`와 `ApprovalPort`는 host-owned typed boundary다. Driver가 route, trust, data boundary 또는 allow를 추측하지 않는다. Pending/deny/blocked/Unknown/model recovery/quiescence는 caller에게 반환하고, unresolved 상태를 자동 폐기하거나 retry하지 않는다. 한 호출의 AgentLoop tick 수는 `NonZeroU32` hard bound로 제한한다. Configuration, lifecycle setup과 Plan acceptance도 tick을 소비하므로 이 값은 외부 action 수가 아니다.

Planner, materializer, material provider, resolver, adapter와 verifier는 기존 port를 그대로 사용한다. OpenAI-compatible provider나 XGEN을 import하지 않으므로 같은 driver에 fake planner, local model, Claude/Codex 호환 외부 harness 또는 XGEN gateway adapter를 선택적으로 조합할 수 있다.

### 4. 이번 SQLite E2E는 core/driver 실증이지 제품 파일 읽기 실증이 아니다

Hermetic E2E는 test-only fake planner와 memory recipe provider를 사용해 다음을 검증한다.

```text
SQLite Run
  -> fake PlanProposal(ReadOnly)
  -> normalized reconstructable input
  -> exact route + approval pending/deny 무효과 확인
  -> request-bound local policy
  -> IntentCommitted SQLite reopen
  -> fake read-only adapter 1회
  -> Validating SQLite reopen
  -> verifier artifact descriptor
  -> Core-bound v2 Artifact Receipt
  -> CompletionCandidate
  -> SQLite reopen
  -> planner/adapter/verifier 추가 호출 0회
```

이 테스트의 adapter는 파일 내용을 반환하지 않고 fake digest만 관찰한다. Recipe map도 test process 안에만 존재한다. 따라서 테스트 이름과 문서에서 filesystem E2E, model continuity 또는 durable raw output이라고 부르지 않는다.

### 5. SQLite schema 6을 유지한다

이번 변경은 새 physical table이나 column을 추가하지 않는다. `ReadOnly` enum, planned profile과 Receipt provenance v2는 새 Run에서만 생성되는 internal serialized 의미다. 기존 schema 3~6 journal/Receipt bytes를 재작성하지 않고 기존 v1 Receipt를 그대로 검증한다.

구버전 binary는 새 enum/profile을 읽지 못해 fail-closed한다. 아직 public CLI가 새 Run을 생성하지 않으므로 schema 6을 유지한다. 제품 adapter가 typed output sidecar를 실제 사용자 DB에 쓰기 시작하는 다음 slice에서는 schema 7 migration과 semantic compatibility fence를 함께 도입한다.

## 제품 filesystem 경로의 후속 단계

Public `xgeny run`을 열기 전 필요한 계약은 한 PR에 묶지 않고 다음 검증 단위로 나눈다.

### A. output durability

1. effect outcome event와 원자 commit되는 `ToolOutputRecord` sidecar 및 schema 7 migration
2. verification snapshot과 planning snapshot에서 state/output 세대 일치
3. Receipt-completed output만 포함하는 `xgeny.planning-context/v2`
4. raw completion summary를 digest와 함께 원자 보존하는 `CompletionOutputRecord`
5. 같은 ReadOnly semantic action의 재관찰을 위한 occurrence/generation identity

### B. filesystem confinement

1. `xgeny-adapter-filesystem` leaf crate와 한 개의 bounded UTF-8 read contract
2. trusted composition root가 한 번 여는 workspace directory capability
3. descriptor-relative component traversal, intermediate/leaf no-follow와 regular-file 검사
4. empty/absolute/parent/NUL/control/backslash/colon·ADS/Windows device name 거부
5. max+1 streaming read, strict UTF-8, byte/canonical output 상한
6. filesystem read 승인과 model egress 승인의 분리
7. Linux symlink, macOS case/Unicode alias, Windows junction/reparse/ADS 테스트

### C. public composition과 배포

1. output durability와 filesystem adapter를 실제 planner 구성에 연결
2. 사용자용 run/resume 표현과 stable exit code
3. packaged binary의 Linux/macOS/Windows install/run smoke
4. 그 뒤 opt-in live provider/tool execution smoke

Filesystem confinement 후보는 capability-oriented `Dir` API다. Bytecode Alliance의 공식 설명에 따르면 `cap-std`의 `Dir`은 absolute path, `..`, symlink escape를 제한하고 Linux에서 `openat2`를 활용하지만 untrusted Rust code 전체의 sandbox는 아니다. 제품 구현은 이 보장만 인용하지 않고 component별 no-follow 정책과 OS 회귀 테스트를 추가해야 한다. 참고: [cap-std README](https://github.com/bytecodealliance/cap-std/blob/main/README.md), [cap_std::fs::Dir](https://docs.rs/cap-std/latest/cap_std/fs/struct.Dir.html).

Hard link, mount/bind mount, FUSE/network filesystem, hostile same-UID process, file-ID-stable authorization과 filesystem timeout은 첫 read slice의 완전 격리 보장 밖으로 명시한다.

## 회귀 gate

- `ReadOnly`/`LocalSyncReadOnlyV1` wire value 고정
- profile/effect/key downgrade를 authorization 소비 전에 거부
- effectful 기존 profile의 key 요구 유지
- ReadOnly Definition의 idempotency support 비요구
- Receipt v1 Artifact 주입 거부
- Receipt v2 count/size/unique/provenance/extension 재검증
- verifier descriptor error와 approval Debug의 raw argument/tool output 비노출
- fake planner driver의 approval pending/deny 무효과와 exact route/admission/execution/verification 순서
- IntentCommitted·Validating·Completed 각각의 SQLite reopen과 duplicate planner/adapter/verifier 호출 0회
- POSIX live DB/WAL/SHM 및 3개 OS clean-close artifact에서 raw/canonical proposal argument 비노출
- 기존 schema 3~6 migration과 v1 Receipt 전체 회귀
- Linux, macOS, Windows workspace suite

## 비목표

- 사용자용 `xgeny run` command
- legacy/unplanned direct ReadOnly Admission
- 실제 workspace filesystem open/read
- typed output body와 Artifact content store
- output을 다음 planning turn에 전달하는 context v2
- 사용자에게 보여 줄 completion 원문 durable 저장
- ReadOnly crash 자동 retry
- 같은 Run에서 동일 ReadOnly semantic action 반복 관찰
- write/process/network/MCP/XGEN adapter
- live `go50902` tool execution
- package installer/signing

## 결과

XGENy는 planned ReadOnly를 effectful idempotency로 위장하지 않고, 기존 v1 Receipt 의미를 깨지 않으면서 artifact-bearing Receipt를 만들 수 있다. CLI composition은 한 bounded driver로 정리됐지만 제품 파일 읽기와 모델 연속성은 아직 열지 않는다. 다음 구현은 먼저 schema 7 output/completion durability를 닫고, 그다음 filesystem confinement, public composition 순으로 진행한다.
