# Public local `run/resume` prototype

이 문서는 ADR-0023과 ADR-0024 구현을 개발·검증하는 최소 운영 절차다. ADR-0025의 mutation 명령과
별도 `--allow-write` 재개 절차는 [Workspace atomic write](workspace-atomic-write.md)를 따른다. 기존 exact-file mode는
OpenAI-compatible planner와 workspace `read-text` 하나를 유지한다. Source/main의 opt-in
workspace mode는 `list-directory`, `stat`, `search-text`, `read-text`, `write-atomic`, `apply-patch`를 제공한다.

## 실행

SQLite 실행 파일이나 server는 필요 없다. 기본 state 위치 대신 격리된 위치를 쓰려면
`XGENY_STATE_HOME`을 설정한다. API token이 필요한 HTTPS endpoint만
`XGENY_OPENAI_API_KEY`를 사용한다. token을 CLI argument로 전달하지 않는다. 반복 입력을 줄이려면
base URL, model과 tokenizer identity를 각각 `XGENY_OPENAI_BASE_URL`, `XGENY_OPENAI_MODEL`,
`XGENY_OPENAI_TOKENIZER`에 둘 수 있다. Tokenizer를 생략하면 model ID를 같은 identity로 사용한다.

처음 연결하는 endpoint는 Run state를 만들기 전에 catalog 조회로 확인할 수 있다.

```bash
xgeny model check
```

이 명령 자체가 현재 endpoint로 보내는 `GET /v1/models` 1회의 명시적 사용자 요청이다. Prompt와
inference는 보내지 않으며 `run`/`resume`이 이를 자동 호출하지 않는다. PASS는 exact model 광고까지만
뜻하고 strict structured generation은 첫 durable model call이 검증한다.

```bash
export XGENY_STATE_HOME=/absolute/private/xgeny-state
export XGENY_OPENAI_BASE_URL=http://127.0.0.1:18000/v1
export XGENY_OPENAI_MODEL=qwen3.8-27b
export XGENY_OPENAI_TOKENIZER=Qwen/Qwen3.8-27B-FP8

xgeny run \
  --workspace /absolute/workspace \
  --planner-id xgeny.live.go50902 \
  --allow-file README.md \
  --allow-remote-model-egress \
  --allow-read \
  'Read the explicitly allowed file and report its exact marker.'
```

프로젝트를 스스로 탐색하게 하려면 directory 권한을 별도로 지정한다.

```bash
xgeny run \
  --workspace /absolute/workspace \
  --planner-id xgeny.live.go50902 \
  --allow-dir . \
  --allow-remote-model-egress \
  --allow-read \
  'Inspect the workspace and find the implementation relevant to the goal.'
```

`--allow-dir`가 하나라도 있으면 discovery execution profile과 다중 tool 예산을 사용한다. 없으면
기존 allow-file profile/digest/예산과 material provider를 그대로 사용한다. `--allow-dir .`은 현재
workspace root를 뜻하며 workspace 밖 path나 symlink target을 허용하지 않는다.

`--workspace`를 생략하면 현재 directory를 사용한다. Command-line `--base-url`, `--model`,
`--tokenizer`는 기존 호출과 script 호환을 위해 계속 지원하며 같은 이름의 environment보다 우선한다.

`127.0.0.1`이 SSH tunnel endpoint여도 `--allow-remote-model-egress`가 필요하다. 정상 완료는
summary만 stdout에 쓰고 Run ID와 상태는 stderr에 쓴다.

`--allow-remote-model-egress`는 같은 invocation에 전달한 현재 `--base-url`을 승인한다. Endpoint는
manifest에 저장하거나 동일성 비교하지 않으므로 tunnel port를 바꿀 수 있지만, 변경된 endpoint로
보낼 때도 flag를 다시 명시해야 한다. Managed endpoint registry와 transport identity pinning은 후속이다.

미완료 Run은 원래 workspace mapping, 현재 endpoint와 같은 allow-file/allow-dir catalog를 다시 제공한다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef \
  --workspace /absolute/workspace \
  --base-url http://127.0.0.1:18000/v1 \
  --allow-file README.md \
  --allow-remote-model-egress \
  --allow-read
```

Discovery Run도 원래와 동일한 directory/file catalog를 제공한다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef \
  --workspace /absolute/workspace \
  --base-url http://127.0.0.1:18000/v1 \
  --allow-dir . \
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
cargo test --locked -p xgeny-cli --test workspace_discovery
cargo test --locked -p xgeny-cli --test environment_onboarding
```

이 세 test는 다음을 실제 child process와 loopback HTTP로 검증한다.

- egress 미동의 시 state/HTTP 0
- model check의 catalog GET 1회, inference/state 0과 loopback credential 미전송
- model Plan → 별도 local-only resume → allow-listed 실제 file read → ToolOutput+Receipt
- source 삭제 뒤 SQLite reopen → 두 번째 model request에 exact output 전달
- wrong workspace/catalog의 외부 호출·journal mutation 0
- 완료 뒤 workspace/model 없이 summary replay와 journal 불변
- 응답 전 process 종료 시 Reserved → Unknown, 자동 HTTP retry 0
- 실제 read 뒤 outcome commit 실패 시 즉시 effect Unknown 분류, offline resume의
  Executing → EffectUnknown 1회와 자동 재실행 0
- list → search → stat → read 네 observation과 completion의 한-process vertical flow
- dynamic search recipe의 approval pause → 별도 local-only process 복원/실행 → remote completion
- completed workspace와 material catalog 삭제 뒤 summary offline replay

## go50902 public live gate

실제 Qwen과 public binary의 model → file → 다음 model turn → offline replay를 검증할 때는 별도
terminal에서 tunnel을 열지 않는다. Ignored integration test가 tunnel lifecycle과 임시
workspace/state 정리를 함께 소유한다. `go50902`의 host key가 기존 `known_hosts`에 등록돼 있어야 하며
대화형 host-key 승인이나 password prompt는 허용하지 않는다. 전용 파일에는 `go50902`라는
`HostKeyAlias`로 사전에 검증한 키를 등록하고, 실행 중 자동 등록하지 않는다. Test는 이 입력을 한 번만
bounded read해 test-owned `0600` 안전 경로에 복사하고 두 tunnel이 같은 snapshot만 사용하게 한다.
`KnownHostsCommand`, SSHFP/DNS 신뢰와 `UpdateHostKeys`도 끈다.

```bash
XGENY_LIVE_CONFIRM=xgeny-go50902-public-cli-v1 \
XGENY_LIVE_KNOWN_HOSTS_FILE=/absolute/path/to/dedicated_known_hosts \
XGENY_LIVE_OPENAI_BASE_URL=http://127.0.0.1:18000/v1 \
cargo test --locked --release -p xgeny-cli \
  --test live_go50902_public \
  public_cli_two_turn_read_and_offline_replay \
  -- --ignored --exact
```

`--nocapture`, `--show-output`, shell tracing과 `tee`를 붙이지 않는다. Test는 매 실행마다 무작위 상대
파일명과 파일 내용 marker를 만들고 child stdout/stderr를 메모리에만 보관한다. 상대 파일명은 첫 Plan을
위해 goal에 포함되지만 marker는 포함되지 않는다. 실패 assertion도 endpoint, SSH forward, Run ID,
goal, path, marker, request/response와 ToolOutput을 출력하지 않는다. `XGENY_OPENAI_API_KEY`는 public
child에서 제거한다. Test는 URL의 nonzero `127.0.0.1` port만 입력받아 SSH forward를
`127.0.0.1:<port>:127.0.0.1:8000`으로 직접 구성하고 SSH target을 `go50902`로 고정한다.

통과 조건은 exit `[10, 10, 0, 0]`, model-call lifecycle `reserved/settled/unknown = 2/2/0`,
Plan/Step/effect/Receipt/completion 각 1개, 원본 삭제 뒤 두 번째 model turn 성공, tunnel·workspace 삭제 뒤
완료 summary의 byte-exact offline replay와 journal 불변이다. 첫 model turn tunnel은 local-only read
전에 종료하고 두 번째 model turn용 tunnel을 새로 열어 read process에 provider 경로가 없음을 고정한다.
일반 CI는 이 test를 compile만 하고 외부 network 없이 ignore한다.

### Workspace discovery live gate

실제 Qwen이 알려지지 않은 상대 path를 스스로 찾는 경로는 같은 test binary의 별도 gate로 실행한다.
이 gate는 기존 exact-file gate와 다른 확인 문자열을 요구하며, 실제 repository 대신 test-owned 임시
workspace만 `--allow-dir .`로 연다.

```bash
XGENY_LIVE_CONFIRM=xgeny-go50902-workspace-discovery-v1 \
XGENY_LIVE_KNOWN_HOSTS_FILE=/absolute/path/to/dedicated_known_hosts \
XGENY_LIVE_OPENAI_BASE_URL=http://127.0.0.1:18000/v1 \
cargo test --locked --release -p xgeny-cli \
  --test live_go50902_public \
  public_cli_workspace_discovery_and_offline_replay \
  -- --ignored --exact
```

Test는 무작위 target path, 검색 locator와 결과 sentinel을 각각 만든다. Locator와 sentinel은 서로 다른
줄에 있어 `search-text` preview만으로 최종 값을 알 수 없으며, goal에는 locator만 들어간다. 통과하려면
모델이 root list, recursive search, matching file stat과 exact read를 모두 Receipt-completed Step으로
수행하고 read 뒤 summary를 sentinel과 byte-exact하게 완성해야 한다. Discovery 전용 profile은 미래
observation에 의존하는 argument를 추측하지 말고 turn마다 concrete Step 하나만 반환하도록 모델에
요구한다. Gate는 실제 accepted Plan마다 Step이 하나이고 dependency가 비어 있는지도 확인한다. 모델이
bounded 추가 관찰을 선택할 수 있어 총 model call 수를 5로 고정하지 않지만, 전체 Step/model turn은
discovery Run budget 안이어야 하고 모든 effect는 1회 실행, model call은 전부 settled,
Unknown/failure는 0이어야 한다.
Tunnel을 닫고 workspace와 `materials.sqlite3`를 삭제한 뒤에도 summary가 offline replay되고 journal이
변하지 않아야 한다.

기존 exact-file live gate가 model egress와 local read를 서로 다른 process/tunnel 구간으로 분리해
검증한다. Workspace gate는 그 권한 회귀를 중복하지 않고 실제 모델이 지시된 structured capability를
호출하고 search observation에서 얻은 동적 path로 다음 turn을 이어 한 Run을 완료하는지 검증한다. 두
gate는 서로 다른 explicit confirmation을 요구하므로 위와 기존 exact-file 명령으로 각각 실행하며, 같은
local forward port에서 동시에 실행하지 않는다. 일반 CI에서는 두 test 모두 외부 연결 없이 compile만
한다.

## Clean-SHA live evidence (2026-08-31)

2026-08-31 05:57 KST에 Linux x86_64, Rust 1.98.0, release profile에서 test 구현 commit
`f58209ced19c6cedab33945818270e7db7dc1897`을 clean working tree로 실행했다. SSH target은
`go50902`로 고정했고 두 tunnel 모두 같은 test-owned host-key snapshot으로 strict 검증했다. 이번 실행의
snapshot은 기존 운영 alias로 접속해 읽은 public host key로 bootstrap했으므로 그 자체가 독립적인 host
identity attestation은 아니다. 재현·승격 환경에서는 별도 경로로 사전 검증한 전용 known-host entry를
입력해야 한다.

같은 시점의 별도 read-only 운영 관측은 vLLM `0.27.1`, served model `qwen3.8-27b`, artifact basename
`Qwen3.8-27B-FP8`, `max_model_len=524288`이었다. 이 값들은 비암호학적 운영 관측이며 gate가
runtime/artifact/max length를 durable result에 bind하지 않는다. Gate가 요청하는 model은
`qwen3.8-27b`, tokenizer identity는 `Qwen/Qwen3.8-27B-FP8`로 고정했다. 원격 endpoint, forward,
host key, Run ID, 임시 path, 무작위 파일명, goal, marker, request/response, ToolOutput과 digest는 증거에
남기지 않았다.

실행 결과는 PASS였다. 관측된 public CLI exit sequence는 `[10, 10, 0, 0]`, model-call lifecycle은
`reserved/settled/unknown = 2/2/0`이었고 Plan, Step, effect, Receipt, passed verification, completion은
각각 하나였다. Effect attempt는 1회였다. 첫 tunnel 종료 뒤 local read를 수행했고, 원본 파일 삭제 뒤
두 번째 model turn이 durable ToolOutput만으로 exact summary를 만들었다. 두 번째 tunnel과 workspace를
삭제한 뒤의 offline replay도 byte-exact였으며 journal은 변하지 않았다. 금지한 runtime 값의
노출 검사는 child 양쪽 stream의 base URL/workspace/state/상대 파일명, 모든 child stderr의 marker,
manifest의 base URL/workspace/state/상대 파일명/marker를 대상으로 통과했다. Run ID는 status stderr와
manifest에, marker는 completion/replay stdout에만 의도적으로 허용된다. Test harness는 capture한 stream
원문을 출력하지 않았고 임시 state와 listener도 실행 종료 뒤 남지 않았다.

## Workspace discovery clean-SHA live evidence (2026-09-01)

2026-09-01 08:48 KST까지 Linux x86_64, Rust 1.98.0, release profile에서 강화된 discovery gate 구현
commit `1af892bc0474abbaecbd9e0ab6698530413fe2ec`을 clean working tree로 검증했다. Gate는 served model
`qwen3.8-27b`와 tokenizer identity `Qwen/Qwen3.8-27B-FP8`를 요청했다. 기존에 신뢰한 resolved host
entry를 strict matching한 뒤 test 전용 `HostKeyAlias=go50902` snapshot으로 bootstrap했으므로, 이 기록
역시 독립적인 host identity attestation은 아니다.

서로 다른 무작위 workspace, target path, locator와 sentinel을 사용한 discovery 실행 두 번이 연속
PASS했다. 각 실행에서 `list-directory`, `search-text`, `stat`, `read-text`가 모두 Receipt에 결합된
완료 Step으로 관측됐고 각 Step의 실행 시도는 1회였다. Search preview에는 locator만 있고 sentinel은
없었다. 모든 accepted Plan은 dependency가 없는 Step 하나만 포함했다. 모든 model call은 settled됐고
모든 effect가 성공했으며 failure, Unknown, reconciliation은 0이었다. Read 뒤 completion과 삭제 뒤
offline replay는 sentinel과 byte-exact했고 journal도 변하지 않았다. Gate 내부에는 model-call 재시도나
실패 시 전체 실행 재시도가 없다.

같은 commit에서 기존 exact-file gate도 별도로 PASS했다. 따라서 constrained sequential discovery
prompt가 기존 exact-path Plan/Step/effect/Receipt, 두 model turn과 삭제 뒤 offline replay 계약을
회귀시키지 않았음을 함께 확인했다. 원격 endpoint, host key, forward, Run ID, 임시 경로, 무작위 입력,
model request/response와 ToolOutput 원문은 이 증거에 남기지 않았다.

## 보안·운영 메모

- `manifest.json`에는 endpoint, token, root path, allow-file/allow-dir path, search query와 content가 없다.
- Run DB에는 goal과 성공한 tool output이 의도적으로 존재한다. Discovery mode의 private
  `materials.sqlite3`에는 dynamic path/query recipe가 존재한다. state root 전체를 민감 데이터로
  취급한다.
- allow-file은 ambient absolute path가 아니라 workspace-relative portable path만 받는다.
- Debug/error 출력으로 내부 path나 provider body를 내보내지 않는다.
- `XGENY_STATE_HOME`은 넓은 기존 경로나 final symlink를 가리키면 안 되며, 기존 directory 권한을
  자동으로 바꾸지 않는다. 정상적인 OS ancestor symlink는 물리 ancestor로 고정한 뒤 새 app-owned
  suffix만 private mode로 생성한다. Windows UNC/device namespace는 거부하며, drive letter로 매핑된
  network volume은 탐지하지 못하므로 지원·검증 대상이 아니다.
- 미완료 resume의 DB 사전검사는 schema-8 read-only이고, workspace/catalog/profile 확인 뒤 writable
  reopen 시 같은 state를 다시 검증한다. Clean DB는 immutable open으로 sidecar를 만들지 않고,
  crash WAL은 private adjacent snapshot에서만 replay하여 source DB/WAL/SHM을 건드리지 않는다.
- DB와 snapshot source의 final symlink/Windows reparse는 물리 parent 아래에서 정적으로 거부한다.
  Windows SQLite VFS의 writable sidecar open과 hostile same-UID entry 교체를 원자적으로 봉쇄하는
  hardening은 현재 프로토타입 범위 밖이다.
- Manifest의 local execution profile은 capability/instance, adapter 제한, route/materializer와
  approval/policy revision을 묶는다.
- Core Run store는 schema 8을 유지한다. Discovery recipe는 CLI-owned `materials.sqlite3` schema 1에
  분리하며 SQLite executable/server를 요구하지 않는다.
- schema-8 completion sidecar가 없거나 manifest/store binding이 다르면 exit `70`이다.
- Unknown model/effect는 exit `30`이며 자동 retry하지 않는다.
- 현재 partial initialization 및 SIGKILL 뒤 preflight scratch cleanup, encrypted state, Windows ACL
  hardening과 hostile same-UID sandbox는 지원하지 않는다.
