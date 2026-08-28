# ADR-0007: Hash-chained RunJournal과 ExecutionReceipt

- 상태: 승인
- 기준일: 2026-08-28

## 문맥

모델 대화와 요약만으로는 장기 작업을 재개하거나 실제 변경·검증·중복 실행 여부를 증명할 수 없다. 로컬과 원격 실행 사이의 동등성도 결과 텍스트만 비교해서는 검증할 수 없다.

## 결정

- RunJournal은 append-only 사실 원장이고 WorkGraph snapshot은 파생 cache다.
- event는 Run별 sequence와 이전 event digest를 가진다.
- digest는 RFC 8785 canonical JSON과 SHA-256을 사용한다.
- snapshot은 마지막 sequence와 event digest를 기록하며 검증 실패 시 journal replay한다.
- `ExecutionReceipt`는 Capability/Instance, input digest, 정책 증거, placement, effect 시작 여부, status, output/artifact digest, verification evidence를 기록한다.
- 성공 Receipt에는 최소 하나의 검증 evidence가 필요하다.
- credential과 민감 argument 원문은 저장하지 않는다.
- MVP는 content hash와 chain을 구현하고 DSSE/in-toto 서명은 후속 adapter로 둔다.
- hash chain은 누락·변조 탐지를 위한 무결성 증거이며 actor 인증, local transaction 원자성 또는 외부 effect exactly-once를 뜻하지 않는다.
- RunJournal의 물리 저장 형식과 external effect recovery는 ADR-0008의 연구 gate에서 결정한다.

## 결과

- context window를 넘어가는 Run을 active frontier에서 재개할 수 있다.
- crash recovery와 tamper detection에 필요한 증거, conservative duplicate 방지, XGEN shadow 비교가 가능해진다.
- 실행 여부를 알 수 없는 effect를 성공·실패로 임의 확정하지 않고 reconcile/manual 경로로 보낼 수 있다.
- canonicalization과 digest conformance fixture가 릴리스 차단 테스트가 된다.

## 폐기안

- 전체 chat transcript를 유일한 정본으로 사용
- model summary를 WorkGraph나 실행 증거로 사용
- snapshot만 저장하고 journal 폐기
- 결과 텍스트만으로 로컬·XGEN 실행 동등성 판정
