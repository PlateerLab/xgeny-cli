# ADR-0026: Strict single-file patch와 공유 atomic commit

- 상태: 채택
- 기준일: 2026-09-01
- 적용 범위: `xgeny.fs/apply-patch@1.0.0`, filesystem adapter, workspace CLI composition
- 공개 protocol v0.1 schema 변경: 없음
- protocol fixture: valid Capability Definition 1개 추가
- core local store schema: 8 유지
- CLI material catalog schema: 1 유지

## 문맥

ADR-0025의 `write-atomic`은 안전한 create/replace primitive지만 작은 수정도 file 전체를 모델 출력과
material recipe에 반복한다. Coding agent에는 앞서 읽은 일부 문맥만 표현하면서도 사용자 변경을 추측해
덮어쓰지 않는 patch가 필요하다. 범용 unified diff parser와 fuzzy hunk placement를 첫 구현에 넣으면
line-ending, offset 보정, 중복 문맥과 부분 적용의 의미가 커지고 모델 편의를 위해 안전 경계가 흐려진다.

Patch 자체가 별도 orchestrator가 되거나 `write-atomic` Capability를 내부 호출해서도 안 된다. XGENy Core는
계속 단일 orchestrator이고, filesystem adapter 내부에서 두 mutation Capability가 같은 물리 commit
primitive만 공유해야 한다.

## 결정

### 1. Patch wire format은 exact contextual edit list다

입력은 기존 UTF-8 file 하나와 앞선 observation의 digest, 1개 이상 32개 이하의 edit를 갖는다.

```json
{
  "path": "src/lib.rs",
  "expectedDigest": "sha256:<64 lowercase hex>",
  "edits": [
    {
      "oldText": "pub fn answer() -> u32 {\n    41\n}",
      "newText": "pub fn answer() -> u32 {\n    42\n}"
    }
  ]
}
```

- `expectedDigest`는 null일 수 없다. 새 file은 계속 `write-atomic` create-only를 사용한다.
- `oldText`는 비어 있을 수 없고 `oldText == newText`인 no-op은 거부한다.
- 모든 old/new UTF-8 byte 합은 64 KiB 이하여야 한다. JSON Schema character limit과 별도로 adapter가
  byte limit을 다시 검사한다.
- 원본 file과 최종 file도 각각 64 KiB 이하여야 한다.

Unified diff text, line number, byte offset, regex, glob과 fuzzy match는 contract에 없다. Structured exact
format은 parser ambiguity를 줄이고 JSON Schema/typed material 경계에서 닫힌 shape를 유지한다.

### 2. 모든 edit는 같은 원본 observation에서 정확히 한 번만 해석한다

Adapter는 preopened workspace handle로 source를 읽고 digest를 비교한 뒤 다음 순서로 결과를 계산한다.

1. 각 `oldText`의 byte-exact occurrence를 원본에서 찾는다.
2. occurrence가 0개면 `context-missing`, 겹치는 occurrence를 포함해 2개 이상이면
   `context-ambiguous` definite failure다.
3. 서로 다른 edit range가 한 byte라도 겹치면 `overlapping-edits` definite failure다.
4. 검증된 range를 뒤에서 앞으로 교체해 하나의 desired UTF-8 byte sequence를 만든다.

앞 edit가 만든 text를 뒤 edit의 문맥으로 사용하지 않으므로 edit 순서가 match 의미를 바꾸지 않는다.
부분 적용은 없다. 어느 edit든 검증에 실패하면 filesystem mutation을 시작하지 않는다. CRLF, trailing
newline, Unicode와 지정하지 않은 모든 byte는 그대로 보존한다.

### 3. 물리 commit은 `write-atomic`과 같은 내부 primitive를 사용한다

기존 rename 기반 구현을 capability-specific adapter에서 `atomic_commit` 내부 모듈로 분리한다. Patch가
검증된 desired bytes를 만든 뒤 이 primitive를 정확히 한 번 호출한다.

- parent와 leaf no-follow, Windows reparse 거부
- expected digest와 두 번의 target/permission observation
- 같은 parent의 random create-new temporary file
- 기존 permission 복사, file sync, rename, Unix directory sync, read-back digest 확인

이 공유는 Rust 내부 코드 재사용일 뿐 Capability 호출이나 harness nesting이 아니다. Core에는 여전히
`apply-patch` Intent 하나, approval 하나, execution start 하나와 Receipt 하나만 존재한다.

### 4. 권한·durable output·불확정 복구는 기존 mutation 규칙을 따른다

`apply-patch`는 `filesystem.write`, `Idempotent`, 새 plan에서는
`LocalSyncOnceOccurrenceV1`, idempotency key와
`durableToolOutput=true`를 사용한다.

- `--allow-dir` component descendant만 후보와 실제 resource로 허용한다.
- 추가 exact `--allow-file`은 patch 권한으로 승격되지 않는다.
- `--allow-write`가 없으면 실행 전 `write_approval_required`로 멈춘다.
- raw old/new text는 approval 재개용 private `materials.sqlite3`에만 저장한다.
- tool output은 `path`, `digest`, `byteSize`, `changed`, `editCount`만 보존하며 patch text를 반복하지 않는다.
- verifier는 같은 root에서 target을 다시 읽어 digest와 size를 확인한다.

Capability catalog와 limits가 늘었으므로 workspace execution/route/materializer/approval/constraint profile은
v2에서 v3로 올린다. 기존 v2 Run을 새 의미로 조용히 재개하지 않고 configuration mismatch로 닫는다.

Precondition failure나 문맥 failure는 target을 바꾸지 않은 definite failure다. Rename 시도 뒤 상태를
단정할 수 없는 오류는 `Unknown`이며 Core는 자동 재실행하지 않는다. 반복 실행이 file에 같은 edit를
두 번 적용하지는 않지만, 이미 반영된 patch를 source 없이 성공으로 추론하지도 않는다. 향후 stable-key
reconciliation은 desired-state commitment를 별도로 설계해야 한다.

## 검토한 대안

- **Unified diff + fuzzy placement**: 모델 친화적이지만 첫 trusted parser로는 의미와 공격 표면이 크다.
- **line/byte range edit**: 결정적이지만 모델이 정확한 offset을 계산해야 하고 CRLF/Unicode 오류가 쉽다.
- **adapter에서 `write-atomic` Capability 재호출**: approval, Receipt와 recovery 경계를 중첩하므로 거부한다.
- **multi-file atomic patch**: 일반 filesystem에서 portable transaction을 보장할 수 없어 별도 WorkGraph
  coordination 연구로 남긴다.

## 검증

- 단일/다중 exact edit, Unicode·CRLF·미지정 byte 보존
- stale digest, missing/ambiguous/overlapping context의 no-mutation
- overlapping occurrence 탐지와 32 edit/64 KiB input·result limit
- raw path/text Debug·error·output 비노출
- 공유 atomic commit의 permission 보존, temporary cleanup, symlink/junction 탈출 차단
- protocol fixture offline schema/Rust round-trip
- allow-dir containment, exact allow-file 비승격과 별도 write approval
- public child-process `plan → approval pause → local-only patch → remote completion`
- output/Receipt에 patch text 미포함과 process 재개 material 복원

Linux unit/E2E와 전체 workspace gate를 로컬에서 수행하고 Linux x86/ARM, macOS x86/ARM, Windows native
CI의 test/clippy/release build를 병합 조건으로 사용한다.

## 결과와 다음 단계

XGENy는 안전한 whole-file create/replace와 strict small edit를 동일한 물리 보장 아래 제공한다. 다음
vertical slice는 bounded `process-execute`로 test/lint/build 결과를 WorkGraph와 다음 model turn에 넣는
것이다. Shell 문법, interactive PTY와 background daemon은 첫 process slice에 포함하지 않는다.
