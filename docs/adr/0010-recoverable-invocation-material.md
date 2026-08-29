# ADR-0010: 복구 가능한 Invocation Material과 durable sidecar

- 상태: 제안 — 연구 gate
- 기준일: 2026-08-29
- 적용 범위: local Run admission, embedded SQLite store, effect 실행 전 복구
- 공개 protocol v0.1 변경: 없음

## 문맥

현재 admission은 exact invocation argument를 schema로 검증하고, trusted resource resolver로 정규화한 뒤 permission, Router 선택, semantic action, 1회 authorization을 하나의 `EffectIntent`로 commit한다. 그러나 canonical argument 자체는 process 안의 `AdmittedEffect`에만 남는다.

따라서 intent transaction이 성공한 직후 process가 종료되거나 commit acknowledgement만 유실되면 다음 상태가 된다.

```text
EffectIntent             durable
authorization use        durable and already consumed
selected Instance        durable digest로 결합
canonical arguments      lost with process
```

이 상태에서 새 argument를 추측하거나 동일 요청을 다시 승인하면 exact permission binding과 1회 budget을 훼손할 수 있다. 반대로 material 없이 `IntentCommitted`에 계속 머물게 하면 재시작 가능한 runtime이라는 제품 계약을 만족하지 못한다.

Raw argument나 credential을 SQLite에 그대로 저장하는 방식도 허용할 수 없다. SQLite는 embedded transactional store이지 암호화 secret store가 아니며 DB 본문, WAL, backup, support bundle과 filesystem snapshot이 새로운 노출 표면이 된다.

## 결정

### 1. EffectIntent와 실행 material을 분리한다

`EffectIntent`는 durable 실행 권한과 의미의 정본으로 유지한다. 실행 material의 복구 정보는 Run store 내부의 `InvocationMaterialRecord` sidecar로 저장한다.

Sidecar는 최소한 다음 identity에 결합한다.

- Run ID와 Step ID
- Effect ID와 semantic action digest
- Capability ID와 contract version
- Definition contract digest
- selected Instance ID와 executable-binding digest
- material kind와 record format version
- recovery descriptor digest

`InvocationMaterialRecord`에는 `serde_json::Value` 형태의 raw/canonical argument, resolved credential, token, password, bearer URL, endpoint secret을 넣지 않는다. Record ID나 reference ID를 argument의 단순 별칭으로 사용해 다른 table 또는 파일에 plaintext를 숨기는 것도 금지한다.

Sidecar의 content digest는 accidental corruption과 intent 간 swap을 탐지하기 위한 값이다. 현재 Run hash chain과 마찬가지로 MAC이나 signature가 아니므로 같은 OS 사용자 권한을 가진 hostile writer에 대한 인증 경계로 주장하지 않는다.

### 2. MVP는 두 recovery kind만 지원한다

#### `Ephemeral`

- Canonical argument는 현재 process memory에서만 보유한다.
- Durable sidecar에는 `Ephemeral` kind와 identity binding만 기록한다.
- 같은 process에서 material이 남아 있을 때만 후속 실행 준비가 가능하다.
- 재시작이나 material 유실 뒤에는 자동 복구나 argument 추측을 하지 않는다.
- 유실이 확인되면 effect를 시작하지 않고 고정 reason code를 가진 `ManualRequired` 전이로 닫는다.

#### `ReconstructableReference`

- Durable sidecar에는 trusted host가 발급한 `provider_id`와 opaque `reference_id`만 기록한다.
- Reference는 secret-free, non-bearer이며 자체로 effect 권한을 부여하지 않아야 한다.
- URL, raw filesystem path, command line, environment value, credential value를 reference로 위장해 저장하지 않는다.
- Provider는 read-only reconstruction만 수행하며 외부 effect를 만들 수 없다.
- 재구성된 canonical argument는 최초 admission과 같은 검증을 다시 통과해야 한다.
- Mutable reference는 immutable revision 또는 trusted content/config digest로 pin할 수 있을 때만 사용한다.

Provider와 reference는 model 또는 untrusted caller가 임의로 신뢰 등급을 지정하지 못한다. Core가 등록한 provider와 trusted admission path만 record를 만들 수 있다.

### 3. `Sealed` material은 이번 결정에서 지원하지 않는다

Raw argument의 restart recovery가 필요한 sealed mode는 다음을 함께 설계해야 한다.

- application-level authenticated encryption
- OS credential store에 보관하는 key와 key reference
- nonce와 AAD binding
- key rotation, reinstall, backup/restore와 device handoff
- plaintext copy, memory lifetime과 crash dump 최소화
- Linux, macOS, Windows별 failure test

이 계약 없이 plaintext를 SQLite에 저장하거나 file permission만으로 암호화를 대체하지 않는다. Unknown material kind와 future sealed version은 fail closed한다.

### 4. Sidecar는 intent와 같은 transaction에서 commit한다

다음 항목은 하나의 local transaction에서 전부 commit되거나 전부 rollback돼야 한다.

```text
RunEvent
+ effect-intent index
+ authorization consumption
+ InvocationMaterialRecord
+ WorkGraph projection
```

`RunStore::append` 뒤 별도 material write를 호출하는 2단계 저장은 금지한다. Event만 있고 material record가 없거나 material record만 남는 crash window를 만들기 때문이다.

Store schema version은 `3`으로 올린다. 이 저장 형식은 아직 내부 연구 형식이므로 version 1과 version 2를 자동 migration하지 않는다.

- 새 database의 version 0은 schema 3으로 초기화한다.
- version 3은 전체 event, projection, intent, authorization, material index를 검증한 뒤 연다.
- version 1, version 2와 future version은 파일을 변경하지 않고 명시적으로 거부한다.
- Legacy intent에 material을 추측하거나 빈 reference를 합성하지 않는다.

### 5. 복구는 새로운 authorization을 만들지 않는다

`ReconstructableReference` 복구는 기존 intent를 실행할 exact material을 다시 얻는 절차다. 새 policy allow, grant ID, effect ID 또는 authorization budget을 발행하지 않는다.

복구 성공 전 다음 항목을 다시 검증한다.

1. 현재 canonical Run lease가 intent의 Run과 일치한다.
2. Sidecar의 Run, Step, Effect와 action binding이 durable intent와 일치한다.
3. Exact Capability Definition이 존재하고 contract digest가 일치한다.
4. Selected Instance가 존재하고 executable-binding digest가 일치한다.
5. Registered provider와 record format version이 지원된다.
6. Reconstructed argument가 Definition input schema와 1 MiB canonical-size limit을 만족한다.
7. Trusted resource resolver를 다시 실행한 canonical resource와 arguments가 유효하다.
8. Semantic action digest를 core가 다시 계산했을 때 intent와 일치한다.

하나라도 실패하면 provider 결과를 adapter에 전달하지 않고 effect 시작 event도 기록하지 않는다. 다른 Instance 또는 provider로 자동 fallback하지 않는다.

### 6. Material 준비 실패는 명시적인 durable 결과로 만든다

Effect 호출 전 material을 얻지 못한 상태는 `effect_unknown`이 아니다. 외부 effect가 시작되지 않았음을 알고 있기 때문이다.

지원된 record가 정상적으로 존재하지만 material을 영구 복구할 수 없는 경우 `IntentCommitted`에서 `ManualRequired`로 전이하는 runtime event를 기록한다. Journal에는 임의 OS/provider 오류 문자열 대신 고정된 non-sensitive reason code만 남긴다.

MVP reason code는 최소 다음 범주를 구분한다.

- `ephemeral_material_lost`
- `reference_unavailable`
- `reference_changed`
- `adapter_binding_unavailable`
- `credential_binding_changed`
- `unsupported_material_version` — 지원된 record가 가리키는 provider recipe version

반면 required sidecar row 누락, orphan row, record digest mismatch, 다른 effect의 record swap과 지원하지 않는 record format은 정상적인 material 부재가 아니라 store corruption이다. 이 경우 손상된 Run에 새 event를 쓰지 않고 store load 또는 recovery를 거부한다.

Manual 전이는 기존 authorization consumption을 되돌리지 않는다. 현재 effect/grant identity는 같은 Run의 semantic action에 고정되므로 새 Step만 만들어 동일 작업을 재승인할 수도 없다. 다시 실행하려면 사용자가 상태를 확인한 뒤 새 Run에서 admission을 거치거나, 후속 occurrence/replacement protocol이 명시적으로 도입되어야 한다.

### 7. Adapter execution boundary는 다음 PR로 분리한다

이번 결정은 durable sidecar, 복구, revalidation과 fail-closed 상태까지만 다룬다. Actual filesystem/process/MCP/XGEN adapter 호출은 포함하지 않는다.

현재 adapter-owned `PreparedEffect`가 action·Definition·Instance digest를 자기 보고하는 경계도 이번 PR에서 교체하지 않는다. 다음 Direct Executor PR에서 core가 exact `binding_ref + operation_ref`로 adapter를 선택하고, core-owned opaque prepared wrapper를 발행하도록 변경한다.

따라서 이번 slice의 성공은 “재시작 뒤 exact material을 복구했거나, effect 시작 전에 명시적으로 안전하게 중단했다”는 뜻이다. 실제 tool effect 실행이 가능해졌다는 뜻은 아니다.

## 불변 조건

1. Raw/canonical argument와 resolved credential은 sidecar, SQLite, WAL, JSONL, Receipt, Debug, error와 telemetry에 저장하지 않는다.
2. Credential이 필요한 argument는 typed `credentialRef`만 사용하며 credential value는 실행 직전에 별도 trusted resolver가 제공한다.
3. Intent, authorization consumption, sidecar와 projection은 원자적으로 commit한다.
4. Material record의 identity와 recovery kind는 core가 만들며 model이나 adapter가 override하지 못한다.
5. Recovery는 exact selected Definition과 Instance에만 허용한다.
6. Reconstructed arguments는 schema, size, resource와 semantic action을 다시 검증한다.
7. Missing, tampered, swapped, unknown material은 effect-start와 sink call 0회로 닫힌다.
8. Replay는 projection과 record integrity만 검증하며 provider, credential resolver 또는 effect adapter를 호출하지 않는다.
9. Recovery는 새 authorization이나 use budget을 만들지 않는다.
10. Sealed mode가 승인되기 전 plaintext durable fallback을 만들지 않는다.

## 결과

- Lost acknowledgement 뒤에도 reconstructable invocation은 기존 intent와 budget으로 복구할 수 있다.
- Ephemeral invocation은 자동 복구를 과장하지 않고 durable manual 상태로 수렴한다.
- Run journal에는 실행 권한과 material identity만 남고 secret-bearing payload는 남지 않는다.
- Store transaction fault matrix가 material sidecar까지 확장된다.
- Direct Executor와 실제 OS adapter가 사용할 복구 전제조건이 생긴다.

비용은 store schema와 RunStore contract가 확장되고, reconstructable provider별 contract test가 필요하며, arbitrary secret-bearing arguments의 자동 resume는 계속 제공하지 못한다는 점이다.

## 명시적 비목표

- Actual adapter와 core-owned `PreparedEffect` 구현
- OS credential store와 raw credential resolver
- Sealed material encryption과 key lifecycle
- Mutable endpoint/config 또는 credential principal의 일반적인 pinning protocol
- Filesystem symlink·TOCTOU와 process sandbox
- Critical approval, managed grant, expiry와 revocation
- Hostile in-process plugin 또는 same-UID database writer 격리
- Secure deletion, swap, core dump와 memory zeroization 보장
- XGEN, Connector, MCP를 통한 material 전송
- 외부 effect의 물리적 exactly-once 보장

## 폐기안

- Raw argument를 `EffectIntent` 또는 journal payload에 포함
- Plaintext argument를 SQLite side table이나 별도 JSON file에 저장
- Commit 뒤 별도 material write로 eventual consistency 허용
- Missing material에서 원 요청을 추측하거나 모델에 재생성 요청
- Lost material을 같은 Run의 새 Step·새 grant로 재승인해 authorization budget을 우회
- 다른 Instance나 adapter로 자동 fallback
- File permission만으로 sealed storage를 주장

## 승인 조건

- Memory store와 SQLite가 동일한 atomic material commit contract를 통과한다.
- Transaction의 event, intent, authorization, material, projection 각 failpoint에서 partial commit이 0건이다.
- Process exit와 lost-ack 뒤 reopen 결과가 deterministic하다.
- Reconstructable reference는 restart 뒤 exact action으로 복구된다.
- Ephemeral loss는 fixed-code `ManualRequired`로 수렴한다.
- Missing, tampered, orphan, swapped record에서 provider/effect 호출이 0회다.
- Version 1과 2 store가 mutation 없이 명시적으로 거부된다.
- DB/WAL/JSONL/Debug/error에 raw argument와 credential sentinel이 나타나지 않는다.
