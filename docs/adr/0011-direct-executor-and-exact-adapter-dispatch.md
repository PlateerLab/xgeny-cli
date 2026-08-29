# ADR-0011: Direct Executor와 exact adapter dispatch

- 상태: 제안 — 연구 gate 기본형 구현
- 기준일: 2026-08-29
- 적용 범위: local orchestration authority의 effect 실행·복구 경계
- 공개 protocol v0.1 변경: 없음
- local store schema 변경: 없음(schema 3 유지)

> 후속 상태: ADR-0012가 adapter outcome을 effect evidence로 명확히 분리하고, exact read-only verifier, Core-owned Execution Receipt와 schema 4 원자 저장을 구현했다. 아래 비목표는 ADR-0011 결정 당시의 범위를 기록한다.

## 문맥

ADR-0010은 `EffectIntent`와 secret-free `InvocationMaterialRecord`를 원자적으로 commit하고, 재시작 뒤 exact material을 복구하는 계약을 만들었다. 그러나 기존 저수준 runtime은 호출자가 sink와 prepared value를 직접 넘겼고, adapter가 action·Definition·Instance digest를 자기보고했다. 이 구조에서는 다음을 core가 증명할 수 없다.

- committed `provider_id`와 실제 reconstruction provider가 같은가
- selected Instance의 exact operation을 구현한 adapter가 선택됐는가
- prepared value가 다른 Run·Step·effect·journal head에서 재사용되지 않았는가
- credential principal이나 동적 health가 admission 뒤 바뀌지 않았는가
- adapter가 임의 오류·receipt 문자열에 argument나 credential을 섞어 journal에 넣지 않았는가

Actual filesystem/process/MCP/XGEN adapter를 붙이기 전에 이 경계를 닫지 않으면 테스트 fake에서는 동작해도 배포 adapter에서 identity confusion과 중복 effect가 생길 수 있다.

## 결정

### 1. Provider identity는 host registry가 소유한다

`InvocationMaterialProvider`는 더 이상 `provider_id()`를 자기보고하지 않는다. Trusted composition root가 `MaterialProviderRegistry`에 stable ID와 provider object를 함께 등록한다.

Recovery는 material record에 commit된 `provider_id`로 정확히 한 번 조회한다. Missing key, invalid key와 duplicate registration은 고정 오류로 닫고, 다른 provider를 순회하거나 default provider로 fallback하지 않는다. Duplicate registration은 기존 entry를 교체하지 않는다.

Provider는 trusted in-process port다. 이 registry는 hostile plugin sandbox가 아니며 `reconstruct`가 외부 effect를 만들지 않아야 한다는 composition contract를 강제 격리로 과장하지 않는다. Actual provider별 conformance/fault test는 해당 provider 구현과 함께 추가한다.

### 2. Adapter는 full Instance binding으로 exact dispatch한다

Capability document catalog와 process-local behavior registry를 분리한다.

```text
CapabilityRegistry           EffectAdapterRegistry
------------------           ---------------------
Definition/Instance 문서      실행 behavior object
immutable semantic catalog   process-local composition
digest 검증 입력              exact dispatch 대상
```

Adapter key는 다음 세 값을 byte-exact로 포함한다.

```text
binding_ref
operation_ref: Option<String>
protocol_version: Option<String>
```

`None`과 `Some`은 다르다. URI normalization, default operation, compatible version 탐색, binding-only fallback과 다른 Instance reroute를 하지 않는다. Duplicate full key는 교체 없이 거부한다. Adapter는 자신의 ID나 binding digest를 반환하지 않는다.

### 3. Prepare와 execute를 분리하고 session을 1회 소비한다

Exact adapter의 `prepare`는 검증된 argument를 빌려 side-effect-free 준비만 수행한다. 반환값은 identity getter가 없는 owned `PreparedAdapterInvocation`이다.

```text
EffectAdapter.prepare(verified request)
        |
        v
Box<dyn PreparedAdapterInvocation>
        |
        | core-owned identity wrapper
        v
durable EffectExecutionStarted commit
        |
        v
session.execute(self: Box<Self>)
```

Session의 `execute(self: Box<Self>)`는 type level에서 같은 value의 두 번째 호출을 막는다. Invocation material은 borrow하므로 Started 전 실패 뒤 fresh session을 다시 prepare할 수 있다. External effect가 시작됐을 가능성이 있는 transport 오류는 definite failure가 아니라 fixed `Unknown` class로 반환해야 한다.

### 4. Prepared identity는 core가 만들고 full journal head에 결합한다

Adapter session은 private-constructor의 core-owned wrapper 안에 들어간다. Wrapper는 최소 다음 identity를 core-verified 값에서 복사한다.

- Run ID, authority와 authority epoch
- journal sequence와 head digest
- Step ID와 effect ID
- complete `InvocationMaterialRecord`
- exact adapter binding key

저수준 `EffectSink`, adapter-owned `PreparedEffect` trait과 `DurableEffectRuntime`은 public API에서 제거한다. Production wrapper는 `DirectExecutor`만 만들 수 있고 Clone, Serialize, Deserialize를 구현하지 않는다.

Adapter `prepare` 뒤 runtime은 store를 다시 load해 full head, Step status, intent와 durable material sidecar를 wrapper와 대조한다. Head가 한 event라도 바뀌었으면 `EffectExecutionStarted`를 쓰지 않고 session을 폐기한다.

### 5. 실행 순서를 고정한다

`IntentCommitted`의 실행 순서는 다음과 같다.

```text
lease
  -> current Run/Step/intent load
  -> exact durable material record match
  -> material digest match
  -> Definition digest match
  -> Instance executable-binding digest match
  -> current health/auth gate
  -> exact adapter lookup
  -> side-effect-free prepare
  -> full head + material sidecar reload/recheck
  -> EffectExecutionStarted CAS commit
  -> one-shot execute
  -> outcome commit
```

Started commit 전 오류는 execute 0회다. Started commit 뒤 process exit 또는 outcome commit 유실은 durable `Executing`을 남긴다. 재시작에서 `Executing`은 provider lookup, prepare와 execute를 호출하지 않고 `EffectUnknown`으로 전이한다. Blind retry는 금지한다.

### 6. MVP credential gate는 credential-free Instance만 허용한다

현재 executable-binding digest는 stable `auth_ref` 문자열을 포함하지만, resolved principal·credential version·expiry witness를 intent에 commit하지 않는다. 같은 reference가 다른 principal로 retarget되는 경우를 실행 직전에 증명할 수 없다.

따라서 이 기본형은 다음 exact shape만 실행한다.

```text
health.status == Available
auth.state == NotRequired
auth.auth_ref == None
```

`Degraded`, `Unavailable`, `Unknown`, credential-bearing 또는 inconsistent auth shape는 adapter prepare 전 fail closed한다. Credential support는 stable principal/version witness, OS credential store와 redaction test를 함께 설계하는 별도 ADR 뒤에 추가한다.

### 7. Adapter outcome은 closed type만 journal로 변환한다

Actual adapter API는 임의 `String` receipt, evidence와 reason을 받지 않는다.

- Receipt/evidence: canonical lowercase `sha256:<64 hex>` typed value
- Execute unknown: fixed enum
- Reconciliation inconclusive: fixed enum
- Prepare failure: fixed enum

Rejected digest와 adapter failure는 raw candidate나 provider/OS 메시지를 Display하지 않는다. Core만 이 typed outcome을 기존 internal state-machine event로 변환한다.

## 신뢰 경계

보장은 built-in verified `RunStore`, canonical local lease, host-owned event factory, schema-validated Capability Registry와 trusted composition root를 전제로 한다. Model argument, protocol/config bytes, durable DB bytes, provider reconstruction 결과와 adapter outcome은 검증 대상이다.

Public Rust trait은 hostile in-process code를 격리하지 않는다. 악성 provider/adapter가 `prepare`에서 직접 I/O를 하거나 process memory를 읽는 행위는 이 contract만으로 막을 수 없다. 실제 third-party extension은 out-of-process protocol, sandbox와 provenance를 별도로 설계해야 한다.

## 불변 조건

1. Provider와 adapter identity는 implementation 자기보고가 아니라 host registration key다.
2. Provider와 adapter lookup은 exact key 한 번이며 fallback은 0회다.
3. Raw arguments는 adapter의 borrowed prepare request 밖으로 durable persist되지 않는다.
4. Adapter가 실행 identity digest를 만들거나 override하지 않는다.
5. Prepared session은 Run·Step·effect·material·full journal head에 core가 결합하고 값으로 1회 소비한다.
6. Started durable commit 전 external execute는 0회다.
7. `Executing` recovery에서 provider, prepare와 execute는 모두 0회다.
8. Definition, Instance, material, head, health 또는 auth drift는 Started와 execute 0회로 닫힌다.
9. Credential witness가 없는 동안 credential-bearing Instance를 실행하지 않는다.
10. Adapter의 arbitrary error text와 malformed digest는 journal에 들어갈 수 없다.
11. Pre-start 실패는 ephemeral material을 소비하지 않으며 재시도마다 fresh one-shot session을 만든다.

## 결과

- Same-process ephemeral material과 restart-reconstructed material이 동일한 Direct Executor 경로를 사용한다.
- Fake adapter로 admission부터 SQLite restart와 durable effect execution까지 실증할 수 있다.
- 기존 state machine의 crash-conservative recovery를 유지하면서 public identity-forging 경계를 제거한다.
- Actual OS/MCP/XGEN adapter는 이 exact contract를 구현하는 다음 수직 slice로 분리된다.

비용은 dynamic adapter registry가 별도로 필요하고, adapter prepare가 owned one-shot session을 만들어야 하며, credential-bearing tool은 후속 witness 설계 전까지 실행할 수 없다는 점이다.

## 명시적 비목표

- Actual filesystem, process, network, MCP, Connector 또는 XGEN adapter
- OS credential resolution, secret injection, sealed material과 memory zeroization
- Hostile in-process plugin 격리, adapter binary signing과 hot reload
- Sandbox, filesystem symlink/TOCTOU와 process tree 제어
- Receipt body, Artifact store와 verification runner
- Async/task execution, cancellation과 streaming
- Provider retry/backoff와 durable blocked scheduling
- Managed, critical 또는 reusable authorization
- 외부 sink의 물리적 exactly-once 보장

## 승인 조건

- Exact provider/adapter lookup과 duplicate non-replacement test가 통과한다.
- Operation/protocol mismatch에서 nearby adapter 호출이 0회다.
- Prepare failure, head drift, material/Definition/Instance/health/auth drift에서 Started와 execute가 0회다.
- Prepare failure와 Started commit failure 뒤 같은 ephemeral material로 fresh prepare retry가 가능하다.
- Fake execute가 Started commit 뒤 정확히 한 번 호출된다.
- Exact reconciliation의 `NotApplied` 뒤 같은 intent/material을 쓰되 authorization use는 증가하지 않는다.
- Lost outcome commit과 process exit 뒤 `Executing` recovery가 execute를 반복하지 않는다.
- SQLite reopen에서 reconstruct → prepare → start → execute E2E가 통과한다.
- Raw argument·path·credential·adapter-error sentinel이 Debug, error, JSONL, SQLite, WAL/SHM에 없고 opaque reference ID는 Debug/error에 노출되지 않는다.
- Linux, macOS와 Windows의 전체 workspace test가 통과한다.
