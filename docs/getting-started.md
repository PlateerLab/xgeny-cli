# XGENy 설치와 첫 실행

이 문서는 Developer Preview `v0.1.0-rc.3` release artifact가 게시된 뒤 binary 하나를 설치하고
OpenAI-compatible model에 연결해 파일 읽기부터 프로젝트 탐색·수정·test/build까지 실행하는 최소
절차다. RC3 후보는 exact
`--allow-file`, `--allow-dir` workspace discovery, atomic write/patch와 host-catalogued shell-free
process 실행을 포함한다. Native installer 경로는 소스 개발 절차와 달리 Rust, C compiler, Python,
Node.js, SQLite 실행 파일, Docker 또는 XGENy state용 별도 daemon이 필요하지 않다. npm 경로만
launcher용 Node.js 22.14 이상이 필요하다. 사용할 model endpoint 자체는 로컬 또는 원격에 별도로 실행
중이어야 한다.

## 5분 빠른 시작

먼저 사용할 OpenAI-compatible endpoint URL, served model ID와 필요한 경우 API key를 준비한다. Node.js
22.14 이상이 있는 지원 OS에서는 아래 순서가 가장 짧다.

```bash
npm install --global --include=optional @xgen/cli@0.1.0-rc.3
xgeny --version
xgeny model setup
xgeny model check --compatibility
cd my-project
xgeny
```

대화형 화면에서 `/status`로 선택 model과 idle 상태를 확인하고, 작은 읽기 전용 작업으로 첫 승인을
확인한 뒤 `/exit`으로 종료한다. Model egress와 file read는 서로 다른 승인이다. Node.js를 설치하지
않으려면 아래 checksum installer를 사용한 뒤 `xgeny model setup`부터 같은 순서로 진행한다. API key를
명령행 인자, shell history 또는 일반 설정 파일에 넣지 않는다.

## 게시 target과 CI 검증 OS

| CI runner OS | Architecture | Release asset |
| --- | --- | --- |
| Ubuntu 24.04 | x86-64 | `xgeny-x86_64-unknown-linux-musl` |
| Ubuntu 24.04 ARM (public preview runner) | ARM64 | `xgeny-aarch64-unknown-linux-musl` |
| macOS 15 | Intel | `xgeny-x86_64-apple-darwin` |
| macOS 15 | Apple Silicon | `xgeny-aarch64-apple-darwin` |
| Windows Server 2025 + Visual Studio 2026 | x86-64 | `xgeny-x86_64-pc-windows-msvc.exe` |

Linux binary는 musl target, Windows binary는 static CRT로 빌드한다. macOS와 Windows artifact는
현재 prototype 단계에서 OS code signing/notarization이 없으므로 Gatekeeper 또는 SmartScreen
경고가 나타날 수 있다. GitHub build attestation은 build provenance를 검증하지만 OS code signing을
대신하지 않는다. 표보다 오래된 OS version은 아직 설치 E2E를 통과했다고 주장하지 않는다.

## 설치

Node.js 22.14 이상이 이미 있는 개발자는 RC3가 npm에 게시된 뒤 다음처럼 exact version을 설치한다.
`@xgen/cli`에는 lifecycle script가 없고 현재 OS/CPU용 native optional package 하나를 registry에서 함께
설치한다. 설치 중 GitHub download나 source compile을 하지 않는다.

```bash
npm install --global @xgen/cli@0.1.0-rc.3
xgeny --version
xgeny licenses
```

Prerelease 최신 채널은 `@xgen/cli@next`, stable은 `@xgen/cli@latest`지만 재현 가능한 설치와 검증에는
exact version을 사용한다. `npm install --omit=optional`은 실행 binary package를 제거하므로 지원하지
않는다. npm 경로에서 platform package를 찾지 못하면 launcher는 다른 binary를 내려받지 않고 종료한다.

Node.js를 설치하고 싶지 않거나 GitHub attestation까지 직접 확인하려면 아래 checksum installer 경로를
사용한다.

Installer를 먼저 파일로 받은 뒤 검토·실행한다. Installer는 shell profile이나 user PATH를 자동으로
수정하지 않고 관리자 권한을 요청하지 않는다. RC3는 prerelease이므로 stable release만 가리키는
`releases/latest` 대신 아래 exact tag를 사용한다. 후보 version PR의 merge만으로 artifact가 생기지는
않으므로 GitHub Release에 `v0.1.0-rc.3`이 게시된 것을 확인한 뒤 실행한다.

Linux/macOS:

```bash
installer=$(mktemp "${TMPDIR:-/tmp}/xgeny-installer.XXXXXX")
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo "$installer" \
  https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0-rc.3/xgeny-installer.sh
sh "$installer" --version v0.1.0-rc.3
rm -f "$installer"
export PATH="$HOME/.local/bin:$PATH"
```

Windows PowerShell:

```powershell
$installer = Join-Path ([System.IO.Path]::GetTempPath()) "xgeny-installer-$([Guid]::NewGuid().ToString('N')).ps1"
curl.exe -q --proto "=https" --proto-redir "=https" --tlsv1.2 --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo $installer "https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0-rc.3/xgeny-installer.ps1"
if ($LASTEXITCODE -ne 0) { throw "XGENy installer download failed" }
& $installer -Version v0.1.0-rc.3
Remove-Item -LiteralPath $installer
$env:Path = "$env:LOCALAPPDATA\XGENy\bin;$env:Path"
```

Windows client의 effective execution policy가 `Restricted`면 `& $installer`가 차단될 수 있다. 내용을
검토한 뒤에만 현재 process 한정으로
`powershell.exe -NoProfile -ExecutionPolicy Bypass -File $installer`를 사용할 수 있다. 서명된
installer/winget 경로는 prototype 이후 범위다.

기본 위치는 Linux/macOS의 `$HOME/.local/bin/xgeny`, Windows의
`%LOCALAPPDATA%\XGENy\bin\xgeny.exe`다. 다른 user-owned 위치는 각각 `--install-dir` 또는
`-InstallDir`로 지정한다. 설치기는 다음 순서를 fail-closed로 수행한다.

1. OS와 architecture에 맞는 raw binary 하나 및 `checksums.sha256`을 같은 exact release에서 받는다.
2. SHA-256이 일치하는지 확인한다.
3. 받은 binary의 `--version`이 요청 tag와 일치하는지 확인한다.
4. `xgeny protocol check`를 외부 network 없이 실행한다.
5. symbolic link/reparse point가 아닌 regular destination에 같은-directory rename으로 설치한다.

Project, Cargo dependency, Rust standard library, Linux musl과 LLVM libunwind의 고지는 설치된
binary에 포함된다. 확인 명령은 runtime state를 만들거나 network를 사용하지 않는다.

```bash
xgeny licenses
```

재현 가능한 설치에는 `latest` 대신 exact tag를 지정한다.

```bash
installer=$(mktemp "${TMPDIR:-/tmp}/xgeny-installer.XXXXXX")
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo "$installer" \
  https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0-rc.3/xgeny-installer.sh
sh "$installer" --version v0.1.0-rc.3
rm -f "$installer"
```

```powershell
$installer = Join-Path ([System.IO.Path]::GetTempPath()) "xgeny-installer-$([Guid]::NewGuid().ToString('N')).ps1"
curl.exe -q --proto "=https" --proto-redir "=https" --tlsv1.2 --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo $installer "https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0-rc.3/xgeny-installer.ps1"
if ($LASTEXITCODE -ne 0) { throw "XGENy installer download failed" }
& $installer -Version v0.1.0-rc.3
Remove-Item -LiteralPath $installer
```

Release asset을 직접 받은 경우 GitHub CLI로 build provenance를 추가 확인할 수 있다.

```bash
gh attestation verify ./xgeny-x86_64-unknown-linux-musl \
  --repo PlateerLab/xgeny-cli
```

Checksum은 전송 손상을 탐지하지만 같은 release publisher 자체를 독립적으로 인증하지는 않는다.
자동 update와 silent downgrade는 현재 지원하지 않는다.

## 로컬 모델 연결

현재 검증된 기준은 vLLM의 OpenAI-compatible Chat Completions endpoint다. 다음 조건을 모두 만족해야
실제 planner call이 통과한다.

- base URL의 path가 `/v1`로 끝난다.
- plaintext HTTP는 `127.0.0.1` 또는 `::1` literal loopback만 사용한다.
- server가 strict JSON Schema response format을 지원한다.
- response의 `model` 문자열이 설정한 served model ID와 정확히 같다.
- custom CA, ambient proxy, redirect와 자동 retry에 의존하지 않는다.

일반 사용자는 대화형 온보딩을 한 번 실행한다. 누락된 URL과 API key를 묻고 catalog에서 model을
선택한다. API key는 숨김 입력이며 일반 config나 SQLite가 아니라 OS 보안 저장소에만 보관한다.

```bash
xgeny model setup
xgeny model list
xgeny model check --compatibility
```

인증 없는 loopback vLLM을 non-interactive하게 지정할 수도 있다. Tokenizer를 생략하면 model ID를
tokenizer/profile identity로 사용한다.

```bash
xgeny model setup \
  --base-url http://127.0.0.1:8000/v1 \
  --model qwen3.8-27b \
  --tokenizer Qwen/Qwen3.8-27B-FP8
```

Loopback HTTP에는 ambient `XGENY_OPENAI_API_KEY`가 있더라도 전송하지 않는다.

원격 provider는 HTTPS를 사용한다. Interactive setup은 macOS Keychain, Windows Credential Manager 또는
Linux Secret Service에 key를 저장한다. Headless/CI는 secret manager 출력을 stdin으로 전달한다.
`--store-token`을 생략하면 현재 setup 검증에만 사용하고 저장하지 않는다.

```bash
secret-manager-command | xgeny model setup \
  --name qwen-xgen \
  --base-url https://provider.example/v1 \
  --model served-model-id \
  --token-stdin \
  --store-token
```

OS 보안 저장소가 없는 Linux server에서는 평문 파일로 fallback하지 않고
`credential_store_unavailable`로 실패한다. 이 경우 환경변수나 stdin을 매 실행에 주입한다.

```bash
XGENY_OPENAI_API_KEY="$(secret-manager-command)" xgeny model check --compatibility
```

일반 설정 우선순위는 명시적 option, `XGENY_OPENAI_*` 환경변수, selected/active profile 순서다.
Credential은 `--token-stdin`, `XGENY_OPENAI_API_KEY`, profile secure store 순서다. Stored credential은
profile URL과 최종 URL이 정확히 같을 때만 쓴다.

`xgeny model setup`은 bounded `GET /v1/models` 뒤 실제 strict JSON Schema
`POST /v1/chat/completions`를 보내 두 요청이 모두 성공한 profile만 commit한다. Workspace, SQLite 또는
Run state를 만들지 않는다. `model check` 기본형은 기존 자동화 호환성을 위해 catalog GET만 수행하고,
`--compatibility`를 지정하면 같은 inference probe를 추가한다. Redirect, retry와 ambient proxy는
사용하지 않는다. 일부 compatible server는 catalog를 제공하지 않거나 alias를 다르게 광고하므로
`model_not_advertised`는 catalog에서 확인되지 않았다는 뜻이다. Setup probe 통과도 전체 coding loop
품질을 보장하지는 않으며 실제 workspace E2E는 별도 release gate다.

Profile 관리는 다음 명령으로 한다.

```bash
xgeny model list
xgeny model use qwen-xgen
xgeny model logout qwen-xgen
xgeny model remove qwen-xgen
```

세부 보안·headless 동작은 [모델 프로필과 최초 온보딩](development/model-onboarding.md)을 따른다.
전송 여부가 불확정한 inference transport 실패는 자동 재시도하지 않고 해당 Run을
`model_call_unknown` recovery 상태로 남길 수 있다.

## 기본 대화형 사용

프로젝트 root에서 `xgeny`만 실행하면 가벼운 REPL이 열린다. TTY에서 active profile이 없으면 URL, 숨김
API key와 model 선택 온보딩을 먼저 수행한다. Pipe/headless 입력에서는 prompt를 자동으로 열지 않는다.

```bash
cd my-project
xgeny
```

기본값은 model egress, read, write, execute를 각각 `ask`로 묻는다. 현재 directory는 탐색 가능한
workspace scope지만 승인이 없으면 실제 I/O를 수행하지 않는다. PATH에서 고정 allowlist에 해당하는 공통
개발 executable을 logical catalog로 발견하되 catalog 등록은 실행 승인이 아니다.

```text
xgeny> 프로젝트를 탐색하고 실패한 테스트를 수정해줘.
Allow sending the goal, session context, and tool observations to the model? [y/N] y
progress: model_call_starting
```

명령은 `/model`, `/status`, `/permissions`, `/resume`, `/clear`, `/exit`이다. 줄 끝 `\`는 multiline
continuation이다. 직전 durable completion summary만 다음 goal에 비신뢰 context로 이어지며 `/clear`는
연결을 제거하지만 Run state를 삭제하지 않는다. 상세 동작과 Ctrl+C 복구 의미는
[대화형 REPL](development/interactive-repl.md)을 따른다.

## 첫 실행과 재개

현재 directory를 workspace로 열고 model이 선택할 수 있는 상대 파일을 명시한다. Model egress와 실제
file read는 서로 다른 동의다.

```bash
xgeny run \
  --allow-file README.md \
  --allow-remote-model-egress \
  --allow-read \
  'README를 읽고 핵심을 요약해줘.'
```

Windows PowerShell에서도 active profile을 사용해 같은 명령을 실행한다.

```powershell
xgeny run `
  --allow-file README.md `
  --allow-remote-model-egress `
  --allow-read `
  "README를 읽고 핵심을 요약해줘."
```

RC3에서는 사용자가 파일명을 미리 나열하지 않고 선택한 workspace directory를 탐색하고 별도 승인 아래
atomic write할 수 있다.

```bash
xgeny run \
  --workspace . \
  --allow-dir . \
  --allow-remote-model-egress \
  --allow-read \
  --allow-write \
  '프로젝트 구조를 확인하고 필요한 파일을 안전하게 수정해줘.'
```

이 mode는 `list-directory`, `stat`, case-sensitive literal `search-text`, bounded UTF-8
`read-text`, `write-atomic`, `apply-patch`를 제공한다. `--allow-write`는 read/model egress와 독립된 동의다. 더 좁게
허용하려면 `--allow-dir src --allow-dir tests`처럼 반복하고,
directory 밖의 exact file만 `--allow-file Cargo.toml`로 추가한다. 미완료 재개에는 원래와 동일한
`--allow-dir`/`--allow-file` 집합을 다시 제공해야 한다.

RC3에서는 test/build용 executable도 absolute path로 명시할 수 있다. 경로를 model에 보내지는 않으며
logical ID만 planning constraint에 노출한다.

```bash
xgeny run \
  --workspace . \
  --allow-dir . \
  --allow-executable cargo="$(command -v cargo)" \
  --allow-remote-model-egress \
  --allow-read \
  --allow-write \
  '프로젝트를 탐색하고 수정한 뒤 테스트해줘.'
```

Process 실행은 file read/write와 별도 승인이다. `--allow-execute`가 없으면 실제 실행 전에
`execute_approval_required`로 pause하며, 동일한 scope와 executable catalog를 다시 제공해 승인한다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef \
  --workspace . \
  --allow-dir . \
  --allow-executable cargo="$(command -v cargo)" \
  --allow-read \
  --allow-write \
  --allow-execute
```

실행은 shell을 사용하지 않지만 OS sandbox도 아니다. 허용한 compiler, package manager, interpreter와
그 하위 process는 현재 사용자 권한으로 project code를 실행할 수 있다. CLI는 ambient 환경 전체를
상속하지 않고 제한된 host 환경만 전달하며 API key와 proxy/클라우드 credential은 child에 전달하지
않는다. Top-level executable catalog는 해당 도구가 실행하는 하위 프로그램의 allow-list가 아니다.
미완료 재개 시 executable 내용, workspace 또는 safe environment binding이 달라지면 자동 대체나 재실행
없이 `configuration_mismatch`로 닫는다.

상태가 pause되면 stderr의 `run_id`를 사용한다. 미완료 Run은 workspace와 원래 사용한 동일한
allow-file/allow-dir catalog를 다시 제공하고 필요한 동의를 명시한다. Base URL은 위 environment에서
다시 읽는다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef \
  --workspace . \
  --allow-file README.md \
  --allow-remote-model-egress \
  --allow-read
```

완료된 Run만 model과 workspace 없이 다음처럼 offline replay할 수 있다.

```bash
xgeny resume run-0123456789abcdef0123456789abcdef
```

상세 재개 절차는 [Public local run/resume prototype](development/public-local-run-resume.md)을 따른다.
RC3는 workspace discovery, bounded atomic text write/patch와 명시적 shell-free process 실행을
제공한다. Browser와 MCP는 후속 범위이므로 현 상태를 Claude Code/Codex 전체 기능과 동일하다고 해석하지
않는다. Discovery 경계는
[Workspace filesystem discovery](development/workspace-filesystem-discovery.md), write 경계는
[Workspace atomic write](development/workspace-atomic-write.md)를 따른다.

## 업데이트와 RC2 rollback

npm 설치본은 새 exact version을 다시 설치해 업데이트한다. `next`는 시간이 지나면 다른 prerelease를
가리킬 수 있으므로 파일럿과 장애 재현에는 사용하지 않는다.

```bash
npm install --global --include=optional @xgen/cli@0.1.0-rc.3
xgeny --version
```

Native 설치본은 위 installer를 같은 install directory에 exact tag로 다시 실행한다. Installer는 새
binary의 checksum, version과 protocol을 먼저 확인하고 성공한 파일만 같은 directory rename으로 교체한다.
실행 중인 `xgeny`를 교체하지 말고 모든 session을 종료한 뒤 업데이트한다.

RC3 npm package만 문제가 있고 같은 RC3 GitHub Release가 정상이라면 npm 설치본을 제거하고 exact RC3
native installer로 전환할 수 있다. 두 채널은 같은 release binary를 담으므로 이는 제품 version
rollback이 아니라 설치 채널 전환이다.

제품 version을 RC2로 되돌려야 하면 기존 RC3 binary와 state를 지우거나 덮어쓰지 말고 RC2를 별도
user-owned directory에 설치한다. RC3가 만든 Run을 RC2로 열거나 resume하는 것은 지원하지 않는다.
RC2에서 시작해 아직 model egress가 필요한 Run은 원래 RC2 binary로 마치고, 새 작업은 RC3에서 새 Run으로
시작한다. 완료된 RC2 Run은 RC3에서 offline replay할 수 있다.

```bash
rollback_dir="$HOME/.local/xgeny-rc2/bin"
installer=$(mktemp "${TMPDIR:-/tmp}/xgeny-installer.XXXXXX")
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo "$installer" \
  https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0-rc.2/xgeny-installer.sh
sh "$installer" --version v0.1.0-rc.2 --install-dir "$rollback_dir"
rm -f "$installer"
"$rollback_dir/xgeny" --version
```

Immutable GitHub Release나 npm version에 치명적 문제가 있으면 기존 tag나 package를 이동·교체·재사용하지
않는다. 수정은 더 높은 새 version으로 게시하고, 영향을 받은 exact version과 우회 방법을 release note에
남긴다.

## 삭제

npm 설치본은 launcher와 현재 platform package를 함께 제거한다.

```bash
npm uninstall --global @xgen/cli
```

Native installer는 runtime state를 삭제하지 않는다. Binary만 제거하려면 설치한 exact regular file을
지운다.

```bash
rm "$HOME/.local/bin/xgeny"
```

```powershell
Remove-Item -LiteralPath "$env:LOCALAPPDATA\XGENy\bin\xgeny.exe"
```

Run state는 별도로 보존된다. Linux는 `$XDG_STATE_HOME/xgeny` 또는 `$HOME/.local/state/xgeny`,
macOS는 `$HOME/Library/Application Support/XGENy`, Windows는 `%LOCALAPPDATA%\XGENy`를 사용한다.
State 삭제는 Run 기록과 durable recovery 정보를 잃으므로 uninstall에 자동 포함하지 않는다.

## 문제 해결과 지원 정보

| 증상 | 확인과 조치 |
| --- | --- |
| `xgeny: command not found` | npm global bin 또는 native install directory가 현재 `PATH`에 있는지 확인하고 새 terminal에서 다시 실행한다. |
| `unsupported_platform` 또는 platform package 없음 | 지원 OS/architecture와 Node.js version을 확인하고 `--omit=optional` 없이 exact package를 재설치한다. |
| Installer가 destination을 거부 | 관리자 공용 경로 대신 user-owned directory를 사용하고 symbolic link/reparse point나 broadly writable directory를 제거하지 말고 다른 빈 경로를 선택한다. |
| PowerShell에서 installer 차단 | Script를 먼저 검토한다. 현재 process 한정 execution-policy 예외는 조직 정책이 허용할 때만 사용한다. SmartScreen 경고는 RC3에 Authenticode가 없기 때문이다. |
| `credential_store_unavailable` | 평문 저장 fallback은 없다. Secret manager 출력을 `--token-stdin`이나 현재 process의 `XGENY_OPENAI_API_KEY`로 전달한다. |
| `model_not_advertised` 또는 compatibility 실패 | URL이 `/v1`로 끝나는지, exact served model ID와 strict JSON Schema Chat Completions 지원을 확인한다. Redirect나 자동 retry에 의존하지 않는다. |
| `provider_output_truncated` | Probe나 planner 응답이 출력 token 예산에서 잘렸다. Reasoning을 많이 쓰는 model은 최종 JSON 전에 예산을 소진할 수 있으므로 model의 thinking 설정이나 profile의 출력 예산을 조정한다. Rate limit이 아니므로 재시도로 해결되지 않는다. |
| `proposal_rejected.*` | 뒤의 class가 Core가 제안을 거부한 이유다. `capability_unavailable`/`capability_unsupported`는 허용하지 않은 capability 선택, `invocation_invalid`는 scope 밖 인자나 스키마 위반, `tool_call_budget_exhausted`는 예산 소진이다. Class는 Core 판정이며 model 출력 원문이 아니다. |
| `configuration_mismatch` | 원래 workspace, file/directory scope, executable와 model profile binding으로 resume한다. 자동 대체하지 말고 필요하면 새 Run을 시작한다. |
| `model_call_unknown` 또는 `effect_outcome_unknown` | 불확정 작업을 자동 반복하지 않는다. `/status`와 `/resume`의 고정 진단을 확인하고 외부 상태를 별도로 검증한다. |

지원 요청에는 `xgeny --version`, OS/architecture, 설치 채널, 종료 코드와 고정된 오류 코드만 우선 제공한다.
API key, endpoint 전체 URL, prompt, model 원문 응답, source, process stdout/stderr, state DB와 Run ID는 공개
issue에 첨부하지 않는다. 일반 버그는
[GitHub Issues](https://github.com/PlateerLab/xgeny-cli/issues), 보안 취약점은 공개 issue가 아닌
[비공개 취약점 신고](https://github.com/PlateerLab/xgeny-cli/security/advisories/new)를 사용한다. 재현용
project는 민감정보가 없는 최소 fixture로 새로 만든다. 세부 범위는 [보안 정책](../SECURITY.md)을 따른다.

## 보안 경계

- XGENy는 model egress, read, write와 execute를 독립적으로 승인한다. 한 승인이 다른 권한을 포함하지 않는다.
- `process.execute`는 shell injection을 줄이지만 OS sandbox가 아니다. 허용한 compiler, package manager와
  child process는 현재 사용자 권한으로 project code를 실행한다.
- Run state에는 goal과 bounded tool output이 포함될 수 있으므로 state root 전체를 민감 데이터로 취급한다.
- SQLite는 binary에 내장된 local library다. SQLite server나 별도 database 설치가 필요하지 않다.
- 전송 여부가 불확정한 model/effect는 Unknown으로 보존하며 자동 replay하지 않는다.
- macOS/Windows RC3 binary에는 OS code signing/notarization이 없다. Checksum과 GitHub attestation은
  전송 무결성과 build provenance를 확인하지만 OS publisher 서명을 대신하지 않는다.
