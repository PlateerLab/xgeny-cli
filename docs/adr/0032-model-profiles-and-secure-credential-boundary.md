# ADR-0032: 모델 프로필과 OS 보안 저장소를 분리한다

- 상태: Accepted
- 날짜: 2026-09-02
- 적용 범위: CLI 모델 온보딩, OpenAI-compatible provider 설정, credential 저장

## 배경

기존 공개 CLI는 `run`과 `model check`마다 base URL과 model을 argument 또는 환경변수로 공급해야 했다.
이는 자동화에는 명시적이지만, 사용자가 설치 뒤 `xgeny`를 바로 실행하는 대화형 제품의 최초 경험으로는
적합하지 않다. 반대로 API key를 일반 JSON, embedded SQLite, WorkGraph 또는 Run manifest에 저장하면
백업·진단·도구 출력 경계에 secret이 섞인다. Linux headless server에는 데스크톱 Secret Service가 없을
수 있으므로 보안 저장소가 없을 때의 동작도 명시해야 한다.

## 결정

### 1. 일반 설정과 credential은 서로 다른 저장소를 쓴다

`model-profiles.json`에는 다음 비밀이 아닌 값만 저장한다.

- profile name
- OpenAI-compatible base URL
- served model ID
- tokenizer/profile identity
- opaque credential reference
- active profile name

파일은 platform config directory 아래 app-owned private directory에 두고 Unix에서는 directory `0700`,
file `0600`을 검증한다. 최종 path의 symbolic link/reparse point, permissive permission, oversized file,
unknown field, duplicate profile와 invalid provider configuration은 거부한다. 갱신은 같은 directory의 private
temporary file을 sync한 뒤 rename하고, 직전에 읽은 digest가 달라졌으면 다른 process의 변경을 덮어쓰지
않는다. Profile과 credential을 함께 바꾸는 command는 OS file lock으로 직렬화하고, network 검증 뒤 lock을
얻었을 때 최초 관찰 revision이 바뀌었으면 credential을 건드리지 않고 다시 시도하도록 종료한다.

Raw API key는 이 파일, Run SQLite, material SQLite, WorkGraph, manifest, receipt, log, error, digest input에
넣지 않는다. Profile debug와 CLI 출력은 endpoint, credential reference와 secret을 노출하지 않는다.

### 2. 데스크톱 credential은 OS 사용자 보안 저장소만 사용한다

지원 backend는 다음과 같다.

- macOS: Keychain Services
- Windows: Credential Manager
- Linux/Unix desktop: freedesktop Secret Service

일반 설정에는 무작위 opaque reference만 저장한다. 새 credential을 저장한 뒤 profile commit이 실패하면
새 entry를 삭제한다. 기존 credential 교체·logout·profile 삭제는 old entry를 먼저 삭제해 orphan secret을
남기지 않는 쪽으로 실패한다. 이 순서에서 profile commit이 실패하면 기존 인증을 다시 입력해야 할 수
있지만 secret이 일반 파일이나 참조 불가능한 보안 entry로 남는 것보다 안전하다.

OS 보안 저장소가 없거나 잠겨 있으면 평문 파일, SQLite, home directory key file로 자동 fallback하지
않는다. `credential_store_unavailable`로 실패하고 사용자는 현재 process 환경변수 또는
`--token-stdin`을 사용할 수 있다.

### 3. 설정과 credential 해석 우선순위를 고정한다

일반 설정은 field별로 다음 순서다.

1. 명시적 CLI option
2. `XGENY_OPENAI_BASE_URL`, `XGENY_OPENAI_MODEL`, `XGENY_OPENAI_TOKENIZER`
3. `--profile`, `XGENY_MODEL_PROFILE`, active profile

Credential은 `--token-stdin`, `XGENY_OPENAI_API_KEY`, 선택 profile의 OS 보안 entry 순서다. Stored
credential은 최종 resolved base URL이 profile의 URL과 byte-exact하게 같을 때만 사용한다. 명시적 URL
override로 credential을 다른 host에 보내지 않는다. Plaintext HTTP는 literal loopback만 허용하며 ambient
환경변수와 stored credential을 읽거나 전송하지 않는다. Plaintext endpoint에서 명시한 `--token-stdin`은
오류로 거부한다.

환경변수와 `--token-stdin` 값은 기본적으로 현재 invocation에만 존재한다. Headless `model setup`에서
`--store-token`을 명시한 경우에만 OS 보안 저장소로 옮긴다. Interactive hidden prompt로 입력한 값은
사용자가 수행한 저장형 온보딩으로 보고 OS 보안 저장소에 저장한다.

### 4. 온보딩은 catalog와 실제 inference를 별도로 검증한다

`xgeny model setup`은 Run state를 만들지 않고 다음 두 network request를 순서대로 수행한다.

1. bounded `GET /v1/models`: model 목록을 bounded하게 decode하고 duplicate/invalid ID를 거부한다.
2. bounded `POST /v1/chat/completions`: exact model, non-streaming Chat Completions envelope와 strict
   `json_schema` 응답 `{"status":"ok"}`를 검증한다.

Redirect와 retry는 없다. Raw provider body와 endpoint는 오류에 포함하지 않는다. Profile과 credential은 두
검증이 모두 성공한 뒤에만 변경한다. `model check` 기본 동작은 기존 자동화 호환성을 위해 catalog GET
하나를 유지하고, `--compatibility`를 주면 같은 inference probe를 추가한다.

Inference probe는 transport와 structured-output 호환성을 확인하지만 전체 coding loop 품질을 증명하지
않는다. 실제 workspace 탐색·수정·test/build·수정 반복은 별도 live E2E gate가 증명한다.

## 플랫폼 제약

- Linux server에 Secret Service session bus가 없으면 secure persistence는 사용할 수 없다. 환경변수 또는
  `--token-stdin`은 계속 지원한다.
- macOS Keychain과 Windows Credential Manager는 현재 로그인한 OS 사용자 문맥을 사용하며 OS가 unlock
  또는 access prompt를 표시할 수 있다.
- CI는 실제 사용자 보안 저장소에 secret을 쓰지 않는다. Credential port는 in-memory fake로 lifecycle을
  검증하고, packaged platform smoke는 backend를 compile/link한다. 실제 backend 저장은 release 전 각
  desktop OS의 수동 보안 gate로 별도 확인한다.

## 검증

- profile file round-trip, private permission, symlink, unknown field, duplicate, concurrent update regression
- credential fake의 put/get/delete lifecycle과 debug redaction
- loopback real HTTP로 catalog → strict compatibility probe → active-profile `run` 수직 E2E
- CLI option > environment > profile 기존 precedence 회귀
- loopback ambient credential 미전송과 `--token-stdin` plaintext 거부
- headless stdin token의 비영속·stdout/stderr redaction
- Linux, macOS, Windows 전체 test와 native release build/audit

## 대안

API key를 profile JSON에 mode `0600`으로 저장하는 방식은 filesystem copy와 backup에서 raw secret이
확산되므로 채택하지 않는다. 자체 master password와 encrypted vault를 구현하는 방식은 key management와
recovery 제품을 새로 만드는 범위이므로 이번 단계에서 채택하지 않는다. 모든 사용자가 환경변수만 쓰게
하는 방식은 대화형 설치 경험을 해결하지 못하므로 headless fallback으로만 유지한다.
