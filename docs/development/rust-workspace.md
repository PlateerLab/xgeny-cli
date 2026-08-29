# Rust 워크스페이스 개발 환경

이 문서는 XGENy 코어와 프로토콜 검증 기반을 같은 조건으로 재현하기 위한 최소 절차다.

## 범위

현재 워크스페이스는 다음 일곱 crate로 구성된다.

```text
crates/
  xgeny-domain/       I/O 없는 정본 Rust 프로토콜 타입
  xgeny-protocol/     bundled/offline schema·fixture·digest 검증
  xgeny-workgraph/    model-free RunEvent 상태 전이·재생 실험
  xgeny-local-store/  메모리 참조 구현과 embedded SQLite 후보
  xgeny-policy/       concrete resource 해석 경계와 순수 정책 교집합
  xgeny-runtime/      durable effect 실행·복구와 Capability Registry·Router 기본형
  xgeny-cli/          xgeny 실행 파일과 protocol check 명령
```

`xgeny-workgraph`, `xgeny-local-store`, `xgeny-runtime`의 durable effect 부분은 ADR-0008 연구 gate를 위한 내부 실험이며 공개 프로토콜 v0.1을 변경하지 않는다. Registry와 Router 기본형도 기존 `CapabilityDefinition`·`CapabilityInstance`를 그대로 사용하며 wire 문서를 추가하지 않는다. `xgeny-policy` 기본형은 기존 `PermissionRequest` 타입을 입력 경계로 사용하지만 Executor가 소비할 authority, reusable `Grant`나 `PolicyDecision` wire 문서를 발행하지 않는다. 이 단계는 모델 호출, 실제 파일·process adapter, 승인 UI, MCP, Connector, XGEN 원격 연동, `InvocationPlan` 투영 또는 사용자용 resume 명령을 구현하지 않는다.

## 준비물

- Git
- [rustup](https://rustup.rs/)
- 소스에서 전체 workspace를 빌드할 때 필요한 플랫폼 C build toolchain

저장소의 `rust-toolchain.toml`이 Rust 1.98.0과 `rustfmt`, `clippy`를 고정한다. `rusqlite`의 `bundled` 기능이 SQLite C source를 Rust build에 함께 링크하므로 소스 빌드에는 Linux C compiler, macOS Command Line Tools 또는 Windows MSVC Build Tools가 필요하다. 향후 `xgeny-local-store`를 연결한 배포 binary도 최종 사용자에게 SQLite 실행 파일, DB server 또는 daemon 설치를 요구하지 않는다. 현재 `xgeny` 실행 파일에는 이 저장소 후보를 아직 연결하지 않았다. PostgreSQL, MinIO, Docker, Kubernetes, Python, Node.js도 기본 실행 의존성이 아니다.

## 현재 durable slice 검증 범위

- RunEvent의 RFC 8785/SHA-256 hash chain과 I/O 없는 결정론적 replay
- authority epoch와 journal head compare-and-swap을 통한 stale writer 거부
- event, effect intent index, authorization consumption, projection의 단일 transaction
- transaction 중간 오류와 자식 process 즉시 종료 후 전량 rollback·재개
- lost acknowledgement 뒤 `effect_unknown` 복원과 비멱등 effect의 blind retry 거부
- Run별 OS file lease를 effect 호출 전체 구간에 유지해 동시 recovery worker 차단
- 실행 직전 ephemeral prepared effect와 durable action digest 일치 검증
- 시작 event commit 이전에는 sink를 호출하지 않고, outcome commit 유실 시 재실행 없이 unknown 복구
- query-capable sink만 read-only reconciliation하고 나머지는 manual 전환
- process 종료 전 실제 counter effect가 발생한 시나리오의 단일 실행·lease 해제·unknown 복원
- durable 실행 attempt 상한과 승인 예산 비중복 소비
- 메모리 참조 저장소와 SQLite 후보의 동일한 canonical JSONL export
- 임의 길이 journal 재생과 event 변조 탐지 property test

세부 실행 순서와 재시작 판정은 [Durable effect 실행·복구 수직 슬라이스](durable-effect-runtime.md)를 따른다. 이는 현재 개발 호스트의 process-crash 검증 결과다. power-loss, filesystem 손상, 실제 권한 경계가 있는 tool adapter, 설치 패키지, Linux/macOS/Windows 반복 fault matrix는 아직 통과했다고 주장하지 않는다. SQLite 채택 여부도 ADR-0008의 hardened JSONL 비교와 나머지 gate가 끝난 뒤 확정한다.

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

현재 CI의 resolver는 I/O 없는 fake다. 실제 path·symlink·process executable·network endpoint canonicalization, Run grant 발급·원자적 소비, PolicyLease 만료, 승인 UI와 wire `PolicyDecision` 투영은 구현하지 않았다. 자세한 경계는 [Permission Broker 기본형](permission-broker.md)을 따른다.

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
- manifest에 선언된 valid/invalid fixture 20개의 기대 결과
- valid fixture의 Rust 도메인 타입 역직렬화·재직렬화·재검증
- optional extension 보존과 unknown required extension fail-closed
- Capability 입력·출력 스키마의 meta-validation과 offline compile
- RFC 8785 argument byte 크기 및 Journal/Receipt/Policy digest
- WorkGraph, RunJournalEvent, ExecutionReceipt, PolicyDecision 간 식별자·cursor 연결

## CI와 OS 경계

GitHub Actions는 Linux에서 format과 clippy를 검사하고 Linux, macOS, Windows 각각에서 workspace test, `protocol check`, release build를 수행한다. 따라서 현재 보장하는 것은 세 OS의 **I/O 없는 Registry·Router·Permission Broker·상태 기계와 내장 프로토콜 검증, CLI 빌드**다.

실제 OS 권한 broker, 파일시스템 sandbox, shell/process 실행, 설치 패키지 서명과 배포 E2E는 각각의 기능이 추가될 때 별도 테스트 계층으로 확장한다. CI 통과를 아직 구현하지 않은 통합 기능의 검증으로 해석하지 않는다.
