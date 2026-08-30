# ADR-0023: Public local `run/resume` prototype와 재시작 경계

- 상태: Accepted
- 날짜: 2026-08-31
- 적용 범위: `xgeny-cli`, `xgeny-adapter-filesystem`, `xgeny-local-store`, local Run lease
- 선행 결정: ADR-0010, ADR-0016, ADR-0020, ADR-0021, ADR-0022

## 배경

ADR-0022까지는 실제 workspace 파일을 읽고 그 `ToolOutput`을 다음 planning turn에 전달한
수직 경로가 library test에만 존재했다. 사용자는 설치된 `xgeny` binary에서 Run을 만들고,
별도 process에서 SQLite 상태를 다시 열어 이어가며, 완료 뒤에는 모델과 workspace 없이 결과를
재생할 수 없었다.

Public composition이 단순히 기존 test 객체를 한 함수에 연결하는 것으로 끝나면 다음 문제가
생긴다.

- 논리 `WorkspaceId`가 재시작 뒤 다른 물리 directory를 가리킬 수 있다.
- 원격 모델 전송과 로컬 파일 읽기를 하나의 동의로 열 수 있다.
- 계획 인자를 plaintext recipe 파일에 저장하면 ADR-0010을 위반한다.
- 존재하지 않는 resume DB를 SQLite가 새로 만들 수 있다.
- 완료된 Run도 endpoint, model 또는 원본 파일을 요구할 수 있다.
- 전송 여부가 불확실한 model call을 재시도하면 외부 호출 횟수를 축소 보고한다.

## 결정

### 1. 이번 prototype은 한 개의 수직 slice를 제품 명령으로 연다

```text
OpenAI-compatible planner (remote boundary)
                 |
                 v
        bounded AgentLoop / WorkGraph
                 |
       exact route + separate approval
                 |
                 v
   preopened workspace read-text adapter
                 |
      ToolOutput + Core Receipt (SQLite)
                 |
                 v
          next planning turn
```

지원 범위는 범용 goal, OpenAI-compatible provider 한 개, preopened workspace 한 개,
`xgeny.fs/read-text@1.0.0` 한 개다. Core는 XGEN 서버, Connector, PostgreSQL, MinIO에
의존하지 않는다. 향후 XGEN은 같은 protocol/port에 맞추며 이 local composition을 역으로
의존시키지 않는다.

### 2. 모델 전송과 파일 읽기는 서로 다른 invocation-scoped 결정이다

- `--allow-remote-model-egress`: goal, planning context와 완료된 tool output을 remote model
  boundary로 보낼 수 있다.
- `--allow-read`: 모델이 allow-file catalog 안에서 선택한 exact resolved permission request를
  `Once`로 승인한다.
- SSH tunnel 주소가 `127.0.0.1`이어도 실제 model이 다른 host에 있으면 data boundary는
  `remote`다.

새 Run은 egress 동의가 없으면 Run ID, 디렉터리, DB와 HTTP 요청을 만들지 않는다. 미완료
Run은 egress 동의 없이도 이미 승인된 local frontier를 진행할 수 있지만, driver가 다음 model
call을 예약하기 직전에 반드시 멈춘다. 따라서 `resume --allow-read`만으로 exact local read와
verification을 끝낸 뒤 `remote_model_egress_consent_required`로 pause할 수 있다. 이는 현재
`AgentLoop`가 provider 호출 전에 `ModelCallReserved`를 먼저 commit하는 순서와 일치한다.

### 3. 물리 workspace identity를 manifest에 결합한다

동일 preopened `cap_std::Dir` handle의 metadata로 다음 equality commitment를 계산한다.

- Linux/macOS: device + inode
- Windows: volume serial + file index에 대응하는 `cap-fs-ext`의 by-handle `dev/ino`
- 공통: OS profile과 domain separator를 포함한 SHA-256

Manifest에는 digest만 저장한다. 절대경로와 raw OS file ID는 저장하거나 출력하지 않는다.
미완료 resume은 같은 handle에서 digest를 다시 계산해 model, material provider, adapter 호출
전에 비교한다. 같은 directory의 rename은 허용하고, 같은 경로에 다른 directory를 재생성하거나
다른 root를 전달하면 거부한다. 완료 replay는 workspace를 요구하지 않는다.

### 4. manifest가 local composition authority를 고정한다

각 Run의 `manifest.json`은 bounded, strict, JCS 검증 문서다. 다음 non-secret 의미만 보존한다.

- Run과 logical workspace ID
- 물리 workspace identity profile/digest
- planner ID, model, tokenizer, immutable request-profile digest
- remote model data boundary
- allow-file catalog digest
- Capability Definition/Instance, adapter limit, route, materializer와 approval/policy revision을
  묶은 local execution profile digest
- model turn/call, Step, tool call, context budget

Endpoint, credential, 절대경로, allow-file 경로, goal과 파일 내용은 manifest에 넣지 않는다.
Endpoint는 tunnel port 이동을 허용하기 위해 durable identity가 아니다. 각 `resume`의
`--allow-remote-model-egress`가 그 invocation에 함께 전달한 현재 `--base-url`로의 전송을 새로
승인한다. Planner ID는 request semantics의 논리 identity이며 endpoint 인증 수단이 아니다.
Manifest record의 domain-separated digest를 `RunCreated.authority`에 결합하고 resume에서
Run ID, authority, epoch와 budget을 재검사한다. Goal은 WorkGraph의 정본이므로 Run DB에는
저장되며, 파일 내용도 성공한 `ToolOutputRecord`로 DB에 의도적으로 저장된다. 따라서 state
directory 자체를 민감 데이터로 취급한다.

### 5. allow-file catalog는 plaintext durable recipe를 만들지 않는다

사용자는 process마다 같은 상대 경로 목록을 제공한다.

```text
--allow-file README.md --allow-file docs/spec.md
```

Host는 workspace resolver로 정규화하고 정렬한 뒤 process memory에서만 exact arguments를
보유한다. Durable material에는 아래 값만 남는다.

- provider: `xgeny.cli.allow-file.v1`
- reference: `entry-00000001` 형식의 opaque ordinal
- revision: domain-separated canonical argument commitment

재시작 시 같은 catalog를 다시 구성해 manifest catalog digest, reference와 revision을 모두
비교한다. 누락, 다른 root, 다른 entry 또는 revision은 effect 시작 전에 거부한다. 별도 SQLite,
JSON, sidecar 파일에 raw/canonical path를 저장하지 않는다. Digest는 암호화가 아니므로 낮은
엔트로피 경로의 존재를 숨기는 기밀성 수단으로 주장하지 않는다.

### 6. create와 resume storage 경로를 분리한다

Run layout은 OS state directory 아래 고정한다.

```text
<state-root>/runs/<run-id>/
  manifest.json
  run.sqlite3
  run.lock
```

`XGENY_STATE_HOME`은 headless test/server에서 state root를 명시할 때만 사용한다. 공개 명령에는
임의 DB/lock 경로 옵션이 없다.

기존 state root에는 절대 `chmod`하지 않는다. filesystem root, home/config base 자체처럼 넓은
경로, `.`/`..`, final state-root symlink, non-directory와 Unix group/other-accessible root는
거부한다. macOS `/var`처럼 정상적인 기존 ancestor symlink는 deepest existing ancestor를 한 번
canonicalize한 뒤 app-owned missing suffix만 no-follow로 생성한다. XGENy가 새로 만드는 directory만
생성 시점부터 `0700`으로 만든다. Windows state root는 drive-letter와 verbatim drive-letter
namespace만 허용하며 명시적 UNC/device namespace는 SQLite WAL/locking 계약 밖이라 거부한다.
Drive letter로 숨겨진 mapped network volume은 표준 path syntax만으로 판별하지 못하므로 지원·검증
대상이 아니며 CI와 운영 prototype은 local volume만 사용한다.

- `SqliteRunStore::create`: durable file path만 독점 생성한다.
- `SqliteRunStore::open_existing`: `CREATE` 없이 열며 schema 0과 누락 파일을 거부한다.
- `SqliteRunStore::open_existing_read_only`: schema 8만 migration/configuration 없이 완전 검증하며,
  UTF-8 local clean DB에는 immutable URI를 사용해 `-wal/-shm`도 만들지 않는다. Crash WAL 또는
  URI로 안전하게 표현할 수 없는 local path는 lease 아래 DB/WAL을 같은 private `0700` scratch에
  `0600`으로 복사하고 source SHM은 복사하지 않은 채 복사본에서만 WAL-index를 재생성한다. 따라서
  authoritative DB/WAL/SHM은 read-only이며 최신 committed page도 잃지 않는다.
- DB final symlink와 Windows reparse point는 물리 parent 아래에서 정적으로 거부한다. Snapshot
  source copy는 OS no-follow로 열고 SQLite open에도 VFS no-follow flag를 전달하며, URI는 clean
  immutable fast path에서만 사용한다. 이는 hostile same-UID process가 검사 직후 entry를 바꾸는
  경쟁까지 원자적으로 막는다는 보장은 아니다.
- lease를 manifest/DB open과 외부 호출보다 먼저 잡고 process가 끝날 때까지 유지한다.
- store와 lease `Debug`/공개 오류는 path를 출력하지 않는다.

SQLite는 `rusqlite/bundled`로 binary에 링크된다. 사용자가 SQLite server나 CLI를 별도로
설치하지 않는다.

### 7. resume은 항상 offline completion을 먼저 확인한다

`resume` 순서는 다음과 같다.

1. strict Run ID와 고정 layout 확인
2. lease 획득
3. manifest self-digest 확인
4. 기존 DB를 schema-8 read-only로 열고 Run/authority/budget 결합 확인
5. schema-8 completion sidecar 전체 결합 확인
6. 완료면 workspace, endpoint, credential, egress 없이 exact summary 반환
7. 미완료일 때만 workspace/catalog/local profile을 검사하고, 실제 mutation 직전에 writable로
   다시 열어 같은 verified state인지 재검사
8. local frontier는 별도 read 승인으로 진행하고 새 model reservation 직전에 egress 동의 요구

Durable candidate에 completion sidecar가 없거나 digest가 다르면 성공으로 강등하지 않고
integrity failure로 닫는다.

### 8. 불확정 호출은 자동 재시도하지 않는다

Provider 전송 중 process가 종료되어 `ModelCallReserved`가 남으면 다음 resume이 새 HTTP 요청
없이 `Interrupted/Unknown`으로 한 번 기록한다. 이미 `Unknown`이면 journal을 바꾸지 않고 manual
recovery 상태를 반복 반환한다. Effect `Started` 뒤 결과가 없는 경우도 기존 runtime 계약대로
자동 재실행하지 않는다.

## Public exit contract

| 코드 | 의미 |
|---:|---|
| `0` | durable completion 또는 offline replay |
| `2` | CLI syntax 오류 (`clap`) |
| `10` | 안전한 pause: 동의/승인/tick 또는 bounded quiescence |
| `20` | 확정 거부 또는 실패 |
| `30` | provider/effect 결과 불확정, 수동 복구 필요 |
| `64` | 구성, workspace, catalog 또는 profile 불일치 |
| `70` | store/manifest integrity 또는 내부 safety failure |
| `75` | 다른 process가 Run lease 보유 |

stdout은 성공 summary 전용이다. 상태와 fixed reason code는 stderr에만 출력하며 내부 error chain,
endpoint, path와 provider response body를 출력하지 않는다. 새 Run ID는 OS random source로 만들고
manifest, DB와 `RunCreated`를 durable commit한 뒤 외부 호출 전에 `XGENY_STARTED`로 알린다.

## 검증 기준

기본 CI는 실제 외부 모델 대신 loopback OpenAI-compatible HTTP server와 실제 `xgeny` child
process를 사용한다.

1. process 1이 model Plan 뒤 read approval에서 멈춤
2. process 2가 model egress 없이 별도 read 승인으로 실제 파일 read, ToolOutput와 Receipt를 저장한 뒤
   다음 model boundary에서 멈춤
3. 원본 파일 삭제
4. 동의 없음, wrong workspace, wrong catalog가 HTTP 0회이고 authoritative DB/WAL/SHM bytes와
   preflight 종료 뒤 Run directory entries를 바꾸지 않는지 확인
5. process 3이 SQLite를 다시 열고 exact ToolOutput을 두 번째 model request에 한 번 전달해 완료
6. model server와 workspace 제거
7. process 4가 Run ID만으로 같은 summary를 반환하고 journal head가 불변인지 확인
8. 별도 test에서 provider 응답 전 process를 종료하고 Reserved → Unknown 1회, 재시도 0회를 확인
9. 별도 fault test에서 실제 file read 뒤 outcome commit만 실패시켜 첫 process부터 exit `30`,
   Executing → EffectUnknown 1회와 offline 반복 resume의 journal 불변을 확인

Linux, macOS, Windows 전체 workspace test와 release build가 merge gate다. 실제 go50902/Qwen
실증은 같은 public 명령을 사용하는 후속 live gate로 분리한다.

## 제한과 후속 작업

- 이번 slice는 read-text 한 도구의 기술 prototype이지 Claude Code/Codex 기능 완성이 아니다.
- hostile same-UID process에 대한 OS sandbox, encrypted/sealed material, ACL hardening은 후속이다.
- Windows mapped-drive/network volume 탐지와 local-volume attestation은 후속이다. Drive-letter
  namespace를 통과했다는 사실만으로 local storage라고 간주하면 안 된다.
- Windows SQLite VFS에서 writable DB와 `-wal`/`-shm`/`-journal` entry를 handle-relative로 열어
  reparse 교체를 원자적으로 차단하는 hardening은 hostile same-UID 격리와 함께 후속이다. 현재는
  app-owned private Run directory와 open 전 정적 final-reparse 거부를 경계로 삼는다.
- Run directory 생성 중 전원 장애로 생긴 partial layout은 자동 덮어쓰거나 복구하지 않고 안전 오류로
  남긴다. 명시적 진단/정리 UX가 필요하다.
- Crash WAL 사전검증용 private snapshot은 정상 반환/error unwind에서 RAII로 지운다. SIGKILL 또는
  전원 장애가 정확히 그 구간에 발생하면 같은 private Run boundary 안에 숨김 scratch가 남을 수 있어
  stale-snapshot 진단/정리는 후속 작업이다.
- goal과 ToolOutput에는 민감정보를 넣을 수 있으므로 encrypted local state 설계 전에는 기밀 저장소로
  간주해야 한다.
- interactive approval UI, process/write/network adapters, MCP/XGEN compatibility adapters,
  memory와 WorkGraph 고도화는 이 계약 위에서 별도 Capability로 추가한다.
- managed deployment의 planner ID→endpoint identity, TLS/SPKI 또는 SSH host-key binding은 trusted
  provider registry에서 추가한다. 현재 prototype은 invocation마다 caller가 준 endpoint를 명시적으로
  승인하는 경계다.
