# Recoverable Invocation Material 기본형

- 기준일: 2026-08-29
- 상태: ADR-0010 연구 gate용 내부 구현
- 공개 protocol v0.1 변경: 없음
- local store schema: 3

## 목적

Admission이 commit한 exact effect를 process restart 뒤에도 안전하게 이어 갈 수 있도록 secret-free recovery sidecar를 둔다. 자동 복구가 불가능한 invocation은 `IntentCommitted`에 무한히 남기지 않고 effect 시작 전 durable `ManualRequired` 상태로 닫는다.

이 slice는 actual adapter를 호출하지 않는다. 다음 Direct Executor가 사용할 material recovery와 검증 경계만 만든다.

## 전체 흐름

```text
AdmissionRequest.arguments
          |
          v
schema + 1 MiB limit + trusted resource resolution
          |
          v
canonical arguments + exact permission request
          |
          v
policy allow + deterministic Router + selected Instance
          |
          v
EffectIntent + authorization + InvocationMaterialRecord
          |                 one SQLite transaction
          v
      commit acknowledged
          |
          +------------------------------+
          |                              |
          v                              v
  Ephemeral material            ReconstructableReference
  same-process only             providerId + opaque referenceId
          |                              |
          | restart/loss                 | restart
          v                              v
 fixed-code ManualRequired      trusted reconstruction provider
                                         |
                                         v
                         schema/resource/action/binding revalidation
                                         |
                              +----------+----------+
                              |                     |
                              v                     v
                      recovered material     fixed-code ManualRequired
                      for next PR only       or Store corruption
```

## Durable sidecar 계약

`InvocationMaterialRecord`는 journal event와 별개의 local-store sidecar다. Journal은 intent와 authorization의 논리 정본이고 sidecar는 그 intent를 실행하기 위해 material을 다시 획득할 수 있는지 설명한다.

Record는 다음 identity를 명시적으로 보존한다.

```text
run_id
step_id
effect_id
action_digest
capability_id
contract_version
definition_digest
instance_id
instance_binding_digest
recovery_kind
record_format_version
recovery_descriptor
record_digest
```

Recovery descriptor는 다음 두 형태만 허용한다.

```text
Ephemeral

ReconstructableReference {
  provider_id,
  reference_id,
  revision
}
```

`provider_id`는 trusted runtime이 선택한 reconstruction provider와 일치해야 한다. 실제 provider registry와 adapter dispatch는 다음 Direct Executor PR에서 연결한다. `reference_id`는 provider namespace 안의 opaque, secret-free, non-bearer identifier이고 `revision`은 같은 identifier의 mutable retarget을 막는다. 세 값 모두 길이와 문자 집합을 제한하며 `Debug`에는 reference ID 원문을 출력하지 않는다.

다음 값은 record에 들어갈 수 없다.

- Raw 또는 canonical JSON arguments
- Credential, API key, password, session cookie와 OAuth token
- Presigned URL이나 bearer URL
- Raw filesystem path 또는 shell command를 감춘 reference
- Environment value와 process stdin
- Adapter가 해석할 임의 untyped JSON blob

Sidecar digest는 record field 누락, accidental mutation과 다른 intent 간 swap을 탐지한다. Unkeyed SHA-256은 local writer 인증 수단이 아니며 hostile database writer에 대한 보안 경계로 설명하지 않는다.

## Admission commit

Admission은 schema 검증, resource canonicalization, policy와 Router 검사를 마친 뒤 effect identity를 만든다. Material recovery kind는 trusted host path가 선택하며 model이 지정한 문자열을 그대로 사용하지 않는다.

Commit 단위는 다음과 같다.

```text
BEGIN IMMEDIATE
  insert RunEvent
  insert effect_intents index
  insert authorization_consumption
  insert invocation_material
  write Run projection
COMMIT
```

어느 insert 뒤에서 오류나 process exit가 발생해도 전체 transaction이 rollback된다. Commit acknowledgement만 유실된 경우 reopen 결과에는 intent, authorization과 material record가 정확히 한 개씩 존재해야 한다. Caller는 같은 action에 새 authorization을 발행하지 않고 durable state를 기준으로 복구한다.

`Ephemeral` record는 argument를 durable store에 넣지 않는다. 정상 commit 뒤 같은 process에 남아 있는 opaque material handle만 사용할 수 있다. Commit 성공 여부가 불확실하거나 process가 재시작되어 handle을 찾을 수 없으면 자동으로 argument를 재생성하지 않는다.

## SQLite schema 3

Material sidecar가 없는 schema 1과 2 intent를 실행 가능하다고 해석할 수 없으므로 migration하지 않는다.

| 발견한 `user_version` | 동작 |
|---|---|
| `0` | schema 3 신규 생성 |
| `3` | 전체 record와 projection 검증 후 open |
| `1`, `2` | mutation 없이 `UnsupportedSchemaVersion` |
| `4+` | mutation 없이 `UnsupportedSchemaVersion` |

Version 2 record에 `Ephemeral`을 자동 backfill하지 않는다. 그렇게 하면 실제 material이 없는 legacy intent를 정상적인 same-process invocation처럼 보이게 할 수 있다. 연구 중 생성한 기존 database가 필요하면 deterministic JSONL export 등 명시적인 보존 절차를 거친 뒤 새 Run을 만든다.

Schema 3 load는 최소 다음을 대조한다.

- 모든 effect intent에 정확히 하나의 material record가 있는가
- Orphan material record가 없는가
- Effect ID, Step ID와 event sequence가 일치하는가
- Action, Definition과 Instance binding이 intent와 일치하는가
- Record digest가 field의 canonical encoding과 일치하는가
- Authorization consumption과 intent 수가 일치하는가
- Persisted projection이 event replay 결과와 일치하는가

구조 불일치는 `ManualRequired`가 아니라 store corruption이다. 손상된 store에는 recovery event를 추가하지 않는다.

## Recovery 계약

Recovery는 Run lease를 확인한 뒤 record를 읽는다. Replay 자체는 provider를 호출하지 않는다. Provider 호출은 명시적인 recovery command에서만 일어난다.

### `Ephemeral`

1. Admission 성공 직후 caller가 보유한 `AdmittedEffect`를 `into_ephemeral_material()`로 소비한다.
2. Handle의 payload digest와 durable record의 retention kind를 다시 확인한다.
3. 일치하면 same-process typed material을 반환한다.
4. Process restart 또는 lost acknowledgement로 `AdmittedEffect`를 잃은 경우 `recover()`는 argument를 추측하지 않고 `EphemeralMaterialUnavailable`을 반환한다.
5. Caller가 영구 유실을 확인한 경우에만 `ephemeral_material_lost`로 manual 전이를 명시적으로 commit한다.

현재 별도의 process-local material registry는 구현하지 않는다. 단순히 caller가 material을 아직 전달하지 않았다는 이유만으로 manual 처리하지 않으며, transient 오류를 runtime이 자동으로 영구 유실로 바꾸지도 않는다.

### `ReconstructableReference`

1. Trusted runtime이 선택한 provider의 `provider_id`가 record와 정확히 일치하는지 확인한다.
2. Opaque `reference_id`를 전달해 canonical argument 후보를 얻는다.
3. Provider는 read-only여야 하고 credential 또는 external effect를 만들지 않는다.
4. Provider가 반환한 argument를 아래 검증 pipeline에 전달한다.
5. 모든 검증이 통과한 경우에만 typed recovered material을 반환한다.

Provider error의 원문은 journal reason으로 사용하지 않는다. Error에는 path, URL, argument, credential이 포함될 수 있으므로 runtime은 내부 진단과 durable reason code를 분리한다.

## Recovery revalidation

재구성 결과는 adapter가 주장한 digest를 신뢰하지 않고 core가 다시 계산한다.

| 검증 | 실패 시 동작 |
|---|---|
| Lease Run ID | provider 호출 전 거부 |
| Record Run/Step/Effect binding | store corruption 또는 recovery 거부 |
| Exact Definition 존재 및 digest | effect 시작 없이 manual/거부 |
| Selected Instance 존재 및 executable-binding digest | 다른 Instance fallback 없이 manual/거부 |
| Record format version 지원 | store load에서 corruption/fail-closed 거부 |
| Provider와 recipe version 지원 | caller가 영구 실패를 확정한 뒤 fixed-code manual |
| RFC 8785 canonical size ≤ 1 MiB | manual/거부 |
| Definition input schema | manual/거부 |
| Trusted resource resolver | manual/거부 |
| Canonical semantic action digest | `reference_changed` 또는 recovery 거부 |
| Durable intent와 authorization binding | store corruption 또는 recovery 거부 |

Definition, Instance, resource와 action 검증은 최초 admission과 같은 core identity 함수를 사용해야 한다. Admission과 Recovery가 유사한 hash 입력을 각각 구현하지 않는다.

Recovery는 기존 `EffectIntent`와 authorization consumption을 그대로 사용한다. Provider 장애나 process restart를 이유로 새 grant ID, effect ID, idempotency key 또는 use budget을 만들지 않는다.

## Fail-closed manual 전이

Material 획득 실패는 외부 effect 시작 전 발생한다. 따라서 `EffectBecameUnknown`이나 reconciliation 상태를 사용하지 않는다.

```text
IntentCommitted
      |
      | permanent material unavailability
      v
ManualRequired
```

Durable event에는 free-form provider/OS error 대신 정해진 code만 기록한다.

| code | 의미 |
|---|---|
| `ephemeral_material_lost` | Process-local material을 복구할 수 없음 |
| `reference_unavailable` | 지원된 provider가 reference를 영구 복구할 수 없음 |
| `reference_changed` | 재구성 결과 또는 revision이 committed material과 다름 |
| `adapter_binding_unavailable` | 선택된 executable binding을 준비할 수 없음 |
| `credential_binding_changed` | 안정적인 credential identity가 admission 뒤 retarget됨 |
| `unsupported_material_version` | 지원된 record가 가리키는 provider recipe version을 현재 runtime이 해석할 수 없음 |

이 전이는 sink guarantee나 effect class와 관계없이 external effect를 호출하지 않는다. Authorization use를 환불하지 않으며 자동으로 Planned 상태로 되돌리지 않는다. 현재 같은 Run의 동일 semantic action은 effect/grant identity도 같으므로 별도 Step만 만들어 재승인할 수 없다. 사용자가 다시 시도하려면 상태를 확인한 뒤 새 Run에서 permission evaluation을 거치거나, 향후 명시적인 occurrence/replacement protocol을 사용해야 한다.

Transient provider 장애의 retry/backoff와 장기 `Blocked` 상태는 이 기본형에서 정의하지 않는다. 영구 부재가 확정되지 않은 오류를 성급하게 manual로 기록하지 않고 caller에게 fail-closed error로 반환한다.

## Secret·credential 경계

Credential은 material argument 안의 raw 문자열이 아니라 protocol의 typed `credentialRef`를 사용한다. Durable record가 보존할 수 있는 것은 secret-free reference identity뿐이다.

실제 token resolution은 다음 Direct Executor의 별도 trusted port가 실행 직전에 수행한다. Resolved credential은 journal, sidecar, Receipt, material digest와 semantic action digest의 plaintext 입력으로 추가하지 않는다.

현재 executable binding은 stable identity로 `auth_ref` 문자열까지 결합하지만, 같은 `auth_ref`가 다른 principal이나 credential version으로 retarget되는 것은 탐지하지 못한다. Credential-bearing adapter를 연결하기 전 resolver가 반환하는 stable principal/version digest를 intent에 결합하거나 retarget 시 새 admission을 강제해야 한다. 이 gate를 통과하기 전에는 credential-bearing actual effect를 지원한다고 주장하지 않는다.

현재 arbitrary Capability input schema가 모든 secret-like string을 자동 탐지하지는 않는다. Field 이름에 `token`, `password`가 있는지 추측하는 heuristic도 사용하지 않는다. Raw credential을 요구하는 actual adapter는 typed credential contract와 resolver가 구현될 때까지 연결하지 않는다.

Semantic action digest가 canonical argument의 public SHA-256 commitment를 포함하므로 저엔트로피 literal에 대한 offline equality 추측 가능성은 남는다. 이 slice는 encryption 또는 keyed commitment를 제공하지 않는다. 실제 credential은 reference로만 받는다는 제한이 이 위험의 MVP gate다.

## Debug·export 규칙

다음 표면에는 raw argument, canonical resource와 credential sentinel이 나타나면 안 된다. Secret-free reference ID는 private SQLite sidecar에만 저장할 수 있으며 `Debug`, JSONL, Receipt와 telemetry에는 노출하지 않는다.

- `InvocationMaterialRecord`와 recovered handle의 `Debug`
- Material/recovery error의 `Display`
- Run JSONL export
- SQLite event, projection, intent와 material row
- SQLite database, WAL과 SHM 원시 바이트
- Manual transition reason
- Receipt와 telemetry

진단에는 Effect ID, fixed reason code, material kind와 지원 version처럼 non-sensitive한 값만 사용한다. Provider가 반환한 arbitrary error text를 그대로 연결하지 않는다.

## 검증 항목

### Store와 transaction

- Memory와 SQLite가 같은 material commit 결과를 만든다.
- Event, intent index, authorization, material, projection 각 단계 fault가 전체 rollback된다.
- 각 단계 실제 child-process exit 뒤 reopen 결과가 seed state와 일치한다.
- Commit acknowledgement 유실 뒤 intent, authorization과 material이 각각 정확히 하나다.
- Reopen과 replay가 provider 또는 adapter를 호출하지 않는다.
- Missing required row, orphan row, duplicate row와 record digest mutation을 corruption으로 탐지한다.

### Identity와 recovery

- Restart 뒤 reconstructable fake provider가 같은 canonical action을 복구한다.
- Ephemeral restart는 effect 시작 0회와 `ephemeral_material_lost`로 수렴한다.
- Run, Step, Effect, action, Capability, Definition, Instance field별 mutation을 거부한다.
- 두 Run 또는 Step의 material record/reference swap을 거부한다.
- Provider가 다른 argument를 반환하면 material digest 또는 semantic action mismatch로 거부한다.
- Definition 또는 Instance가 변경되면 provider 결과를 실행 material로 발행하지 않는다.
- 잘못된 Run lease에서는 provider 호출이 0회다.
- 다른 provider나 Instance로 fallback하지 않는다.

### Redaction과 compatibility

- Raw argument, canonical path와 credential sentinel이 DB/WAL/JSONL/Debug/Error에 없다.
- Reference ID를 포함한 opaque type이 derived `Debug`로 원문을 노출하지 않는다.
- Fresh database가 schema 3으로 생성되고 reopen된다.
- Schema 1과 2를 변경 없이 거부한다.
- Future schema version을 변경 없이 거부한다.
- Linux, macOS와 Windows CI에서 portable unit/contract test가 통과한다.

## 현재 포함하지 않는 범위

- `EffectSink`에 material을 전달하는 Direct Executor
- Core-owned opaque `PreparedEffect`와 adapter registry
- Exact `binding_ref + operation_ref` adapter dispatch
- Filesystem/process/network/MCP/Connector/XGEN actual adapter
- OS credential store와 credential injection
- Sealed argument encryption
- Provider retry/backoff와 durable Blocked 상태
- Receipt body와 Artifact store
- Symlink·TOCTOU, sandbox와 process tree 제어
- Managed/critical/reusable authorization
- Public protocol의 material projection

다음 PR은 이 recovery 결과를 exact selected adapter의 side-effect-free `prepare`에 연결하고, raw argument accessor를 trusted in-process Direct Executor 안으로 좁힌 뒤 core가 identity를 소유하는 consume-only prepared wrapper를 기존 durable effect runtime으로 전달한다. 그 전까지 이 slice는 material의 안전한 복구 또는 명시적 중단만 보장하며 public 저수준 API를 untrusted plugin 격리 경계로 주장하지 않는다.
