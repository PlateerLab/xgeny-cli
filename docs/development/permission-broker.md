# Permission Broker 기본형

이 문서는 ADR-0005의 concrete resource 정책을 실제 OS 실행 권한과 혼동하지 않도록 현재 구현 경계와 검증 계약을 고정한다.

## 사용자 시나리오

XGENy가 effectful Capability를 실행하기 전에 호스트가 실제 resource를 정규화하고, host·사용자 profile·선택적 조직 정책의 교집합이 전체 요청을 허용하는지 provisional 판정을 얻는다. 한 계층이라도 거부하거나 일부 resource만 허용하면 요청 전체를 실행하지 않으며, critical action은 넓은 로컬 profile에서도 자동 허용하지 않는다. 현재 Allow 결과 자체는 Executor가 소비할 실행 권한이 아니다.

## 구성과 의존 방향

```text
schema-validated PermissionRequest
              |
              v
trusted scope-specific ResourceResolver
              |
              v
opaque ResolvedPermissionRequest
              |
              v
host boundary ∩ user profile [∩ managed lease]
              |
              v
       deny > ask > allow
```

`xgeny-policy`는 `xgeny-domain`과 `thiserror`에만 의존하는 I/O 없는 crate다. XGEN, Connector, DB, MinIO, local store, WorkGraph, OS API와 filesystem/process 구현에 의존하지 않는다.

## Resource 권위 경계

wire의 `ResolvedResource.normalized: true`는 자체 증명이 아니다. `PermissionRequestResolver`는 이 값이 false면 거부하고, true여도 호스트가 주입한 `ResourceResolver`를 모든 resource에 호출한다. broker가 받는 `ConcreteResource`는 필드가 비공개인 opaque 타입이므로 raw 문자열을 직접 policy 입력으로 바꿀 수 없다. resolver rejection은 고정된 non-sensitive enum만 전달할 수 있어 raw OS 오류·경로·credential이 정책 오류에 섞이지 않는다.

resolver 결과는 `(scope, canonical_resource)` exact identity로만 비교한다. metadata, request reason, timestamp는 권한 근거에 포함하지 않는다. prefix, glob, 문자열 `starts_with`는 지원하지 않는다. 같은 concrete identity로 수렴하는 alias는 metadata가 달라도 중복으로 거부한다.

현재 policy extension handler는 없다. 따라서 `requiredExtensions`가 하나라도 있으면 URI를 안다는 이유만으로 지원 처리하지 않고 fail-closed한다. 향후 extension을 추가할 때는 payload를 실제 검증·해석한 handler의 witness를 resolver와 broker에 전달하는 별도 계약이 필요하다.

실제 resolver는 각 adapter가 구현한다. 예를 들어 filesystem은 symlink·junction·존재하지 않는 write target·TOCTOU·Windows drive/UNC/case·macOS case/Unicode를 처리해야 한다. process와 network도 executable과 endpoint에 맞는 별도 규칙이 필요하다. 현재 fake resolver 테스트는 이 OS 보안을 검증했다는 뜻이 아니다.

## 정책 합성

`PolicyInputs::local`은 정확히 하나의 host와 user profile 계층을 요구한다. `PolicyInputs::managed`는 여기에 managed lease 계층을 반드시 추가한다. 자유로운 contribution 목록을 받지 않으므로 host ceiling이나 managed mode의 lease를 실수로 누락할 수 없다.

각 allow 계층은 현재 resolved request에 대해 허용하는 exact scope, concrete resource, critical action과 lifetime 집합을 `from_trusted_evaluation`으로 제공한다. 이 API는 모델·wire 입력용이 아니며 host가 선택한 policy evaluator만 호출해야 한다. policy source digest는 감사 evidence이지 서명이나 단독 권한 근거가 아니다. local/managed mode도 request나 모델이 아니라 trusted host config가 선택한다. 모든 요청 atom이 모든 allow 계층에 포함돼야 한다. 일부만 포함되면 요청을 축소 실행하지 않고 전체를 deny한다. source가 사용자 승격 가능성을 허용하려면 부분 allow가 아니라 명시적으로 ask를 반환해야 한다.

합성 우선순위는 다음과 같다.

1. 명시적 deny 또는 allow ceiling의 coverage 누락
2. 명시적 ask
3. 모든 계층의 전체 coverage가 확인된 allow

출력 source evidence는 host, user profile, managed lease 순으로 고정된다. 요청과 policy 입력 순서가 결과를 바꾸지 않는다. Allow 결과의 `ProvisionalAuthorization`은 요청 범위로 잘라서 추가 scope가 새어 나오지 않는다. 이 타입에는 canonical action digest와 atomic use budget이 없으므로 Executor 입력이나 reusable grant로 사용할 수 없다.

lifetime은 `once < run < session < project < persistent` 같은 크기 비교를 하지 않는다. 각 계층이 현재 요청 lifetime을 명시적으로 포함해야 한다. 특히 run과 session의 의미 범위가 항상 포함 관계라고 가정하지 않는다.

ADR-0005의 완성형 교집합에는 current Run grant가 포함된다. 현재 crate는 Run/action binding과 소비 예산 계약이 없는 상태에서 이를 흉내 내지 않기 위해 Run grant 입력을 받지 않는다. 따라서 Allow는 host·profile·선택적 managed ceiling을 통과했다는 provisional 결과일 뿐이며, ADR-0005 전체 구현이나 Executor 권한으로 해석하면 안 된다.

## Critical action

현재 구현은 critical action이 하나라도 있으면 deny가 없는 경우 항상 ask를 반환한다. host와 user profile이 모두 넓게 허용해도 자동 allow하지 않는다.

현재 v0.1 `Grant`만으로는 승인을 동일 Run, canonical action, 사용 예산과 안전하게 결합하거나 EffectIntent와 원자적으로 소비했음을 증명할 수 없다. 따라서 이번 기본형은 Run grant를 입력받아 critical action을 허용하거나 reusable wire `Grant`를 발행하지 않는다. 후속 수직 슬라이스에서 다음 계약을 먼저 확정한다.

- `run_id + canonical_action_digest + critical_actions + max_uses` 승인 identity
- 만료와 취소
- EffectIntent commit과 승인 예산의 원자적 소비
- crash/replan/retry 뒤 semantic replay 차단
- `PolicyDecision` wire 투영과 receipt digest 연결

## 현재 테스트와 제외 범위

계약 테스트는 다음 실패 경로를 포함한다.

- resolver 우회와 resolver rejection
- unnormalized resource, scope 양방향 불일치, canonical alias 중복
- unsupported API version과 required extension
- local·managed 필수 계층과 source kind/digest 오류
- deny 우선, ask, 전체 allow
- 부분 resource와 lifetime coverage 거부
- request reason·metadata의 approval 사칭 무시
- critical action 자동 허용 차단
- resource 입력 순서와 evidence 순서 결정성

아직 보장하지 않는 범위는 다음과 같다.

- 실제 filesystem/process/network resolver와 OS sandbox
- Capability Definition에서 effect·scope·critical action을 host-derived request로 만드는 builder
- profile 설정 parser와 permission prompt UI
- Run grant 저장·재사용·소비와 PolicyLease expiry/clock-skew 판정
- `PolicyDecisionBody`의 ID·timestamp·interaction projection
- Router, Executor, CLI, MCP, Connector, XGEN 연결

Permission Broker는 sandbox가 아니며 adapter는 결정된 exact resource를 실제 open/execute 시점에도 다시 강제해야 한다.
