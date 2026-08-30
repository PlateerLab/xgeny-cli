# ADR-0021: Durable CompletionOutputRecord와 local store schema 8

- 상태: 채택
- 기준일: 2026-08-30
- 적용 범위: WorkGraph completion 의미, local RunStore, AgentLoop, CLI driver, OpenAI-compatible provider 회귀
- 공개 protocol v0.1 schema 변경: 없음
- local store schema: 7에서 8로 변경

## 문맥

[ADR-0020](0020-generation-checked-planning-context-v2.md)은 Receipt로 완료된 exact tool
output을 같은 generation의 다음 model turn에 전달한다. 그러나 모델이 반환한 최종 summary는
`CompletionCandidateRecorded`의 digest로만 남았다. 프로세스가 종료되면 candidate가 있었다는
사실은 복원할 수 있어도 사용자가 받아야 할 exact UTF-8 본문은 복원할 수 없었다.

재시작 뒤 provider를 다시 호출해 summary를 재생성하면 같은 결과라는 보장이 없고, 이미 성공
정산된 model-call을 중복 전송하게 된다. 반대로 raw summary를 journal이나 `RunState`에 넣으면
일반 replay, export, `Debug`와 projection read 전체에 본문이 확산된다. 따라서 completion도
tool output과 마찬가지로 **journal commitment에 결합된 원자 local sidecar**로 보존한다.

이번 결정은 최종 candidate를 terminal `Completed` Run으로 승인하는 의미나 사용자용 `xgeny
run/resume` 명령을 추가하지 않는다. Candidate는 계속 검증·사용자 확인 전의 비종결 결과다.

## 결정

### 1. CompletionOutputRecord v1은 exact summary와 생성 문맥을 결합한다

새 internal record는 다음 값을 보존한다.

```text
formatVersion = 1
candidateId
runId
turnIndex
modelCallId
contextDigest
proposalDigest
summarySizeBytes
summaryDigest
exact UTF-8 summary
recordDigest
```

새 record는 durable model-call binding이 있는 `ExpectedPlanningTurn`에서만 만들 수 있다.
`proposalDigest`, `summaryDigest`, `candidateId`와 `recordDigest`는 서로 다른 domain으로 계산한다.
Load와 cold audit는 summary bytes에서 size와 digest를 다시 계산하고, proposal/context/call/turn,
candidate와 journal commitment를 모두 대조한다. Raw summary를 `recordDigest` 입력에 중복
직렬화하지는 않지만 재계산한 `summaryDigest`를 record가 commit하므로 본문은 exact bind된다.

Summary 입력 계약은 다음과 같다.

- 비어 있지 않은 UTF-8 문자열
- 최대 5,000 UTF-8 bytes
- Markdown/text 보존을 위한 LF와 TAB 허용
- CR을 포함한 다른 control character 거부
- OS별 newline 정규화 없이 입력 bytes 그대로 저장·반환

이 상한은 문자 수가 아니라 Rust UTF-8 byte 길이에 적용한다. 한글 등 multibyte 입력도
5,000-byte 경계와 5,001-byte 거부를 별도로 검증한다.

### 2. Journal과 projection에는 record digest만 남긴다

`CompletionCandidateRecorded`와 `CompletionCandidateState`에 optional
`completionOutputRecordDigest`를 추가한다. `None`은 deserialize default이고 serialize에서
생략하므로 schema 7 event/projection bytes를 다시 쓰지 않는다.

새 candidate는 항상 `Some(recordDigest)`여야 한다. Raw summary는 다음 위치에 넣지 않는다.

- Run journal event와 JSONL export
- `RunState` projection
- ExecutionReceipt와 Receipt export
- 일반 `Debug`, fixed error와 telemetry

`CompletionOutputRecord`의 custom `Debug`는 identity와 digest만 보이고 summary를 redaction한다.
다만 SQLite DB/WAL/backup에는 exact summary가 평문으로 존재한다. Digest는 암호화나 DLP가
아니며 이 ADR은 at-rest encryption을 주장하지 않는다.

### 3. Event, completion sidecar와 projection을 한 transaction으로 commit한다

Store 계약은 다음 bundle을 추가한다.

```text
append_with_completion_output(
  expectedHead,
  CompletionCandidateRecorded,
  CompletionOutputRecord
)

load_completion_output(expectedHead, candidateId)
  -> Option<CompletionOutputRecord>
```

새 `CompletionCandidateRecorded`를 plain `append`로 쓰는 우회는 record digest의 유무와
무관하게 거부한다. Bundle은 event와 record의 Run/call/turn/context/proposal/candidate/summary
digest binding을 mutation 전에 검사한다. Built-in Memory store도 같은 none-or-all 의미와
replay 검증을 제공한다.

SQLite schema 8은 Run당 하나의 sealed completion candidate 의미에 맞춰 singleton table을
추가한다.

```sql
CREATE TABLE completion_outputs (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    event_sequence INTEGER NOT NULL UNIQUE CHECK (event_sequence >= 1),
    candidate_id TEXT NOT NULL UNIQUE,
    turn_index INTEGER NOT NULL CHECK (turn_index >= 1),
    model_call_id TEXT NOT NULL UNIQUE,
    context_digest TEXT NOT NULL,
    proposal_digest TEXT NOT NULL,
    summary_size_bytes INTEGER NOT NULL CHECK (summary_size_bytes BETWEEN 1 AND 5000),
    summary_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL UNIQUE,
    record_json BLOB NOT NULL CHECK (length(record_json) BETWEEN 1 AND 20000),
    FOREIGN KEY (event_sequence) REFERENCES run_events(sequence)
) STRICT;
```

Transaction 의미 순서는 다음과 같다.

```text
BEGIN IMMEDIATE
  verify current generation and expected journal head
  reduce CompletionCandidateRecorded
  INSERT run_events(recordDigest commitment)
  INSERT completion_outputs(exact record)
  WRITE run_projection(recordDigest only)
COMMIT
```

Event, CompletionOutput와 Projection 각 단계의 fault injection과 completion row insert 직후
child-process 종료에서 event/row/projection 전부가 rollback되는지 검사한다.

### 4. Load와 cold audit는 singleton 전체를 fail-closed 검증한다

`load_completion_output`은 caller의 exact `ExpectedHead`와 candidate ID를 요구한다. SQLite는
같은 read transaction의 verified generation에서 row를 읽는다. Cold audit는
`completion_outputs` 전체를 순회해 다음을 검사한다.

- row cardinality가 digest-bound completion event 수와 일치하는가
- 유일한 row의 singleton 값이 정확히 1인가
- event sequence와 indexed column이 `record_json`과 같은가
- record의 모든 identity/content digest가 journal anchor와 같은가
- row가 missing, tampered, orphan 또는 candidate와 교체되지 않았는가

정상 schema constraint를 test-only로 우회해 singleton 2 orphan row를 삽입한 hostile DB도 open
감사에서 거부한다. 외부 connection이 row나 projection을 바꾸면 기존 `data_version` 기반 cache
무효화가 full audit를 다시 수행한다.

### 5. 재시작은 provider 재호출 없이 durable 결과만 반환한다

`AgentLoop`가 새 completion proposal을 수락할 때 record를 먼저 만들고 success settlement event와
원자 commit한다. Commit acknowledgement가 유실되어 caller가 오류를 받더라도 다음 open은
candidate와 exact summary를 반환하며 provider를 다시 부르지 않는다.

이미 projection된 candidate의 동작은 닫힌 두 경우뿐이다.

| candidate | replay 동작 |
|---|---|
| schema 8 digest-bound | exact sidecar를 load·검증해 반환; missing/mismatch는 오류 |
| schema 7 legacy `None` | summary를 발명하지 않고 `output = None`; provider 호출 0회 |

Legacy case에서는 새 `RunStore` extension을 호출하지 않는다. 따라서 schema 7 candidate를 읽는
기존 custom store 구현도 새 loader를 구현할 필요 없이 replay할 수 있다. 반대로 digest-bound
candidate에서 loader가 `None`을 반환하면 legacy로 낮추지 않고 fail-closed한다.

CLI vertical regression은 실제 test process를 새로 실행해 SQLite와 Run lease를 다시 열고,
planner/adapter/verifier가 비어 있는 상태에서도 동일 summary bytes를 반환하며 journal head와
event 수가 바뀌지 않는지 검증한다. OpenAI-compatible HTTP regression도 서버를 내린 뒤 같은
candidate를 재생해 추가 HTTP 요청이 없음을 확인한다.

### 6. Schema 3~7 migration은 completion을 추론하지 않는다

Schema 7→8 migration은 `BEGIN IMMEDIATE` 안에서 빈 `completion_outputs` table을 만든 뒤 기존
journal, projection과 모든 sidecar를 full audit하고 `user_version = 8`을 함께 publish한다.
기존 event/projection/tool-output/Receipt bytes는 수정하지 않는다.

Schema 7에 이미 legacy completion candidate가 있더라도 raw summary를 digest, provider 또는
외부 log에서 추론해 backfill하지 않는다. Optional digest는 `None`, 새 table은 빈 상태로
남으며 runtime은 `output = None`으로만 replay한다.

Schema 3~6 direct migration fixture도 실제 legacy topology처럼 completion table을 갖지 않는다.
각 migration은 Receipt, planned invocation, tool output과 completion table 중 해당 version에 없는
table을 원자 생성하고 schema 8로 수렴한다. Migration용 completion DDL은 `IF NOT EXISTS`를
사용하지 않으므로 예기치 않은 선행 table을 신뢰하지 않는다. Audit가 실패하면 생성한 table과
version 변경이 모두 rollback되고 원래 legacy rows가 그대로 남는다.

Version 1·2와 9 이상은 mutation 없이 거부한다. Schema 8은 binary compatibility fence이므로
schema 7 binary가 새 DB를 열어 completion commitment를 모른 채 쓰지 못한다.

## 호환성과 비목표

- Public XGEN/XGENy protocol v0.1은 바뀌지 않는다.
- XGEN, Connector, PostgreSQL, MinIO 또는 특정 model/provider 의존성을 Core에 추가하지 않는다.
- `DriverOutcome`은 새 candidate에는 `Some(CompletionOutputRecord)`, legacy에는 `None`을 반환한다.
- Candidate 검증, terminal Run completion과 사용자 승인 의미는 후속이다.
- Public `xgeny run/resume`, filesystem/process adapter와 설치 패키지는 후속이다.
- Streaming/chunked completion, 여러 candidate generation, retention/GC와 암호화는 후속 schema다.
- Raw provider envelope, hidden reasoning과 chain-of-thought는 저장하지 않는다.

## 회귀 gate

- exact UTF-8/LF/TAB/quote/backslash round-trip과 5,000/5,001-byte 경계
- CR/control/empty summary 거부
- Run/call/turn/context/proposal/candidate/body/size/digest tamper 거부
- journal/projection/Receipt/export/Debug에 raw summary sentinel 비노출
- plain append 우회와 missing/extra/mismatched sidecar의 mutation 0
- Memory/SQLite atomic commit, close/reopen과 exact byte parity
- Event/CompletionOutput/Projection fault rollback과 child-process crash rollback
- missing/tampered/indexed/orphan completion row cold-audit 거부
- commit acknowledgement 유실 뒤 provider 호출 0회와 exact replay
- schema 7 legacy candidate의 output `None`, provider 호출 0회
- schema 7→8 tool-output byte 보존과 corrupt migration DDL/version rollback
- schema 3/4/5/6 authentic topology에서 schema 8 수렴
- 실제 별도 process restart에서 planner/adapter/verifier 호출 0회와 state mutation 0
- Linux, macOS, Windows workspace gate

## 결과

Model이 local tool output을 받은 다음 turn에서 만든 최종 summary를 SQLite 재시작과 process
교체 뒤에도 exact하게 반환할 수 있다. 이로써 기술 prototype에 필요한
`model → local tool → next model turn → durable completion replay`의 저장 의미가 닫혔다.
다음 slice는 bounded filesystem adapter이며, 그 다음 public CLI와 go50902 live E2E가 이
durable 경계를 그대로 조합한다.
