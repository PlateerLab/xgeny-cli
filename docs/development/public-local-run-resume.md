# Public local `run/resume` prototype

이 문서는 ADR-0023 구현을 개발·검증하는 최소 운영 절차다. 현재 공개 slice는 OpenAI-compatible
planner와 workspace `read-text` 하나다.

## 실행

SQLite 실행 파일이나 server는 필요 없다. 기본 state 위치 대신 격리된 위치를 쓰려면
`XGENY_STATE_HOME`을 설정한다. API token이 필요한 HTTPS endpoint만
`XGENY_OPENAI_API_KEY`를 사용한다. token을 CLI argument로 전달하지 않는다.

```bash
export XGENY_STATE_HOME=/absolute/private/xgeny-state

xgeny run \
  --workspace /absolute/workspace \
  --base-url http://127.0.0.1:18000/v1 \
  --model qwen3.8-27b \
  --tokenizer Qwen/Qwen3.8-27B-FP8 \
  --planner-id xgeny.live.go50902 \
  --allow-file README.md \
  --allow-remote-model-egress \
  --allow-read \
  'Read the explicitly allowed file and report its exact marker.'
```

`127.0.0.1`이 SSH tunnel endpoint여도 `--allow-remote-model-egress`가 필요하다. 정상 완료는
summary만 stdout에 쓰고 Run ID와 상태는 stderr에 쓴다.

`--allow-remote-model-egress`는 같은 invocation에 전달한 현재 `--base-url`을 승인한다. Endpoint는
manifest에 저장하거나 동일성 비교하지 않으므로 tunnel port를 바꿀 수 있지만, 변경된 endpoint로
보낼 때도 flag를 다시 명시해야 한다. Managed endpoint registry와 transport identity pinning은 후속이다.

미완료 Run은 원래 workspace mapping, 현재 endpoint와 같은 allow-file catalog를 다시 제공한다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef \
  --workspace /absolute/workspace \
  --base-url http://127.0.0.1:18000/v1 \
  --allow-file README.md \
  --allow-remote-model-egress \
  --allow-read
```

완료된 Run은 provider와 workspace가 없어도 재생한다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef
```

`--max-ticks`는 한 process가 수행하는 coordination work만 제한한다. Durable model/tool 예산은
manifest에 별도로 고정된다. 테스트에서 `--max-ticks 6`을 쓰면 ToolOutput+Receipt 직후 process를
끝내고 다음 model turn을 별도 process로 분리할 수 있다.

읽기 승인을 모델 전송과 분리하려면 provider option 없이 local frontier만 진행할 수 있다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef \
  --workspace /absolute/workspace \
  --allow-file README.md \
  --allow-read
```

파일 read와 verification 뒤에는 새 model call을 예약하지 않고 egress 동의 pause를 반환한다.

## 로컬 검증

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
cargo run --locked --quiet -p xgeny-cli -- protocol check
```

핵심 process proof만 반복할 때는 다음을 사용한다.

```bash
cargo test --locked -p xgeny-cli --test public_run_resume
```

이 test는 다음을 실제 child process와 loopback HTTP로 검증한다.

- egress 미동의 시 state/HTTP 0
- model Plan → 별도 local-only resume → allow-listed 실제 file read → ToolOutput+Receipt
- source 삭제 뒤 SQLite reopen → 두 번째 model request에 exact output 전달
- wrong workspace/catalog의 외부 호출·journal mutation 0
- 완료 뒤 workspace/model 없이 summary replay와 journal 불변
- 응답 전 process 종료 시 Reserved → Unknown, 자동 HTTP retry 0
- 실제 read 뒤 outcome commit 실패 시 즉시 effect Unknown 분류, offline resume의
  Executing → EffectUnknown 1회와 자동 재실행 0

## 보안·운영 메모

- `manifest.json`에는 endpoint, token, root path, allow-file path와 content가 없다.
- Run DB에는 goal과 성공한 file content가 의도적으로 존재한다. state root를 민감 데이터로 취급한다.
- allow-file은 ambient absolute path가 아니라 workspace-relative portable path만 받는다.
- Debug/error 출력으로 내부 path나 provider body를 내보내지 않는다.
- `XGENY_STATE_HOME`은 넓은 기존 경로나 final symlink를 가리키면 안 되며, 기존 directory 권한을
  자동으로 바꾸지 않는다. 정상적인 OS ancestor symlink는 물리 ancestor로 고정한 뒤 새 app-owned
  suffix만 private mode로 생성한다. Windows UNC/device namespace는 거부하며, drive letter로 매핑된
  network volume은 탐지하지 못하므로 지원·검증 대상이 아니다.
- 미완료 resume의 DB 사전검사는 schema-8 read-only이고, workspace/catalog/profile 확인 뒤 writable
  reopen 시 같은 state를 다시 검증한다. Clean DB는 immutable open으로 sidecar를 만들지 않고,
  crash WAL은 private adjacent snapshot에서만 replay하여 source DB/WAL/SHM을 건드리지 않는다.
- Manifest의 local execution profile은 capability/instance, adapter 제한, route/materializer와
  approval/policy revision을 묶는다.
- schema-8 completion sidecar가 없거나 manifest/store binding이 다르면 exit `70`이다.
- Unknown model/effect는 exit `30`이며 자동 retry하지 않는다.
- 현재 partial initialization 및 SIGKILL 뒤 preflight scratch cleanup, encrypted state, Windows ACL
  hardening과 hostile same-UID sandbox는 지원하지 않는다.
