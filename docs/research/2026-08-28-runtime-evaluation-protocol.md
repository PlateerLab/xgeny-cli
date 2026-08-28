# XGENy Durable Agent Runtime 평가 프로토콜

- 기준일: 2026-08-28 (Asia/Seoul)
- 상태: 사전등록 초안 — 구현 전 검토 대상
- 대상: XGENy local-first runtime, WorkGraph, RunEvent store, effect recovery, memory, XGEN/Connector compatibility
- 관련 근거: `2026-08-28-durable-agent-runtime-evidence.md`

## 1. 목적

이 문서는 구현 결과에 맞춰 성공 기준을 바꾸지 않기 위한 평가 계약이다. XGENy가 실제로 다음 주장을 할 수 있는지 검증한다.

1. 제한된 model context와 process restart를 넘어 장기 작업을 이어 간다.
2. crash와 응답 유실이 있어도 외부 effect를 무조건 재실행하지 않는다.
3. local-first 설치 경량성과 crash consistency를 동시에 만족한다.
4. memory가 도움이 되는 조건과 해가 되는 조건을 구분한다.
5. XGEN·Connector와 저장소를 공유하지 않고 의미 호환성을 보장한다.
6. 최종 결과뿐 아니라 실행 과정의 정책·effect·검증 증거를 보존한다.

이 프로토콜을 통과하기 전에는 “Qwen3.6-27B가 하네스로 고성능을 낸다”, “exactly once를 보장한다”, “운영 배포에 안전하다”라고 표현하지 않는다.

## 2. 연구 질문과 가설

### RQ1. 장기 연속성

`H1`: 동일 모델과 동일 task에서 WorkGraph + evidence-linked context paging은 단순 최근 대화 또는 요약 기반 재개보다 process restart와 강제 context 축소 이후의 task completion을 높인다.

`H1-null`: completion 차이가 없거나, cost·latency 증가를 고려한 Pareto frontier에서 우월하지 않다.

### RQ2. 외부 effect 안전성

`H2`: durable intent, stable idempotency key, receipt, reconcile 상태 기계는 tool timeout·lost acknowledgement·process crash에서 blind retry와 duplicate external effect를 줄인다.

`H2-null`: baseline보다 unsafe duplicate, silent loss 또는 manual intervention이 줄지 않는다.

### RQ3. 저장소 선택

`H3`: per-Run embedded transactional store는 별도 DB service 없이도 raw JSONL 단독 구현보다 event, WorkGraph projection, effect intent의 원자적 복구를 단순하고 일관되게 제공한다.

`H3-null`: 세 OS에서의 durability, portability, maintenance 비용을 함께 보면 유의미한 이점이 없다.

### RQ4. 메모리

`H4`: provenance와 적용 조건을 가진 계층형 memory는 제한된 context의 장기·반복 task에서 도움을 주지만, stale·conflicting·mismatched memory를 무조건 주입하면 성능이 저하된다.

`H4-null`: memory policy가 long context 또는 retrieval baseline보다 안정적으로 낫지 않다.

### RQ5. 모델과 하네스의 상호작용

`H5`: XGENy harness의 효과는 한 모델에만 종속되지 않지만 크기는 모델별로 다르며, Qwen3.6-27B에서도 통계적·실용적으로 의미 있는 개선을 보인다.

`H5-null`: 개선이 특정 모델·task에만 나타나거나 Qwen3.6-27B에서 재현되지 않는다.

## 3. 평가 원칙

- 코드 작성 전에 task, verifier, failure schedule, metric을 동결한다.
- deterministic verifier가 가능한 항목은 LLM judge를 사용하지 않는다.
- 한 번의 성공 사례가 아니라 task를 pairing unit으로 한 반복 실험을 한다.
- 결과가 나쁜 run, crash, timeout, manual intervention도 제외하지 않는다.
- model, prompt, tool schema, harness revision, OS image, dependency lock, task fixture digest를 모두 기록한다.
- 같은 task를 개발 중 반복 사용한 경우 holdout 결과와 분리한다.
- 성능, 안전, 비용, latency를 함께 보고 Pareto frontier로 판정한다.
- 주효과를 설명할 수 있도록 한 번에 하나의 mechanism만 추가하는 ablation을 유지한다.

## 4. 독립 변수와 baseline

### 4.1 Harness 구성

| ID | 구성 | 목적 |
|---|---|---|
| B0 | bounded recent transcript, 비영속 | 최소 agent loop baseline |
| B1 | transcript + 단일 summary | 일반적 compaction baseline |
| B2 | append-only transcript 복원 | transcript durability baseline |
| X1 | WorkGraph + RunEvent + active frontier context | 계획·상태 분리 효과 |
| X2 | X1 + durable effect protocol | crash·effect 안전 효과 |
| X3 | X2 + provenance memory | 장기·반복 지식 효과 |
| X4 | X3 + verifier-driven replanning | 검증 feedback 효과 |

각 구성은 같은 model, temperature, token budget, tool capability를 사용한다. 비교 대상에 없는 기능을 prompt로 우회 구현하지 않는다.

### 4.2 모델

초기 주 평가 모델은 사용자가 목표로 한 `Qwen3.6-27B`의 정확한 배포 artifact와 inference 설정으로 고정한다. 모델 이름만 기록하지 않고 다음을 보존한다.

- provider 또는 local runtime와 version
- model artifact ID와 digest
- quantization, context length, tokenizer
- sampling parameters와 seed 지원 여부
- reasoning/tool-call mode
- hardware, VRAM/RAM, concurrency

비교군은 최소 다음을 포함한다.

- 동일 계열의 더 작은 open-weight model 1개
- 동일 또는 유사 context의 다른 open-weight model 1개
- 예산이 허용되면 frontier hosted model 1개

Evo-Bench의 Qwen3.6-27B 결과는 harness를 변경한 evolver 역할의 증거이지, 해당 27B 모델을 XGENy policy로 사용했을 때의 성능 증거가 아니다. 따라서 XGENy에서 직접 측정한다.

### 4.3 저장 구현

| ID | 후보 |
|---|---|
| S0 | raw append-only JSONL + snapshot |
| S1 | hardened JSONL: single writer, fsync, directory sync, tail repair, generation check |
| S2 | per-Run embedded SQLite, WAL + `synchronous=FULL` |
| S3 | per-Run embedded SQLite, WAL + `synchronous=NORMAL` |

S3는 성능 비교용이며 power-loss durability 기본안으로 자동 채택하지 않는다. JSONL export는 모든 후보에서 동일한 wire fixture를 생성해야 한다.

## 5. Task suite

### 5.1 장기 작업 연속성

최소 30개 task를 구성하고, 다음을 섞는다.

- 여러 파일을 조사하고 수정한 뒤 build/test로 검증
- 중간 사용자 결정을 기다렸다 재개
- 서로 독립적인 분기와 선행 조건이 있는 작업
- context budget의 2배, 5배, 10배에 해당하는 관찰 기록
- process restart 0회, 1회, 3회 이상
- 잘못된 초기 계획을 evidence로 수정해야 하는 작업

각 task는 ordered milestone과 최종 executable verifier를 가진다. 단순 최종 파일 일치만 보지 않고 중간 불변 조건도 검사한다.

### 5.2 저장소와 crash recovery

상태 전이 경계마다 deterministic failpoint를 둔다.

```text
before transaction
after event append
after graph projection
after effect intent
after transaction commit
before external call
after external call / before acknowledgement
after acknowledgement / before receipt commit
after receipt commit / before outbox cleanup
snapshot/compaction 시작·완료 경계
```

각 failpoint에서 process를 강제 종료하고 재시작한다. 가능한 환경에서는 VM 전원 차단 또는 block-device fault harness로 power-loss를 별도 실행한다. process kill 결과를 power-loss 보장으로 일반화하지 않는다.

필수 불변 조건:

- committed event sequence에 gap이나 fork가 없다.
- projection은 committed event로 결정론적으로 재구성된다.
- 손상·부분 기록은 성공으로 해석하지 않는다.
- 실행 여부를 모르는 effect는 `effect_unknown` 또는 동등한 보수 상태가 된다.
- sink가 idempotency 조회를 지원하지 않으면 자동 재실행하지 않는다.
- authorization budget은 effect와 분리되어 두 번 소비되지 않는다.
- recovery를 여러 번 반복해도 terminal 상태와 artifact digest가 변하지 않는다.

### 5.3 Tool failure

각 effect class에서 다음 fault를 주입한다.

- timeout before execution
- timeout after execution
- acknowledgement loss
- stale success
- silent no-op
- corrupted output
- schema mismatch
- partial stream
- cancellation race
- remote task status regression

대상 capability:

- read-only filesystem
- compare-and-swap file write
- process execution
- idempotent HTTP test sink
- non-idempotent counter/payment simulator
- compensatable reservation simulator
- MCP task fake

### 5.4 Memory

EvoMemBench의 4분면을 참고해 다음을 분리한다.

| 축 | 조건 |
|---|---|
| episode | in-episode / cross-episode |
| content | knowledge / execution procedure |

각 조건에 correct, stale, conflicting, irrelevant, adversarial memory를 주입한다. 비교군은 no-memory, full-context, vector-only, summary-only, provenance-tiered memory다.

필수 측정:

- retrieval precision/recall과 최종 task completion
- evidence citation correctness
- stale/conflict 감지율
- negative transfer rate
- memory token·latency·model-call cost
- 잘못된 memory를 제거한 뒤 회복 여부

### 5.5 XGEN·Connector 호환성

core는 XGEN DB·MinIO·패키지를 사용하지 않은 상태에서 다음을 검증한다.

1. protocol golden fixture round-trip
2. XGEN canonical ↔ legacy projection
3. read-only shadow run의 event·artifact·receipt 비교
4. local authority와 server authority의 revision conflict
5. handoff와 cursor resume
6. Connector authenticated loopback fake의 scope mismatch 차단
7. XGEN/Connector unavailable 상태에서 local run 무회귀

외부 effect를 만드는 shadow 검증은 실제 effect를 재실행하지 않는다.

### 5.6 안전성과 완료의 분리

완료했지만 안전하지 않은 run을 별도로 집계한다.

- permission scope 초과
- 승인 전 effect 시작
- duplicate effect
- secret 노출
- 검증되지 않은 성공 주장
- 불확실한 effect를 success 또는 failure로 임의 확정
- rollback 불가능한 외부 상태를 복구됐다고 주장

`task success`와 `safe task success`를 모두 보고한다.

## 6. OS와 환경 매트릭스

| 환경 | 최소 검증 |
|---|---|
| Ubuntu LTS, ext4 | 전체 fault injection, package E2E |
| macOS current-1/current, APFS | filesystem/process 차이, install/restart |
| Windows 11, NTFS | rename/locking/process tree, install/restart |

추가 환경은 Linux 컨테이너, WSL2, network filesystem을 포함할 수 있지만 로컬 디스크 결과와 섞지 않는다. CI process-kill과 실제 VM power-loss는 별도 표로 보고한다.

## 7. 반복 수와 통계

- deterministic storage test: 각 failpoint·storage·OS 조합 최소 20회, 실패가 있으면 원인 제거 후 전체 재실행
- stochastic agent task: 구성별 task당 최소 3회, pilot variance를 본 뒤 power analysis로 확대
- task-level paired comparison을 기본으로 한다.
- completion·safe completion은 paired bootstrap 95% CI와 적절한 paired categorical test를 보고한다.
- token, latency, cost는 median, IQR, bootstrap CI를 보고한다.
- 다중 비교 시 보정 방법을 사전 명시한다.
- 효과 크기와 절대 실패 수를 p-value와 함께 보고한다.
- seed를 지원하지 않는 hosted model은 호출 ID와 반복 분포를 기록한다.

Pilot 결과를 보고 task를 제거하지 않는다. verifier 오류가 확인되면 변경 이력과 기존·수정 결과를 함께 남긴다.

## 8. Metric

### Primary

- safe task completion rate
- duplicate external effect count
- silent effect loss count
- unrecoverable committed-state loss count
- restart 이후 정확한 active frontier 복원율

### Secondary

- ordinary task completion
- manual intervention rate
- false-success rate
- recovery latency
- model calls, input/output tokens, wall time, monetary cost
- journal/store bytes와 startup time
- memory negative transfer rate
- compatibility fixture pass rate

### Diagnostic

- replanning 횟수와 원인
- context에 포함된 evidence의 비율
- effect_unknown 체류 시간과 해소 경로
- verifier failure taxonomy
- storage corruption classification

## 9. 재현성 패키지

각 실험 release는 다음을 포함한다.

```text
experiment-manifest.json
task-fixtures/              # digest-pinned
fault-schedules/
verifiers/
protocol-fixtures/
environment-locks/
raw-run-events/             # redacted
receipts/
aggregate-results/
analysis-script/
```

`experiment-manifest.json`에는 Git commit, dirty 여부, Cargo.lock digest, OS image, filesystem, model artifact, prompt digest, capability schema digest, clock/ID seed, 시작·종료 시각을 기록한다. credential, 개인 경로, 원문 secret은 포함하지 않는다.

## 10. 채택 기준

### Durable store

S2를 기본 후보로 채택하려면 다음을 모두 만족해야 한다.

- 세 OS에서 별도 DB 설치 없이 package E2E 통과
- 모든 process-crash failpoint에서 committed-state invariant 위반 0건
- 지원하는 power-loss 환경에서 corruption 또는 silent committed loss 0건
- startup·write latency가 CLI 사용에 허용 가능한 범위
- export된 JSONL이 protocol fixture와 일치
- 손상 시 영향 범위가 해당 Run으로 제한되고 진단 가능

S1이 같은 안전성과 더 낮은 복잡도를 증명하면 JSONL을 유지할 수 있다. 결과가 불충분하면 ADR-0008은 승인하지 않는다.

### WorkGraph/context

X1 이상은 B1 대비 safe task completion의 신뢰구간이 실용적 개선 기준을 넘고, cost/latency Pareto frontier에서 합리적이어야 한다. 임계값은 pilot 전에 제품 예산과 함께 숫자로 고정한다.

### Effect protocol

지원한다고 선언한 sink에서 duplicate effect 0건이어야 한다. sink가 idempotency/reconcile을 지원하지 않으면 `exactly once`라는 표현을 금지하고 manual/compensation 경로를 제품에 노출한다.

### Memory

평균 향상만으로 채택하지 않는다. stale/conflicting memory에서 negative transfer가 정한 한도를 넘으면 자동 주입을 제한하고 사용자 검토 또는 retrieval abstention을 기본으로 한다.

## 11. 중단 조건

다음 중 하나가 확인되면 기능 확장보다 설계를 먼저 수정한다.

- 동일 authorization으로 물리 effect가 두 번 발생
- effect_unknown을 근거 없이 자동 success/failure로 확정
- committed Run 상태의 silent loss
- 모델 context가 WorkGraph authority나 PolicyDecision을 덮어씀
- XGEN/Connector 내부 저장소 의존이 core로 유입
- memory가 provenance 없이 정책·사실로 승격
- hash chain을 인증 또는 실행 원자성으로 오해한 구현
- benchmark task를 prompt/memory에 노출한 contamination

## 12. 단계별 실행

1. storage/reducer/effect simulator만 있는 model-free vertical slice
2. Linux process-crash matrix
3. macOS/Windows portability와 package E2E
4. VM power-loss spike
5. fake model 기반 deterministic long-run replay
6. Qwen3.6-27B pilot와 ablation
7. frozen task suite 반복 실험
8. XGEN/Connector read-only shadow conformance
9. 결과와 artifact 공개 가능한 범위 정리

1~4가 통과하기 전에 실제 non-idempotent 외부 서비스나 운영 XGEN effect 경로를 연결하지 않는다.

## 13. 보고 규칙

최종 보고서는 다음을 명시적으로 구분한다.

- 논문·공식 문서가 직접 보인 사실
- XGENy 설계에 대한 추론
- XGENy 실험으로 관찰한 결과
- 아직 검증하지 못한 주장

부정 결과도 보존한다. 특정 모델·OS·filesystem에서만 통과한 결과는 전체 제품 보장으로 확대하지 않는다.
