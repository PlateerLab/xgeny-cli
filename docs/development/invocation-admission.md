# Run-bound Invocation Admission 기본형

- 기준일: 2026-08-29
- 상태: 로컬 one-shot effect 연구 gate용 내부 구현
- 공개 protocol v0.1 변경: 없음

## 목적

모델이나 호출자가 별도로 만든 permission 문서와 실제 tool argument를 섞어 실행하지 못하게 하고, 현재 Run·Step·authority·journal head·Capability contract·선택된 Instance·semantic action에 결합된 1회 권한을 `EffectIntent`와 한 transaction에서 기록한다. 이 경로는 Permission Broker의 provisional allow와 Router의 placement 결과를 실행 가능한 durable intent로 연결하는 최소 수직 슬라이스다.

```text
exact invocation arguments
          |
          v
Definition input schema + 1 MiB canonical-size limit
          |
          v
Definition resourceSelectors -- JSON Pointer --> trusted ResourceResolver
          |                                      |
          +--> canonical arguments <-------------+
          +--> exact ResolvedPermissionRequest
          |
          v
PendingInvocation (policy 입력, 실행 권한 아님)
          |
          +--> host policy ∩ user policy --> Broker Allow
          |                                      |
          +--------------------------------------v
                                   internal deterministic Router
                                                  |
                                                  v
                                  selected executable binding digest
                                                  |
                                                  v
Run/Step/authority/head/action/policy에 결합된 1회 권한
                                                  |
                                                  v
                    EffectIntent + authorization consumption atomic commit
                                                  |
                                                  v
                           ephemeral AdmittedEffect -> trusted adapter
```

## 2단계 계약

`InvocationAdmission::prepare`는 lease와 durable Run head를 확인하고 Planned Step만 받는다. exact Definition의 input schema를 offline으로 검증한 뒤 각 `resourceSelector.argumentPointer`가 가리키는 non-empty string을 trusted resolver에 전달한다. resolver가 돌려준 canonical identity는 permission resource와 실행 argument 사본의 같은 위치에 함께 반영되고, 이 canonical argument를 같은 input schema와 크기 상한으로 다시 검증한다. 따라서 caller가 허용받은 path와 실행할 path를 따로 제공하거나 resolver 변환으로 schema 제약을 우회할 수 없다.

반환되는 `PendingInvocation`은 clone·deserialize할 수 없고 raw argument를 `Debug`에 노출하지 않는다. 다만 이는 같은 process 안의 trusted Rust API 오배선을 줄이는 witness이지 hostile plugin을 격리하는 capability token은 아니다.

`authorize_and_commit`은 policy 평가 뒤 state와 Definition을 다시 읽는다. 준비 시점의 Run ID, authority, epoch, journal sequence/head digest 중 하나라도 바뀌면 닫힌다. `PolicyInputs`도 exact `ResolvedPermissionRequest`에 private하게 결합되어 다른 argument로 준비한 policy stack을 재사용할 수 없다. public `RouteOutcome`을 입력으로 신뢰하지 않고 exact bound policy 결과로 Router를 내부에서 다시 실행한다. `Ask`와 `Deny`는 durable effect를 만들지 않는다.

## identity 분리

서로 다른 의미를 한 hash로 뭉치지 않는다.

| digest | 결합 대상 | 목적 |
|---|---|---|
| Definition contract | Capability ID/version, spec, required extension | 준비 뒤 계약 변경 탐지 |
| semantic action | Capability, Definition digest, effect class, canonical arguments/resources | alias와 JSON key 순서에 무관한 동일 작업 식별 |
| executable binding | Instance ID, Definition ref, source, placement, platform, trust, boundary, features, binding | 다른 adapter/placement로 권한 재사용 방지 |
| policy evidence | exact request identity/authority atoms와 allowance, ordered sources | 어떤 invocation·정책 교집합이 허용했는지 결합 |
| authorization | Run, Step, authority/epoch, issued head, action, Definition/Instance, policy evidence, max uses | reducer가 검증하고 원자적으로 소비할 durable 1회 권한 |

semantic action은 의도적으로 Run·Step·Instance와 독립적이다. 같은 의미 작업의 retry/replan을 식별하는 값과 실제 선택된 실행체에 대한 권한을 분리하기 위해서다. 반대로 authorization budget ID와 effect ID/idempotency key는 Run과 semantic action에 결합되어 같은 Run에서 Step이나 Instance만 바꾼 중복 효과를 허용하지 않는다.

이 hash들은 현재 domain-separated canonical SHA-256 integrity identifier다. MAC이나 signature가 아니므로 untrusted process가 durable event 생성 API에 직접 접근하는 배포 경계의 위조를 막지 않는다.

## 현재 허용 범위

기본형은 다음 조건을 모두 만족할 때만 intent를 발행한다.

- local host + user-profile 정책 모드
- `GrantLifetime::Once`, `max_uses = 1`
- critical action 없음
- synchronous execution
- Definition과 route가 idempotency key 지원을 요구
- effect class가 정확히 `Idempotent` 또는 `NonIdempotent`
- sink guarantee는 증명되지 않은 `None`
- Definition/Instance required extension 없음

`ReadOnly`, `Compensatable`, `Unknown`, managed lease, critical approval은 의미를 축약 변환하지 않고 fail-closed한다. 특히 managed policy는 expiry·revocation witness가 없으므로 provisional 평가 결과가 있더라도 권한을 발행하지 않는다.

## durable·runtime 방어

WorkGraph reducer는 `EffectIntent`를 적용하기 전에 authorization binding의 Run, Step, authority, epoch, 발행 journal head, action digest, Capability contract와 Instance binding을 intent 및 현재 state와 교차 검증한다. Run+semantic action에서 one-shot budget ID를 다시 유도하고 `max_uses = 1`과 authorization digest도 재계산한다. 따라서 grant ID만 바꿔 같은 작업의 새 예산을 만드는 저수준 오배선도 거부한다. 검증 실패 시 state를 변경하지 않는다.

SQLite schema version은 2다. journal event, effect-intent index, authorization-consumption row와 projection은 한 transaction에 기록된다. intent index 뒤 authorization row 전의 fault injection도 포함해 어느 단계에서 process가 종료돼도 일부만 commit되지 않는지 검사한다. 내부 실험용 version 1 store는 불완전한 권한 binding을 읽지 않도록 명시적으로 거부하며 자동 migration하지 않는다.

실행 직전 runtime도 ephemeral `PreparedEffect`의 action digest뿐 아니라 Capability ID/version, Definition digest, Instance ID와 executable-binding digest를 durable intent와 다시 비교한다. 하나라도 다르면 sink 호출은 0회다.

## 검증과 알려진 한계

테스트는 canonical alias와 JSON key 순서, Run/Step 독립 semantic identity, 같은 resource지만 다른 비-selector argument에 대한 policy stack 재사용, 1 MiB 초과, canonicalization 뒤 schema 위반, selector shape와 resolver 실패, ask/deny/critical/managed 차단, stale head·잘못된 lease·Definition drift, durable binding field별 mutation, SQLite 재시작, lost acknowledgement, authorization 예산 중복 방지와 transaction fault matrix를 포함한다. raw argument와 canonical resource sentinel은 journal serialization과 `PendingInvocation`/`AdmittedEffect`의 `Debug` 출력에 남지 않는지도 검사한다.

현재 raw/canonical arguments는 의도적으로 durable journal에 저장하지 않는다. 따라서 process가 intent commit 직후 종료되면 adapter argument를 안전한 별도 저장소에서 복구하는 계약이 아직 없어 사용자 실행을 자동 resume할 수 없다. secret reference, encrypted argument store 또는 deterministic reconstruction 중 하나를 후속 adapter slice에서 결정해야 한다.

semantic action digest에는 canonical argument가 들어간다. 이 값은 암호화가 아니므로 저엔트로피 secret을 argument literal로 넣으면 journal 접근자가 후보 값을 대입해 equality를 추측할 수 있다. 실제 adapter를 연결하기 전 secret은 opaque reference로 분리하고 digest 입력에서 credential material을 배제하는 계약이 필요하다.

다음 항목은 아직 보장하지 않는다.

- 실제 filesystem/process/network resolver의 symlink·TOCTOU·OS sandbox 강제
- actual adapter가 canonical argument와 Instance binding을 정직하게 집행한다는 증명
- public WorkGraph/RunStore 저수준 API를 untrusted extension에서 격리하는 crate/API 경계
- critical approval, reusable Run grant, expiry·revocation과 cross-run/global budget
- read-only direct path, compensatable/unknown effect 의미와 async/task 실행
- credential/secret binding, receipt body store와 argument 복구
- 동일 Run에서 의도적으로 같은 semantic action을 여러 번 수행할 trusted occurrence identity
- CLI 명령, MCP/Claude/Codex harness, Connector 또는 XGEN wire 연동

따라서 이 구현은 지원된 trusted in-process 경로에서의 misuse-resistant admission 기반이다. 사용자 PC 전체 권한 sandbox나 외부 plugin에 대한 보안 경계의 완성으로 해석하면 안 된다.
