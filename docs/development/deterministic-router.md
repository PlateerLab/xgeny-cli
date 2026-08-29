# 결정론적 Capability Router 기본형

- 기준일: 2026-08-29
- 상태: MVP 기반 구현
- 공개 protocol v0.1 변경: 없음

## 목적과 권한 경계

Router는 exact `CapabilityDefinition`에 연결된 여러 `CapabilityInstance` 중 현재 요청에 맞지 않는 후보를 먼저 제거하고, 남은 후보를 같은 입력에서 항상 같은 순서로 정렬한다. 모델이나 provider의 자유 형식 점수로 placement를 결정하지 않는다.

```text
schema-validated Definition + Instance
                  |
                  v
       deterministic Registry
                  |
                  +------------------+
                                     v
resolved request -> Permission Broker -> allow / ask / deny
                                     |
trusted host requirements -----------+----> Router
                                            |  hard filter
                                            |  lexicographic rank
                                            |  pin + policy gate
                                            v
                         Selected / InteractionRequired / Blocked
                                            |
                                  (execution authority 아님)
```

`Selected`는 Instance placement 결과일 뿐 `Grant`, `PolicyDecision`, `InvocationPlan` 또는 Executor 입력이 아니다. 현재 `ProvisionalAuthorization`에는 Run과 canonical action digest, 원자적 use budget이 없으므로 Router가 이를 실행 권한으로 승격하지 않는다.

## 입력 계약

`RouteRequest`는 다음 값을 trusted caller에게서 받는다.

- exact `CapabilityRef`
- `Any`가 아닌 target OS와 architecture
- 필요한 sync/task, cancellation, idempotency key, idempotency query
- 허용 가능한 trust와 data boundary의 명시적 집합
- 선택적인 trust, data boundary, Instance ID 선호 순서
- 선택적인 pinned Instance ID

trust와 data boundary enum의 선언 순서를 권한 또는 선호 순서로 해석하지 않는다. 허용 여부는 set membership으로만 판단하고, ranking 선호는 caller가 별도로 준 순서만 사용한다. 빈 허용 집합, 중복되거나 허용 집합 밖인 선호와 빈·중복 Instance ID는 입력 오류로 닫힌다.

현재 `Platform`은 한 route의 target OS만 표현한다. local/device/remote 후보가 서로 다른 실행 OS를 동시에 요구한다면 caller가 route를 분리해야 한다. host OS 감지와 remote platform 증명은 Router 책임이 아니다.

## Hard filter

후보 reason은 아래 순서로 수집하며, 후보 출력은 항상 Instance ID byte order다.

1. OS와 architecture 불일치
2. health
3. auth
4. trust와 data boundary
5. Definition과 Instance required extension
6. execution style, cancellation, idempotency key/query
7. numeric hint 유효성

상태별 판정은 다음과 같다.

| 입력 | 판정 |
|---|---|
| `available` | 통과 |
| `degraded` | 통과하되 `available` 뒤로 ranking |
| `unavailable` | 제거 |
| `unknown` | 긍정적인 availability 증거가 없으므로 MVP에서 제거 |
| auth `not_required`, `available` | 통과 |
| auth `required`, `expired` | 제거 |

`observedAt` freshness는 TTL과 clock 계약이 없으므로 Router가 추측하지 않는다. 현재는 상태 제공자가 freshness를 보장해야 하며 stale health 판정은 후속 계약이다.

required extension은 URI를 아는 것만으로 지원 처리하지 않는다. 실제 payload를 검증·해석한 opaque handler witness가 아직 없으므로 기본형에서는 Definition 또는 Instance에 required URI가 하나라도 있으면 후보를 제거한다. optional extension은 placement authority가 아니다.

`monetaryCost`는 finite이고 0 이상, `reliability`는 finite이고 `[0, 1]` 범위여야 한다. 위반한 후보는 제거한다. 정렬 전에 `-0.0`과 `0.0`을 같은 값으로 정규화하며, missing hint는 알려진 유효 값 뒤에 둔다.

## Lexicographic ranking

hard filter를 통과한 후보는 단일 magic score를 합산하지 않고 다음 tuple을 앞에서부터 비교한다.

1. `available` 우선, `degraded` 후순위
2. 알려진 reliability 우선, 높은 값 우선
3. caller가 명시한 trust 선호
4. caller가 명시한 data-boundary 선호
5. 알려진 latency 우선, 낮은 값 우선
6. 알려진 monetary cost 우선, 낮은 값 우선
7. caller가 명시한 Instance 선호
8. Instance ID byte order 최종 tie-break

ADR-0006의 의미 적합성·검증 강도·추가 permission·reversibility/effect 위험·단계 수는 현재 후보별 차이를 신뢰성 있게 표현하는 field가 없다. exact Definition 후보끼리는 의미·effect·verification 계약이 같고, Broker 결과도 request-wide이며, Instance에는 단계 수가 없다. 따라서 기본형은 이 항목의 점수를 발명하지 않는다. candidate-specific permission과 adapter execution-cost 계약을 추가할 때 앞쪽 ranking 차원으로 확장한다.

`Placement::{Local, Device, Remote}` 자체에도 암묵적 우선순위를 두지 않는다. 현재 data locality는 caller가 명시한 data-boundary preference로 표현되는 범위만 반영하며, placement preference는 별도 trusted request 계약이 생길 때 추가한다.

## Policy, critical action과 pin

후보별 placement filter와 ranking 뒤에 request-wide 정책 gate를 적용한다. 따라서 audit의 `PlacementEligible`은 platform·상태·기능 조건을 통과했다는 뜻이며 policy allow나 실행 권한을 뜻하지 않는다. ADR-0006의 policy hard filter는 이 후단 gate가 `Selected`를 차단하는 방식으로 보존한다.

Router는 bare `BrokerOutcome`을 받지 않고 Broker만 생성할 수 있는 `BoundPolicyEvaluation`을 받는다. 그 안의 exact Capability, effect class, critical action과 Definition selector scope가 현재 route와 일치하지 않으면 입력 오류로 닫힌다. Router 단독 결과는 concrete resource, Run, Step, canonical action까지 결합한 실행 권한이 아니다. 제한된 [Run-bound Invocation Admission 기본형](invocation-admission.md)이 Router를 내부에서 다시 실행하고 local one-shot 결과만 durable binding과 use budget으로 발행한다.

- placement-eligible 후보가 없으면 policy가 ask여도 `Blocked`다.
- policy 누락은 `Blocked(policy_missing)`이다.
- deny는 `Blocked(policy_denied)`이며 Broker reason을 보존한다.
- ask는 Instance를 선택하지 않고 `InteractionRequired`와 Broker reason을 반환한다.
- allow일 때만 `Selected`가 가능하다.
- 다른 critical-action 계약의 bound 결과는 `PolicyCriticalActionsMismatch` 입력 오류로 거부한다.
- Definition과 일치하는 critical request는 Broker가 ask로 고정하므로 `InteractionRequired`다.

pin은 일반 ranking보다 먼저 선택 의사를 표현하지만 어떤 안전 gate도 우회하지 않는다. pinned Instance가 존재하지 않거나 다른 Capability/version을 가리키거나 placement filter에서 탈락하면 다른 후보로 조용히 fallback하지 않고 `Blocked`다. pin이 유효해도 ask와 deny는 그대로 적용된다.

## 결정성과 현재 제외 범위

계약 테스트는 exact-version lookup, platform wildcard, health/auth table, trust/data-boundary set, feature·extension filter, policy 네 경로, critical gate, pin, ranking 차원, invalid float, signed zero, 등록 순열, 탈락 후보 추가의 metamorphic invariant를 검증한다.

현재 포함하지 않는 범위는 다음과 같다.

- semantic search와 모델의 후보 제안
- candidate-specific PermissionRequest와 추가 권한량 비교
- health TTL, background probing과 remote platform attestation
- `InvocationPlanBody` ID·timestamp·policyDecisionId 투영
- critical·managed·reusable Run/action-bound grant 발급과 소비
- 실제 adapter 실행과 실행 직전 재검증
- effect 시작 전 실패의 다음 후보 fallback
- effect 시작 후 idempotency query, resume와 보상 Step
- CLI, MCP, Connector와 XGEN 원격 연동

특히 `ranked_instance_ids`는 감사 가능한 placement 순서이지 effect 실패 뒤 자동 재시도 권한이 아니다. fallback은 effect 시작 여부와 durable evidence를 확인하는 후속 실행 계층에서만 결정한다.
