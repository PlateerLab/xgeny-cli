# Process execute adapter

`xgeny-adapter-process`는 `xgeny.process/execute@1.0.0`의 shell-free local adapter다. Public CLI는
사용자가 executable catalog를 명시한 Run에서만 이 tool을 등록한다. 다른 host composition도 workspace,
executable catalog와 환경을 명시적으로 제공해야 한다.

## Public CLI composition

```bash
xgeny run \
  --workspace . \
  --allow-dir . \
  --allow-executable cargo="$(command -v cargo)" \
  --allow-remote-model-egress \
  --allow-read \
  '프로젝트를 살펴보고 테스트해줘.'
```

이 호출에서 model이 `cargo` 실행을 계획해도 `--allow-execute`가 없으므로 process 시작 전에
`execute_approval_required`로 pause한다. 출력된 Run ID를 사용해 로컬 실행만 별도로 승인할 수 있다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef \
  --workspace . \
  --allow-dir . \
  --allow-executable cargo="$(command -v cargo)" \
  --allow-read \
  --allow-execute
```

미완료 재개에는 원래와 같은 allow-file/allow-dir와 executable catalog가 필요하다. 실행 뒤 다음 model
turn이 필요하면 같은 인자에 `--base-url`과 `--allow-remote-model-egress`를 추가한다. Windows에서는
`--allow-executable cargo=C:\\absolute\\path\\cargo.exe`처럼 absolute `.exe`/`.com`을 지정한다.

## Host composition

```rust
use std::collections::BTreeMap;
use xgeny_adapter_process::{
    ExecutableCatalog, ProcessEnvironment, ProcessWorkspace, ProcessWorkspaceId,
};

let catalog = ExecutableCatalog::from_paths([
    ("cargo", "/absolute/trusted/path/to/cargo"),
])?;
let environment = ProcessEnvironment::new(BTreeMap::from([
    ("PATH".to_owned(), "/host-selected/toolchain/bin".to_owned()),
]))?;
let workspace = ProcessWorkspace::open_ambient(
    "/absolute/user-selected/project",
    ProcessWorkspaceId::new("primary")?,
    catalog,
    environment,
)?;

let resolver = workspace.resolver();
let binding = workspace.binding();
let adapter = workspace.adapter();
let verifier = adapter.verifier();
```

Composition root는 다음 항목을 같은 exact binding으로 Core에 등록한다.

1. bundled process Capability Definition
2. `binding`을 사용하는 local verified Capability Instance
3. `resolver`를 사용하는 `process.execute` permission resolution
4. `adapter`를 사용하는 effect dispatch
5. `verifier`를 사용하는 durable output verification

실행 material의 `executable`에는 logical ID를 직접 쓰거나 resolver가 반환한
`process:<workspace>/executables/<id>`를 쓴다. Admission 이후에는 canonical resource만 adapter에
전달된다.

## 보안 경계

- Model은 executable path를 지정하거나 PATH 검색을 요청하지 못한다.
- Executable symlink/proxy는 canonical target/content를 재검사하면서 launch alias와 `argv[0]`를 보존한다.
- Model env는 host snapshot key, PATH/home/system/temp/toolchain/loader key를 덮어쓰지 못한다.
- cwd는 workspace-relative existing directory이며 symlink/reparse traversal을 거부한다.
- argv는 shell을 거치지 않는다.
- timeout 또는 leader 종료 시 descendant tree도 종료한다.
- stdout/stderr는 UTF-8 변환 뒤에도 각각 32 KiB 이하만 durable output에 보존한다.
- Adapter와 verifier `Debug`에는 path, argv, env value, raw output을 넣지 않는다.

이 경계는 command injection과 accidental path escape를 줄이는 capability boundary이지 OS sandbox가
아니다. 허용된 `cargo test`, Python, Node 같은 executable은 project code와 자체 child process를
사용자 계정 권한으로 실행할 수 있다. Host catalog는 신뢰한 개발 도구만 넣고 credential-bearing 환경을
상속하지 않아야 한다.

## Failure와 recovery

| 관찰 | durable output | 다음 행동 |
|---|---|---|
| exit 0 | `exited`, `success=true` | verifier/Receipt 뒤 다음 planning turn |
| non-zero 또는 signal | `exited`, `success=false` | stderr를 읽고 수정 또는 다른 명령 계획 |
| timeout | `timed_out`, `success=false` | 자동 재실행 없이 모델 판단 |
| spawn 전에 launch 실패 | `launch_failed` | catalog/host 상태 점검 |
| spawn 뒤 outcome 불명확 | 없음, `EffectUnknown` | manual required; blind replay 금지 |

Non-zero test 결과가 Receipt-completed된 뒤 모델이 코드를 수정하면, 같은 executable/argv/cwd/env/timeout을
다음 turn에 다시 제안할 수 있다. ADR-0029에 따라 Core가 새 Step occurrence와 별도 permission request,
one-shot grant, effect와 idempotency key를 만든다. 반대로 `Executing`/`EffectUnknown`인 occurrence는
재승인 flag를 다시 주어도 새 Step으로 자동 대체하거나 같은 process를 replay하지 않는다.

`NonIdempotent + durableToolOutput=true` profile과 SQLite schema 8 no-replay 의미는
[ADR-0027](../adr/0027-non-idempotent-durable-tool-output.md)을 따른다. OS/process 결정은
[ADR-0028](../adr/0028-shell-free-process-execute.md)을 따르고 반복 action identity는
[ADR-0029](../adr/0029-core-derived-action-occurrence.md)을 따른다.

## Local verification

```text
cargo fmt --all -- --check
cargo clippy -p xgeny-adapter-process --all-targets -- -D warnings
cargo test -p xgeny-adapter-process -- --test-threads=1
cargo run --quiet -p xgeny-cli -- protocol check
```

Platform-specific process group, Job Object, path와 executable 판단은 Linux, macOS, Windows CI의
workspace test에서 실행한다. CLI integration test는 loopback model을 사용해 catalog-only 승인 pause,
변경된 catalog의 재개 거부, 별도 실행 승인, durable output의 다음 model turn 전달과 단일 Receipt를
검증한다. 별도 fault test는 실제 child가 durable marker를 한 번 만든 직후 SQLite outcome commit을
실패시킨다. 첫 재개는 남은 `Executing`을 `EffectUnknown`으로 바꾸고, 동일 executable과 새
`--allow-execute`를 다시 주어도 marker count, journal과 Receipt가 변하지 않아야 한다.
