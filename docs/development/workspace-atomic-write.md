# Workspace atomic write

`xgeny.fs/write-atomic@1.0.0`은 `--allow-dir` 아래의 UTF-8 file 하나를 create 또는 atomic replace한다.
설계·비보장·복구 경계는 [ADR-0025](../adr/0025-capability-confined-atomic-write.md)를 따른다.
기존 file의 작은 exact edit는 같은 commit primitive를 사용하는
[Workspace apply patch](workspace-apply-patch.md)를 참고한다.

## 사용

```bash
xgeny run \
  --workspace . \
  --allow-dir src \
  --base-url "$XGENY_OPENAI_BASE_URL" \
  --model "$XGENY_OPENAI_MODEL" \
  --allow-remote-model-egress \
  --allow-read \
  --allow-write \
  'src 아래 구현을 확인하고 필요한 파일을 안전하게 수정해줘.'
```

세 동의는 독립적이다.

| Flag | 허용 경계 |
| --- | --- |
| `--allow-remote-model-egress` | 새 model request |
| `--allow-read` | allow-file/allow-dir 안의 one-shot read |
| `--allow-write` | allow-dir 안의 one-shot atomic write |

Flag를 빼면 action 직전에 pause한다. Write 계획 뒤에는 다음처럼 local I/O만 먼저 승인할 수 있다.

```bash
xgeny resume <run-id> \
  --workspace . \
  --allow-dir src \
  --allow-write
```

Write가 끝나고 다음 model turn이 필요하면 `remote_model_egress_consent_required`로 다시 멈춘다. 같은 Run을
동일 workspace/catalog와 endpoint로 `--allow-remote-model-egress`를 붙여 이어간다.

## Precondition

- 새 file: `expectedDigest: null`; 이미 존재하면 conflict다.
- 기존 file: 앞선 `read-text` output의 exact digest; 그 뒤 bytes가 바뀌면 conflict다.
- 대상 bytes가 이미 desired content와 같으면 `changed=false` 성공이다.

Content는 최대 64 KiB다. Parent directory는 미리 존재해야 한다. `--allow-file`만으로 write 권한을 얻을
수 없고 symlink, junction/reparse, directory와 special file은 target이 될 수 없다.

Approval pause를 복원하기 위해 path/content/digest는 private `materials.sqlite3`에 저장된다. Manifest,
status/error, Receipt와 write tool output에는 content를 넣지 않는다. Local state는 암호화되지 않으므로
`XGENY_STATE_HOME` 전체를 source code와 같은 민감도로 관리한다.

## 검증

```bash
cargo test --locked -p xgeny-adapter-filesystem --all-targets
cargo test --locked -p xgeny-cli --test workspace_discovery
cargo test --locked -p xgeny-runtime --test direct_executor
cargo test --locked -p xgeny-local-store
cargo run --locked --quiet -p xgeny-cli -- protocol check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

PR CI는 Linux x86/ARM, macOS x86/ARM, Windows에서 전체 tests와 filesystem adapter clippy를 실행한다.
Windows의 existing-file replace와 junction 차단은 Windows native runner에서 검증한다.
