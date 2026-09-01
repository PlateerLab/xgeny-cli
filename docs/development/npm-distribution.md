# npm 배포와 Trusted Publishing 운영

`@xgen/cli`는 native XGENy를 npm으로 설치하기 위한 선택형 배포 계층이다. 직접 GitHub installer를 쓰는
사용자는 Node.js가 필요 없고, npm 경로를 고른 사용자는 Node.js 22.14 이상이 필요하다. 두 경로는 같은
release binary를 실행한다.

이 package 이름은 설치 entrypoint를 뜻하며 화면 구현을 뜻하지 않는다. 현재 가벼운 REPL이나 향후 같은
`xgeny` binary에 포함될 TUI는 동일한 `@xgen/cli`로 배포할 수 있다. TUI를 별도 제품·release cadence로
분리할 때만 별도 package naming ADR을 먼저 작성한다.

## Package 구성

| Package | 대상 |
| --- | --- |
| `@xgen/cli` | OS/CPU 선택과 native process 실행만 담당하는 launcher |
| `@xgen/cli-linux-x64-musl` | Linux x86-64 |
| `@xgen/cli-linux-arm64-musl` | Linux ARM64 |
| `@xgen/cli-darwin-x64` | macOS Intel |
| `@xgen/cli-darwin-arm64` | macOS Apple Silicon |
| `@xgen/cli-win32-x64` | Windows x86-64 |

모든 package는 같은 exact version으로 게시한다. Lifecycle script, 설치 중 GitHub download와 native
compile은 없다. Launcher는 현재 platform package가 없으면 종료한다. `--omit=optional` 설치는 지원하지
않는다.

## 최초 1회 운영 준비

이 절차는 release tag를 만들기 전에 npm scope 관리자 한 명이 대화형으로 수행한다.

1. npm 계정의 2FA와 recovery 수단을 확인한다. 공유되었거나 노출 가능성이 있는 비밀번호는 먼저
   교체한다. 비밀번호, OTP, recovery code와 token을 repository나 shell script에 기록하지 않는다.
2. 해당 계정 또는 npm organization이 `@xgen` scope 여섯 package를 public으로 게시할 권한이 있는지
   확인한다. 권한이나 scope 확보 여부가 불명확하면 `@xgen/cli`를 게시하지 말고 naming ADR을 먼저
   갱신한다.
3. clean `main` checkout에서 bootstrap tarball을 만든다.

```bash
npm ci --ignore-scripts --prefix npm
bootstrap_dir=$(mktemp -d "${TMPDIR:-/tmp}/xgeny-npm-bootstrap.XXXXXX")
node npm/scripts/bootstrap.mjs --output-dir "$bootstrap_dir"
```

4. `npm-bootstrap-manifest.json`의 package name, version, integrity를 확인하고 각 tarball에
   `package.json`, `README.md`, `LICENSE`만 있는지
   `npm publish --dry-run --ignore-scripts --tag bootstrap --json TARBALL`로 검토한다.
5. npm web login/2FA가 활성화된 현재 session에서 여섯 tarball을 `--access public --tag bootstrap`으로
   한 번 게시한다. Bootstrap에는 GitHub OIDC provenance 설정이 없으며 이 단계는 자동화하지 않는다.
   OTP를 명령행 인자로 남기지 않는다.
6. npm package 여섯 개 각각에 GitHub Actions Trusted Publisher를 설정한다.
   - organization/user: `PlateerLab`
   - repository: `xgeny-cli`
   - workflow filename: `release.yml`
   - environment: 사용하지 않음
   - allowed actions: `npm publish`만 선택하고 `npm stage publish`는 선택하지 않음
   2026-05-20 이후 생성하는 Trusted Publisher는 allowed action을 하나 이상 명시해야 한다. 현재 release
   workflow는 직접 `npm publish`를 호출하므로 여섯 package 모두 위 선택이 같아야 한다.

   npm CLI 11.5.1 이상을 쓰면 npmjs.com의 동일 설정을 아래처럼 적용하고 즉시 다시 읽어 확인할 수 있다.
   이전 CLI를 유지해야 하면 package settings 화면에서 같은 값을 직접 설정한다.

```bash
trusted_packages='@xgen/cli-linux-x64-musl @xgen/cli-linux-arm64-musl @xgen/cli-darwin-x64 @xgen/cli-darwin-arm64 @xgen/cli-win32-x64 @xgen/cli'
for package in $trusted_packages; do
  npm trust github "$package" \
    --repository PlateerLab/xgeny-cli \
    --file release.yml \
    --allow-publish
  npm trust list "$package" --json
done
```

7. Package settings의 Publishing access를 `Require two-factor authentication and disallow tokens`로
   설정하고 write token을 추가하지 않았는지 확인한 뒤 bootstrap version을 deprecated 처리한다.
   이 설정은 GitHub OIDC Trusted Publisher를 막지 않는다.
   Package 자체는 unpublish하지 않는다.
8. GitHub repository variable `XGENY_NPM_PUBLISH_ENABLED=true`를 설정한다. 이 값은 여섯 package의
   소유권과 Trusted Publisher 설정을 사람이 확인했다는 fail-closed acknowledgement다.

Trusted Publisher는 이미 존재하는 package에만 설정할 수 있기 때문에 bootstrap이 필요하다. Bootstrap
version은 실행 파일과 `bin` entry가 없어 사용 가능한 XGENy release가 아니다.

## Release 동작

보호 tag가 현재 `main`과 일치하면 기존 `release.yml`이 다음 순서로 실행한다.

```text
quality + 5-platform native build
  -> native/npm pack + install smoke
  -> immutable GitHub Release + attestations
  -> GitHub Release exact bundle 재다운로드/검증
  -> 5 platform npm packages publish/verify
  -> @xgen/cli publish/verify
  -> 5-platform public registry install smoke
```

npm publish job은 GitHub-hosted Ubuntu runner, Node.js 24.20.0/npm 11.19.0과 OIDC
`id-token: write`를 사용한다. `NODE_AUTH_TOKEN`, `NPM_TOKEN`과 npm password는 사용하지 않는다.
Prerelease는 `next`, stable은 `latest` dist-tag로 게시한다. 모든 package에는 npm provenance가 있어야
하며 registry metadata에서 SLSA provenance predicate가 확인되지 않으면 job이 실패한다.

## 부분 실패와 재실행

Platform package를 launcher보다 먼저 게시하므로 중간 실패가 가능하다. 실패한 GitHub Actions job을
같은 run에서 재실행한다. Publisher는 이미 존재하는 exact version의 SRI와 dist-tag가 local immutable
tarball과 모두 같을 때만 skip한다. 하나라도 다르면 자동 수정, unpublish 또는 overwrite하지 않는다.

- 동일 bytes인 일부 게시: 실패 job 재실행
- Trusted Publisher/권한 오설정: 설정을 바로잡은 뒤 실패 job 재실행
- registry bytes 또는 package name 소유권 불일치: 중단하고 새 version PR과 tag 사용
- GitHub Release 전 실패: 원인을 main에서 수정하고 더 높은 version/tag 사용

## 로컬 및 PR 검증

실제 registry publish 없이 다음 계약을 확인한다.

```bash
npm ci --ignore-scripts --prefix npm
npm run check --prefix npm
npm test --prefix npm
sh scripts/check-npm-distribution-workflow.sh
```

각 platform CI는 release binary를 platform tarball에 넣고 loopback registry에서 `npm install -g`,
대화형 `/status`/`/exit`, 동일 version 재설치와 `npm uninstall -g`를 수행한다. Release assemble은 여섯
tarball의 file allow-list, 고지와 raw binary byte parity를 확인한다.
로컬이나 PR에서는 실제 npm publish, package bootstrap, Trusted Publisher 변경과 release tag 생성을
수행하지 않는다.
