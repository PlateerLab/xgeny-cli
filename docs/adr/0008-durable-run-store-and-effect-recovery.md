# ADR-0008: Durable Run Store와 외부 effect 복구

- 상태: 제안 — 연구 gate
- 기준일: 2026-08-28
- 승인 조건: `docs/research/2026-08-28-runtime-evaluation-protocol.md`의 storage·effect gate 통과

## 문맥

기존 설계는 hash-chained JSONL을 RunJournal의 물리 정본으로 두었다. JSONL은 사람이 읽고 교환하기 좋지만 event, WorkGraph projection, effect intent, authorization consumption을 하나의 crash-consistent commit으로 묶으려면 locking, generation, fsync, directory sync, tail repair, compaction protocol을 직접 구현해야 한다.

더 중요한 문제는 DB 선택만으로 해결되지 않는다. local transaction을 commit한 뒤 외부 effect가 발생하고 receipt를 기록하기 전에 process가 종료되면, 재시작한 runtime은 effect 실행 여부를 알 수 없다. sink가 stable idempotency key나 상태 조회를 제공하지 않으면 물리적 exactly-once를 일반적으로 보장할 수 없다.

사용자가 SQLite server를 설치하지 않아도 Rust binary가 embedded SQLite library를 내부 구현으로 사용할 수 있다. 따라서 “별도 DB를 설치하지 않는다”와 “내부 transactional store를 사용하지 않는다”를 같은 제약으로 취급하지 않는다.

## 제안 결정

### 1. 논리 정본과 물리 저장을 분리

- `RunEvent`의 committed history가 논리적 source of truth다.
- 물리 저장 후보는 Run별 embedded SQLite다.
- JSONL은 deterministic export, protocol interchange, debug, conformance artifact다.
- WorkGraph, current step과 Receipt ID/digest index는 event에서 재구성 가능한 projection이다. ADR-0012의 complete Receipt body는 terminal event와 원자 commit되는 검증된 sidecar다.
- 사용자·프로젝트가 검토하는 memory는 Markdown 정본을 유지할 수 있으며 Run transactional state와 분리한다.
- artifact body는 content-addressed file로 저장하고 transaction에는 digest와 metadata만 기록한다.

SQLite 채택은 아직 확정하지 않는다. hardened JSONL과 세 OS fault-injection을 비교한 뒤 승인한다.

### 2. Run별 failure domain과 단일 작성자

- Run별 store를 기본 failure domain으로 둔다.
- 한 Run에는 한 durable writer만 허용한다.
- local writer는 OS lock과 store generation을 검증한다.
- remote authority/handoff에는 authority epoch 또는 fencing token을 사용한다.
- stale writer의 commit은 거부한다.

### 3. 하나의 local transaction

다음 상태는 가능한 한 하나의 transaction으로 commit한다.

- 새 RunEvent
- WorkGraph projection revision
- effect intent/outbox
- authorization/approval budget consumption
- receipt 또는 reconcile 상태

transaction commit 전에는 외부 effect를 시작하지 않는다. replay 중에는 event를 재적용하되 외부 effect를 다시 발생시키지 않는다.

### 4. 외부 effect 상태 기계

```text
planned
  -> authorized
  -> intent_committed
  -> executing
  -> succeeded | failed | effect_unknown
effect_unknown
  -> reconciling
  -> succeeded | failed | compensation_required | manual_required
```

- effectful invocation은 canonical action digest와 stable idempotency key를 가진다.
- key 존재만으로 sink idempotency를 주장하지 않는다.
- sink가 key 조회 또는 결과 reconciliation을 지원하면 같은 key로 상태를 확인한다.
- 실행되지 않았음이 증명되거나 sink가 key로 중복 제거할 때만 자동 resume한다.
- 그렇지 않으면 blind retry하지 않고 `manual_required` 또는 명시적 compensation step으로 전환한다.
- compensation은 원 effect의 물리 rollback이 아니라 별도 의미 action이며 별도 승인·receipt를 가진다.

### 5. Authorization consumption

- 사용자 승인과 조직 PolicyLease는 effect intent에 durable하게 결합한다.
- canonical action, approval/lease digest, remaining budget, authority epoch를 기록한다.
- 같은 승인으로 허용된 횟수보다 많은 semantic replay를 실행하지 않는다.
- 입력·대상·effect 의미가 달라지면 새 승인을 요구한다.

### 6. 무결성과 인증을 구분

- hash chain은 event 누락·변조를 탐지하는 무결성 증거다.
- hash chain만으로 actor authenticity, OS compromise 저항성, 외부 effect 발생 사실을 증명하지 않는다.
- 향후 DSSE/in-toto 또는 platform signing을 추가해도 execution semantics와 별도 계층으로 유지한다.

## 제안 물리 구조

```text
~/.xgeny/projects/<project-id>/
  runs/<run-id>/
    run.db                 # 후보: event, projection, intent, receipt index
    artifacts/
      sha256/<digest>      # immutable content
    export/
      journal.jsonl        # deterministic derived export
  memory/
    MEMORY.md
    topics/
  index/                   # 삭제·재구성 가능한 검색 index
```

XGEN, Connector, 외부 agent는 `run.db`를 열지 않는다. 오직 versioned wire contract, event cursor, ArtifactRef, ExecutionReceipt를 통해 연동한다.

## 필요한 schema 변화

ADR 승인 전에는 wire schema를 즉시 변경하지 않는다. 실험 vertical slice에서 다음 후보를 검증한다.

- WorkGraph step: `effect_unknown`, `reconciling`, `compensation_required`, `manual_required`
- invocation: sink idempotency support와 reconcile strategy
- effect intent: canonical action digest, authority epoch, authorization consumption
- receipt: attempt, reconciliation evidence, compensation link
- event payload: 핵심 lifecycle event의 typed schema

## 결과

### 기대 이점

- event와 effect intent 사이의 local atomicity를 저장 엔진에 위임할 수 있다.
- process restart에서 projection과 pending effect를 명시적으로 복구한다.
- 사용자는 별도 DB server나 daemon을 설치하지 않는다.
- Run별 손상 격리와 export 가능성을 유지한다.
- XGEN과 storage 독립성을 유지하면서 의미 호환성을 강화한다.

### 비용과 위험

- SQLite binding과 migration 정책이 binary·release surface에 추가된다.
- WAL, fsync, antivirus, file locking의 OS별 동작을 검증해야 한다.
- external effect exactly-once 문제는 여전히 sink 협력이 필요하다.
- Run별 DB가 많아질 때 listing, backup, compaction 비용을 측정해야 한다.
- JSONL export가 실제 상태와 어긋나지 않도록 conformance test가 필요하다.

## 검증 gate

승인 전에 최소 다음을 통과한다.

1. Linux/macOS/Windows package 설치와 restart E2E
2. transaction·effect 경계의 deterministic process-kill matrix
3. 가능한 VM에서 power-loss 실험
4. WAL `FULL`/`NORMAL`과 hardened JSONL 비교
5. corrupted tail/WAL/DB의 격리와 진단
6. effect timeout·lost ack·duplicate delivery simulator
7. authorization budget replay test
8. deterministic JSONL export와 replay conformance

## 폐기안

- raw JSONL append만 구현하고 crash consistency를 운영체제에 가정
- 하나의 전역 DB에 모든 Run·artifact·memory를 결합
- local DB를 XGEN이나 Connector가 직접 공유
- effect intent commit 전에 외부 호출
- 모든 timeout을 같은 idempotency key 없이 재시도
- compensation을 transaction rollback처럼 취급
- hash chain을 서명 또는 exactly-once 보장으로 표현

## 현재 판정

이 ADR은 구현 방향을 좁히는 제안이지 승인된 저장소 결정이 아니다. 첫 제품 코드는 model-free vertical slice와 fault-injection harness로 만들며, 평가 결과가 반대이면 물리 저장 선택을 바꾼다. 논리 RunEvent, single authority, conservative effect recovery라는 의미 계약은 저장 엔진과 독립적으로 유지한다.
