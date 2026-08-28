# ADR-0005: Permission Broker와 critical action gate

- 상태: 승인
- 기준일: 2026-08-28

## 문맥

모델이나 외부 tool server의 선언만으로 로컬 PC, device, XGEN 실행 권한을 결정할 수 없다. 같은 Capability도 실제 path, command, domain, account에 따라 위험이 달라진다.

## 결정

- 권한은 정규화된 concrete resource를 포함한 `PermissionRequest`로 평가한다.
- `PolicyDecision`은 `allow`, `ask`, `deny` 중 하나다.
- 유효 허용 범위는 host boundary, 사용자 profile, Run grant, 선택적 XGEN PolicyLease의 교집합이며 deny가 우선한다.
- grant lifetime은 `once`, `run`, `session`, `project`, `persistent`를 지원한다.
- 모델이 생성한 ID, scope, token과 untrusted MCP annotation은 권한 근거로 사용하지 않는다.
- `full_local`에서도 irreversible delete, credential export, payment, external publish/message, production deploy, privilege escalation, persistent startup은 별도 Run-scoped critical approval을 요구한다.
- credential은 reference로만 전달하고 모델 context, journal, receipt, telemetry에는 원문을 기록하지 않는다.

## 결과

- 사용자는 반복 prompt를 줄이면서도 위험도가 큰 외부 효과를 분리해 통제할 수 있다.
- adapter는 resource resolver와 host enforcement를 구현해야 한다.
- Permission Broker는 sandbox를 대체하지 않으며 강한 isolation은 별도 계층이다.

## 폐기안

- Capability 이름만으로 영구 allow
- 모델이 직접 approval을 해석해 executor 호출
- `full_local`에서 모든 critical action 무조건 허용
- MCP annotation을 enforcement로 신뢰
