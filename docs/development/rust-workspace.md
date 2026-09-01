# Rust 워크스페이스 개발 환경

이 문서는 XGENy 코어와 프로토콜 검증 기반을 같은 조건으로 재현하기 위한 최소 절차다.

## 범위

현재 워크스페이스는 다음 열한 개 crate로 구성된다.

```text
crates/
  xgeny-adapter-reference/ publish하지 않는 preopened-handle conformance 기준
  xgeny-adapter-filesystem/ capability-confined workspace read/list/stat/search/write/patch 제품 adapter
  xgeny-adapter-process/ shell-free bounded process execute 제품 adapter
  xgeny-domain/       I/O 없는 정본 Rust 프로토콜 타입
  xgeny-protocol/     bundled/offline schema·fixture·digest 검증
  xgeny-workgraph/    model-free RunEvent 상태 전이·재생 실험
  xgeny-local-store/  메모리 참조 구현과 embedded SQLite 후보
  xgeny-policy/       concrete resource 해석 경계와 순수 정책 교집합
  xgeny-runtime/      durable effect 실행·복구와 Capability Registry·Router 기본형
  xgeny-provider-openai/ OpenAI-compatible 단일 요청 planner leaf adapter
  xgeny-cli/          protocol check와 public local run/resume를 제공하는 실행 파일·driver library
```

`xgeny-workgraph`, `xgeny-local-store`, `xgeny-runtime`의 durable effect, invocation material, Direct Executor, Core verification, VerifiedRunIndex, Persistent dependency frontier, bounded AgentLoop와 model-call lifecycle 계약은 ADR-0008·0010·0011·0012·0013·0014·0015·0016 연구 gate를 위한 내부 실험이다. ADR-0018은 내부 ReadOnly profile, Artifact-bearing Core Receipt v2와 기존 구성요소를 조합하는 bounded CLI library driver를 추가했다. ADR-0019는 event-anchored `ToolOutputRecord`와 SQLite physical schema 7을 추가했고, ADR-0020·0021은 generation-checked planning context와 durable completion을 schema 8에 연결했다. Registry와 Router는 기존 Definition/Instance를 사용한다. `xgeny-policy`의 Allow는 provisional이며 runtime Admission이 exact invocation의 one-shot authority를 Run/Step/action/Instance/material에 결합한다. `xgeny-adapter-reference`는 비제품 conformance 기준이다. 제품 `xgeny-adapter-filesystem`은 exact-file `read-text`와 opt-in directory `list-directory`·`stat`·`search-text`·`write-atomic`·`apply-patch`를 제공한다. `xgeny-adapter-process`는 host-catalogued executable을 shell 없이 bounded 실행한다. Public CLI는 기존 `--allow-file` profile을 보존하면서 `--allow-dir` discovery, 별도 `--allow-write` atomic mutation과 별도 `--allow-execute` process 승인을 제공한다. Bare `xgeny` REPL은 이 composition을 재사용해 interactive 승인, durable progress와 재개를 제공한다. Network adapter, MCP, Connector와 XGEN 원격 연동은 아직 구현하지 않는다.

## 준비물

- Git
- [rustup](https://rustup.rs/)
- 소스에서 전체 workspace를 빌드할 때 필요한 플랫폼 C build toolchain

저장소의 `rust-toolchain.toml`이 Rust 1.98.0과 `rustfmt`, `clippy`를 고정한다. `rusqlite`의 `bundled` 기능이 SQLite C source를 Rust build에 함께 링크하므로 소스 빌드에는 Linux C compiler, macOS Command Line Tools 또는 Windows MSVC Build Tools가 필요하다. Public `xgeny run/resume` binary는 이 embedded store를 연결하므로 최종 사용자에게 SQLite 실행 파일, DB server 또는 daemon 설치를 요구하지 않는다. PostgreSQL, MinIO, Docker, Kubernetes, Python, Node.js도 기본 실행 의존성이 아니다.

## 현재 durable slice 검증 범위

- RunEvent의 RFC 8785/SHA-256 hash chain과 I/O 없는 결정론적 replay
- authority epoch와 journal head compare-and-swap을 통한 stale writer 거부
- event, effect intent index, authorization consumption, secret-free material sidecar, projection의 단일 transaction과 각 사이 fault injection
- verification event, complete ExecutionReceipt sidecar와 projection의 단일 transaction 및 Receipt insert fault rollback
- transaction 중간 오류와 자식 process 즉시 종료 후 전량 rollback·재개
- lost acknowledgement 뒤 `effect_unknown` 복원과 비멱등 effect의 blind retry 거부
- Run별 OS file lease를 effect 호출 전체 구간에 유지해 동시 recovery worker 차단
- 실행 직전 ephemeral prepared effect와 durable action·Capability·Definition·Instance binding 일치 검증
- 시작 event commit 이전에는 sink를 호출하지 않고, outcome commit 유실 시 재실행 없이 unknown 복구
- query-capable sink만 read-only reconciliation하고 나머지는 manual 전환
- process 종료 전 실제 counter effect가 발생한 시나리오의 단일 실행·lease 해제·unknown 복원
- durable 실행 attempt 상한과 승인 예산 비중복 소비
- 메모리 참조 저장소와 SQLite 후보의 동일한 journal JSONL 및 complete Receipt JSONL export
- 임의 길이 journal 재생과 event 변조 탐지 property test
- schema 3/4/5/6/7 durable bytes를 보존하는 schema 8 원자 migration과 version 1·2·9+ fail-closed 거부
- 외부 reference crate의 Started 이후 preopened-file I/O, fixed-error redaction과 exact binding
- write·sync·read-back 뒤 outcome 전 child process 종료 + SQLite 재시작에서 adapter execute 0회와 unknown 복원
- `Validating` 재시작의 exact verifier-only 재개, Core Receipt schema/digest/binding 검증과 tamper 탐지
- connection generation과 journal head에 묶인 VerifiedRunIndex, runtime 최소 view와 외부 projection/material/Receipt mutation 감지
- 1,000/10,000-event Run의 warm history rescan 0, 1,001-event/40-Receipt Run의 Receipt별 exact lookup 1회
- immutable dependency DAG, Receipt-bound release와 결정론적 actionable/waiting/blocked frontier
- recovery·reconciliation·verification 우선의 single-orchestrator continuation 순서와 admission/reducer 이중 gate
- 10,000-Step 반복형 traversal, Memory/SQLite frontier parity와 reopen continuity
- Receipt finalization fault/process-exit rollback 중 child 비해제와 atomic retry commit 뒤 child 해제
- 모델 출력과 분리된 `PlanAccepted` DAG, N개 reconstructable input sidecar의 원자 commit과 schema 8 재개
- bounded context·accepted model-turn·planned-step·external-start 예산, 단일 frontier action 우선 AgentLoop와 completion candidate gate
- provider 호출 전 deterministic possible-send reservation, accepted/rejected/Unknown 분리와 불확정 호출 자동 재시도 차단
- model-call reservation·Unknown의 Memory/SQLite parity와 reopen, lifecycle event/projection 및 accepted Plan sidecar fault rollback
- 별도 ReadOnly 계획 profile, key/sink downgrade 차단과 Artifact-bearing Core Receipt v2 provenance
- bounded CLI library driver의 fake plan→exact approval→execute→verify→completion과 SQLite no-call replay
- exact UTF-8 `CompletionOutputRecord`의 event/sidecar/projection 원자 commit, schema 7 legacy 호환과 실제 별도 process no-model-recall replay

세부 실행 순서와 재시작 판정은 [Durable effect 실행·복구 수직 슬라이스](durable-effect-runtime.md), Receipt 종결 경계는 [Core Verification과 Execution Receipt](execution-receipt.md), 장기 Run 검증 비용은 [Verified Run Index와 장기 Run 검증](verified-run-index.md), dependency release와 frontier는 [Persistent WorkGraph와 재개 frontier](persistent-workgraph.md), model 호출 예약과 불확정 복구는 [Durable model-call lifecycle](durable-model-call-lifecycle.md), ReadOnly/driver 기반은 [ReadOnly와 bounded CLI driver 기반](read-only-driver-foundation.md), 외부 adapter 범위는 [Preopened Reference Adapter Conformance](reference-adapter-conformance.md)를 따른다. Reference test는 임시 디렉터리의 전용 일반 파일만 사용하며 path sandbox가 아니다. ADR-0020은 typed output의 SQLite 재시작 후 다음 model turn 전달을 fake planner와 loopback OpenAI-compatible HTTP까지 검증하고, ADR-0021은 exact completion summary의 별도 process 재시작 복원을 검증한다.

Public CLI는 embedded SQLite와 실제 filesystem `read-text` adapter를 사용하며, loopback model E2E와 별도 opt-in live gate를 둔다. Native installer와 게시 target별 process·installer smoke는 release workflow에서 검증한다. 다만 실제 공개 release 설치 결과, power-loss, filesystem 손상, 지원 표보다 오래된 OS, 일반 workspace sandbox와 반복 fault matrix까지 통과했다고 주장하지 않는다.

## 현재 Capability Registry 검증 범위

- Capability ID와 contract version의 exact identity
- 같은 Capability의 여러 contract version 공존
- 전역 Instance ID 중복과 기존 항목 덮어쓰기 차단
- 등록되지 않은 exact Definition version을 가리키는 Instance 차단
- Definition보다 강한 sync/task/cancellation 기능 주장 차단
- 등록 순서와 무관한 Definition·Instance 조회 순서
- platform, health, auth 상태를 실행 가능성으로 해석하지 않고 Router 입력으로 보존

자세한 책임 경계와 제외 범위는 [Capability Registry 기본형](capability-registry.md)을 따른다.

## 현재 Capability Router 검증 범위

- Capability ID와 contract version exact lookup, 호환 version 추측 금지
- concrete target platform과 Instance의 OS·architecture wildcard 비교
- unavailable·unknown health, auth required·expired의 fail-closed 제거
- 명시적 trust·data-boundary 허용 집합과 required feature filter, handler witness가 없는 required extension의 전면 fail-closed
- invalid cost·reliability hint 제거와 signed zero 정규화
- available/degraded, reliability, 명시적 trust·boundary 선호, latency, cost, 사용자 선호, Instance ID 순의 lexicographic ranking
- pin의 hard filter·policy·critical action gate 우회 차단
- Permission Broker의 allow·ask·deny와 누락을 구분한 `Selected`·`InteractionRequired`·`Blocked`
- 후보 등록 순서와 무관한 결과·후보·reason 순서

`Selected`는 실행 위치 결정일 뿐 실행 권한이 아니다. Router는 provisional policy allow를 `Grant`, `PolicyDecision` 또는 `InvocationPlan`으로 바꾸지 않으며 실제 effect를 호출하지 않는다. 자세한 계약과 후속 범위는 [결정론적 Capability Router 기본형](deterministic-router.md)을 따른다.

## 현재 Permission Broker 검증 범위

- schema 검증 뒤에도 모든 resource를 trusted resolver에 통과시키는 불변 경계
- scope별 exact canonical resource identity와 alias 중복 차단
- 필수 host·user profile 및 managed mode의 필수 PolicyLease 계층
- 전체 요청에 대한 정책 교집합과 `deny > ask > allow` 우선순위
- source 순서, resource 순서와 무관한 결정적 결과
- 모델이 만든 reason·metadata를 권한 근거에서 제외
- critical action의 자동 허용 차단과 별도 승인 요구
- lifetime을 크기 순서로 추측하지 않고 각 계층의 명시적 허용 집합으로 확인

현재 CI의 resolver는 I/O 없는 fake다. 실제 path·symlink·process executable·network endpoint canonicalization, reusable/critical Run grant, PolicyLease 만료·취소, 승인 UI와 wire `PolicyDecision` 투영은 구현하지 않았다. 자세한 경계는 [Permission Broker 기본형](permission-broker.md)을 따른다.

## 현재 Invocation Admission 검증 범위

- exact argument와 Definition selector에서 permission request와 canonical 실행 argument를 함께 유도
- raw 및 canonical argument의 JSON Schema offline 재검증과 canonical argument 1 MiB 상한
- exact resolved request에 결합된 policy stack으로 invocation 간 pre-evaluated Allow 재사용 차단
- 준비 뒤 Run head·authority·Definition 변경 차단과 Router 내부 재실행
- local host+user, once, noncritical, sync, idempotency-key 조건의 명시적 허용
- semantic action과 executable Instance binding을 분리한 domain-separated digest
- Run·Step·authority·head·action·policy·Instance에 결합된 max-use 1 authorization
- EffectIntent와 authorization consumption의 원자적 commit 및 lost-ack 수렴
- EffectIntent·authorization·material sidecar의 원자적 commit과 reconstructable lost-ack 복구
- raw argument의 journal·SQLite/WAL·Debug 비노출과 SQLite 재시작 검증

actual OS resource resolver, 사용자용 adapter, sandbox, sealed secret/argument 저장, managed/critical/reusable grant와 external harness 연동은 아직 포함하지 않는다. 자세한 admission 계약은 [Run-bound Invocation Admission 기본형](invocation-admission.md), 재시작 material 경계는 [Recoverable Invocation Material 기본형](recoverable-invocation-material.md), exact 실행 경계는 [Direct Executor 기본형](direct-executor.md), 비제품 actual-I/O 기준은 [Preopened Reference Adapter Conformance](reference-adapter-conformance.md)를 따른다.

## 검증

저장소 루트에서 실행한다.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo run --locked --quiet -p xgeny-cli -- protocol check
cargo build --locked --release -p xgeny-cli
```

마지막 명령의 산출물은 Linux/macOS에서 `target/release/xgeny`, Windows에서 `target/release/xgeny.exe`다.

`protocol check`는 다음을 검사한다.

- JSON Schema draft 2020-12 스키마 9개의 meta-schema 유효성
- 임의 HTTP/file retrieval을 끈 bundled registry의 `$ref` 해석
- manifest에 선언된 valid/invalid fixture 26개의 기대 결과
- valid fixture의 Rust 도메인 타입 역직렬화·재직렬화·재검증
- optional extension 보존과 unknown required extension fail-closed
- Capability 입력·출력 스키마의 meta-validation과 offline compile
- RFC 8785 argument byte 크기 및 Journal/Receipt/Policy digest
- WorkGraph, RunJournalEvent, ExecutionReceipt, PolicyDecision 간 식별자·cursor 연결

## CI와 OS 경계

GitHub Actions는 Linux에서 format과 clippy를 검사하고 Linux x86-64/ARM64, macOS Intel/Apple Silicon,
Windows x86-64에서 workspace test, `protocol check`, native release build와 loopback installer smoke를
수행한다. 따라서 현재 보장하는 것은 이 다섯 target의 **I/O 없는 Registry·Router·Permission
Broker·상태 기계, 내장 프로토콜 검증, 격리된 실제 workspace read/atomic write/strict patch, public run/resume process 회귀와
checksum 기반 설치 경로**다.

실제 OS 권한 broker, 일반 shell/process 실행, OS code signing/notarization과 게시된 GitHub Release의
외부 network 다운로드는 각각의 기능이 추가될 때 별도 테스트 계층으로 확장한다. CI 통과를 아직
구현하지 않은 통합 기능의 검증으로 해석하지 않는다.
