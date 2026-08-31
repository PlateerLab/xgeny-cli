# Native release 운영 절차

이 문서는 XGENy prototype native artifact의 생성·검증·게시 경계를 고정한다. npm package, package
manager formula, MSI, background updater와 OS code signing은 이 단계에 포함하지 않는다.

## Release 전 조건

- release 대상 commit이 현재 `origin/main`의 head다.
- workspace와 `xgeny-cli` version이 release tag와 정확히 같다.
- tag는 `vMAJOR.MINOR.PATCH` 또는 SemVer prerelease 형식이다.
- 새 stable tag는 이미 게시된 가장 높은 stable SemVer보다 커야 한다.
- GitHub repository의 immutable releases와 tag 보호 설정을 첫 release 전에 활성화한다.
- 활성화 사실을 관리자가 확인한 뒤 Actions repository variable
  `XGENY_IMMUTABLE_RELEASES_ENABLED=true`를 설정한다. Workflow에 administration token은 저장하지 않는다.
- `refs/tags/v*` 또는 모든 tag의 update와 deletion을 막는 active repository ruleset을
  exclude와 bypass actor 없이 활성화한다.
- `main`에 PR, deletion과 non-fast-forward 차단을 적용하는 active branch ruleset을 exclude와
  bypass actor 없이 활성화한다. Required status checks의 strict policy를 켜고 아래 여섯 context를
  모두 필수로 지정한다. 각 check의 expected source는 반드시 `GitHub Actions`로 선택한다. 이름만 같고
  source가 다르거나 source가 지정되지 않은 check는 release workflow가 거부한다.
  - `Quality / Linux`
  - `Platform / x86_64-unknown-linux-musl`
  - `Platform / aarch64-unknown-linux-musl`
  - `Platform / x86_64-apple-darwin`
  - `Platform / aarch64-apple-darwin`
  - `Platform / x86_64-pc-windows-msvc`
  설정 후
  `XGENY_MAIN_PROTECTION_ENABLED=true`를 둔다.
- Bypass actor가 없음을 관리자가 확인한 뒤 Actions repository variable
  `XGENY_RELEASE_RULESET_NO_BYPASS=true`를 설정한다. Read-only ruleset API는 bypass 목록을 숨길 수 있다.
- macOS notarization과 Windows Authenticode가 없는 build는 SemVer와 별개로 prototype이라고 명시하고
  OS 경고 가능성을 안내한다.

로컬 확인:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
sh scripts/check-third-party-licenses.sh --check
```

`Cargo.lock`, `about.toml` 또는 license template를 바꾼 뒤 고지를 갱신할 때는
`sh scripts/check-third-party-licenses.sh --write`를 실행하고 diff를 사람이 검토한다. Script는
host별 `cargo-about 0.9.2` artifact의 고정 SHA-256을 확인한 뒤 실행한다. 공식 prebuilt artifact가
없는 Intel macOS에서는 이 유지보수 명령 대신 Linux CI 또는 지원되는 개발 host를 사용한다. 최종 사용자용
XGENy Intel macOS binary와 installer 검증 범위에는 영향이 없다.

Version을 올린 변경을 먼저 PR로 merge한 뒤 main head에 tag를 만든다. `main`에는 직접 push하지 않는다.

```bash
release_version=0.1.0-rc.2
release_tag="v$release_version"
git fetch origin main
git tag -a "$release_tag" origin/main -m "XGENy $release_version"
git push origin "$release_tag"
```

## Workflow gate

Tag push는 권한 없는 `.github/workflows/release-request.yml`만 실행한다. 기본 브랜치에 있는
`.github/workflows/release.yml`은 성공한 request의 `workflow_run` event를 받아 publisher를 시작한다.
Publisher는 source run ID, canonical workflow ID/path, repository, tag와 SHA를 API로 다시 확인하고,
현재 default branch `main`, `GITHUB_SHA`, tag의 peeled commit이 모두 같은 SHA일 때만 release commit을
checkout한다. Request workflow의 code, cache나 artifact는 publisher 입력으로 신뢰하지 않는다.

Publish 이전에는 다음을 다시 확인한다.

1. tag grammar, Cargo package version과 `xgeny --version` 일치
2. tag commit과 현재 `origin/main` head 일치, stable version의 단조 증가
3. format, clippy와 전체 workspace test
4. pinned `cargo-about`으로 `Cargo.lock` 기반 제3자 고지 최신성, Rust 1.98.0 standard library와
   Linux musl/libunwind 고지의 고정 size·SHA-256 및 binary 내장 여부
5. Linux x86-64/ARM64 musl, macOS Intel/Apple Silicon, Windows x86-64 native build
6. Linux ELF architecture와 `INTERP`/`NEEDED`/`RPATH`/`RUNPATH` 부재, macOS Xcode 16.4·SDK 15.5와
   Mach-O architecture/system-only dependency, Windows Server 2025 + Visual Studio 2026의 x64 PE와
   static CRT, physical System32 DLL 또는 정확히 고정·검토된 API-set contract로 한정된 import와 원본·설치본 실행
7. 각 target의 release `public_run_resume`, `environment_onboarding` process test
8. loopback fixture에서 exact-tag download, checksum, 설치, 재설치, protocol check와 test-owned 삭제
9. 최종 asset allow-list, `sha256sum -c`, 조립된 Linux fixture installer smoke와 GitHub artifact attestation
10. 게시 뒤 다섯 target에서 public exact-tag 설치, stable release의 `releases/latest/download` bootstrap과
   내부 `latest` 해석, state 미생성

Read-only assemble job은 검증된 target artifact와 installer·고지를 조립하고 checksum을 확인한 뒤
하나의 allow-listed bundle로 전달한다. Checkout이나 repository script를 실행하지 않는 publish job은
bundle의 exact file set과 checksum, 원격 `main`, annotated/lightweight tag의 peeled commit 및 trusted
workflow commit이 여전히 같은지 policy gate에서 확인하고, attestation 뒤 `gh release create` 직전에도
원격 `main`과 tag를 다시 조회한다. Publisher는 Git checkout 문맥에 의존하지 않도록
`--repo "$GITHUB_REPOSITORY"`를 명시하며 `--verify-tag`로 원격 tag 존재를 다시 확인한다. 관리자가
immutable releases 활성화 후 설정하는 repository variable이
exact `true`가 아니면 게시를 fail-closed로 중단한다. GitHub 설정 조회에는 Administration
권한이 필요하므로 고권한 PAT를 workflow secret으로 추가하지 않는다. 반면 공개 repository의 active
ruleset은 workflow가 API로 읽어 exact include/exclude, 빈 bypass actor와 update/deletion rule을 직접
검증한다. API가 bypass actor를 반환하면 빈 배열만 허용하고, 권한 때문에 숨겨지는 경우에는 별도
관리자 acknowledgement를 요구한다. 게시할 때 prerelease는 명시적으로 latest에서 제외하고 stable은
latest로 지정한다. 게시 직후에는 일반 release 조회로 exact tag, draft/prerelease 분류,
`immutable=true`, stable의 latest 해석과 잠긴 tag의 peeled commit을 다시 확인한다. 이 사후검사가
실패하면 이미 게시된 release의 이상을 알리는 것이므로,
사전 ruleset과 관리자 확인을 대신하지 않는다.

Quality, build와 assemble job은 repository read 권한만 가진다. 모든 target과 assembly가 통과한 뒤
publish job에만 `contents: write`, `id-token: write`, `attestations: write`를 부여한다. 모든 checkout은
credential persistence를 끈다. 외부 GitHub Action은 mutable tag가 아니라 full commit SHA로 고정한다.

## 게시 artifact

- `xgeny-x86_64-unknown-linux-musl`
- `xgeny-aarch64-unknown-linux-musl`
- `xgeny-x86_64-apple-darwin`
- `xgeny-aarch64-apple-darwin`
- `xgeny-x86_64-pc-windows-msvc.exe`
- `xgeny-installer.sh`
- `xgeny-installer.ps1`
- `checksums.sha256`
- `LICENSE.txt`
- `NATIVE_RUNTIME_PROVENANCE.md`
- `THIRD_PARTY_LICENSES.txt`

Installer는 archive를 해제하지 않고 raw binary만 설치한다. 따라서 archive path traversal,
symlink/hardlink entry 해석이 설치 경로에 존재하지 않는다. Release와 checksum을 같은 GitHub source에서
받는 것만으로 publisher authenticity가 완성되지는 않으므로 승격 검증에서는 다음 명령으로 attestation을
확인한다.

```bash
gh attestation verify PATH_TO_ASSET --repo PlateerLab/xgeny-cli
```

Release가 실패하면 기존 tag나 asset을 교체해 재사용하지 않는다. 원인을 수정하고 version을 올린 새
tag로 게시한다. 자동 update, downgrade와 실행 중 self-replacement는 signed update metadata와 rollback
정책을 별도로 설계하기 전까지 추가하지 않는다.

`v0.1.0-rc.1`은 Release API 호출 전 publisher의 repository-context 해석 실패로 GitHub Release와
게시 asset 없이 폐기된 tag다. 보호된 tag를 이동·삭제·재사용하지 않고 `v0.1.0-rc.2`부터 이어간다.
