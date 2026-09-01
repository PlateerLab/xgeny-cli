# Developer Preview RC3 후보

- 후보 version: `0.1.0-rc.3`
- protocol: `xgeny.io/v1alpha1`, bundled/offline protocol check
- local store: embedded SQLite physical schema 8와 CLI material catalog schema 1
- 상태: version PR의 필수 CI가 모두 통과하고 main에 merge되기 전에는 게시 가능한 tag가 아니다

## 사용자 결과

RC3 후보는 사용자가 native `xgeny` binary 하나를 설치하고 OpenAI-compatible model을 연결한 뒤 다음
코딩 loop를 수행하는 첫 Developer Preview다.

```text
workspace search/read
  -> atomic write/patch
  -> shell 없는 test/build process
  -> 실패 output을 다음 model turn에서 관찰
  -> corrective patch
  -> re-test/build
  -> durable completion과 offline replay
```

Native installer 경로에서는 Rust, Node.js, Python 또는 SQLite 실행 파일이 XGENy 자체의 설치 의존성이
아니다. npm 설치 경로만 launcher 실행을 위해 Node.js 22.14 이상이 필요하며 Rust compiler나 install
script는 필요하지 않다. Model endpoint와 사용자가 실행하려는 compiler/test runner는 별도로 존재해야
하며 executable absolute path를 host catalog에 명시해야 한다.

## 포함 범위

| 영역 | RC3 후보의 보장 |
| --- | --- |
| 모델 연결 | OpenAI-compatible Chat Completions, exact model ID, strict structured proposal, 별도 egress 동의 |
| 파일 관찰 | exact file read 또는 bounded directory list/stat/literal search/read |
| 파일 변경 | digest-bound `write-atomic`과 strict contextual `apply-patch`, 별도 write 동의 |
| process | host-catalogued executable, shell 없는 cwd/argv/env, timeout, stdout/stderr byte limit, 별도 execute 동의 |
| 종료 | timeout과 정상 leader 종료 뒤 descendant process tree 정리 |
| 지속성 | journal/WorkGraph/Receipt/ToolOutput과 private invocation recipe의 process-restart 복원 |
| 복구 | Started 뒤 outcome을 확정하지 못한 effect를 Unknown으로 닫고 자동 replay하지 않음 |
| 장기 turn | PlanningContext v3가 Step plan 순서와 passed Receipt ToolOutput 관찰 순서를 보존 |
| 설치 | 다섯 target native/checksum installer와 무스크립트 `@xgen/cli` exact-version npm package |

Filesystem, process와 model egress 승인은 서로 대체하지 않는다. Process result의 `success=false`는 adapter
장애가 아니라 test/build의 durable non-zero 결과일 수 있으며 모델은 그 bounded output을 다음 turn에서
교정 근거로 사용한다.

## RC2 호환 경계

PlanningContext v3는 prompt/request-profile 의미가 달라 RC2와 다른 immutable digest를 사용한다.

- 완료된 RC2 Run은 provider와 workspace 없이 completion을 offline replay할 수 있다.
- 이미 계획된 RC2 local action은 같은 local-execution profile과 별도 승인을 만족하면 모델 없이 마칠 수
  있다.
- 다음 model egress가 필요한 미완료 RC2 Run은 `configuration_mismatch`로 fail-closed한다.
- v2 context fallback과 journal 자동 변환은 제공하지 않는다. 원래 RC2 binary로 마치거나 새 RC3 Run을
  시작한다.

## 검증 근거와 release gate

RC3 기능 main에는 다음 검증이 포함돼 있다.

- Linux x86-64의 실제 `qwen3.8-27b` coding gate 2회 연속 통과
- search → read → patch → failed `cargo test --offline` → corrective patch → successful re-test →
  `cargo build --offline`
- model call 8 reserved/8 settled/0 Unknown, single-Step Plan과 passed Receipt 각각 7개
- workspace와 material catalog 삭제 뒤 byte-exact completion replay와 journal 불변성
- process outcome transaction 장애, process crash, lost acknowledgment의 no-replay 회귀
- in-memory/SQLite warm/cold reopen의 PlanningContext journal chronology parity

Version PR은 다음 명령과 동일한 품질 gate를 통과해야 한다.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
cargo run --quiet --locked -p xgeny-cli -- protocol check
sh scripts/check-third-party-licenses.sh --check
sh scripts/check-release-workflow.sh
sh scripts/check-npm-distribution-workflow.sh
npm ci --ignore-scripts --prefix npm
npm run check --prefix npm
npm test --prefix npm
```

GitHub PR에서는 Quality/Linux와 Linux x86-64/ARM64, macOS Intel/Apple Silicon, Windows x86-64의 여섯
필수 check가 모두 성공해야 한다. 각 platform job은 전체 workspace test, filesystem/process adapter
clippy, protocol check, native dependency audit, release binary staging, installer smoke와 npm pack/global
install smoke를 수행한다.

## 알려진 한계

- `process.execute`는 command injection을 줄이는 capability boundary이지 OS sandbox가 아니다. 허용한
  executable과 child는 현재 사용자 권한으로 project code를 실행한다.
- Compiler/test stdout과 stderr는 workspace path나 source snippet을 포함할 수 있어 private state root
  전체를 민감 데이터로 취급한다.
- 실제 Qwen evidence 전 한 실행은 failed-test 뒤 model proposal rejection으로 끝났다. 최종 clean SHA의
  두 실행은 연속 성공했지만 모델 변동성이 0이라는 의미는 아니다. 자동 무제한 retry는 제공하지 않는다.
- macOS notarization과 Windows Authenticode가 없어 Gatekeeper/SmartScreen 경고가 나타날 수 있다.
- Browser/TUI, MCP, 장기 사용자 메모리, XGEN/Connector adapter와 범용 network tool은 RC3 범위가 아니다.
- XGEN, Connector, PostgreSQL, MinIO, Kubernetes와 XGEN Python package는 core 실행 의존성이 아니다.

## 게시 경계

이 문서를 merge하는 것만으로 release를 게시하지 않는다. Merge 뒤 npm scope/bootstrap/Trusted
Publisher 준비와 `XGENY_NPM_PUBLISH_ENABLED=true`를 확인하고, 현재 `origin/main` head와 package version이
정확히 일치할 때 별도 보호 tag `v0.1.0-rc.3`을 만들면 release workflow가 모든 release gate를 다시
실행한다. GitHub Release 전 실패한 tag나 asset은 이동·교체·재사용하지 않고 더 높은 새 version으로
수정한다. Immutable GitHub Release 뒤 동일 npm bundle의 부분 실패만 SRI 검증 아래 재실행한다.
