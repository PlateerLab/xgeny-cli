# ADR-0024: Workspace filesystem discovery와 durable dynamic material

- 상태: 채택
- 기준일: 2026-08-31
- 적용 범위: `list-directory`·`stat`·`search-text`, workspace read authorization, public CLI run/resume
- 공개 protocol v0.1 schema 변경: 없음
- protocol fixture: Capability Definition 3개 추가
- core local store schema: 8 유지
- CLI material catalog schema: 별도 `materials.sqlite3` schema 1 추가

## 문맥

ADR-0023의 public prototype은 사용자가 `--allow-file`로 정확히 알려준 파일만 모델이 읽을 수 있다.
따라서 모델이 프로젝트 구조를 보고 관련 코드를 검색한 뒤 필요한 파일을 고르는 일반적인 coding-agent
흐름을 만들 수 없다. 단순히 ambient path walk를 추가하면 workspace 밖 symlink 탈출, 무제한 directory
enumeration, 검색 결과 폭증과 재시작 뒤 검색어 복원 문제가 생긴다.

이번 slice의 사용자 시나리오는 다음으로 고정한다.

> 사용자가 명시적으로 허용한 workspace directory 안에서 모델이 목록 확인, literal text 검색, metadata
> 확인과 기존 UTF-8 file read를 여러 planning turn에 걸쳐 수행하고, process가 중간에 끝나도 정확히 같은
> invocation material로 재개한다.

## 결정

### 1. 기존 exact-file mode를 보존하고 directory mode를 명시적으로 연다

`--allow-dir`가 없으면 기존 RC2 경로를 그대로 사용한다.

- `--allow-file`은 하나 이상의 exact file을 요구한다.
- Capability catalog에는 `xgeny.fs/read-text@1.0.0`만 나타난다.
- 기존 allow-file material provider, execution profile digest와 Run 예산을 유지한다.
- 별도 material catalog를 만들지 않는다.

`--allow-dir`를 하나 이상 주면 workspace discovery mode가 된다.

- `--allow-dir .`은 선택한 workspace root 전체의 read-only discovery를 뜻한다.
- 하위 directory와 exact `--allow-file`을 함께 지정할 수 있다.
- 같은 component boundary의 directory 자체와 descendant만 승인한다. `src`가 `src-secret`을 승인하지
  않는다.
- Capability catalog에는 `list-directory`, `stat`, `search-text`, `read-text` 네 Definition과 각각의
  root-bound Instance가 나타난다.
- Capability Definition은 ID/version별 정적 계약으로 유지한다. Input schema에는 portable `.` root와
  relative path 문법만 들어가며 Run별 허용 경로를 삽입하지 않는다.
- Caller가 허용한 directory/file 범위는 최대 16 KiB의 bounded `PlanningContext.planningConstraints`
  항목으로 별도 전달한다. Catalog는 file/directory 합계 64개까지다. Constrained provider prompt는 이를
  후보 plan 제한으로 따르되 권한 grant로 취급하지 않도록 명시한다. 실제 권한은 catalog digest,
  materializer, policy/admission이 독립적으로 강제한다. Empty legacy context는 이 optional field를
  직렬화하지 않아 기존 PlanningContext v2 payload를 보존한다.
- Dynamic path는 미래 tool output을 placeholder로 참조할 수 없다. Discovery 전용 constrained provider
  profile은 모델에 현재 context에서 complete literal argument가 확정된 Step 하나만 반환하도록 요구하고,
  후속 path/value가 필요하면 receipt-completed tool output이 다음 planning turn에 들어올 때까지
  기다리게 한다. Core의 multi-Step DAG 계약과 기존 exact-file provider profile은 바꾸지 않는다.

Model egress와 local read 권한은 계속 분리한다. `--allow-read`가 없으면 exact normalized resource에 대한
승인은 pending이다. Flag가 있으면 catalog 안에서 선택된 각 action에 별도 one-shot 승인을 발행한다.
`--allow-remote-model-egress`가 없으면 새 provider call을 시작하지 않는다. Exact-file host policy와
workspace-descendant host policy는 서로 다른 profile digest를 사용한다.

### 2. Workspace root는 별도 portable logical resource다

기존 portable path grammar는 file과 하위 directory에 그대로 적용한다. 추가로 resolver가 `.`을 다음
root token으로 정규화한다.

```text
resolve(".") = "workspace:primary"
resolve("workspace:primary") = "workspace:primary"
```

Root token은 list/stat/search의 대상이 될 수 있다. Legacy `--allow-file .`은 계속 거부한다. 다른
Workspace ID, absolute path, parent traversal, backslash/drive/UNC/ADS, reserved Windows name과
non-portable component는 모두 거부한다.

각 Instance는 같은 preopened directory handle을 사용하지만 operation binding은 exact하게 분리한다.

```text
builtin://core-os/filesystem/workspaces/primary + listDirectory
builtin://core-os/filesystem/workspaces/primary + stat
builtin://core-os/filesystem/workspaces/primary + searchText
builtin://core-os/filesystem/workspaces/primary + readText
```

Router와 adapter/verifier registry는 capability/version/instance/binding이 모두 일치할 때만 dispatch한다.

### 3. 세 query Capability는 bounded read-only observation이다

`xgeny.fs/list-directory@1.0.0`은 한 directory의 직계 entry를 portable path 순으로 반환한다.

- 한 directory에서 최대 4,096 entry를 scan한다. 초과하면 partial subset을 내지 않고 실패한다.
- 최대 512 entry를 반환하고 나머지가 있으면 `truncated=true`다.
- file은 byte size, directory/other는 `null` size를 반환한다.
- symlink, junction/reparse 또는 안정적으로 열 수 없는 entry는 따라가지 않고 `other`로 표시한다.

`xgeny.fs/stat@1.0.0`은 같은 opened handle에서 regular file size 또는 directory kind만 반환한다.
Symlink와 다른 특수 file은 실패한다. 수정 시각, owner, ACL과 platform-specific mode는 이번 portable
계약에 넣지 않는다.

`xgeny.fs/search-text@1.0.0`은 directory 아래를 portable path 순서로 재귀 탐색하는 case-sensitive
literal UTF-8 검색이다.

- query는 control character 없는 1..64 Unicode scalar이면서 1..256 UTF-8 bytes다.
- raw dirent를 읽는 시점부터 전역 최대 4,096 entry 예산을 소비한다. 재귀 stack이 보유하는 이름 수도
  이 전역 예산을 넘지 않는다.
- file당 256 KiB, 실제로 읽은 aggregate byte 기준 합계 8 MiB까지만 관찰한다. Invalid UTF-8 또는
  읽는 중 커진 file도 이미 읽은 byte를 예산에서 소비하며 aggregate 한도에서는 전체 traversal을 멈춘다.
- 최대 128 match와 match당 최대 512-byte preview를 반환한다.
- `.git`, `.hg`, `.svn`, invalid UTF-8, oversized 또는 열 수 없는 file은 읽지 않고
  `truncated=true`로 불완전성을 표시한다.
- limit에 도달하면 정렬된 traversal의 현재 지점에서 멈추고 `truncated=true`다.

모든 query output은 digest를 포함한 전체 RFC 8785 JCS 기준 최대 128 KiB다. Entry/match가 이 byte
한도를 넘기기 전 결과를 자르고 `truncated=true`로 표시한다. Digest 자체는 digest field를 제외한
payload의 JCS bytes를 SHA-256으로 commit한다.
Verifier는 durable output의 exact shape, 정렬, portable path, limit와 digest/evidence를 다시 확인하고
fixed-name JSON Artifact를 만든다. Mutable path를 다시 열어 검증하지 않는다.

### 4. Dynamic argument는 Run 전용 material catalog에 먼저 durable하게 저장한다

Search query와 directory descendant path는 미리 열거한 allow-file catalog로 복원할 수 없다. Discovery
mode의 PlanMaterializer는 accepted plan commit 전에 exact normalized arguments를 Run directory의
`materials.sqlite3`에 저장한다.

- `materials.sqlite3`는 XGENy binary에 link된 bundled SQLite library를 사용하며 별도 설치나 daemon이
  필요 없다.
- core `run.sqlite3` schema와 table을 수정하지 않는다.
- recipe는 run/step/proposal/capability/material digest와 arguments를 JCS로 묶는다.
- opaque reference ID와 revision은 recipe JCS의 SHA-256에서 파생한다. Raw path/query는 reference,
  WorkGraph event, Receipt와 manifest에 없다.
- Run directory와 file은 private permission으로 만들고 final symlink를 거부한다.
- reconstruction은 DB row, JCS canonical bytes, reference/revision, Run ID와 현재 authorization을 모두
  재검사한 뒤 Core의 ordinary schema/resource/action digest 검증으로 넘긴다.
- material catalog schema version, recipe domain/format version은 workspace execution profile에 직접
  commit한다. 모든 SQLite connection은 `trusted_schema=OFF`로 연다.
- completed Run의 summary replay는 workspace, provider와 material catalog 없이 가능하다.

Raw path와 search query는 private material DB에, successful tool output은 기존 private Run DB에
의도적으로 존재한다. 현재 at-rest encryption은 제공하지 않으므로 state root 전체를 민감 데이터로
취급한다.

### 5. Discovery mode만 다중 관찰 예산을 사용한다

Legacy mode의 기존 예산은 바꾸지 않는다. Discovery mode manifest는 최대 model turn 8, provider call
reservation 16, planned Step 8, tool call 8과 context 512 KiB를 고정한다. Limit은 무한 agent loop를
허용하지 않는다. 128 KiB query-output cap은 예상된 list/search/stat/read 연속 실행이 context 안에
남도록 하고, context budget gate는 추가적인 fail-closed 상한으로 유지한다.

## 검증

- protocol fixture 3개 schema validation/Rust round-trip/offline bundle
- root token idempotence와 다른 workspace/path traversal 거부
- list 정렬·size·root-relative output
- stat file/directory와 symlink 거부
- recursive literal search, 전역 entry/aggregate-byte budget, Unicode column, invalid UTF-8 incomplete
  표시와 symlink 탈출 거부
- 전체 경로 정렬의 prefix collision과 128 KiB canonical output truncation
- output digest/order/size tamper verifier 거부
- authorization의 exact component containment와 query shape/version 거부
- material recipe의 process reopen 복원, missing/revision/row tamper와 symlink catalog 거부
- public child process의 `list → search → stat → read → completion → offline replay`
- approval pause 뒤 별도 process에서 dynamic search material 복원·실행·다음 model turn 전달
- resume의 allow-dir catalog 변경과 legacy/discovery mode 전환 거부
- immutable planning schema의 portable root 규칙과 별도 digest-bound caller constraint 전달
- constrained prompt의 concrete-literal 단일 Step 요구, live accepted Plan의 단일 Step 확인과 기존 exact
  profile digest 불변
- 기존 allow-file public E2E와 SQLite schema-8 replay 회귀

Linux, macOS와 Windows CI가 같은 portable test를 실행한다. Unix symlink test와 Windows
junction/reparse test의 platform-specific 보장은 해당 runner에서 실행한다.

## 비목표와 잔여 위험

- regex, glob, ignore-file language와 user-configurable search exclusion
- binary/range/chunk search, file watch와 incremental index
- modification time, owner, permissions/ACL와 stable file identity stat
- hard link, mount point/FUSE/network filesystem, hostile same-UID process 격리
- encrypted local state와 Windows ACL hardening
- write/patch/process/network/browser/MCP/XGEN adapters
- interactive approval UI와 per-operation allow prompt

## 결과

XGENy는 사용자가 파일명을 미리 알려주지 않아도 명시적으로 허용된 workspace를 스스로 관찰할 수 있다.
이 기능은 Core나 XGEN에 filesystem/SQLite 의존성을 추가하지 않고 leaf adapter와 CLI composition 안에
머문다. 다음 vertical slice는 이 read-only 관찰 위에 `write-atomic/patch`를 올리고, 그 다음
`process execute`로 수정 결과의 test/lint/build를 검증하는 것이다.
