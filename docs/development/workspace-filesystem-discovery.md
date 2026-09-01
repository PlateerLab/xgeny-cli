# Workspace filesystem discovery

이 문서는 source/main의 workspace read-only discovery 구현과 회귀 절차다. Directory mode에 추가된
별도 승인형 mutation은 [Workspace atomic write](workspace-atomic-write.md)를 따른다. 최신 배포 prerelease
`v0.1.0-rc.2`는 exact `--allow-file` read만 포함하므로, `--allow-dir`는 이 변경을 포함한 다음 release
artifact부터 사용할 수 있다.

## 실행 모드

기존 exact-file mode:

```bash
xgeny run \
  --allow-file README.md \
  --allow-remote-model-egress \
  --allow-read \
  'README를 읽고 요약해줘.'
```

Workspace discovery mode:

```bash
xgeny run \
  --workspace . \
  --allow-dir . \
  --allow-remote-model-egress \
  --allow-read \
  '프로젝트 구조를 확인하고 관련 구현을 찾아 설명해줘.'
```

`--allow-dir .`은 선택한 workspace root 전체를 네 read-only Capability로 관찰할 후보 경계를 제공한다.
같은 mode에서 `write-atomic`도 광고되지만 실제 mutation은 별도 `--allow-write`가 있어야 한다.
Planner는 immutable input schema에서 `.` 규칙을 받고, 별도 `planningConstraints`에서 caller가 허용한
directory/file 목록을 받는다. Constrained provider prompt는 이를 후보 제한으로 따르되 권한으로
간주하지 않는다. 또한 미래 observation을 placeholder로 참조하지 않고, 현재 context에서 argument가
확정된 Step 하나만 plan한 뒤 receipt-completed tool output을 다음 turn에서 받는다. 실제 실행 경계는
동일 catalog를 component 단위로 다시 검사한다.

| Capability | 동작 |
| --- | --- |
| `xgeny.fs/list-directory@1.0.0` | 한 directory의 직계 entry 목록 |
| `xgeny.fs/stat@1.0.0` | regular file size 또는 directory kind |
| `xgeny.fs/search-text@1.0.0` | directory 아래 case-sensitive literal UTF-8 검색 |
| `xgeny.fs/read-text@1.0.0` | bounded UTF-8 file content 읽기 |
| `xgeny.fs/write-atomic@1.0.0` | 별도 승인 아래 UTF-8 file create/atomic replace |

더 좁은 범위는 directory를 반복 지정하고 필요한 exact file만 추가한다.

```bash
xgeny run \
  --workspace . \
  --allow-dir src \
  --allow-dir tests \
  --allow-file Cargo.toml \
  --allow-remote-model-egress \
  --allow-read \
  '구현과 테스트 구조를 비교해줘.'
```

미완료 Run은 동일한 workspace와 catalog를 다시 제공한다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef \
  --workspace . \
  --allow-dir src \
  --allow-dir tests \
  --allow-file Cargo.toml \
  --allow-remote-model-egress \
  --allow-read
```

완료된 Run은 기존과 같이 다른 입력 없이 offline replay한다.

## Durable material과 state

Discovery mode는 Run directory에 두 SQLite file을 둔다.

```text
runs/<run-id>/
├── manifest.json
├── run.sqlite3
├── materials.sqlite3
└── run.lock
```

`run.sqlite3`는 Core journal/WorkGraph/Receipt/tool output을 보유하고 schema 8을 유지한다.
`materials.sqlite3` schema 1은 모델이 선택한 dynamic path, search query와 write content를 opaque
digest reference로 복원하기 위한 CLI-owned recipe catalog다. 둘 다 binary에 embedded된 SQLite library를 사용하므로
SQLite 프로그램이나 server를 설치하지 않는다.

Manifest, WorkGraph event, Receipt와 status output에는 raw write content가 들어가지 않는다. 하지만 private
material DB에는 path/query/content가, Run DB에는 goal과 successful output이 존재하므로 state root를 민감
데이터로 취급한다. Completed summary replay는 두 DB 중 material catalog와 workspace가 없어도 된다.

## Limit과 해석

- authorization catalog: file/directory 합계 64, run-local planner constraint 16 KiB
- list: directory scan 4,096, 반환 512
- list/stat/search output: digest 포함 canonical JCS 최대 128 KiB
- search: query 최대 64 Unicode scalar이면서 256 UTF-8 bytes, raw visited entry
  4,096, file 256 KiB, 실제 read aggregate 8 MiB, match 128,
  preview 512 bytes
- read-text: file 64 KiB
- discovery Run: model turn 8, provider reservation 16, planned/tool Step 8, context 512 KiB

`truncated=true`는 결과가 complete하지 않다는 뜻이다. Search는 limit 도달뿐 아니라 VCS metadata,
invalid UTF-8, oversized 또는 열 수 없는 file을 건너뛰어도 이 값을 세운다. Regex와 `.gitignore`
해석은 아직 지원하지 않는다.

## 검증

```bash
cargo test --locked -p xgeny-adapter-filesystem --all-targets
cargo test --locked -p xgeny-cli --test workspace_discovery
cargo test --locked -p xgeny-cli --test public_run_resume
cargo run --locked --quiet -p xgeny-cli -- protocol check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

`workspace_discovery` E2E는 실제 child process와 loopback fake provider로 다음 두 경로를 검증한다.

```text
list -> search -> stat -> read -> completion -> workspace/material 삭제 -> offline replay

model plan -> approval pause -> process 종료
  -> local-only resume에서 dynamic search material 복원/실행
  -> remote resume에서 durable output 전달 -> completion

write plan -> 별도 write approval pause -> process 종료
  -> local-only resume에서 content 복원/atomic write
  -> remote resume에서 content 없는 durable output 전달 -> completion
```

실제 Qwen의 structured capability 호출과 동적 경로 continuation은 별도 ignored release test로
검증한다. Search preview와 read 결과를 구분하기 위해 locator와 최종 sentinel을 서로 다른 줄에 둔 임시
workspace를 만들고, 모델이 지시된 `list-directory`, `search-text`, `stat`, `read-text`를 모두 실행한
뒤에만 sentinel을 exact summary로 완성하는지 확인한다. Accepted Plan마다 dependency 없는 Step 하나만
있는지도 검사한다. 완료 후 tunnel, workspace와 dynamic material catalog를 제거하고 offline replay와
journal 불변까지 검사한다.

```bash
XGENY_LIVE_CONFIRM=xgeny-go50902-workspace-discovery-v1 \
XGENY_LIVE_KNOWN_HOSTS_FILE=/absolute/path/to/dedicated_known_hosts \
XGENY_LIVE_OPENAI_BASE_URL=http://127.0.0.1:18000/v1 \
cargo test --locked --release -p xgeny-cli \
  --test live_go50902_public \
  public_cli_workspace_discovery_and_offline_replay \
  -- --ignored --exact
```

전용 known-host file에는 `HostKeyAlias=go50902`로 별도 경로에서 검증한 public host key가 있어야 한다.
Test가 tunnel lifecycle을 직접 소유하므로 다른 terminal에서 같은 local port의 tunnel을 미리 열지 않는다.
`--nocapture`, `--show-output`, shell tracing과 `tee`도 사용하지 않는다. 상세한 보안 전제와 exact-file
gate 및 discovery gate의 clean-SHA 증거는
[Public local run/resume](public-local-run-resume.md)를 따른다.

결정 근거와 보장 한계는 [ADR-0024](../adr/0024-workspace-filesystem-discovery.md)를 따른다.
