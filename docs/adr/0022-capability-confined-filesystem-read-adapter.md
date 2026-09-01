# ADR-0022: Capability-confined filesystem read adapter

- 상태: 채택
- 기준일: 2026-08-30
- 적용 범위: local workspace resource resolution, filesystem read adapter/verifier, CLI driver 회귀
- 공개 protocol v0.1 schema 변경: 없음
- local store schema: 8 유지

> 후속 ADR-0024가 list/stat/search를 위해 `.` workspace-root logical resource를 추가했다. 기존
> read-text의 regular-file-only 실행 의미와 legacy allow-file의 root 거부는 유지한다.
> 후속 ADR-0029가 file-ID identity가 아니라 Core-derived Run/Step occurrence를 도입해, 완료된 read 뒤
> 같은 logical resource를 별도 승인 identity로 다시 관찰할 수 있게 했다.

## 문맥

[ADR-0021](0021-durable-completion-output-and-schema-8.md)까지 실제 tool output을 다음 model
turn에 전달하고 최종 summary를 재시작 뒤 복원하는 저장 의미는 닫혔다. 그러나 기존 CLI driver
수직 테스트의 파일 읽기는 process-local fake였다. `std::fs::canonicalize(root.join(path))` 뒤
prefix를 비교하고 다시 파일을 여는 구현은 검사와 사용 사이에 symlink나 directory entry가
바뀌는 TOCTOU를 만들며, Windows drive/UNC/ADS/device 이름 의미도 같은 방식으로 다룰 수 없다.

이번 slice의 사용자 시나리오는 다음 한 문장으로 고정한다.

> 승인된 한 workspace의 bounded UTF-8 일반 파일을 local adapter가 실제로 한 번 읽고, 그
> 관찰을 Receipt와 durable output에 결합해 다음 planning turn 및 SQLite 재개에 전달한다.

이 결정은 아직 사용자용 `xgeny run/resume` command나 live `go50902` tool call을 열지 않는다.

## 결정

### 1. 제품 adapter는 독립 leaf crate다

새 `xgeny-adapter-filesystem` crate가 기존 공개 port만 구현한다.

```text
trusted composition root
  └─ preopened WorkspaceRoot(cap_std::fs::Dir)
       ├─ WorkspaceResourceResolver
       ├─ ReadTextAdapter -> EffectAdapter
       └─ ReadTextVerifier -> EffectVerifier

XGENy Core
  ├─ PermissionRequestResolver
  ├─ DirectExecutor
  ├─ ToolOutputRecord
  └─ VerificationRunner -> ExecutionReceipt
```

Core, WorkGraph, protocol과 local store는 `cap-std`, OS path 또는 filesystem adapter에
의존하지 않는다. 비제품 write marker인 `xgeny-adapter-reference`도 이름·의미를 바꾸지 않는다.

구현은 Bytecode Alliance의 `cap-std 4.0.3`과 `cap-fs-ext 4.0.3`을 사용한다. `cap-std::Dir`은
ambient path가 아니라 열린 directory capability를 기준으로 접근하며 Linux, macOS와 Windows를
지원한다. `cap-std` 자체는 untrusted in-process Rust code의 sandbox가 아니므로 그런 보장은
주장하지 않는다. 참고: [cap-std capability model](https://github.com/bytecodealliance/cap-std/blob/main/README.md),
[cap-fs-ext `open_dir_nofollow`](https://docs.rs/cap-fs-ext/latest/cap_fs_ext/trait.DirExt.html).

### 2. Ambient authority는 composition root에서 한 번만 사용한다

`WorkspaceRoot::open_ambient`만 사용자가 고른 root path를 받는다. 성공 뒤 adapter에는 절대경로가
아니라 `Arc<Dir>` capability만 전달한다. Root path와 OS error는 구조체, `Debug`, fixed error,
journal, Receipt와 tool output에 넣지 않는다.

호스트는 non-secret `WorkspaceId`와 실제 root의 매핑을 durable Run이 존재하는 동안 안정적으로
유지해야 한다. Resolver는 이 ID를 경로 authority에 넣고 Instance binding도 다음처럼 root별로
분리한다.

```text
workspace:primary/README.md
builtin://core-os/filesystem/workspaces/primary + readText
```

따라서 다른 ID의 root나 기존 generic fixture binding으로는 exact adapter dispatch가 되지 않는다.
같은 ID를 다른 root에 재사용하는 trusted-host 구성 오류까지 crate가 알아낼 수는 없다. Public CLI는
Run 재개 시 ID-to-root mapping을 검증해야 하며 이를 생략한 composition은 production-safe로
간주하지 않는다. 절대 root path를 durable identity로 저장하지는 않는다.

### 3. Resolver 결과는 idempotent logical authority다

기존 `PermissionRequestResolver`는 resolver 결과를 normalized `/path` argument에 다시 넣고
planning preflight와 final 단계에서 재호출한다. 따라서 다음 등식이 반드시 성립한다.

```text
resolve("README.md")
  = "workspace:primary/README.md"
resolve("workspace:primary/README.md")
  = "workspace:primary/README.md"
```

이 문자열은 파일 inode가 아니라 exact authorization comparison을 위한 logical identity다. Adapter는
같은 Workspace ID를 확인한 뒤 suffix를 별도로 다시 검증하고 descriptor-relative로 연다.

Portable path grammar는 UTF-8과 `/` separator만 허용한다.

- 전체 1..4096 bytes, component 1..255 bytes, 최대 256 components
- empty component, `.`과 `..`, absolute/root/drive/UNC 형태 거부
- NUL/control, backslash, colon/ADS, `< > " | ? *` 거부
- leading ASCII space, trailing ASCII space/dot 거부
- 대소문자 무관 `CON/PRN/AUX/NUL/CLOCK$/CONIN$/CONOUT$`, `COM0..9`, `LPT0..9`,
  superscript `⁰/¹/²/³`와 extension 형태 거부

Unicode normalization과 case folding은 하지 않는다. Linux 또는 case-sensitive APFS에서 서로 다른
파일을 하나의 권한으로 잘못 합치지 않기 위해서다. 그 결과 case-insensitive/normalizing filesystem의
물리 alias가 서로 다른 logical identity가 될 수 있다. 이 slice는 root confinement를 보장하지만
file-ID-stable permission이나 duplicate-action identity까지 보장하지 않는다.

### 4. 실제 open은 durable Started 뒤 component별 no-follow로 수행한다

`EffectAdapter::prepare`는 exact capability/version/root-bound binding, ReadOnly/no-key 의미와 exact
`{"path": ...}` material만 확인한다. 파일 open, metadata와 read는 하지 않는다.

`PreparedReadText::execute`는 Core가 `EffectExecutionStarted`를 commit한 뒤 다음 순서로 실행한다.

```text
clone preopened root handle
  -> each intermediate component: open_dir_nofollow(component)
  -> reject Windows reparse attribute on opened directory handle
  -> leaf: read + FollowSymlinks::No + nonblock open
  -> metadata from that same opened leaf handle
  -> regular-file and Windows reparse checks
  -> max+1 bounded read from that same handle
```

검사 뒤 ambient path로 재open하지 않는다. Intermediate와 leaf symlink는 workspace 내부를 향하더라도
모두 거부한다. Windows는 no-follow가 막는 symlink/junction 외에도 열린 handle의
`FILE_ATTRIBUTE_REPARSE_POINT`를 검사해 cloud placeholder 등 다른 reparse file도 보수적으로
거부한다. 이 선택은 일부 Windows 동기화 파일을 읽지 못하는 호환성 비용이 있다.

POSIX FIFO/device의 open block을 줄이기 위해 leaf는 nonblocking으로 연 뒤 regular-file 여부를
검사한다. 일반 파일 read의 timeout/cancellation은 synchronous filesystem API가 제공하지 않으므로
이번 보장 밖이다. 이 때문에 root-bound 제품 Instance는 `sync=true`, `task=false`,
`cancellable=false`, `idempotencyQuery=false`만 advertise한다. Definition이 더 넓은 가능성을
표현할 수는 있지만 adapter와 verifier는 실제 구현보다 넓은 feature claim을 거부한다.

### 5. 한 번의 관찰은 64 KiB strict UTF-8 output으로 닫는다

`ReadTextLimits`의 hard maximum은 raw 64 KiB다. Metadata length는 allocation hint일 뿐 성공 판정에
사용하지 않고, 같은 열린 handle을 `max + 1` bytes까지 읽는다. 파일이 커지는 race에서도 allocation과
read가 bounded이며 한 byte 초과하면 실패한다. 그 뒤 `String::from_utf8`로 strict UTF-8을 확인한다.

64 KiB는 Core `ToolOutputRecord`의 JCS 1 MiB 상한뿐 아니라 control-heavy text의 JSON escaping이
최악인 경우에도 현재 context envelope를 포함해 Core 512 KiB PlanningContext 상한 안에 한 output이
머물도록 선택했다. PR3의 MVP composition 회귀는 effective Run budget도 512 KiB로 설정하고 그
동일한 budget과 비교한다. 더 작은 context budget을 선택한 host에서는 유효한 큰 read가 실행된 뒤
다음 planning turn 직전에 `ContextBudgetExceeded`로 pause할 수 있다. 큰 파일은 상한을 높이지 않고
후속 range/chunk capability로 다룬다.

성공 output은 기존 public Definition과 정확히 같다.

```json
{
  "content": "exact UTF-8 bytes",
  "digest": "sha256:<digest of the exact file bytes>"
}
```

Execution evidence와 Artifact digest는 exact content bytes의 SHA-256이고 Artifact size도 UTF-8 byte
길이다. ToolOutput/Receipt `outputDigest`는 이 두 field 전체를 JCS로 canonicalize한 별도 digest다.

Open/non-regular/oversize/read/UTF-8 실패는 path나 OS error를 포함하지 않는 fixed-class digest와
`Failed` observation으로 닫는다. ReadOnly read는 외부 mutation을 수행하지 않으므로 반환된 I/O
실패 자체는 definite failure다. 반면 process가 read 뒤 outcome commit 전에 죽으면 기존 durable
runtime 규칙대로 `Executing -> Unknown -> manual`이며 자동 재읽지 않는다.

### 6. Verifier는 mutable path를 다시 열지 않는다

제품 verifier는 `VerificationRequest.tool_output()`의 durable exact output만 읽는다.

1. exact two-field object, canonical digest spelling과 64 KiB hard maximum을 확인한다.
2. `content.as_bytes()`의 SHA-256을 다시 계산한다.
3. output의 `digest` 및 execution evidence와 비교한다.
4. `output_schema`와 `postcondition` rule을 positional report로 반환한다.
5. `VerifierOutputDigest`에는 `ToolOutputRecord.output_digest()`를 사용한다.
6. fixed logical Artifact descriptor에 content byte size/digest를 넣는다.

검증 시 파일이 변경·삭제돼도 당시 관찰은 바뀌지 않는다. Receipt는 현재 path 상태가 아니라 durable
관찰을 증명한다. Raw path를 Artifact name으로 넣지 않는다.

### 7. Local read 승인과 model egress는 별개다

`filesystem.read` 승인은 local process가 workspace 파일을 읽는 권한이다. 그 content를 remote
OpenAI-compatible endpoint나 XGEN LLM에 보내는 권한을 의미하지 않는다. 이번 crate는 provider와
네트워크에 의존하지 않는다. Public CLI composition은 provider 위치와 data boundary를 보고 별도
egress 결정을 해야 한다.

## 실제 검증

새 crate unit/OS regression은 다음을 실행한다.

- resolver idempotence, wrong Workspace ID/scope와 portable invalid path 전체 거부
- case/Unicode byte-exact identity 보존
- exact UTF-8 bytes/digest, max와 max+1, invalid UTF-8, missing/directory
- 열린 leaf handle 뒤 path entry 교체 시 원래 handle bytes만 관찰
- Unix leaf/intermediate symlink와 Unix socket 거부
- Windows leaf/intermediate junction/reparse 거부
- root/path/content의 `Debug`와 fixed error 비노출

CLI SQLite vertical regression은 fake planner만 유지하고 제품 resolver/adapter/verifier를 사용한다.

```text
relative README.md
  -> approval + root-bound normalized identity
  -> real file read exactly once
  -> durable ToolOutputRecord
  -> Artifact-bearing Receipt
  -> next PlanningContext exact content
  -> durable CompletionOutputRecord
  -> source file deletion
  -> SQLite close/reopen
  -> model/adapter/verifier additional calls = 0
```

Linux, macOS와 Windows의 `cargo test --workspace --locked`가 동일 gate를 실행한다.

## 비목표와 잔여 위험

- public `xgeny run/resume`, interactive approval UI와 packaged install smoke
- live `go50902` model-to-tool-to-next-turn 실행
- 같은 Workspace ID를 다른 root에 재사용하는 trusted-host 구성 오류의 자동 탐지
- hard link가 root 밖 inode와 같은 경우, bind mount/mount point, FUSE/network filesystem 의미
- hostile same-UID process, kernel/driver와 untrusted in-process plugin 격리
- concurrent in-place write 중 coherent/atomic file snapshot
- filesystem timeout, cancellation, streaming, binary, range/chunk read
- case/Unicode alias의 stable physical file identity
- ReadOnly crash 뒤 자동 retry와 tool failure를 다음 model turn에 전달하는 typed error output
- remote model egress 승인

## 결과

XGENy의 기존 fake ReadOnly 경계를 실제 3-OS workspace file 관찰로 대체할 제품 leaf adapter가 생겼다.
경로 권한, 실행 순서, output durability와 verifier 의미는 Core에 filesystem 의존성을 넣지 않고
연결된다. 다음 slice는 이 composition을 public CLI lifecycle과 실제 `go50902` 두 planning turn에
연결하고, workspace ID mapping 및 remote egress 결정을 사용자-facing configuration으로 닫는다.
