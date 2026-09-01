# 모델 프로필과 최초 온보딩

`xgeny model setup`은 모델 목록 조회와 실제 Chat Completions compatibility probe를 통과한 profile만
활성화한다. Workspace나 Run SQLite는 만들지 않는다.

## 대화형 설정

터미널에서 실행하면 누락된 URL, API key와 model을 순서대로 묻는다. API key 입력은 화면에 표시되지
않으며 HTTPS provider에서만 허용된다.

```bash
xgeny model setup
xgeny model list
xgeny model check --compatibility
```

설정 뒤에는 URL과 model 환경변수 없이 기존 명령을 실행할 수 있다.

```bash
xgeny run \
  --workspace . \
  --allow-dir . \
  --allow-remote-model-egress \
  '프로젝트 구조를 확인해줘.'
```

## Headless와 CI

환경변수와 stdin token은 기본적으로 저장하지 않는다. stdin 값은 command argument나 process list에
나타나지 않으며 첫 줄만 bounded하게 읽는다.

```bash
secret-manager-command | xgeny model setup \
  --name qwen-xgen \
  --base-url https://provider.example/v1 \
  --model served-model-id \
  --token-stdin
```

OS 보안 저장소에 명시적으로 보존하려면 `--store-token`을 함께 사용한다.

```bash
secret-manager-command | xgeny model setup \
  --name qwen-xgen \
  --base-url https://provider.example/v1 \
  --model served-model-id \
  --token-stdin \
  --store-token
```

Linux headless host에 Secret Service가 없으면 `credential_store_unavailable`이 정상적인 fail-closed
결과다. `--store-token`을 빼고 실행 때마다 secret manager의 environment 또는 stdin을 주입한다. 평문
credential file fallback은 없다.

## Profile 관리

```bash
xgeny model list
xgeny model use qwen-xgen
xgeny model check
xgeny model check --compatibility
xgeny model logout qwen-xgen
xgeny model remove qwen-xgen
```

`logout`은 일반 model 설정은 유지하고 OS 보안 저장소의 credential만 지운다. `remove`는 둘 다 지운다.
명시적 option, 환경변수, selected/active profile 순서로 일반 설정을 해석한다. Credential은
`--token-stdin`, `XGENY_OPENAI_API_KEY`, profile secure store 순서다. Profile credential은 profile URL과
최종 URL이 정확히 같을 때만 사용한다.

`model check`는 기본적으로 기존 계약인 catalog GET만 보낸다. `--compatibility`는 strict JSON Schema
Chat Completions POST를 한 번 추가한다. `model setup`은 profile commit 전에 두 요청을 항상 수행한다.

## 로컬 검증

```bash
cargo test -p xgeny-provider-openai --lib
cargo test -p xgeny-cli --lib
cargo test -p xgeny-cli --test environment_onboarding
cargo test -p xgeny-cli --test model_profiles
cargo clippy --workspace --all-targets -- -D warnings
```

실제 desktop keychain 저장은 사용자 credential을 CI에 넣지 않고 macOS, Windows, Linux desktop에서
수동 release gate로 확인한다. 일반 CI는 in-memory credential fake와 platform compile/link, no-secret
file/log regression을 담당한다. 설계와 실패 순서는 [ADR-0032](../adr/0032-model-profiles-and-secure-credential-boundary.md)를 따른다.
