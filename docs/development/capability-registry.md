# Capability Registry 기본형

- 기준일: 2026-08-29
- 상태: MVP 기반 구현
- 공개 protocol v0.1 변경: 없음

## 목적

`CapabilityDefinition`의 정적 의미 계약과 `CapabilityInstance`의 실행 binding을 섞지 않고, 이후 Router가 같은 입력에서 같은 후보 집합을 얻도록 결정론적 catalog를 제공한다. Registry는 실행 위치를 선택하거나 권한을 부여하지 않는다.

```text
schema-conformant Definition / Instance
                  |
                  v
       deterministic Registry -----+
                                    v
Permission Broker -------------> Router -> InvocationPlan -> Executor
```

외부 manifest, MCP, Connector, XGEN에서 들어온 문서는 Registry 등록 전에 protocol schema 검증을 통과해야 한다. Registry는 schema 정본을 정규식으로 복제하지 않고, 문서 사이의 연결과 실행 계약 모순만 추가로 검사한다.

## 정체성과 등록 규칙

- Definition key는 exact `(capabilityId, contractVersion)`이다.
- Instance key는 Registry 전체에서 유일한 `instanceId`다.
- 같은 Capability ID의 여러 contract version은 공존할 수 있다.
- `latest`, semver range, compatible-major fallback을 추론하지 않는다.
- Instance는 이미 등록된 exact Definition version만 참조한다.
- duplicate 등록은 기존 값을 교체하지 않고 실패한다.
- 실패한 등록은 다른 Definition이나 Instance를 변경하지 않는다.
- Definition의 default timeout은 0보다 크고 maximum timeout 이하여야 한다.
- Instance의 sync/task/cancellation 주장은 Definition 계약의 부분집합이어야 한다.

목록은 `BTreeMap` key 순서를 사용한다. Definition은 capability ID와 contract-version 문자열 순서, Instance는 instance ID 순서로 반환하므로 discovery 등록 순서에 의존하지 않는다. 이 순서는 후보 ranking이 아니며 semver 우선순위도 아니다.

## Router와의 경계

Registry는 다음 상태를 보존할 뿐 해석하지 않는다.

- local, device, remote placement와 platform
- available, degraded, unavailable, unknown health
- auth required 또는 expired
- trust와 data boundary
- latency, monetary cost, reliability hint
- source가 builtin, local CLI, MCP, Connector, XGEN인지 여부
- optional extension payload와 required extension URI

host OS 감지 자체는 Router 밖의 trusted host가 담당한다. Router 기본형은 caller가 준 concrete target platform, trust/data boundary, required feature와 bound Permission Broker 결과를 명시적으로 받아 filter와 ranking을 수행한다. availability/auth는 Instance 상태를 사용하고, handler witness가 아직 없는 required extension은 모두 fail-closed한다. 따라서 다른 OS용, 현재 unavailable인 Instance 또는 required extension이 있는 문서도 catalog 등록 자체는 가능하다. 자세한 규칙은 [결정론적 Capability Router 기본형](deterministic-router.md)을 따른다.

Definition의 `idempotencyKeySupported`와 Instance의 `idempotencyQuery` 사이에는 protocol v0.1이 강제하는 cross-field 규칙이 아직 없다. Registry는 둘의 관계를 추측하지 않으며, durable sink guarantee와 Router feature 요구 계약을 확정할 때 별도 테스트로 고정한다.

## 실행 가능한 검증

- 기존 v0.1 Definition·Instance fixture를 fake binding으로 등록
- exact Definition 조회와 exact-version Instance 조회
- 여러 version과 여러 Instance의 결정적 순서
- duplicate Definition·Instance의 원본 보존
- orphan·nearby-version Instance의 무변경 거부
- unsupported sync/task/cancellation 주장 거부
- Definition이 허용한 execution style 부분집합 수용
- 실행 style이 없는 Instance와 required extension을 Router 입력으로 보존
- unsupported API version과 역전된 timeout 거부
- 모든 source와 platform·health·auth 상태를 Router 입력으로 보존

## 아직 포함하지 않는 범위

- schema-conformant document를 만드는 ingress loader/newtype
- prerequisite closure와 cycle 검증
- keyword·semantic search와 catalog context paging
- effect 시작 전·후 실행 실패를 처리하는 fallback과 resume
- InvocationPlan 생성과 실행 직전 재검증
- persistence, hot reload, background health polling과 upsert
- adapter discovery trait와 실제 adapter
- Permission Broker, filesystem/process 실행, CLI와 모델 loop

성공적으로 등록된 snapshot의 조회 순서는 결정적이지만 등록 admission은 순서 독립적이지 않다. Instance보다 exact Definition을 먼저 등록해야 하며, discovery 결과의 순서 없는 batch finalization은 hot reload를 설계할 때 별도로 다룬다.

따라서 fake Instance는 테스트 자료이며 production fake adapter 추상화가 아니다. Adapter port는 Direct Executor가 소유하고, 별도 public-port reference suite가 exact binding과 actual I/O 순서를 검증한다. 공통 contract testkit 추출은 두 번째 adapter에서 구현별 fixture를 분리할 때 진행한다.
