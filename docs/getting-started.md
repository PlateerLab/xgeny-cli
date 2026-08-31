# XGENy 설치와 첫 실행

이 문서는 release binary 하나를 설치하고 OpenAI-compatible model에 연결해 현재 공개
`read-text` capability를 실행하는 최소 절차다. 소스 개발 절차와 달리 Rust, C compiler, Python,
Node.js, SQLite 실행 파일, Docker 또는 XGENy state용 별도 daemon이 필요하지 않다. 사용할 model
endpoint 자체는 로컬 또는 원격에 별도로 실행 중이어야 한다.

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

Installer를 먼저 파일로 받은 뒤 검토·실행한다. Installer는 shell profile이나 user PATH를 자동으로
수정하지 않고 관리자 권한을 요청하지 않는다.

Linux/macOS:

```bash
installer=$(mktemp "${TMPDIR:-/tmp}/xgeny-installer.XXXXXX")
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo "$installer" \
  https://github.com/PlateerLab/xgeny-cli/releases/latest/download/xgeny-installer.sh
sh "$installer"
rm -f "$installer"
export PATH="$HOME/.local/bin:$PATH"
```

Windows PowerShell:

```powershell
$installer = Join-Path ([System.IO.Path]::GetTempPath()) "xgeny-installer-$([Guid]::NewGuid().ToString('N')).ps1"
curl.exe -q --proto "=https" --proto-redir "=https" --tlsv1.2 --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo $installer "https://github.com/PlateerLab/xgeny-cli/releases/latest/download/xgeny-installer.ps1"
if ($LASTEXITCODE -ne 0) { throw "XGENy installer download failed" }
& $installer
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
  https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0/xgeny-installer.sh
sh "$installer" --version v0.1.0
rm -f "$installer"
```

```powershell
$installer = Join-Path ([System.IO.Path]::GetTempPath()) "xgeny-installer-$([Guid]::NewGuid().ToString('N')).ps1"
curl.exe -q --proto "=https" --proto-redir "=https" --tlsv1.2 --connect-timeout 15 --max-time 60 --max-filesize 1048576 -fsSLo $installer "https://github.com/PlateerLab/xgeny-cli/releases/download/v0.1.0/xgeny-installer.ps1"
if ($LASTEXITCODE -ne 0) { throw "XGENy installer download failed" }
& $installer -Version v0.1.0
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

로컬 vLLM 예시:

```bash
export XGENY_OPENAI_BASE_URL=http://127.0.0.1:8000/v1
export XGENY_OPENAI_MODEL=qwen3.8-27b
# 선택: 생략하면 model ID와 같은 identity를 사용한다.
export XGENY_OPENAI_TOKENIZER=Qwen/Qwen3.8-27B-FP8
xgeny model check
```

Windows PowerShell에서는 같은 설정을 다음처럼 지정한다.

```powershell
$env:XGENY_OPENAI_BASE_URL = "http://127.0.0.1:8000/v1"
$env:XGENY_OPENAI_MODEL = "qwen3.8-27b"
$env:XGENY_OPENAI_TOKENIZER = "Qwen/Qwen3.8-27B-FP8"
xgeny model check
```

Loopback HTTP에는 ambient `XGENY_OPENAI_API_KEY`가 있더라도 전송하지 않는다.

원격 provider는 HTTPS를 사용하고 key는 secret manager 또는 현재 process environment로만 주입한다.
`--api-key` 옵션과 plaintext key config file은 없다.

```bash
export XGENY_OPENAI_BASE_URL=https://provider.example/v1
export XGENY_OPENAI_MODEL=served-model-id
# 셸의 secret manager에서 현재 process로 주입한다. 값 자체를 명령행에 쓰지 않는다.
export XGENY_OPENAI_API_KEY="$(secret-manager-command)"
xgeny model check
```

Windows PowerShell에서 대화형 입력이 필요하면 plaintext CLI argument 대신 다음처럼 현재 process의
environment에만 둔다.

```powershell
$secureKey = Read-Host "API key" -AsSecureString
$keyPointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secureKey)
try {
    $env:XGENY_OPENAI_API_KEY = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($keyPointer)
} finally {
    [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($keyPointer)
}
xgeny model check
```

`xgeny model check`는 사용자가 명시적으로 실행할 때만 현재 endpoint에 bounded
`GET /v1/models` 한 번을 보낸다. Prompt와 inference 요청은 보내지 않고 workspace, SQLite 또는 Run
state를 만들지 않는다. Redirect, retry와 ambient proxy는 사용하지 않으며 loopback HTTP에는 ambient
API key를 보내지 않는다. 일부 compatible server는 catalog를 제공하지 않거나 alias를 다르게
광고하므로 `model_not_advertised`는
strict generation 비호환 확정이 아니라 catalog에서 확인되지 않았다는 뜻이다. PASS도 catalog 접근과
그 endpoint가 강제한 인증만 뜻한다. Chat Completions의 인증·권한, strict JSON Schema와 exact
response model 호환성은 첫 `run`이 durable reservation 뒤 검증한다.
전송 여부가 불확정한 inference transport 실패는 자동 재시도하지 않고 해당 Run을
`model_call_unknown` recovery 상태로 남길 수 있다.

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

Windows PowerShell에서도 동일한 환경변수를 설정한 뒤 다음처럼 실행한다.

```powershell
xgeny run `
  --allow-file README.md `
  --allow-remote-model-egress `
  --allow-read `
  "README를 읽고 핵심을 요약해줘."
```

상태가 pause되면 stderr의 `run_id`를 사용한다. 미완료 Run은 workspace와 같은 allow-file catalog를
다시 제공하고 필요한 동의를 명시한다. Base URL은 위 environment에서 다시 읽는다.

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
현재 공개 capability는 allow-list 기반 UTF-8 file read 하나다. 일반 shell/process, file write,
browser와 MCP는 후속 범위이므로 현 상태를 Claude Code/Codex 전체 기능과 동일하다고 해석하지 않는다.

## 삭제

Installer는 runtime state를 삭제하지 않는다. Binary만 제거하려면 설치한 exact regular file을 지운다.

```bash
rm "$HOME/.local/bin/xgeny"
```

```powershell
Remove-Item -LiteralPath "$env:LOCALAPPDATA\XGENy\bin\xgeny.exe"
```

Run state는 별도로 보존된다. Linux는 `$XDG_STATE_HOME/xgeny` 또는 `$HOME/.local/state/xgeny`,
macOS는 `$HOME/Library/Application Support/XGENy`, Windows는 `%LOCALAPPDATA%\XGENy`를 사용한다.
State 삭제는 Run 기록과 durable recovery 정보를 잃으므로 uninstall에 자동 포함하지 않는다.
