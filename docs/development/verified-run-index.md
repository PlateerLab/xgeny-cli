# Verified Run Index와 장기 Run 검증

- 기준일: 2026-08-30
- 상태: ADR-0013 연구 gate 기본형
- 공개 protocol v0.1 변경: 없음
- local store schema: 4 유지

## 목적

이미 검증한 Run prefix를 같은 SQLite generation에서 다시 전수 검사하지 않으면서, 외부 writer·변조·rollback·crash에서 기존 fail-closed 의미를 유지한다.

## 구조

```text
fresh open / generation change / explicit full load
                         |
                         v
        journal + persisted RunState replay/hash 검증
                         |
                         +-- one-pass event anchors
                         |
     intent/auth/material tables + Receipt chain 대조
                         |
                         v
                 VerifiedRunIndex
        (state, head, IDs, intent/start/final anchors,
               material IDs, Receipt chain head)
                         |
             +-----------+------------------+
             |                              |
       load_current()          load_verification_snapshot(step)
             |                              |
  Admission/Executor/runtime        VerificationRunner
```

`load()`와 export는 전체 history가 필요한 명시적 경로다. Runtime coordination은 owned journal vector를 요청하지 않는다.

## SQLite append 순서

```text
BEGIN IMMEDIATE
      |
      +-- PRAGMA data_version + durable (sequence, digest)
      |
      +-- cache identity 일치? -- 아니오 --> 같은 transaction에서 full audit
      |
      +-- ExpectedHead 비교
      +-- candidate event/material/Receipt 검증
      +-- rows + projection 기록
      +-- COMMIT
      |
      +-- 성공 뒤에만 VerifiedRunIndex 전진

오류/rollback/commit 실패 --> cache 폐기 --> 다음 접근에서 full audit
```

`BEGIN IMMEDIATE` 뒤 head를 확인하므로 두 writer가 같은 stale head를 모두 성공시킬 수 없다. 다른 connection의 sidecar-only 변경은 event head가 같더라도 `data_version`이 달라져 full audit로 전환된다.

## API 사용 기준

| 목적 | API | 비용과 의미 |
|---|---|---|
| Runtime 상태 조정 | `load_current()` | history vector 없음, verified state clone |
| Receipt finalization | `load_verification_snapshot(step_id)` | state + start 시각 + Receipt chain head |
| Integrity audit/진단 | `load()` | 전체 event materialization과 replay 검증 |
| Journal 내보내기 | `export_jsonl()` | 전체 journal 출력 |
| Receipt 내보내기 | `export_execution_receipts_jsonl()` | 전체 Receipt 출력 |

외부 `RunStore` 구현은 새 최소 API를 override하지 않아도 기존 default full-load 경로로 동작한다. 실제 장기 Run 최적화를 제공하려면 store 고유의 동일-generation 검증 index를 구현해야 한다.

## 검증 가능한 성능 계약

Private `AuditMetrics`는 product API나 global state가 아니며 test build와 test-only accessor에서만 counter를 보존한다. Release build에서는 같은 hook이 no-op이다. 각 test `SqliteRunStore` connection에 귀속되어 병렬 테스트끼리 섞이지 않는다.

- `full_audits`
- `historical_events`, `historical_materials`, `historical_receipts`
- `candidate_events`, `candidate_materials`, `candidate_receipts`
- Receipt anchor intent/start lookup과 Receipt binding intent lookup

CI는 elapsed-time assertion을 두지 않는다. 다음 exact count가 계약이다.

```text
warm append: historical_* = 0, candidate_events = append 수
cold 10k audit: full_audits = 1, replay와 별개인 event anchor count = 10,000
1,001 events + 40 Receipts: Receipt별 anchor/binding lookup = 각각 40
external mutation: full_audits = 1, 오류 반환
```

전체 append를 `O(1)`이라고 부르지 않는다. WorkGraph reducer가 `RunState`를 clone하고 SQLite가 projection JSON 전체를 쓰기 때문에 현재 총비용은 projection 크기에 비례한다. 이번 gate는 history validation amplification을 제거한 것이다.

Index도 constant-memory 구조가 아니다. Duplicate event/Receipt ID와 durable binding을 검증하려고 history key를 보존하므로 steady memory는 `O(E + I + R)`이다. Archive segment, compaction과 bounded-memory duplicate detection은 이번 범위가 아니다.

## 손상과 동시성 점검표

- 다른 connection이 projection만 바꿔도 다음 hot read가 실패하는가
- material index/document만 바꿔도 point read 전에 full audit하는가
- Receipt document만 바꿔도 complete chain을 다시 검증해 실패하는가
- stale writer가 duplicate material보다 먼저 `HeadConflict`로 닫히는가
- fault injection 뒤 candidate가 cache에 남지 않고 동일 append를 재시도할 수 있는가
- Receipt insert 뒤 process exit가 event/Receipt/projection을 모두 rollback하는가
- lost acknowledgement 뒤 effect 또는 verifier를 반복하지 않는가

## 로컬 검증

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo run --locked --quiet -p xgeny-cli -- protocol check
cargo build --workspace --release --locked
```

일부 가상화 filesystem은 SQLite `synchronous=FULL`의 fsync latency가 매우 클 수 있다. 구조적 1,000/10,000-event 테스트는 `:memory:` SQLite로 durability와 무관한 검증량을 측정하며, 별도의 tempfile fault/crash 테스트가 WAL/FULL durability 경계를 계속 검사한다.

## 후속 작업

- Tracked/Persistent WorkGraph와 runnable frontier에 graph 규모 counter 추가
- Step 수 증가 시 `apply_record` clone과 projection rewrite characterization
- 필요할 때 persistent collections, reducer mutation 전략 또는 projection 분할을 별도 ADR로 비교
- portable journal+Receipt archive/import generation 계약

의미 결정과 정확한 범위는 [ADR-0013](../adr/0013-verified-run-index-and-bounded-history-validation.md)을 따른다.
