# ADR-0002: WorkGraph 권위와 동기화

- 상태: Accepted
- 날짜: 2026-08-27

## Context

XGENy는 모델 context보다 긴 작업을 여러 턴에 걸쳐 지속해야 한다. XGEN 서버와 연결되면 같은 Run을 관찰·실행·감사할 수 있어야 하지만, 로컬과 서버가 동시에 그래프를 수정하면 충돌과 중복 side effect가 발생한다.

## Decision

WorkGraph는 Run마다 단일 authority를 가진다.

- 로컬에서 만든 Run: XGENy authority, XGEN은 선택적 mirror
- XGEN이 만든 Run: XGEN authority, XGENy는 edge executor
- 외부 에이전트가 XGEN을 통해 만든 Run: XGEN authority

동기화는 append-only `EventEnvelope`와 sequence cursor를 사용한다. mutation은 `expected_revision`과 `idempotency_key`를 요구한다. authority 변경은 명시적 handoff event다.

네트워크 단절 시 로컬 authority Run은 계속되고, 서버 authority Run은 승인된 in-flight step을 정리한 뒤 lease 경계에서 정지한다.

PolicyLease의 서버 만료 시각은 수신 시 로컬 monotonic deadline으로 변환한다. clock skew가 불명확하면 보수적으로 더 이른 만료를 사용하고, 만료 뒤 새 side effect를 시작하지 않는다. effectful invocation이 시작된 뒤에는 실행 위치를 자동 변경하지 않는다.

## Consequences

- multi-master 충돌과 이중 side effect를 피한다.
- 오프라인 동작 규칙이 명확하다.
- 협업 편집은 직접 동시 수정이 아니라 handoff/branch/merge 의미를 별도로 설계해야 한다.

## Rejected alternatives

- local/server last-write-wins
- DB와 JSONL dual-write
- 연결이 끊기면 같은 ID의 새 로컬 Run으로 자동 전환
- 전체 대화 transcript를 상태의 진실로 사용
