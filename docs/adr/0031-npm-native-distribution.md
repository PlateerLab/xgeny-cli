# ADR-0031: npm은 네이티브 XGENy의 무스크립트 배포 계층이다

- 상태: Accepted
- 날짜: 2026-09-01
- 적용 범위: npm package, GitHub Actions release, 사용자 설치

## 배경

GitHub Release installer는 Node.js 없이 단일 binary를 설치하지만, 이미 Node.js를 쓰는 개발자에게는
`npm install -g`가 더 익숙하다. 반대로 npm 설치 시 native binary를 내려받거나 source build를 수행하는
lifecycle script를 두면 registry 외부 network, compiler와 실행 가능한 install hook이 새 공급망 경계가
된다. XGENy core가 npm 또는 Node.js에 종속되어서도 안 된다.

## 결정

### 1. 제품은 계속 하나의 Rust binary다

`@xgen/cli`는 JavaScript로 구현된 얇은 launcher다. `process.platform`과 `process.arch`로 정확히 하나의
platform package를 선택하고, 그 package에 들어 있는 `xgeny` binary를 argv 그대로 shell 없이 실행한다.
AgentLoop, WorkGraph, embedded SQLite, model/provider와 tool 구현은 npm package에 존재하지 않는다.

직접 GitHub installer를 사용하면 Node.js가 필요 없다. npm 설치 경로만 Node.js 22.14 이상을 요구한다.
어느 경로로 설치해도 실행하는 native binary bytes는 같은 GitHub Release asset과 같아야 한다.

### 2. 여섯 package를 exact version으로 함께 게시한다

- `@xgen/cli`
- `@xgen/cli-linux-x64-musl`
- `@xgen/cli-linux-arm64-musl`
- `@xgen/cli-darwin-x64`
- `@xgen/cli-darwin-arm64`
- `@xgen/cli-win32-x64`

Launcher의 다섯 `optionalDependencies`는 range가 아닌 같은 exact version이다. npm이 현재 OS/CPU와 맞지
않는 optional package를 제외하므로 한 host에는 native package 하나만 설치된다. Linux binary는 static
musl target이지만 glibc host에서도 실행해야 하므로 npm의 `libc` 제한은 두지 않는다.

Package에는 `preinstall`, `install`, `postinstall` 또는 다른 lifecycle script가 없다. GitHub download,
runtime download와 source compile fallback도 없다. Platform package를 얻지 못하면 launcher가 bounded
오류로 종료하며 network fallback을 시도하지 않는다.

### 3. GitHub Release bundle이 npm 게시 입력의 권위다

각 platform tarball은 GitHub Release raw binary와 byte-exact해야 한다. Package manifest, file allow-list,
license/provenance notice와 binary hash를 게시 전에 다시 검증한다. Prerelease는 npm `next`, stable은
`latest` dist-tag를 사용한다. Platform package 다섯 개를 먼저 게시하고 모두 검증된 뒤 launcher를
마지막에 게시한다.

GitHub Release를 만드는 workflow의 `GITHUB_TOKEN`이 별도 `release` event workflow를 시작한다고
가정하지 않는다. 같은 `.github/workflows/release.yml`의 후속 `publish-npm` job이 npm Trusted Publisher
OIDC를 사용한다. 이 job만 `id-token: write`를 가지며 npm password나 장기 access token은 저장하지 않는다.

### 4. 최초 package 생성만 사람의 2FA 승인을 요구한다

npm은 아직 존재하지 않는 package에 Trusted Publisher를 미리 연결할 수 없다. 그래서 실행 파일이 없는
`0.0.0-bootstrap.0` tarball 여섯 개를 사람이 검토한 뒤 npm 2FA로 한 번 게시한다. 그 다음 각 package의
Trusted Publisher를 repository `PlateerLab/xgeny-cli`, workflow `release.yml`에 연결하고
`XGENY_NPM_PUBLISH_ENABLED=true`를 설정한다. Scope 또는 package 소유권을 확인할 수 없으면 release하지
않고 package 이름 계약부터 다시 결정한다.

Bootstrap package는 launcher, native binary와 lifecycle script를 포함하지 않는다. 계정 비밀번호,
recovery code, OTP와 npm token은 repository, 명령 기록 또는 GitHub secret에 넣지 않는다.

### 5. 부분 실패는 동일 bytes만 재개한다

npm version은 덮어쓸 수 없다. 재실행 시 이미 존재하는 package version의 registry SRI가 local tarball과
같고 dist-tag가 맞을 때만 skip한다. 다르면 즉시 중단한다. 따라서 platform package 일부 게시 뒤 실패한
job은 같은 immutable GitHub Release로 재실행할 수 있지만, bytes가 다르거나 package 소유권이 잘못된
경우 기존 version을 고치려 하지 않고 더 높은 새 version으로 수정한다.

## 보안 및 호환성 결과

- npm은 선택형 설치 채널이며 XGEN, Connector, DB, MinIO와 core 의존성을 추가하지 않는다.
- npm registry compromise와 account/scope takeover는 GitHub installer와 다른 공급망 위험이다. OIDC,
  provenance, exact version, SRI와 byte parity는 이를 줄이지만 OS code signing을 대신하지 않는다.
- `--omit=optional`은 native package를 제거하므로 지원하지 않는다.
- 지원하지 않는 OS/CPU는 다른 binary를 추측하거나 내려받지 않고 실패한다.

## 검증

- manifest/catalog/exact optional dependency/no-script 정적 계약
- 다섯 OS/architecture에서 package pack과 loopback registry global-install smoke
- GitHub Release raw asset과 tar member의 streaming SHA-256 parity
- 게시 전 `npm publish --dry-run` file allow-list
- 장기 token marker, OIDC 권한, 검증-before-publish와 launcher-last workflow 정적 검사
- 게시 뒤 다섯 target에서 exact npm version 설치, platform package 존재, `--version`, `licenses`, state
  미생성 확인

## 대안

단일 npm package의 `postinstall`에서 GitHub binary를 받는 방식은 실행 hook과 두 registry 간 결합을
추가하므로 채택하지 않는다. Native source를 npm install 중 compile하는 방식은 Rust toolchain을 사용자
설치 조건으로 만들므로 채택하지 않는다. JavaScript/TypeScript로 CLI를 다시 구현하는 방식은 native
경로와 제품 의미가 분기되므로 채택하지 않는다.
