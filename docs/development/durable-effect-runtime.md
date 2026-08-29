# Durable effect 실행·복구 수직 슬라이스

- 기준일: 2026-08-29
- 상태: ADR-0008 연구 gate용 내부 실험
- 공개 protocol v0.1 변경: 없음

## 목적

외부 effect를 호출하기 전에 intent와 시작 사실을 durable하게 남기고, 호출 결과 기록이 유실된 재시작에서는 같은 effect를 임의로 다시 실행하지 않는다. 저장 엔진, 실제 tool adapter, XGEN 연결과 독립적인 실행 의미를 먼저 검증한다.

## 정상 실행 순서

```text
canonical Run lease 획득
  -> durable state와 lease Run ID 확인
  -> ephemeral PreparedEffect의 action·Capability·Definition·Instance binding 확인
  -> EffectExecutionStarted commit (journal-head CAS)
  -> EffectSink.execute 1회
  -> succeeded | failed | effect_unknown commit
```

- Run lease는 시작 event transaction뿐 아니라 외부 호출과 outcome commit이 끝날 때까지 유지한다.
- event factory는 ID와 시각만 만든다. run ID, authority, epoch와 event body는 runtime이 durable state에서 조립한다.
- raw arguments와 resolved credential은 journal에 넣지 않는다. adapter가 가진 ephemeral `PreparedEffect`의 action digest, Capability contract, Definition digest, Instance ID와 executable-binding digest가 committed intent와 모두 일치할 때만 실행한다.
- sink가 요청 전송 여부를 확정할 수 없는 transport 오류는 definite failure가 아니라 `Unknown`으로 분류해야 한다.
- success·failure의 receipt digest는 adapter가 durable receipt를 만든 뒤 반환해야 한다. 이 실험은 receipt body 저장소까지 구현하지 않는다.
- 빈 receipt/evidence/reason은 확정 event로 기록하지 않는다. 외부 호출 뒤 이런 adapter 오류가 발생하면 `executing` 또는 `reconciling`을 유지해 다음 recovery가 보수적으로 판정하게 한다.

## 재시작 판정표

| 진입 상태 | runtime 동작 | 외부 effect 재실행 |
|---|---|---|
| `intent_committed` | matching PreparedEffect가 있을 때 시작 commit 후 실행 | 허용 |
| `executing` | 결과 미기록으로 보고 `effect_unknown` commit | 금지 |
| `effect_unknown` + key query 지원 | `reconciling` commit 후 read-only query | 금지 |
| `effect_unknown` + query 미지원 | `manual_required` commit | 금지 |
| `reconciling` | 중단된 read-only query 재수행 | 금지 |
| query가 applied 증명 | `validating`으로 전환 | 금지 |
| query가 not-applied 증명 | 같은 intent를 실행 가능 상태로 전환 | 다음 drive에서만 허용 |
| query 결과 불명 | `manual_required`로 fail closed | 금지 |
| terminal·validating | no-op | 금지 |

idempotency key가 있다는 사실만으로 query 가능성이나 중복 제거를 가정하지 않는다. 현재 runtime은 deduplicate-only sink도 자동 재실행하지 않는다. 실행되지 않았다는 query evidence가 있을 때만 같은 intent를 다시 실행하며, durable attempt 상한과 기존 authorization consumption을 그대로 적용한다.

## 동시 실행 경계

`LocalRunLease`는 Rust 표준 library의 exclusive file lock을 사용한다. 같은 Run의 모든 local runtime은 Run directory의 동일한 canonical `run.lock` 경로를 사용해야 한다. lock은 advisory이므로 이를 무시하는 별도 process를 막는 sandbox가 아니며, network filesystem 동작은 아직 보장하지 않는다.

Run lease를 가진 worker만 `drive_step`을 호출한다. journal CAS는 stale commit을 거부하지만 외부 호출 중 동시 recovery 자체를 막지 못하므로 lease를 CAS의 대체물이 아니라 추가 불변 조건으로 사용한다.

## 실행 가능한 검증

- 시작 event commit 실패 시 sink 호출 0회
- event metadata 생성 실패·prepared digest 불일치 시 sink 호출 0회
- sink 호출 뒤 outcome commit 실패 시 `executing` 보존
- 재시작한 `executing`에서 sink 재호출 없이 `effect_unknown` 전환
- 실제 자식 process가 counter effect 직후 종료된 뒤 counter 1회 유지
- process 종료 후 file lease 재획득과 SQLite journal 복구
- query 지원·미지원·불명·applied·not-applied 분기
- definite failure와 reconciliation failure의 receipt/evidence 보존
- 빈 receipt/evidence를 확정 결과로 수용하지 않는 fail-closed 검증
- not-applied 재시도에서 authorization 비중복 소비와 attempt 상한
- 잘못된 Run의 lease로 event·effect 변경 0회
- PreparedEffect의 Definition 또는 Instance binding 불일치 시 sink 호출 0회

## 아직 포함하지 않는 범위

- filesystem/process/MCP/Connector/XGEN 실제 adapter
- sandbox, secret resolver와 OS별 권한 UI
- async cancellation과 process tree 종료
- receipt body·artifact content store와 verifier loop
- canonical Run directory factory와 network filesystem 진단
- CLI init/run/resume UX와 설치 패키지 E2E
- VM power-loss, disk corruption과 반복 fault matrix

따라서 이 slice는 실제 사용자 도구 실행 기능의 완료가 아니라, 다음 adapter가 반드시 통과해야 할 durable execution contract다.

effect intent 앞단의 exact argument→permission→Router→one-shot authorization 계약은 [Run-bound Invocation Admission 기본형](invocation-admission.md)을 따른다. 재시작 전 material 확보와 fail-closed 전이는 [Recoverable Invocation Material 기본형](recoverable-invocation-material.md)을 따른다. 이 연결 이후에도 public 저수준 store/reducer API와 trusted adapter가 보안 sandbox가 되는 것은 아니며, recovered material을 exact adapter에 전달하는 Direct Executor는 아직 포함하지 않는다.
