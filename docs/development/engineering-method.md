# XGENy 개발 방법론과 테스트 전략

이 문서는 1인 개발 단계부터 XGENy의 구현 속도와 신뢰성을 함께 유지하기 위한 기본 작업 규칙이다. 테스트 개수나 line coverage 자체가 목적이 아니라, 로컬 실행·장기 작업·권한·XGEN 호환성에서 발생할 수 있는 실패를 병합 전에 발견하는 것이 목적이다.

## 기본 개발 흐름

하나의 변경은 가능한 한 작은 vertical slice로 완결한다.

1. 사용자 시나리오와 완료 조건을 한 문장으로 고정한다.
2. 프로토콜·보안 경계가 바뀌면 schema와 ADR을 먼저 갱신한다.
3. 기대 동작 또는 실패 조건을 재현하는 테스트를 먼저 작성하고 실패를 확인한다.
4. 테스트를 통과시키는 최소 구현을 작성한다.
5. 테스트가 통과하는 상태에서 중복과 경계를 리팩터링한다.
6. format, clippy, 전체 test, protocol check, release build를 실행한다.
7. 문서와 실제 검증 범위가 일치하는지 확인한 뒤 PR을 생성한다.

안정된 요구사항에는 Red–Green–Refactor를 기본으로 적용한다. 다만 문제를 아직 정의할 수 없는 연구 작업은 time-boxed spike를 허용한다. spike 코드는 제품 경로에 그대로 병합하지 않고, 얻은 결론을 ADR·계약·실패 테스트로 변환한 뒤 다시 구현한다.

기존 XGEN 또는 Connector 동작을 바꾸는 경우에는 현재 동작을 고정하는 characterization test와 양쪽 구현이 함께 실행할 contract test를 먼저 만든다. XGENy가 기존 구현에 의존하지 않더라도 호환성은 실행 가능한 테스트 자산으로 유지한다.

## 테스트 계층

### 1. Domain unit test

- I/O 없이 상태 전이, 권한 교집합, routing 결정, digest 입력을 검증한다.
- 정상 경로와 함께 deny, timeout, cancellation, unknown 상태를 검사한다.
- wall clock, random ID, 환경 변수에 직접 의존하지 않는다.

### 2. Protocol conformance test

- JSON Schema, valid/invalid fixture, Rust round-trip, extension 보존을 함께 검증한다.
- RFC 8785 digest와 객체 간 참조 무결성을 재계산한다.
- 외부 schema fetch를 금지하고 bundled 또는 digest-pinned resource만 허용한다.
- protocol 변경 PR은 fixture를 추가하거나 변경하지 않은 이유를 명시한다.

### 3. Adapter contract test

- builtin, local CLI, MCP, Connector, XGEN adapter가 동일한 Capability contract suite를 실행한다.
- core는 adapter 내부 타입이나 저장소에 의존하지 않는다.
- 외부 시스템은 port 경계에서 fake로 대체하되, fake와 실제 adapter에 동일한 contract test를 적용한다.

### 4. OS integration test

- 임시 workspace에서 실제 filesystem, process, cancellation, permission boundary를 검증한다.
- Linux, macOS, Windows의 path, symlink, process tree, signal 차이를 각각 확인한다.
- 테스트가 사용자 홈이나 기존 파일을 수정하지 않도록 격리한다.

### 5. Packaged CLI E2E

- release binary 또는 설치 패키지로 초기화, 실행, 중단, 재개, 제거를 검증한다.
- 설치되지 않은 DB·daemon·언어 runtime에 우연히 의존하지 않는지 깨끗한 환경에서 확인한다.
- 사용자 승인과 critical action은 성공 경로뿐 아니라 거부·취소 경로도 검증한다.

### 6. XGEN·Connector compatibility E2E

- 버전별 contract fixture를 XGENy와 서버 호환 계층 양쪽에 실행한다.
- 서버가 없을 때 local-only 기능이 유지되고, 연결 후에도 기존 XGEN 경로가 깨지지 않는지 확인한다.
- 실제 개발 환경 연동은 release gate로 관리하되 unit test의 필수 조건으로 만들지 않는다.

## 결정론과 test double 원칙

- clock, ID generator, model client, filesystem/process executor, network transport는 명시적 port로 주입한다.
- 내부 구현을 그대로 흉내 낸 과도한 mock보다 작은 in-memory fake와 contract test를 선호한다.
- unit test에서 network, sleep, 현재 시각, 실행 순서에 의존하지 않는다.
- concurrency test는 timeout으로 성공을 판단하지 않고 관찰 가능한 상태와 event를 기다린다.
- snapshot과 golden fixture 변경은 명시적으로 review하며 자동 덮어쓰기를 금지한다.
- 발견된 버그는 수정 전에 최소 재현 regression test를 추가한다.

## Coverage 운영

초기에는 임의의 전체 line coverage 숫자를 품질 목표로 두지 않는다. 대신 다음 영역은 branch와 실패 경로를 우선한다.

- permission·policy·critical action
- WorkGraph 상태 전이와 journal 복구
- idempotency와 effect 이후 fallback 금지
- digest·receipt·artifact 무결성
- secret redaction과 data boundary
- adapter timeout·취소·부분 실패

모듈이 안정되면 `cargo llvm-cov`로 baseline을 기록하고, 새 변경으로 critical module coverage가 하락하지 않도록 CI gate를 추가한다. 높은 숫자를 위해 의미 없는 assertion을 추가하지 않는다.

## Definition of Done

다음 조건을 충족해야 기능 구현이 완료된 것으로 본다.

- 사용자 시나리오와 acceptance criterion이 명확하다.
- 변경된 동작에 unit, contract 또는 regression test가 있다.
- 권한·외부 effect가 있으면 deny, 취소, 중복 실행 경로를 검사했다.
- protocol 변경은 schema, fixture, typed representation이 함께 변경됐다.
- `cargo fmt`, `cargo clippy -D warnings`, 전체 test, protocol check가 통과한다.
- portable code는 Linux, macOS, Windows CI가 통과한다.
- release binary build가 통과하고 새 runtime 의존성이 문서화됐다.
- secret·credential 원문이 fixture, log, journal, receipt에 포함되지 않는다.
- 문서가 현재 구현과 검증 범위를 과장하지 않는다.

## 현재부터 적용할 우선순위

1. 모든 core state machine에 table-driven unit test를 둔다.
2. WorkGraph와 Journal에는 재시작·중복 event·손상 tail property test를 추가한다.
3. 첫 public-port 기준인 preopened reference adapter suite를 유지하고, 두 번째 adapter에서 구현별 fixture를 분리해 공통 contract testkit으로 추출한다.
4. filesystem/process capability와 함께 3개 OS integration test를 추가한다.
5. 설치 패키지가 생기면 깨끗한 VM의 install/run/uninstall smoke test를 release gate에 넣는다.
