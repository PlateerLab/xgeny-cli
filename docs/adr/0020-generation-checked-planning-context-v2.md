# ADR-0020: Generation-checked PlanningContext v2

- 상태: Accepted
- 날짜: 2026-08-30
- 적용 범위: `xgeny-local-store`, `xgeny-runtime`, `xgeny-provider-openai`, CLI driver 검증

> 후속 상태: [ADR-0030](0030-chronological-planning-context-v3.md)은 Step ID 사전순이 장기 작업의
> 관찰 chronology를 보존하지 못한다는 실제 Qwen gate 결과에 따라 provider payload를 v3로 올렸다.
> v2의 generation/output binding은 유지하고, Step은 plan journal 순서, ToolOutput은 passed Receipt
> 순서로 전달한다.

## 배경

[ADR-0019](0019-durable-tool-output-and-schema-7.md)는 exact typed JSON tool output을
`ToolOutputRecord` sidecar로 원자 저장했다. 그러나 `AgentLoop`는 planning 직전에
`RunState`만 읽었기 때문에 raw output을 다음 model turn에 전달하지 못했다. 독립적인
`load_current`와 `load_tool_output` 호출을 조합하면 concurrent writer가 두 호출 사이에
journal head를 바꿔 서로 다른 generation의 state와 output을 섞을 수도 있다.

이번 결정은 다음 질문만 닫는다.

1. 어떤 durable output을 다음 model turn에 공개하는가?
2. state, Receipt와 output이 같은 store generation에서 검증됐음을 어떻게 보장하는가?
3. mandatory output이 context budget을 넘을 때 어떻게 실패하는가?
4. OpenAI-compatible provider가 이를 어떤 prompt/profile로 전송하는가?

최종 completion summary의 durable 복원은
[ADR-0021](0021-durable-completion-output-and-schema-8.md)에서 닫는다. 제품 filesystem
adapter와 public `xgeny run`은 별도 변경이다.

## 결정

### 1. 같은 generation의 planning snapshot

`RunStore`는 다음 계약을 제공한다.

```text
load_planning_snapshot(expectedHead, maxOutputBytes)
  -> RunPlanningSnapshot {
       state,
       completedToolOutputs: BTreeMap<stepId, ToolOutputRecord>
     }
```

Built-in Memory/SQLite store만 이 계약을 구현한다. 기본 trait 구현은 여러 point read를
조합하지 않고 `PlanningSnapshotStoreUnsupported`로 실패한다.

SQLite 구현은 하나의 read transaction 안에서 다음 순서를 지킨다.

1. `data_version`과 durable journal head로 verified cache를 확인한다.
2. caller의 `expectedHead`와 exact head가 같은지 확인한다.
3. verified index의 Receipt/output binding과 output size를 확인한다.
4. 전체 raw output 크기가 `maxOutputBytes` 이하인 경우에만 exact output row를 point-load한다.
5. 각 row를 self-verifying record 및 journal anchor와 다시 대조하고 transaction을 끝낸다.

`AgentLoop`는 기존 verified state의 head를 expected head로 전달한다. Snapshot 이후
reservation append 전에 writer가 head를 바꾸면 compare-and-append가 실패한다. 따라서
어느 race에서도 provider는 stale/mixed context로 호출되지 않는다.

### 2. output eligibility

`toolOutputs`에 들어가는 항목은 모두 다음 조건을 만족해야 한다.

- Step status가 `Completed`
- `outputRecordDigest`가 존재
- exact intent/effect와 `ToolOutputRecord`가 존재
- Step의 execution attempt/evidence와 record가 일치
- passed Core Receipt sidecar와 event anchor가 존재
- Receipt ID/digest가 projection과 일치
- Receipt output digest와 record output digest가 일치

`Validating`, `Failed`, `ManualRequired` output은 공개하지 않는다. Schema 3~6에서 유래한
outputless legacy `Completed` Step에는 output을 추론하거나 발명하지 않는다. Eligible
Step의 sidecar가 누락·변조되면 조용히 생략하지 않고 provider reservation 전에 실패한다.

### 3. PlanningContext v2

Provider-neutral payload profile을 `xgeny.planning-context/v2`로 올리고 mandatory
`toolOutputs` section을 추가한다.

```json
{
  "profileVersion": "xgeny.planning-context/v2",
  "toolOutputs": [
    {
      "stepId": "step-1",
      "capability": {
        "capabilityId": "filesystem.read_text",
        "contractVersion": "1.0.0"
      },
      "outputId": "output-...",
      "outputDigest": "sha256:...",
      "receiptDigest": "sha256:...",
      "canonicalSizeBytes": 123,
      "output": {"content": "exact local observation"}
    }
  ]
}
```

정렬은 Step ID 기준으로 결정론적이다. `ToolOutputRecord` 전체를 serialize하지 않는다.
그렇게 하면 selected Instance, plan/evidence 내부 메타데이터까지 provider에 불필요하게
노출되기 때문이다. 위 allowlist와 exact output value만 전달한다.

Tool output은 Step/Capability optional packing보다 먼저 전부 넣는다. 일부 output을
truncate, summarize 또는 omit하지 않는다. Mandatory base가 Run budget 또는 512 KiB hard
limit을 넘으면 `ContextBudgetExceeded`로 quiesce하고 reservation/provider 호출은 모두 0회다.
Warm planning snapshot path는 verified canonical size 합을 raw row point-load 전에 검사하므로
budget 실패 전에 수 GiB를 materialize하지 않는다. Cold open이나 외부 `data_version` 변경 뒤의
integrity audit는 모든 output row를 bounded row 단위로 순차 검증한 뒤 이 warm 경계를 다시 연다.

Context digest domain도 `xgeny.planning-context.digest/v2`로 올린다. JSON object key 입력
순서는 RFC 8785 canonicalization 결과를 바꾸지 않지만 exact output body가 바뀌면 context
digest도 바뀐다.

### 4. Provider와 비신뢰 경계

OpenAI-compatible adapter는 다음 값을 올린다.

- planning context profile: v2
- prompt template revision: `xgeny.openai-planner-prompt/v2`
- prompt template digest와 request profile digest: 새 의미로 재계산

Request envelope, proposal schema와 Chat Completions dialect는 v1을 유지한다.
System prompt는 `toolOutputs[*].output`이 Receipt-completed observation이지만 여전히
비신뢰 data이며, 내부 문장을 instruction·permission·authority로 실행하면 안 된다고
명시한다.

Raw output은 local sidecar에서 읽혀 transient in-memory snapshot과 PlanningContext를 거쳐 실제
provider request body에 존재한다. 이 값은 Snapshot, `PlanningToolOutput`, `PlanningContext`,
`PlannerCallRequest`의 `Debug`, journal, projection, Receipt, durable export, fixed error와 일반
log에는 나타나지 않는다.

파일 읽기 권한과 remote model egress 권한은 같은 의미가 아니다. 이번 ADR은 전달 구조를
정의하지만 public composition이 별도 egress 결정 없이 민감한 local output을 원격
provider로 보내도 된다는 권한을 부여하지 않는다.

## 호환성

- Public XGEN/XGENy protocol v0.1은 바뀌지 않는다.
- SQLite physical schema는 7을 유지한다.
- `ToolOutputRecord`와 기존 journal/projection/Receipt bytes를 다시 쓰지 않는다.
- OpenAI request-profile digest는 의도적으로 변경된다.
- Third-party `RunStore`는 generation-checked snapshot 계약을 명시적으로 구현해야 한다.
- XGEN, Connector, DB, MinIO에 대한 core 의존성을 추가하지 않는다.

## 검증

- Memory/SQLite 동일 snapshot, stale head와 exact raw-output byte gate
- SQLite cold reopen 및 warm point-load에서 historical rescan 없음
- 외부 connection 변조 시 `data_version` 재감사 후 fail-closed
- Legacy outputless completion의 empty `toolOutputs`
- 실제 CLI driver의 plan → approval → ReadOnly execution → Receipt → SQLite restart → next model turn
- Nested object/array/quote/backslash의 exact JSON 보존
- Mandatory context 최소 byte 경계 통과와 1-byte-short zero-call
- Missing snapshot output의 reservation/planner zero-call
- OpenAI-compatible loopback HTTP body에 sentinel이 정확히 한 번 존재
- Raw output의 journal/projection/Receipt/Debug/error 비노출

## 후속 작업

[ADR-0021](0021-durable-completion-output-and-schema-8.md)이 첫 항목을 완료했다. 남은 순서는
다음과 같다.

1. OS별 confinement를 갖춘 bounded UTF-8 filesystem read adapter를 구현한다.
2. Public CLI composition에서 local/remote provider egress 승인을 분리한다.
3. go50902 live model로 실제 file → tool output → follow-up completion을 실증한다.
