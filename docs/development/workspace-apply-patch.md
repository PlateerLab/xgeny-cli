# Workspace apply patch

`xgeny.fs/apply-patch@1.0.0`은 앞서 읽은 기존 UTF-8 file에 exact contextual edit를 적용하고 전체 결과를
한 번에 atomic commit한다. 정본 의미와 비보장은
[ADR-0026](../adr/0026-strict-single-file-atomic-patch.md)을 따른다.

## 사용자 흐름

```bash
xgeny run \
  --workspace . \
  --allow-dir src \
  --base-url "$XGENY_OPENAI_BASE_URL" \
  --model "$XGENY_OPENAI_MODEL" \
  --allow-remote-model-egress \
  --allow-read \
  --allow-write \
  '구현을 읽고 필요한 부분만 수정해줘.'
```

모델은 먼저 `read-text`에서 받은 exact digest와 content를 사용해 patch를 계획한다. 입력 예시는 다음과
같다.

```json
{
  "path": "src/lib.rs",
  "expectedDigest": "sha256:<read-text digest>",
  "edits": [
    {"oldText": "const LIMIT: usize = 8;", "newText": "const LIMIT: usize = 16;"}
  ]
}
```

`--allow-write`가 없으면 파일을 바꾸기 전에 pause한다. 동일 workspace와 allow-dir catalog를 제공해 모델
접속 없이 local patch만 먼저 재개할 수 있다.

```bash
xgeny resume <run-id> \
  --workspace . \
  --allow-dir src \
  --allow-write
```

그 뒤 새 model turn이 필요하면 별도로 `--allow-remote-model-egress`를 명시한다.

## Strict match 규칙

- 기존 file과 exact non-null digest만 허용한다. 새 file은 `write-atomic`을 사용한다.
- edit는 1..=32개, old/new text 전체는 최대 64 KiB다.
- 각 oldText는 원본에 byte-exact하게 정확히 한 번 있어야 한다.
- edit range끼리 겹치면 전체 patch를 거부한다.
- fuzzy match, line-number 보정, regex와 부분 적용은 없다.
- 원본 또는 결과 file이 64 KiB를 넘으면 거부한다.

성공 output에는 path/digest/byte size/changed/edit count만 들어간다. Raw edit는 process 재개를 위해 private
`materials.sqlite3`에 저장되며 Receipt나 tool output에는 복사하지 않는다. State root는 source와 같은
민감도로 관리한다.

## 검증

```bash
cargo test --locked -p xgeny-adapter-filesystem --all-targets
cargo test --locked -p xgeny-cli --test workspace_discovery
cargo run --locked --quiet -p xgeny-cli -- protocol check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --locked --release -p xgeny-cli
```

PR CI는 Linux x86/ARM, macOS x86/ARM, Windows native runner에서 공유 atomic commit과 전체 CLI 회귀를
검증한다.
