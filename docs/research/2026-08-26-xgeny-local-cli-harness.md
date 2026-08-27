# XGENy 로컬 CLI 하네스 조사 메모

- 조사일: 2026-08-26 (Asia/Seoul)
- 상태: 아키텍처 판단용 연구 메모. 제품 코드나 배포 설정은 변경하지 않음
- XGEN 런타임 조사 기준: `PlateerLab/xgen-agent-runtime`
- 기준 커밋: `f7181d900effe98fde30c43fff710374c1e56a58`
- 핵심 질문: Claude Code, Codex, Qwen Code 같은 로컬 에이전트 하네스를 XGENy로 만들 때 무엇을 설치하고, 어디까지 포함하며, 기존 XGEN 모듈 중 무엇을 재사용할 것인가

> **후속 결정 알림 (2026-08-27)**
> 이 문서는 초기 조사 기록으로 보존한다. 이후 최신 XGEN·Connector·Runtime을 다시 대조하면서
> “기존 XGEN 에이전트 코어를 공유한다”, “서버 연계는 나중의 선택 기능이다”라는 표현은 수정됐다.
> 현재 정본은 [XGENy 독립 코어와 XGEN 무중단 진화](../architecture/2026-08-27-xgeny-xgen-evolution.md)다.
> XGENy 코어는 XGEN 인프라나 Python 런타임에 의존하지 않으며, XGEN 호환 통합 E2E는 MVP 필수다.

## 1. 결론

XGENy의 사용자 설치 계약은 다음 한 문장이어야 한다.

> 사용자는 `xgeny` 하나만 설치한다. SQLite, PostgreSQL, Qdrant, Redis, Docker, Kubernetes, 별도 데몬, Python 또는 Node.js를 따로 설치하지 않는다.

SQLite를 내부 구현에 사용한다는 것과 사용자가 SQLite를 설치한다는 것은 다른 문제다. Codex, Goose, OpenCode는 내부 상태에 임베디드 SQLite를 사용하지만 사용자는 해당 데이터베이스 서버를 설치하지 않는다. 다만 XGENy 초기 버전은 요구사항과 운영 복잡도를 고려해 SQLite 없이 JSONL과 Markdown으로 시작하는 편이 낫다.

프로젝트 경계에 대한 결론은 다음과 같다.

1. XGENy CLI는 별도 제품·저장소로 두는 것이 맞다.
2. 에이전트 루프까지 새로 복제해서는 안 된다.
3. 현재 세 갈래로 나뉜 XGEN 하네스 구현에서 하나의 최소 코어를 지정하거나 추출해야 한다.
4. XGENy는 그 코어와 얇은 로컬 호스트 어댑터를 조합한다.
5. 현재 `xgen-agent-runtime` 또는 `xgen-sdk` 전체를 XGENy의 기본 의존성으로 설치해서는 안 된다.
6. XGEN 서버, 원격 샌드박스, 고급 메모리는 선택형 어댑터로 나중에 연결한다.

## 2. 설치 무게에 대한 판단

### 2.1 SQLite는 설치 부담의 본질이 아니다

- Python은 표준 라이브러리 `sqlite3`를 제공한다.
- Rust/Bun 기반 CLI는 SQLite 라이브러리를 바이너리 안에 포함할 수 있다.
- 이 경우 사용자에게 별도 데이터베이스 프로세스나 관리 작업이 생기지 않는다.
- 실제 부담은 스키마 버전, 마이그레이션, 동시 쓰기, 손상 복구, 백업·인덱스 정책을 제품이 책임져야 한다는 점이다.

따라서 “SQLite를 절대 쓰지 않는다”가 원칙은 아니다. 원칙은 “사용자 설치 항목과 운영 서비스를 늘리지 않는다”이다.

### 2.2 현재 `xgen-agent-runtime`이 무거운 실제 이유

최신 `pyproject.toml`은 다음 기능을 모두 필수 의존성으로 흡수한다.

- Anthropic 및 AWS Bedrock
- OpenAI, Google GenAI, Google 인증
- MCP, 웹소켓, YAML, JSON Schema
- PostgreSQL, pgvector, Qdrant
- 웹 검색, 브라우저 자동화, 문서 편집, SSH, cron
- NumPy 기반 메모리·벡터 처리

진단 환경에서 개발 의존성을 제외해도 약 180개 이상의 패키지가 해석됐다. SQLite 때문이 아니라 선택 기능이 전부 코어에 들어가 있기 때문이다. 서버 런타임에는 이 정책의 이유가 있을 수 있지만, 로컬 CLI의 첫 설치면으로는 과하다.

### 2.3 XGENy 배포 원칙

- macOS, Linux, Windows별 단일 실행 파일 또는 그에 준하는 네이티브 패키지를 제공한다.
- 최초 실행 시 사용자 데이터 디렉터리와 파일만 자동 생성한다.
- API 키나 XGEN 로그인 같은 자격 설정 외에 시스템 패키지 설치를 요구하지 않는다.
- Docker 기반 샌드박스는 명시적으로 켠 사용자에게만 선택지를 제공한다.
- 브라우저, Office 문서, SSH, 원격 샌드박스는 플러그인 또는 서버 제공 기능으로 분리한다.
- 기본 설치가 네트워크 데몬을 띄우지 않는다.

## 3. 공개 하네스 최신 소스 비교

| 프로젝트 | 조사 커밋 | 설치·구조 | 로컬 상태 | XGENy에 가져올 점 |
|---|---|---|---|---|
| Qwen Code | `22df1422` (v0.22.2) | Node 22+, CLI와 UI 독립 코어 분리 | 세션 JSONL, Markdown 메모리 | 직접 실행 우선, core/UI 경계, writer lease |
| Gemini CLI | `64b5b79a` | Node 20+, 터미널 하네스 | 프로젝트별 세션 파일 | 명시적 정책 엔진, 자동 메모리 승인 흐름 |
| Codex | `f5420174` | Rust 네이티브 CLI | JSONL rollout + 임베디드 SQLite 인덱스 | 네이티브 배포, 권한과 샌드박스 분리, headless JSONL |
| Goose | `9a05f020` | Rust 네이티브, MCP 중심 | 임베디드 SQLite | 확장 중심 구조, 장기 세션 관리 사례 |
| OpenCode | `a0f36c9d` | Bun, 로컬 server/client 계약 | 임베디드 SQLite | 여러 UI가 필요해질 때 API-first 경계 |
| Aider | `5dc9490b` | Python CLI | Markdown 대화 기록 | 얇은 코딩 CLI의 하한선 |

### 3.1 Qwen Code에서 우선 참고할 구조

Qwen Code는 `packages/cli`를 표현 계층으로, `packages/core`를 UI 독립 에이전트 루프로 분리한다. 기본 터미널과 headless 모드는 코어를 직접 호출하며, ACP나 `qwen serve` 같은 장기 실행·다중 클라이언트 경로는 별도로 둔다.

XGENy도 초기에는 다음처럼 직접 경로를 택한다.

```text
xgeny CLI/TUI
    -> local host adapter
        -> shared agent core
            -> provider / tools / session store
```

초기부터 다음처럼 만들 필요는 없다.

```text
xgeny UI -> HTTP daemon -> workspace server -> agent core
```

Qwen Code의 세션 저장 방식도 초기 XGENy에 적합하다.

- 프로젝트별 `chats/<session-id>.jsonl`
- append-only 이벤트 기록
- 동일 세션에 여러 writer가 붙지 못하도록 lease 적용
- 심볼릭 링크와 경로 탈출 방어
- 프로젝트·사용자 지침은 Markdown으로 계층화
- 자동 메모리도 사용자 디렉터리의 Markdown으로 관리

반면 `qwen serve`, ACP, 다중 클라이언트, 장기 워크스페이스 런타임은 XGENy 1차 범위에서 제외한다.

### 3.2 Claude Code에서 참고할 사용자 경험

Claude Code의 네이티브 설치는 플랫폼별 바이너리를 설치한다. 사용자가 데이터베이스나 Node 런타임을 직접 구성하는 방식이 아니다. 프로젝트·사용자 지침은 `CLAUDE.md`와 Markdown 자동 메모리로 노출된다. 파일 수정과 명령 실행은 승인 정책 및 샌드박스와 결합한다.

Claude Code는 폐쇄 소스이므로 내부 저장 엔진이 무엇인지 추정해서 설계 근거로 사용하지 않는다. 참고 대상은 다음 사용자 계약이다.

- 한 번의 CLI 설치
- 프로젝트 안에서 바로 실행
- 사람이 읽고 수정할 수 있는 지침·메모리
- 읽기, 쓰기, 명령 실행의 단계별 권한
- 기본 작업공간 경계

### 3.3 Codex, Goose, OpenCode의 SQLite가 의미하는 것

이 세 도구는 SQLite를 내부 상태·인덱스에 쓴다. 이것은 로컬 에이전트가 성장하면 다음 요구가 생긴다는 증거다.

- 세션 목록과 검색
- 이벤트 순서 및 페이지네이션
- 재시작 후 빠른 복원
- 여러 UI·프로세스의 조회
- 스키마가 있는 메타데이터

하지만 DB를 일찍 도입하면 마이그레이션과 동시성 정책을 함께 확정해야 한다. Goose의 실제 변경 이력에도 DB 마이그레이션과 동시 접근 문제가 반복된다. XGENy가 세션 수와 다중 프로세스 요구를 확인하기 전에는 JSONL을 정본으로 두는 편이 안전하다.

### 3.4 Gemini CLI에서 참고할 권한·자동 메모리 정책

Gemini CLI는 `allow`, `deny`, `ask_user` 정책과 실행 모드를 분리한다. 자동 메모리는 실험 기능이며 기본적으로 꺼져 있고, 과거 세션에서 생성한 변경안을 자동 적용하지 않고 승인함에 둔다.

XGENy도 자동 메모리 추출을 초기 기본값으로 두지 않는다. 첫 버전은 `/remember`, `/forget` 같은 명시적 동작만 지원하고, 백그라운드 모델 호출이나 무승인 메모리 변경은 미룬다.

## 4. 내부 XGEN 모듈 전면 확인

| 저장소 | 조사 커밋 | 현재 역할 | XGENy 판단 |
|---|---|---|---|
| xgen-agent-runtime | `f7181d9` | 21-stage 서버 중심 에이전트 런타임 | 전체 설치 금지, 코어 추출 후보 |
| xgen-harness-executor | `91738f9` | 가벼운 Python 10-stage 하네스 | 최소 코어의 유력한 재료, 정본 여부 선결 |
| xgen-sdk | `8d9c2fc` | 플랫폼 SDK와 vendored 하네스 | CLI 의존성으로 너무 무거움 |
| xgen-mcp-harness | `140d28d` | 원격 XGEN MCP 워크플로우 브리지 | 향후 서버 연계 어댑터 참고 |
| xgen-sandbox | `c4ab7d7` | Kubernetes 원격 샌드박스 | 선택형 enterprise/remote backend |
| xgen-agent-memory | `c7abdba` | SQLite 기반 고급 로컬 메모리 | 후속 선택 플러그인 후보 |
| harnesser | `a0d9a0c` | 코딩 테스트·평가 플랫폼 | 이름만 유사, CLI 구조와 무관 |

### 4.1 중복 하네스 문제

현재 확인된 에이전트 루프는 최소 세 계열이다.

1. `xgen-agent-runtime`의 21-stage 파이프라인
2. `xgen-harness-executor`의 10-stage Python 하네스
3. `xgen-sdk`에 vendoring된 `xgen_sdk.harness`

`xgen-sdk` 문서는 vendored 사본을 정본이라고 설명하지만, 외부 `xgen-harness-executor`도 이후 별도 변경됐고 두 트리에는 상당한 파일·구현 차이가 있다. Node 엔진 문서도 10단계와 13단계, Python 동등성에 관해 서로 맞지 않는 설명이 있다.

따라서 XGENy가 이 중 하나를 무검증 복사하거나 새 루프를 만들면 동작 계약이 더 갈라진다. 먼저 다음 중 하나를 조직 차원에서 결정해야 한다.

- `xgen-agent-core` 같은 가벼운 공용 패키지를 추출해 단일 정본으로 삼는다.
- 또는 기존 `xgen-harness-executor`를 정본으로 선언하고 SDK의 vendored 사본을 제거·생성물화한다.

장기적으로는 첫 번째가 더 안전하다. 공용 코어에는 다음만 남긴다.

- 이벤트·메시지 모델
- provider 요청·스트리밍 계약
- tool 호출 루프와 결과 계약
- 승인·권한 계약
- session store 인터페이스
- hook·guard 인터페이스
- 취소, 오류, 재시도 계약

서버 runner, 로컬 CLI, 원격 채널, 브라우저, 문서 도구, 벡터 메모리, PostgreSQL은 호스트나 어댑터에 남긴다.

### 4.2 `xgen-agent-runtime` 최신본의 CLI 차단 사항

최신 v4 계열은 에이전트 실행을 서버 runner로 일원화하면서 로컬 sidecar 구현을 삭제했다. 그런데 패키지 스크립트에는 아직 다음 진입점이 남아 있다.

```toml
xgen-agent-sidecar = "xgen_agent_runtime.host.sidecar:main"
```

대상 모듈은 존재하지 않는다. `host` 패키지 설명에도 로컬 호스트가 제공된다는 오래된 문구가 남아 있다. README는 Python 3.11+와 기능 extras를 안내하지만 실제 메타데이터는 Python 3.12+이고 기능 extras가 모두 core로 흡수됐다.

이는 XGENy 연구와 별개인 최신 런타임의 패키징·문서 결함이다. 지금 XGENy를 이 패키지 위에 바로 얹으면 설치 성공 여부와 실행 경로가 어긋난다. 별도 이슈로 수정하되, 이번 연구 메모에서는 제품 코드를 변경하지 않는다.

### 4.3 각 내부 모듈의 재사용 경계

#### xgen-harness-executor

장점:

- Python 3.10+이며 필수 의존성이 `httpx` 정도로 가볍다.
- tool source, provider, guard, compile 같은 분리가 이미 있다.
- 기본 파일 세션 저장소가 임시 파일과 `os.replace`로 원자적 교체를 한다.

한계:

- 콘솔 CLI 제품이 아니다.
- 세션 전체 JSON을 다시 쓰며 append-only log나 writer lock이 없다.
- subprocess + resource limit 방식의 sandbox는 보안 격리가 아니다.
- SDK vendored 사본과 어느 쪽이 정본인지 불명확하다.

결론: 최소 코어의 유력한 출발점이지만 그대로 XGENy 의존성으로 확정하지 않는다.

#### xgen-sdk

PostgreSQL, Redis, MinIO, FastAPI, MCP 등 플랫폼 기능을 한 번에 포함한다. 서버 SDK에는 유용하지만 로컬 CLI 기본 설치에 넣으면 목적과 무게가 맞지 않는다. XGENy는 SDK 전체가 아니라 정리된 하네스 코어 계약만 사용해야 한다.

#### xgen-mcp-harness

Node 표준 기능만으로 원격 XGEN 클러스터의 MCP 워크플로우를 stdio에 연결하는 얇은 브리지다. 로컬 에이전트 루프는 아니지만, 향후 XGEN 로그인과 원격 도구 노출 방식의 참고 구현으로 적합하다.

#### xgen-sandbox

Docker, Kind/Kubernetes, Helm을 사용하는 원격 격리 플랫폼이다. 강한 실행 격리가 필요한 조직 환경에서 선택할 수 있지만, 로컬 CLI 실행 전제 조건으로 두면 안 된다.

#### xgen-agent-memory

Python 표준 SQLite와 NumPy를 사용하는 단일 파일 메모리 엔진이며 DB 서버는 요구하지 않는다. 검색 인덱스는 지우고 재생성할 수 있게 설계돼 있다. 초기 XGENy의 명시적 Markdown 메모리보다 고급인 후속 플러그인으로 둔다.

## 5. 목표 아키텍처

```text
                         +----------------------+
                         | XGEN server / MCP API|
                         +----------+-----------+
                                    ^ optional
                                    |
+-------------+       +-------------+-------------+
| xgeny CLI   | ----> | local host adapter        |
| TUI/headless|       | config, paths, permissions|
+-------------+       +-------------+-------------+
                                    |
                         +----------v-----------+
                         | shared agent core    |
                         | loop/events/contracts|
                         +---+---------+--------+
                             |         |
                       +-----v--+  +---v----------------+
                       |provider|  |tools / MCP adapters|
                       +--------+  +--------------------+
                                    |
                         +----------v-----------+
                         | JSONL + Markdown     |
                         | local session/memory |
                         +----------------------+
```

XGEN 서버 연결은 로컬 상태 저장과 에이전트 루프에 스며들지 않고 adapter로 붙어야 한다. 로컬에서 완결된 경험을 제공하되, 나중에 다음 항목만 명시적으로 동기화한다.

- 사용자·조직 인증
- 원격 provider 또는 workflow 실행
- 조직 정책과 도구 카탈로그
- 선택한 세션·메모리의 업로드·동기화
- 원격 샌드박스 실행

서버에 접속하지 않아도 로컬 세션 조회·재개·삭제와 기본 코딩 도구가 동작해야 한다.

## 6. 초기 저장 설계

권장 기본 구조 예시는 다음과 같다.

```text
~/.xgeny/
  config.toml
  global/
    XGENY.md
    memory.md
  projects/
    <stable-project-id>/
      project.json
      sessions/
        <session-id>.jsonl
        <session-id>.lock
      memory/
        MEMORY.md
        topics/
```

필수 규칙:

- 세션 이벤트는 schema version을 가진 append-only JSONL이다.
- 한 세션에는 한 writer만 허용한다.
- 저장 전에 프로젝트 루트와 심볼릭 링크를 검증한다.
- 잘린 마지막 JSONL 레코드를 탐지하고 복구할 수 있어야 한다.
- 메모리는 사람이 직접 읽고 수정 가능한 Markdown이다.
- 세션 삭제와 메모리 삭제를 별도 동작으로 제공한다.
- 비밀 값과 전체 환경 변수는 세션에 저장하지 않는다.
- shell 출력과 tool 결과에는 크기 제한과 redaction hook을 둔다.

SQLite 도입 조건:

- 수천 개 이상 세션의 빠른 검색이 실제 요구가 됨
- 여러 UI나 프로세스가 동시에 같은 상태를 조회함
- 태그, 전문 검색, 정렬, 페이지네이션이 파일 스캔으로 느려짐
- 마이그레이션, 잠금, 백업, 복구 테스트를 릴리스 게이트로 운영할 수 있음

도입하더라도 SQLite는 제품에 포함하며, JSONL을 정본으로 유지하고 DB를 재생성 가능한 인덱스로 두는 방식을 우선 검토한다.

## 7. 범위

### 7.1 MVP에 포함

- 단일 `xgeny` 설치와 자동 업데이트 경로
- 대화형 CLI/TUI
- 자동화용 headless 모드와 JSONL 이벤트 출력
- 세션 생성, 목록, 재개, 삭제
- provider 추상화와 최소 1개 실제 provider
- 파일 읽기, 검색, 패치 편집, shell 실행
- `allow` / `ask` / `deny` 권한 규칙
- 작업공간 경계와 경로 탈출 방어
- 프로젝트·사용자 지침 Markdown
- 명시적 `/remember`, `/forget`
- 취소, timeout, 재시도, tool output 제한
- macOS/Linux/Windows 패키징 및 깨끗한 머신 E2E

### 7.2 초기 범위에서 제외

- 상시 daemon 또는 app server
- ACP 및 다중 UI·다중 클라이언트
- 자동·백그라운드 메모리 추출
- 벡터 DB, Qdrant, PostgreSQL
- 브라우저 엔진과 Office 문서 엔진 기본 번들
- Docker/Kubernetes 필수 샌드박스
- subagent, multi-agent, 채널 gateway
- 세션·메모리의 자동 서버 동기화
- 원격 컴퓨터 사용과 SSH 기본 제공

### 7.3 후속 선택 기능

- MCP stdio client와 XGEN 원격 workflow 연결
- 임베디드 SQLite 검색 인덱스
- `xgen-agent-memory` 기반 고급 메모리
- OS-native sandbox 또는 `xgen-sandbox` 원격 실행
- 데스크톱·IDE UI가 필요해질 때 local app-server 계약
- 조직 정책·도구 카탈로그·감사 이벤트 연계

## 8. 출시 전 검증 게이트

“개발 서버에서는 됐지만 배포판에서는 안 됨”을 막기 위해 소스 테스트만으로 완료 처리하지 않는다.

1. macOS Intel/Apple Silicon, Linux x64/arm64, Windows x64용 실제 산출물을 생성한다.
2. 개발 도구가 없는 깨끗한 VM에서 설치한다.
3. Python, Node, SQLite CLI, Docker가 없는 상태에서 `xgeny --version`과 첫 대화를 검증한다.
4. 한글·공백·긴 경로와 Windows 경로를 포함한 프로젝트에서 파일 도구를 검증한다.
5. 승인, 거부, timeout, Ctrl-C, 네트워크 끊김 후 세션 복구를 검증한다.
6. 동시에 같은 세션을 열었을 때 두 번째 writer가 안전하게 거부되는지 검증한다.
7. 설치, 업데이트, 다운그레이드, 제거 후 사용자 데이터 보존 정책을 검증한다.
8. 오래된 세션 schema fixture로 마이그레이션 또는 호환 읽기를 검증한다.
9. API 키, 환경 변수, 파일 내용이 로그에 예기치 않게 노출되지 않는지 검증한다.
10. 실제 XGEN 개발 서버와 선택형 연계를 별도 E2E로 검증하되, 서버 없이도 로컬 기본 기능이 통과해야 한다.

## 9. 즉시 필요한 후속 결정

구현 전에 다음 두 가지만 먼저 확정해야 한다.

1. `xgen-agent-runtime`, `xgen-harness-executor`, `xgen-sdk.harness` 중 어느 구현을 공용 코어의 기준으로 삼을 것인가.
2. XGENy의 구현 언어와 배포 형식을 무엇으로 할 것인가.

배포 경험만 보면 Rust 또는 Go 단일 바이너리가 가장 단순하다. 기존 Python 하네스 재사용 비용을 고려하면 Python 코어 + 독립 실행 번들(PyInstaller/Nuitka 계열)도 현실적이다. 이 선택은 작은 packaging spike로 세 OS에서 실제 산출물 크기, 시작 시간, 업데이트, 서명 가능성을 비교한 뒤 결정한다.

그 전까지는 XGENy 제품 저장소의 뼈대와 계약 문서는 만들 수 있지만, 에이전트 루프를 복사해 구현하지 않는다.

## 10. 근거 자료

### 공개 프로젝트

- Qwen Code architecture: <https://qwenlm.github.io/qwen-code-docs/en/developers/architecture/>
- Qwen Code memory: <https://qwenlm.github.io/qwen-code-docs/en/users/features/memory/>
- Qwen Code sandbox: <https://qwenlm.github.io/qwen-code-docs/en/users/features/sandbox/>
- Qwen Code deployment: <https://qwenlm.github.io/qwen-code-docs/en/developers/development/deployment/>
- Qwen Code source: <https://github.com/QwenLM/qwen-code>
- Claude Code setup: <https://code.claude.com/docs/en/setup>
- Claude Code memory: <https://code.claude.com/docs/en/memory>
- Claude Code security: <https://code.claude.com/docs/en/security>
- Gemini CLI session management: <https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/session-management.md>
- Gemini CLI auto memory: <https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/auto-memory.md>
- Gemini CLI policy engine: <https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/policy-engine.md>
- Codex source: <https://github.com/openai/codex>
- Codex SQLite state source: <https://github.com/openai/codex/blob/main/codex-rs/state/src/sqlite.rs>
- Goose source: <https://github.com/block/goose>
- OpenCode source: <https://github.com/anomalyco/opencode>
- OpenCode permissions: <https://opencode.ai/v2/docs/permissions>
- Aider source: <https://github.com/Aider-AI/aider>

### 내부 프로젝트

- xgen-agent-runtime: <https://github.com/PlateerLab/xgen-agent-runtime>
- xgen-harness-executor: <https://github.com/PlateerLab/xgen-harness-executor>
- xgen-sdk: <https://github.com/PlateerLab/xgen-sdk>
- xgen-mcp-harness: <https://github.com/PlateerLab/xgen-mcp-harness>
- xgen-sandbox: <https://github.com/PlateerLab/xgen-sandbox>
- xgen-agent-memory: <https://github.com/PlateerLab/xgen-agent-memory>
- harnesser: <https://github.com/PlateerLab/harnesser>

## 11. 최종 의사결정 기록

- SQLite를 사용자가 설치하게 하지 않는다.
- MVP는 JSONL 세션과 Markdown 메모리로 시작한다.
- SQLite는 요구가 생겼을 때 제품에 임베드한 재생성 가능 인덱스로 검토한다.
- XGENy는 별도 프로젝트로 만들되, 하네스 코어는 새로 복제하지 않는다.
- 현재 모놀리식 `xgen-agent-runtime`과 `xgen-sdk` 전체를 CLI에 설치하지 않는다.
- 로컬 실행에 Docker, Kubernetes, 별도 DB, 상시 daemon을 요구하지 않는다.
- XGEN 서버와 고급 메모리·샌드박스는 선택형 어댑터로 연결한다.
- 구현 시작 전에 기존 세 하네스 계열의 정본을 확정하거나 공용 코어를 추출한다.
