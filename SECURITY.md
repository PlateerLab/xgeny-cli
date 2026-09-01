# XGENy 보안 정책

## 지원 범위

XGENy는 Developer Preview다. 새 보안 수정은 원칙적으로 가장 최근에 게시된 immutable preview version에
더 높은 새 version으로 제공한다. 이미 게시된 GitHub tag, Release asset 또는 npm package version을
이동·교체·재사용하지 않는다.

`v0.1.0-rc.3`이 게시되기 전의 최신 immutable release는 `v0.1.0-rc.2`다. RC3가 게시된 뒤에는 RC2에 새
기능이나 보안 수정을 backport한다고 약속하지 않는다. RC2/RC3 Run의 호환·rollback 경계는
[시작하기](docs/getting-started.md#업데이트와-rc2-rollback)를 따른다.

## 비공개 신고

민감한 취약점은 public issue, discussion 또는 pull request에 작성하지 말고 GitHub의
[비공개 취약점 신고](https://github.com/PlateerLab/xgeny-cli/security/advisories/new)를 사용한다.

다음과 같은 최소 정보만 먼저 제공한다.

- 영향을 받는 exact XGENy version과 설치 채널
- OS family와 architecture
- 고정된 오류 코드 또는 민감정보를 제거한 재현 단계
- 예상한 보안 경계와 실제 관찰의 차이

API key, endpoint credential, 실제 업무 source, prompt, model 원문 응답, tool stdout/stderr, Run state DB와
개인 경로를 첨부하지 않는다. 추가 자료가 필요하면 private advisory 안에서 범위와 전달 방식을 먼저
합의한다. Developer Preview에는 응답 시간 SLA가 없다. 공개 disclosure 시점과 교정 방안은 private
advisory에서 먼저 조율한다. 기존 immutable version은 수정하지 않고 필요한 경우 더 높은 새 version으로
대응한다.

일반 설치·사용 오류와 민감정보 없는 버그는
[GitHub Issues](https://github.com/PlateerLab/xgeny-cli/issues)를 사용한다.

## 주요 보안 경계

특히 다음 문제를 보안 신고 대상으로 본다.

- model/read/write/execute 승인 우회 또는 승인 전 물리 I/O
- workspace scope 밖의 file 접근, symlink/reparse-point escape 또는 atomic write 경계 위반
- API key나 protected environment의 model, child process, log, manifest 또는 artifact 유출
- 불확정 model/effect의 자동 replay, 중복 non-idempotent 실행 또는 Receipt 검증 우회
- checksum, provenance, npm package integrity나 installer destination 검증 우회
- timeout/종료 뒤 descendant process tree가 계속 실행되는 문제

Model이 부정확한 답을 생성하는 것 자체는 보안 취약점이 아니다. 다만 비신뢰 model 제안이 위 승인,
capability, durable no-replay 또는 정보 노출 경계를 우회한다면 보안 문제다.

`process.execute`는 shell을 사용하지 않지만 OS sandbox가 아니다. 사용자가 허용한 compiler, package
manager, interpreter와 그 child는 현재 사용자 권한으로 project code를 실행할 수 있다. 이 명시된 경계
자체와, 구현이 그 경계를 위반하는 문제를 구분한다.

## 게시물 확인

재현에는 가능하면 exact version을 사용한다. GitHub asset은 `checksums.sha256`과 build attestation을,
npm package는 exact version, registry integrity와 provenance를 확인한다. Checksum은 전송 무결성을
확인하지만 publisher identity나 OS code signing을 대신하지 않는다. RC3 macOS/Windows binary에는 아직
notarization/Authenticode가 없다.
