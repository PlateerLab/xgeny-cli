# Preopened Reference Adapter Conformance

- 기준일: 2026-08-29
- 상태: 비제품·비배포 public-port 참조 구현
- 공개 protocol v0.1 변경: 없음
- local store schema: 4

## 목적

`xgeny-runtime` 내부 fake만으로 검증하던 Direct Executor 계약을 워크스페이스의 별도 crate가 공개 API만 사용해 실제 OS I/O까지 지킬 수 있는지 확인한다. 사용자 시나리오는 다음 한 문장으로 제한한다.

> 신뢰된 host가 격리된 일반 파일을 미리 열어 주면, 참조 adapter는 durable Started 이후에만 비민감 evidence marker를 한 번 기록하고 결과 기록이 유실된 재시작에서는 파일을 다시 쓰지 않는다.

`xgeny-adapter-reference`는 `publish = false`다. 제품용 filesystem capability, CLI 명령 또는 plugin SDK가 아니다.

## 경계

```text
trusted test composition root
  ├─ create isolated regular file
  ├─ preopen File handle
  └─ register exact Instance binding
                  │
opaque targetRef + marker
  -> Admission / material digest
  -> DirectExecutor.prepare           no target mutation
  -> durable EffectExecutionStarted
  -> PreparedAdapterInvocation.execute
       seek -> truncate -> write -> sync -> bounded read-back
  -> evidence digest
  -> durable outcome -> Validating
  -> PreopenedMarkerVerifier bounded read-only 확인
  -> Core ExecutionReceipt + terminal event atomic commit
```

호출 material에는 OS path가 없고 allow-listed opaque `targetRef`만 있다. Adapter 생성자는 신뢰된 host가 넘긴 `std::fs::File`이 일반 파일인지 metadata로 확인하지만 파일 내용은 읽거나 쓰지 않는다. `prepare`는 exact capability·contract·Instance·binding·effect·idempotency key와 argument shape/size를 확인하고 one-shot session을 만든다. 실제 파일 변경은 core가 Started event를 commit한 뒤 `execute`에서만 일어난다.

이 구조는 호출자가 임의 경로를 adapter에 주입하지 못하게 하는 composition 예시다. 같은 process의 신뢰된 host가 어떤 handle을 넘기는지 통제하지 못하며 hostile adapter를 격리하는 sandbox도 아니다.

## 기록 내용과 redaction

파일에는 raw marker나 target reference 대신 RFC 8785 canonical evidence record만 기록한다.

- domain
- effect ID
- semantic action digest
- invocation material digest
- Instance ID
- executable binding digest
- idempotency key

기록 byte의 SHA-256을 adapter outcome의 `evidence_digest`로 전달한다. 이는 실제 파일 byte와 연결되는 실행 evidence이며 protocol Receipt가 아니다. 이후 별도 verifier가 같은 preopened handle을 read-only로 다시 읽고 digest를 비교한다. Core는 그 결과와 admission provenance로 protocol `ExecutionReceipt`를 만들고 SQLite schema 4에 terminal event와 함께 저장한다.

Reference adapter에는 typed tool output이 없으므로 Receipt `outputDigest`는 canonical empty object의 고정 digest다. Artifact 또는 실제 output body를 제공한다는 뜻이 아니다.

Marker, raw/canonical target reference와 OS 오류 문자열은 adapter `Debug`, journal, SQLite 및 닫힌 WAL/SHM artifact에 기록하지 않는다. 다만 action/material digest는 공개 SHA-256 commitment이므로 저엔트로피 입력에 대한 추측을 막는 암호화나 keyed commitment가 아니다. 실제 secret 또는 credential을 marker로 전달하면 안 된다.

## 장애 의미

| 시점 | durable 상태 | 파일 | 재시작 동작 |
|---|---|---|---|
| prepare 또는 Started commit 전 실패 | `IntentCommitted` | 변경 없음 | 같은 material로 새 session 준비 가능 |
| Started 뒤 I/O 실패·부분 쓰기·검증 실패 | `EffectUnknown` | 없거나 부분 기록 가능 | 확정 실패로 축소하지 않음 |
| 파일 반영 뒤 outcome commit 유실 | `Executing` | evidence 존재 | adapter/provider 없이 `EffectUnknown`, adapter execute 0회 |
| 성공 outcome commit | `Validating` | evidence 존재 | verifier만 재개, adapter execute 0회 |
| 검증·Receipt commit | `Completed` 또는 `Failed` | 변경 없음 | terminal no-op, verifier/execute 0회 |

파일은 host가 미리 생성하므로 `sync_all`은 파일 내용 flush만 시험한다. 디렉터리 entry durability, 전원 차단, 디스크·filesystem 손상 또는 hardware write cache까지 보장하지 않는다.

## 실행 가능한 검증

Public-port integration test와 adapter 내부 fault-injection unit test는 다음을 확인한다. Integration test는 runtime의 private 구현에 접근하지 않는다.

- path처럼 보이는 target reference와 잘못된 limit을 고정 오류로 거부
- reference Definition/Instance를 bundled protocol schema로 offline 재검증
- exact full binding 외의 operation/protocol fallback 0회
- marker size 실패 시 Started와 I/O 0회
- Started commit 전 파일 변경 0회, 성공 commit 전 실제 I/O 완료
- 성공 evidence byte와 reported digest 일치
- 성공 outcome 뒤 read-only verifier와 Core-owned succeeded Receipt
- effect 이후 파일 tamper 시 failed Receipt와 파일 재작성 0회
- `Validating` SQLite close/reopen 뒤 verifier-only 재개
- read-only handle의 OS 오류를 고정 `ResponseUnverifiable` unknown으로 축소
- 내부 fault target으로 주입한 truncate 뒤 partial write와 write 뒤 sync 실패를 고정 unknown으로 축소
- Started commit 실패 뒤 파일 무변경 및 안전한 재시도
- outcome commit 유실 뒤 in-process recovery의 execute 0회
- SQLite child process를 write·sync·read-back 뒤 outcome commit 전에 종료한 뒤 재시작해 execute 0회
- 최초 SQLite reopen 전의 live WAL과 recovery 후 artifact 양쪽에서 raw marker/target 비노출
- raw marker/target의 Debug·journal·SQLite/WAL/SHM·물리 evidence 비노출

검증 명령은 다음과 같다.

```bash
cargo test -p xgeny-adapter-reference --locked
cargo clippy -p xgeny-adapter-reference --all-targets -- -D warnings
```

Workspace CI가 Linux, macOS, Windows에서 이 integration test를 함께 실행한다. 테스트는 임시 디렉터리 안의 전용 파일만 생성한다. Process-crash gate는 완전한 file write와 검증 뒤 outcome이 durable해지기 전 경계이며, truncate/write/sync 각각의 중간 process 종료나 power-loss matrix는 아직 포함하지 않는다.

## 의도적으로 제외한 범위

- `xgeny.fs/read-text` 또는 사용자 workspace의 임의 파일 접근
- path normalization, directory confinement, symlink/junction/reparse-point, TOCTOU 방어
- ReadOnly effect 의미와 idempotency 없는 호출
- typed tool output, Artifact와 제품용 output digest 계약
- adapter definite failure·effect unknown·cancelled/not-started Receipt
- process, network, MCP, Connector 또는 XGEN adapter
- credential resolution, approval UI, CLI 명령과 model loop
- in-process hostile plugin 격리

실제 filesystem adapter는 이 참조 구현을 이름만 바꿔 확장하지 않는다. WorkGraph의 ReadOnly 의미, bounded typed output와 Artifact, failure/unknown Receipt, root directory capability와 OS별 path race 규칙을 별도 ADR과 실패 테스트로 먼저 정해야 한다. 전체 구현 순서상 다음 큰 slice는 Tracked/Persistent WorkGraph와 crash recovery다.

공통 adapter contract testkit은 두 번째 실제 adapter가 생길 때 이 suite에서 구현별 fixture를 분리해 추출한다. 지금은 재사용 추상화를 미리 고정하지 않고 public-port conformance의 첫 실행 가능한 기준으로 유지한다.
