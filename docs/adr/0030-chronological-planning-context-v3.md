# ADR-0030: Journal chronology를 보존하는 PlanningContext v3

- 상태: Accepted
- 날짜: 2026-09-01
- 적용 범위: `xgeny-local-store`, `xgeny-runtime`, `xgeny-provider-openai`

## 배경

ADR-0020의 PlanningContext v2는 completed ToolOutput과 Step을 Step ID 사전순으로 직렬화했다. Step
ID가 내용 digest에서 파생되면서 이 순서는 계획·실행 시간과 관계없는 순서가 됐다. 실제 Qwen coding
gate에서 모델은 실패한 test 뒤의 corrective patch보다 재-test와 build를 먼저 선택했다. Durable
ToolOutput 자체는 정확했지만 여러 turn의 관찰 순서가 context에서 사라져, WorkGraph가 장기 연속성을
보존한다는 목표를 만족하지 못했다.

Step ID 정렬은 같은 snapshot의 결정론성만 제공한다. 에이전트가 과거 관찰을 시간 순서대로 해석할 수
있다는 의미는 제공하지 않는다.

## 결정

### 1. Store가 두 chronology를 같은 generation에서 제공한다

`RunPlanningSnapshot`은 기존 state와 output map에 다음 두 order vector를 추가한다.

```text
planningStepOrder
completedToolOutputOrder
```

- `planningStepOrder`: `StepPlanned` 또는 `PlanAccepted` journal sequence 오름차순이다. 같은
  `PlanAccepted` 안에서는 accepted proposal의 Step 배열 순서를 사용한다.
- `completedToolOutputOrder`: passed Core Receipt event sequence 오름차순이다. 즉 output이 다음 turn에
  관찰 가능해진 순서다.

VerifiedRunIndex는 기존 journal과 Receipt anchor에서 두 순서를 재구성한다. Memory/SQLite의 warm path와
cold reopen은 같은 결과를 내야 한다. SQLite table 또는 journal schema는 변경하지 않는다.

### 2. Runtime이 order vector를 다시 검증한다

Runtime은 provider context를 만들기 전에 다음을 확인한다.

- planning order가 projected WorkGraph Step의 중복 없는 exact permutation
- output order가 receipt-completed output binding의 중복 없는 exact permutation
- 각 ToolOutput이 기존과 동일한 Step/intent/evidence/Receipt/digest에 결합

누락, 중복, unknown Step 또는 binding 불일치는 provider reservation 전에
`PlanningSnapshotMismatch`로 실패한다. 정렬 오류를 보정하거나 Step ID 순서로 fallback하지 않는다.

### 3. PlanningContext v3의 배열 순서에 의미를 부여한다

Provider payload와 context digest domain을 각각 다음으로 올린다.

- `xgeny.planning-context/v3`
- `xgeny.planning-context.digest/v3`

`steps`는 durable planning chronology, `toolOutputs`는 durable receipt-completion chronology로
직렬화한다. JSON field shape와 size budget은 v2를 유지한다. ToolOutput은 여전히 mandatory이며
truncate, summarize 또는 일부 omit하지 않는다.

OpenAI-compatible adapter도 v3만 수락한다. System prompt는 두 배열의 chronology 의미를 명시하고
prompt revision/request-profile digest를 갱신한다. Provider가 받는 exact request 의미가 달라졌으므로
v2 이름을 유지한 채 조용히 순서만 바꾸지 않는다.

## 호환성과 보안

- Public XGEN/XGENy protocol v0.1과 SQLite physical schema 8은 바뀌지 않는다.
- 기존 journal, projection, ToolOutput, Receipt bytes를 다시 쓰지 않는다.
- 완료된 legacy Run의 offline replay에는 provider context가 필요하지 않아 영향이 없다.
- 미완료 v2 Run의 manifest는 이전 immutable request-profile digest를 고정한다. 이미 journal에 계획된
  로컬 action은 같은 local-execution profile과 별도 승인을 만족하면 모델 없이 마칠 수 있지만, 다음
  model egress를 요청하면 RC3 binary가 provider 호출 전에 `configuration_mismatch`로 fail-closed한다.
  v2 context로 조용히 model planning을 재개하면 확인된 chronology 결함을 다시 활성화하므로 자동
  upgrade나 fallback을 제공하지 않는다. 필요하면 원래 binary로 Run을 마치거나 새 v3 Run을 시작한다.
- Raw output의 provider egress·Debug redaction 경계는 ADR-0020과 동일하다.
- XGEN, Connector, DB, MinIO 의존성을 추가하지 않는다.

## 검증

- Step ID 사전순과 반대인 accepted proposal order의 Memory/SQLite cold-reopen parity
- Receipt vector 입력 순서와 무관한 event-sequence ToolOutput chronology
- Runtime의 duplicate/missing/unknown order fail-closed
- Context v3 직렬화 순서와 context/request-profile digest 회귀
- Provider loopback request의 v3 profile과 chronological system prompt
- v2 request-profile manifest의 다음 model call 전 fail-closed와 완료 Run offline replay
- 기존 workspace test 전체와 실제 Qwen search → read → patch → failed test → corrective patch →
  re-test → build gate

## 대안

Goal 문구에 단계 번호를 반복하는 방법은 모델별 prompt 준수율만 높이고 뒤섞인 durable context를
고치지 못하므로 채택하지 않는다. Step ID에 timestamp를 넣는 방법은 identity와 chronology를 다시
결합하고 기존 journal을 깨뜨리므로 채택하지 않는다.
