# ADR-0025: Capability-confined atomic write와 idempotent durable output

- 상태: 채택
- 기준일: 2026-09-01
- 적용 범위: `xgeny.fs/write-atomic@1.0.0`, workspace write authorization, public CLI run/resume
- 공개 protocol v0.1 schema 변경: `CapabilityExecution.durableToolOutput` optional boolean 추가
- protocol fixture: valid Capability Definition과 effect-class 제약 invalid fixture 추가
- core local store schema: 8 유지
- CLI material catalog schema: 1 유지, recipe canonical byte 상한 512 KiB로 확대

> 후속 상태: ADR-0026이 이 rename 기반 물리 commit을 내부 primitive로 분리하고 strict single-file
> `apply-patch`가 공유하도록 확장했다. 아래 patch 제외 설명은 ADR-0025 채택 당시 범위 기록이다.

## 문맥

ADR-0024까지의 제품은 workspace를 찾고 읽을 수 있지만 수정할 수 없다. 일반 coding-agent의 다음 최소
수직 슬라이스는 모델이 만든 UTF-8 file content를 workspace 경계 안에 저장하고, 승인 pause와 process
재시작을 거쳐도 정확히 한 번의 의도만 실행하는 것이다. 단순 `truncate + write`는 중단 시 부분 파일을
노출하고, 무조건 재시도는 effect 결과 acknowledgement가 사라진 경우 사용자 파일을 다시 덮어쓸 수
있다. 읽기와 동일한 flag로 쓰기를 승인하는 것도 권한 확대를 숨긴다.

## 결정

### 1. 첫 mutation은 `write-atomic` 하나로 제한한다

입력 계약은 다음 세 field가 모두 있어야 한다.

```json
{
  "path": "src/generated.rs",
  "content": "pub const GENERATED: bool = true;\n",
  "expectedDigest": null
}
```

- `content`는 최대 64 KiB의 UTF-8 bytes다. JSON Schema의 character 상한과 별도로 adapter가 byte 상한을
  다시 검사한다.
- `expectedDigest: null`은 대상이 없어야 하는 create-only 요청이다.
- 기존 file 교체는 모델이 앞서 읽은 exact lowercase `sha256:<64 hex>`를 넣어야 한다.
- 대상이 이미 원하는 content digest이면 stale precondition이더라도 `changed=false` 성공으로 처리한다.
  같은 semantic action의 물리적 재적용이 같은 결과로 수렴하기 때문이다.
- parent directory 생성, binary/range write, append, chmod, move, delete와 patch는 포함하지 않는다.

출력은 canonical workspace path, digest, byte size, `changed`만 포함한다. File content는 tool output과
Receipt에 중복하지 않는다. Core verifier는 output path를 같은 preopened root에서 다시 열어 실제 bytes의
digest와 size를 확인한 뒤 Artifact-bearing Receipt를 만든다.

### 2. 쓰기는 directory scope와 별도 사용자 동의를 모두 요구한다

`write-atomic`은 `filesystem.write` resource scope를 사용한다. Resolver의 portable path grammar와 physical
workspace handle은 read와 공유하지만 scope는 섞지 않는다.

- `--allow-dir` component boundary 안의 descendant만 write material과 permission resource로 인정한다.
- 추가 exact `--allow-file`은 계속 stat/read 전용이며 write 권한으로 승격되지 않는다.
- `--allow-write`가 없으면 exact action은 `write_approval_required`로 pending이다.
- `--allow-read`, `--allow-write`, `--allow-remote-model-egress`는 서로 독립적인 process-local 동의다.
- 각 승인 결과는 Run/Step/action/material/Instance에 결합된 one-shot authorization이다.

Exact-file legacy mode에는 write Capability를 광고하지 않는다. Directory mode는 기존 네 read Capability와
`write-atomic`을 함께 광고한다.

### 3. 물리 commit은 같은 parent의 temporary file과 rename으로 수행한다

Adapter `prepare`는 contract와 bounded material만 검증하며 filesystem mutation을 하지 않는다. Core가
`EffectExecutionStarted`를 durable하게 기록한 뒤 `execute`가 다음 순서를 수행한다.

1. preopened root에서 각 parent를 no-follow handle로 열고 Windows reparse point를 거부한다.
2. 대상이 regular non-reparse file인지 확인하고 현재 bytes digest로 precondition을 검사한다.
3. 같은 parent에 cryptographically random한 `.xgeny-write-<hex>.tmp`를 `create_new`로 만든다.
4. 기존 file이면 permission을 temporary file에 복사하고, content 전체 기록 뒤 file `sync_all`을 한다.
5. 대상 bytes digest나 permission이 처음 관찰과 달라졌으면 temporary file을 지우고 conflict로 끝낸다.
6. 같은 opened parent handle 안에서 temporary entry를 target entry로 rename한다.
7. Unix는 parent directory를 동기화하고, 모든 OS에서 target을 다시 열어 exact digest를 확인한다.

Rename 시도 전 오류는 fixed/redacted definite failure다. Rename을 시도한 뒤 오류, directory sync 오류 또는
read-after-write 불일치는 어느 쪽 상태인지 단정하지 않고 `Unknown`이다. Temporary name, ambient path와
content는 Debug/error/evidence digest에 들어가지 않는다.

이 계약은 partial file 노출을 막고 일반적인 editor-style concurrent change를 두 번의 digest 관찰로
거부하지만, 범용 filesystem에는 path entry와 content digest를 함께 비교·교체하는 portable linearizable
CAS primitive가 없다. 따라서 hostile concurrent writer에 대한 lock이나 transaction을 보장한다고
표현하지 않는다. Network filesystem/FUSE의 rename·durability 의미도 local filesystem과 같다고 가정하지
않는다.

### 4. Idempotent effect의 durable output은 Definition이 명시적으로 opt-in한다

기존 Core Receipt v2/tool-output profile은 ReadOnly에만 사용됐다. Write 후 검증과 다음 model turn 전달에는
typed output이 필요하므로 `CapabilityExecution.durableToolOutput` optional boolean을 추가한다.

- field가 없거나 `false`인 기존 Definition의 wire/digest와 v1 Receipt 동작은 그대로다.
- `Idempotent + durableToolOutput=true`만 Core Receipt v2와 bounded tool-output sidecar를 사용한다.
- WorkGraph, memory/SQLite store, Direct Executor와 verifier는 v1 idempotent replay도 계속 허용한다.
- `write-atomic` Definition만 현재 이 flag를 켠다.

Raw `content`는 approval pause 뒤 다른 process에서 복원되어야 하므로 private `materials.sqlite3` recipe에
저장한다. 최대 escaping까지 수용하도록 canonical recipe 상한은 512 KiB다. Database file은 기존처럼
private mode, final symlink 거부, JCS digest/revision 검증을 사용한다. At-rest encryption은 없으므로 Run
state root는 source code를 포함한 민감 데이터다.

### 5. 결과 불확정 상태에서는 자동 재실행하지 않는다

현재 adapter는 stable-key query를 구현하지 않고 Intent의 `SinkGuarantee`도 `None`이다.

- execution start 전 process 종료: 아직 effect가 없으므로 기존 material로 정상 재개할 수 있다.
- start marker 뒤 process 종료 또는 commit acknowledgement 유실: reopen 시 `EffectUnknown`으로 전환하고
  `effect_outcome_unknown`을 반환한다.
- Core는 이 상태에서 write를 자동 반복하지 않는다. 동일 content가 현재 존재하더라도 operator 확인 없이
  성공을 추론하지 않는다.
- 성공 outcome과 tool output이 durable한 뒤 Receipt commit 전 종료: effect는 반복하지 않고 read-only
  verifier만 재개한다.

향후 queryable reconciliation은 raw arguments 없이 exact desired state를 식별할 별도 durable commitment와
cleanup 규칙을 먼저 설계한 뒤 추가한다.

## 검증

- create/replace와 same-content idempotent success
- stale digest와 create collision의 no-mutation 결과
- 64 KiB bound, malformed digest/shape와 content/path Debug redaction
- temporary file cleanup, 기존 permission 보존과 post-commit digest 확인
- Unix leaf/intermediate symlink 탈출 거부
- Windows junction/reparse parent 탈출 거부
- protocol valid fixture offline schema/Rust round-trip과 non-idempotent durable-output invalid fixture
- `durableToolOutput` opt-in과 legacy idempotent v1 Receipt 회귀
- 별도 `--allow-write`, allow-dir component containment과 exact-file 비승격
- public child-process `plan → write approval pause → local-only resume/write → remote resume/completion`
- material DB process reopen, output에 content 미포함, workspace profile/digest 재검증
- lost effect outcome의 no blind retry 기존 runtime/store 회귀

Linux unit/E2E는 로컬에서 확인하고, Linux x86/ARM, macOS x86/ARM, Windows native runner의 전체
test/clippy/release build를 PR CI gate로 사용한다. Linux host의 MSVC cross-check는 native linker가 없어
제품 검증으로 세지 않으며, Windows replace semantics와 junction test는 Windows runner에서 실제 수행한다.

## 결과와 다음 단계

XGENy는 이제 workspace를 관찰하고, 별도 승인을 받은 한 file을 partial content 없이 생성·교체하며,
그 결과를 durable WorkGraph/Receipt와 다음 model turn에 연결할 수 있다. 다음 slice는 이 primitive 위에
`patch`를 추가해 small edit와 multi-file 변경 표현을 줄이고, 그 다음 `process execute`로 test/lint/build
결과를 다시 WorkGraph에 넣는다.
