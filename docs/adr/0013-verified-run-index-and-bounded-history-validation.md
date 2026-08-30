# ADR-0013: Verified Run Index와 bounded history 검증

- 상태: 제안 — 장기 Run 성능 gate 기본형 구현
- 기준일: 2026-08-30
- 적용 범위: built-in Memory/SQLite RunStore와 runtime hot path
- 공개 protocol v0.1 변경: 없음
- local store schema 변경: 없음 (schema 4 유지)

> 후속 상태: ADR-0014가 journal/index 구조를 바꾸지 않고 immutable dependency DAG와 derived frontier를 추가했다. Schema 4가 새 dependency 의미를 무시하는 downgrade를 막기 위해 current local store schema는 5이며, 아래 schema 4 설명은 ADR-0013 결정 당시의 기록이다.

## 문맥

ADR-0012까지는 store append와 runtime coordination이 매번 journal, projection, material sidecar와 전체 Receipt chain을 다시 읽고 검증했다. Receipt 검증도 각 Receipt마다 intent, start와 final event를 journal에서 다시 찾았기 때문에 Receipt 수를 `R`, event 수를 `E`라 할 때 최악 `O(R × E)`의 역사 검색이 발생했다.

전수 검증 자체를 제거하면 과거 변조, stale writer와 Receipt chain 손상을 놓칠 수 있다. 반대로 이미 검증한 immutable prefix를 같은 SQLite generation 안에서도 매 append마다 다시 검증하면 장기 Run이 진행될수록 정상 경로가 불필요하게 느려진다. Runtime은 대부분 current `RunState`만 필요했지만 소유된 전체 `Vec<EventRecord>`를 반환하는 `load()`를 호출하고 있었다.

## 결정

### 1. cold audit에서 한 번 만드는 `VerifiedRunIndex`를 사용한다

Built-in store는 journal과 persisted projection을 replay/hash-chain 검증한 뒤 한 순회에서 다음 private index를 만든다.

- journal sequence, head digest와 마지막 record
- event ID 집합
- effect ID별 intent event sequence, Step ID와 immutable intent
- effect ID별 최신 start event sequence, Step ID와 시작 시각
- 각 `VerificationRecorded` 시점에 캡처한 intent/start/final anchor
- material이 존재하는 effect ID 집합
- Receipt ID, digest, effect ID 집합과 마지막 Receipt digest
- replay와 일치하는 current `RunState`

Receipt final anchor는 journal 순회가 끝난 뒤의 최신 start가 아니라 finalization event를 만난 시점의 start를 캡처한다. 따라서 향후 retry가 추가되어도 과거 Receipt를 뒤의 start event에 잘못 연결하지 않는다. Derived intent/authorization/material table과 Receipt table은 이 index와 각각 한 번 대조한다. Schema 4 row와 공개 wire 형식은 바꾸지 않는다.

### 2. SQLite 검증 session을 DB generation과 durable head에 묶는다

`SqliteRunStore::open`은 한 read transaction에서 full audit을 수행한 뒤 connection-local verified cache를 설치한다. Cache identity는 다음 두 값을 함께 사용한다.

- 같은 connection에서 비교한 `PRAGMA data_version`
- durable journal의 마지막 `(sequence, digest)`

`data_version`은 다른 connection이 commit하면 달라지지만 같은 connection의 commit에는 달라지지 않는다. 따라서 둘 중 하나라도 cache와 다르면 journal suffix를 추측해 신뢰하지 않고 전체 audit을 다시 수행한다. 같은 connection의 정상 append는 commit 성공 뒤 verified index와 durable head를 함께 전진시킨다.

Append는 `BEGIN IMMEDIATE`로 writer를 확보한 뒤 generation과 실제 DB head를 확인한다. 그 transaction 안에서 caller의 `ExpectedHead`를 비교하고 candidate event/material/Receipt 한 묶음만 기존 verified prefix에 대해 검증한다. DB commit이 성공한 뒤에만 cache를 갱신한다. Insert, checkpoint 또는 commit 오류가 나면 candidate를 cache에 설치하지 않고 cache 전체를 폐기한다.

### 3. runtime hot path와 명시적 audit/export API를 분리한다

`RunStore`에 다음 호환 가능한 최소 view를 추가한다.

- `load_current()`: current verified `RunState`
- `load_verification_snapshot(step_id)`: current state, 해당 effect의 시작 시각, 이전 Receipt digest

Default 구현은 기존 full load를 사용하므로 외부 store 구현을 즉시 깨뜨리지 않는다. Built-in Memory/SQLite store는 verified index에서 반환한다. Admission, material recovery, Direct Executor와 durable runtime은 `load_current()`를 사용하고, `VerificationRunner`는 전체 journal/Receipt vector 대신 verification snapshot을 사용한다.

전체 journal이 필요한 `load()`, `export_jsonl()`과 complete Receipt export는 명시적 감사·진단 경로로 유지한다. 전체 결과를 반환하는 API 자체의 출력 비용을 상수 시간이라고 주장하지 않는다.

### 4. 성능 계약은 시간 대신 구조적 작업량으로 검증한다

CI의 merge gate는 OS와 runner 부하에 민감한 millisecond 상한을 사용하지 않는다. Test-only connection-local counter로 다음 작업량을 정확히 검사한다.

| 경로 | 보장하는 역사 접근 |
|---|---|
| unchanged warm append | 과거 event/material/Receipt 검증 0, candidate event 1 |
| 1,000-event warm Run | 모든 append에서 과거 검증 0 |
| 10,000-event retry Run | 모든 append에서 과거 검증 0 |
| cache를 비운 cold audit | replay 한 순회와 별도로 event anchor 10,000, material 1, Receipt 0을 각각 한 번 방문 |
| 1,001-event/40-Receipt Run | cold audit에서 Receipt별 intent/start/final binding lookup 각 1회, 마지막 warm Receipt append의 과거 scan 0과 candidate Receipt 1 |
| 외부 projection/material/Receipt 변경 | cache 폐기 후 full audit 1회 |

10,000-event fixture는 한 effect가 `Started → Unknown → Reconciling → ProvedNotApplied`를 반복하는 유효한 WorkGraph다. Step 수를 불필요하게 10,000개로 늘리지 않으면서 긴 journal과 retry continuity를 검사한다.

## 복잡도와 비보장

이번 결정이 bounded하게 만드는 것은 **과거 journal/sidecar/Receipt의 재검증 횟수**다.

- cold audit의 application-level row visit: journal replay 1회 + index anchor 생성 1회 + `I/A/M/R` 대조로 선형
- ordered index 생성·lookup을 포함한 CPU upper bound: `O(E log E + I log I + R log R)`; 기존 `O(R × E)` journal 검색은 없음
- unchanged warm append의 역사 scan: `0`
- candidate 검증: tree index membership/binding lookup `O(log(E + I + R))` + `O(candidate size)`; 과거 row scan은 없음
- `load_current()`: 전체 history와 무관하지만 소유된 projection clone 비용 `O(|projection|)`
- 전체 append: `apply_record`의 state clone과 projection 직렬화를 포함해 `O(|projection| + |candidate|)`
- full `load()`/journal export: 결과 materialization만으로 최소 `O(E)`
- complete Receipt load/export: 결과 materialization만으로 최소 `O(R)`
- verified index steady memory: duplicate ID와 binding 검증을 위한 event/intent/Receipt key 때문에 `O(E + I + R)`

현재 `replay/apply_record`는 event마다 `RunState`를 clone하고 SQLite projection 전체를 다시 직렬화한다. 따라서 Step 수까지 크게 증가하는 Run의 전체 시간과 메모리가 선형이라고 주장하지 않는다. 구조적 counter는 DB 역사 재검증 회귀를 막으며 wall-clock characterization과 reducer/projection 최적화는 별도 후속 gate다.

## 일관성 및 위협 모델

- 외부 SQLite connection의 정상 commit은 `data_version` 변경으로 감지하고 full audit한다.
- Event head가 그대로인 projection/material/Receipt-only 변경도 `data_version` 때문에 cache hit로 처리하지 않는다.
- 자기 connection commit 뒤 process가 종료되거나 cache 갱신이 유실되어도 재오픈은 full audit하며, 살아 있는 connection에서는 durable head 불일치가 cache 재사용을 막는다.
- Store의 SQLite connection은 private다. Test-only same-connection corruption helper는 `data_version`이 바뀌지 않으므로 cache를 명시적으로 폐기한다.
- Raw database/WAL 파일을 SQLite locking 밖에서 수정하는 hostile OS, 메모리 변조, 악성 in-process code와 actor authentication은 이 cache의 위협 모델 밖이다.
- Hash chain과 Receipt digest의 기존 tamper detection 의미를 서명이나 물리적 exactly-once로 확대하지 않는다.

SQLite의 `data_version`과 transaction 의미는 공식 문서를 기준으로 한다.

- <https://www.sqlite.org/pragma.html#pragma_data_version>
- <https://www.sqlite.org/lang_transaction.html>

## 회귀 gate

- Memory/SQLite journal state와 complete Receipt export parity
- 두 SQLite handle의 stale writer compare-and-swap
- projection, material과 Receipt-only 외부 mutation 감지
- append fault 전 단계 rollback 후 동일 candidate 재시도 성공
- process exit, reopen과 lost acknowledgement의 effect/verifier 중복 0
- schema 3 → 4 migration과 기존 journal byte 보존
- 1,000/10,000-event와 1,001-event/40-Receipt 구조적 작업량 assertion
- workspace format, clippy, test, protocol check와 release build

## 결과

장기 Run의 정상 coordination과 Receipt finalization은 더 이상 전체 journal과 Receipt chain을 매번 복제·재검증하지 않는다. 동시에 fresh open, 외부 generation 변경과 명시적 audit/export에서는 기존 hash-chain, projection, material과 Receipt 검증을 유지한다.

다음 성능 병목은 `RunState` 전체 clone과 projection rewrite다. 후속 ADR-0014는 Tracked/Persistent WorkGraph와 runnable frontier, graph 규모 계측을 추가했으며 reducer 구조 변경은 여전히 측정 결과를 근거로 별도 ADR에서 결정한다.
