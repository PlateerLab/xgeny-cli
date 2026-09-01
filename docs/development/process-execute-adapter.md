# Process execute adapter

`xgeny-adapter-process`는 `xgeny.process/execute@1.0.0`의 shell-free local adapter다. 아직 CLI가
자동으로 등록하는 public tool은 아니며, host composition이 workspace, executable catalog와 환경을
명시적으로 제공해야 한다.

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

`NonIdempotent + durableToolOutput=true` profile과 SQLite schema 8 no-replay 의미는
[ADR-0027](../adr/0027-non-idempotent-durable-tool-output.md)을 따른다. OS/process 결정은
[ADR-0028](../adr/0028-shell-free-process-execute.md)을 따른다.

## Local verification

```text
cargo fmt --all -- --check
cargo clippy -p xgeny-adapter-process --all-targets -- -D warnings
cargo test -p xgeny-adapter-process -- --test-threads=1
cargo run --quiet -p xgeny-cli -- protocol check
```

Platform-specific process group, Job Object, path와 executable 판단은 Linux, macOS, Windows CI의
workspace test에서 실행한다. 실제 CLI approval/catalog/model loop는 별도 vertical slice에서 검증한다.
