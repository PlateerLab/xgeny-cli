# Rust 워크스페이스 개발 환경

이 문서는 XGENy 코어와 프로토콜 검증 기반을 같은 조건으로 재현하기 위한 최소 절차다.

## 범위

현재 워크스페이스는 다음 세 crate로 구성된다.

```text
crates/
  xgeny-domain/    I/O 없는 정본 Rust 도메인 타입
  xgeny-protocol/  bundled/offline schema·fixture·digest 검증
  xgeny-cli/       xgeny 실행 파일과 protocol check 명령
```

이 단계는 모델 호출, 파일 실행, MCP, Connector, XGEN 원격 연동을 구현하지 않는다. 대신 이후 모듈들이 공유할 프로토콜 경계와 검증 기준을 먼저 실행 가능한 상태로 만든다.

## 준비물

- Git
- [rustup](https://rustup.rs/)

저장소의 `rust-toolchain.toml`이 Rust 1.98.0과 `rustfmt`, `clippy`를 고정한다. SQLite, PostgreSQL, MinIO, Docker, Kubernetes, Python, Node.js 또는 별도 데몬은 필요하지 않다.

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

GitHub Actions는 Linux에서 format과 clippy를 검사하고 Linux, macOS, Windows 각각에서 test, `protocol check`, release build를 수행한다. 따라서 현재 보장하는 것은 세 OS의 **CLI 빌드와 내장 프로토콜 검증**이다.

실제 OS 권한 broker, 파일시스템 sandbox, shell/process 실행, 설치 패키지 서명과 배포 E2E는 각각의 기능이 추가될 때 별도 테스트 계층으로 확장한다. CI 통과를 아직 구현하지 않은 통합 기능의 검증으로 해석하지 않는다.
