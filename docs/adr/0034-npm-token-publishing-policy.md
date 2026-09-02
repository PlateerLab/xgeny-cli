# ADR-0034: npm은 granular token으로 게시하고 provenance는 OIDC로 증명한다

- 상태: Accepted
- 날짜: 2026-09-02
- 적용 범위: npm package bootstrap, GitHub Actions release, npm credential 운영
- 대체 결정: ADR-0031의 npm Trusted Publisher와 계정 2FA 요구

## 배경

ADR-0031은 장기 credential을 없애기 위해 npm Trusted Publisher를 채택했다. 그러나 package가 존재하기
전에는 Trusted Publisher를 연결할 수 없고, 최초 package 생성과 trust 설정에 npm 계정 2FA가 필요하다.
운영자는 이 사람 개입을 유지하지 않고 `bypass 2FA` granular access token으로 release를 단순화하기로
결정했다.

이 결정은 token 탈취 시 npm publish 권한이 token 만료 또는 폐기까지 유지되는 위험을 수용한다. 대신
native artifact의 checksum, GitHub artifact attestation, exact version/SRI, launcher-last 게시와 npm
provenance 검증은 유지한다.

## 결정

### 1. npm 인증은 repository secret 하나로 제한한다

Package read/write와 `bypass 2FA` 권한이 있는 granular access token을 GitHub Actions repository secret
`NPM_TOKEN`에 저장한다. Token 값은 source, workflow literal, release artifact, npm package와 명령 출력에
넣지 않는다. 가능하면 `@xgen` package 범위와 만료 시점을 제한하며, 만료나 권한 변경 시 secret을
교체한다.

Release workflow는 token을 `publish-npm` job의 최종 publish step에만 `NODE_AUTH_TOKEN`으로 주입한다.
Checkout, GitHub Release 다운로드, checksum과 package 검증은 secret 없이 먼저 끝나야 한다.
`actions/setup-node`가 만드는 npmrc에는 환경변수 참조만 있고 실제 token 값은 기록하지 않는다.

### 2. OIDC는 인증이 아니라 provenance에만 사용한다

Publish job의 `id-token: write`는 npm registry 로그인에 쓰지 않는다. `npm publish --provenance`가 public
GitHub repository와 release commit을 증명하는 SLSA provenance를 생성하는 데만 사용한다. 게시 뒤 모든
package의 registry metadata에서 provenance predicate, local tarball SRI와 dist-tag를 다시 확인한다.

### 3. bootstrap과 release가 같은 인증 경계를 사용한다

실행 파일 없는 `0.0.0-bootstrap.0` 여섯 package도 같은 granular token으로 platform 우선, launcher
마지막 순서로 게시한다. 실제 `0.1.0-rc.3`은 immutable GitHub Release bundle을 유일한 입력으로 사용한다.
이미 존재하는 version은 SRI, dist-tag와 provenance가 모두 같을 때만 재실행에서 건너뛴다.

Repository variable `XGENY_NPM_PUBLISH_ENABLED=true`는 token의 scope, write 권한과 `bypass 2FA`를 관리자가
확인했다는 acknowledgement다. Variable이 없거나 secret이 비어 있으면 release는 fail-closed한다.

## 결과

- npm 계정 2FA와 Trusted Publisher 설정 없이 자동 release할 수 있다.
- GitHub Actions secret 관리 권한과 token 수명이 새로운 공급망 경계가 된다.
- Token이 유출되면 허용 범위의 package를 공격자가 게시할 수 있으므로 repository secret 접근과 workflow
  변경 권한을 제한해야 한다.
- npm provenance와 GitHub artifact attestation은 유지되지만, token-free Trusted Publishing보다 인증
  보안 수준은 낮다.

## 대안

Trusted Publisher는 token-free 인증과 더 좁은 workflow identity를 제공하지만 계정 2FA와 최초 설정이
필요해 운영 편의 요구와 맞지 않아 대체했다. Staged publishing도 최종 승인에 2FA가 필요하므로 채택하지
않는다. Token을 source나 repository npmrc에 넣는 방식은 credential 유출 범위가 커서 채택하지 않는다.

## 검증

- workflow에는 `${{ secrets.NPM_TOKEN }}` 참조가 정확히 한 번만 존재한다.
- `NODE_AUTH_TOKEN`은 publish 단일 step에서만 non-empty 확인 후 사용한다.
- token 주입 전에 GitHub Release binding, checksum과 npm bundle 검증을 완료한다.
- publisher는 `--provenance`, launcher-last, SRI·dist-tag·provenance 사후검증을 유지한다.
- workflow와 문서에 literal `npm_...` token 값이나 `_authToken` 설정을 허용하지 않는다.
