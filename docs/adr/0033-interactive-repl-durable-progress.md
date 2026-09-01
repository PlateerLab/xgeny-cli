# ADR-0033: 대화형 REPL은 기존 durable Run을 조합하고 검증된 경계만 스트리밍한다

- 상태: Accepted
- 날짜: 2026-09-02
- 적용 범위: bare `xgeny`, terminal input/output, 승인 UI, cooperative cancellation

## 배경

기존 `xgeny run/resume`은 자동화와 복구에는 명시적이지만, 일반 사용자가 매번 workspace scope,
executable catalog와 승인 flag를 조합해야 했다. 반대로 별도 TUI agent loop를 만들면 CLI와 Core가 서로
다른 승인·저장·복구 의미를 갖게 된다. OpenAI adapter도 자유 형식 chat이 아니라 strict JSON plan을
생성한다. 검증 전 raw model token을 terminal에 표시하면 구조화 proposal 일부, escape sequence 또는
나중에 거부될 주장을 사용자 답변처럼 노출할 수 있다.

Ctrl+C도 단순 process 종료로 처리할 수 없다. Model request나 NonIdempotent effect가 이미 시작된 뒤
process를 끊으면 실제 결과를 모를 수 있으며, 이를 cancelled로 기록하고 자동 재실행하면 중복 부작용이
발생한다.

## 결정

### 1. REPL은 새 orchestrator가 아니라 `run/resume`의 표현 계층이다

Subcommand 없이 `xgeny`를 실행하면 현재 directory를 workspace root로 하는 terminal REPL을 시작한다.
TTY에서 model profile이 없으면 기존 `model setup` 온보딩을 먼저 실행한다. Headless pipe에서는 자동
prompt를 열지 않고 기존 환경변수/profile 해석을 사용한다.

각 사용자 goal은 기존 `LocalRunRequest`로 하나의 durable Run을 만든다. Approval pause 뒤에는 동일한
workspace와 catalog로 `LocalResumeRequest`를 호출한다. Journal, WorkGraph, receipt, material sidecar,
model-call reservation과 offline completion replay는 기존 composition이 계속 소유한다. 기존 `run`과
`resume` CLI 계약은 변경하지 않는다.

완료된 Run의 bounded summary만 process-local session context로 보존한다. 다음 goal에는 이를
`untrusted context; revalidate before acting`로 표시해 포함하며 새 Run 자체에 goal로 기록한다. `/clear`는
연결만 끊고 durable Run을 삭제하지 않는다. Process 재시작 뒤 사용자가 `/resume RUN_ID`로 completion을
offline replay하면 같은 bounded summary를 후속 context로 다시 얻는다. 이는 장기 메모리나 전체 transcript
저장소가 아니다.

### 2. 승인 종류와 수명은 분리한다

Model egress, read, write, execute는 각각 `ask`, `allow`, `deny` session mode를 가진다. 기본값은 모두
`ask`다. `ask`의 yes는 현재 bounded continuation에만 기존 boolean grant를 전달한다. `allow`는 사용자가
REPL에서 명시한 session 동의이며 기존 Core의 exact action-bound authorization을 대체하지 않는다.
`deny`는 effect를 수행하지 않고 Run을 pause 상태로 남긴다.

Workspace discovery scope는 `.`이다. Process executable은 PATH의 임의 이름을 허용하지 않고 설치된 공통
개발 도구의 고정 allowlist만 host catalog에 넣는다. 현재 family는 Git, Rust, Node, Python, Go,
Make/CMake, .NET, Java, Swift/Xcode다. 따라서 빈 directory에서 새 프로젝트를 만드는 작업도 특정 project
marker에 종속되지 않는다. Relative PATH entry와 Windows `.cmd`/`.bat`은 자동 catalog에서 제외한다.
Catalog에 있다는 사실은 실행 승인이 아니며 executable과 child process는 사용자 OS 권한으로 실행되므로
sandbox가 아니다.

Executable content와 safe environment snapshot은 첫 실제 goal에서 physical workspace에 묶어 한 REPL
process 동안 재사용한다. Resume 때 전체 toolchain을 다시 해시하지 않지만 process adapter는 선택된
executable의 canonical target과 content를 매 실행 직전에 다시 검증한다. REPL 재시작 뒤 continuation은
새 snapshot을 만들고 manifest execution-profile digest와 일치해야 한다.

### 3. 스트리밍은 redacted durable progress이며 최종 답변은 검증 뒤 표시한다

Runtime observer는 다음 고정 event만 terminal에 전달한다.

- model call starting
- plan committed
- approval required / action authorized
- effect starting / committed
- verification starting / committed
- completion committed

Event에는 prompt, model output, path, argv, environment, file/process output, credential이 없다. Structured
planner token은 표시하지 않는다. 최종 summary는 completion sidecar와 durable binding 검증이 끝난 뒤
control character를 escape해 표시한다. 따라서 RC3의 streaming은 실제 진행 경계 스트리밍이지 token
타이핑 효과가 아니다. 자유 형식 assistant token streaming은 planner와 분리된 검증 가능한 response
channel을 설계하기 전까지 포함하지 않는다.

### 4. Ctrl+C는 협력적으로 멈추되 불확정 결과를 숨기지 않는다

입력 대기 중 Ctrl+C는 현재 입력을 취소한다. Driver observer가 외부 작업 전 또는 durable commit 뒤
신호를 보면 `user_cancelled` pause를 반환하며 Run은 `/resume` 가능하다. 이미 model/effect I/O가 진행
중이면 즉시 성공·실패를 만들지 않는다.

- 작업이 durable outcome에 도달하면 다음 observer 경계에서 멈춘다.
- 전송 여부나 effect outcome을 확정할 수 없으면 기존 `model_call_unknown` 또는
  `effect_outcome_unknown` recovery가 우선한다.
- REPL은 unknown을 자동 retry하거나 replay하지 않는다.

이 의미 때문에 Ctrl+C 응답 시간은 in-flight adapter의 bounded timeout보다 빠르다고 보장하지 않는다.

## 명령과 입력 경계

- `/model [PROFILE]`: active profile 조회 또는 선택
- `/status`: active/last Run, session context, tool count, 승인 mode
- `/permissions [KIND ask|allow|deny]`: 승인 mode와 logical executable ID 조회
- `/resume [RUN_ID]`: 현재/마지막 또는 명시 Run 재개와 offline replay
- `/clear`: session link 제거, durable state 유지
- `/exit`: 정상 종료
- 줄 끝 unescaped `\`: 다음 줄과 newline으로 결합

한 line과 합성 goal은 16 KiB로 제한한다. Model summary의 terminal control character는 escape한다. REPL
progress와 오류는 고정 code만 사용한다.

## 검증

- pure REPL test: multiline, command parse, permission mode, clear, bounded input, context carry, terminal escape
- mock provider process E2E: bare `xgeny` → model/read/execute 각각 승인 → read → shell-free Git 실행 →
  completion → provider 없는 `/resume`, receipt 수 불변
- Unix SIGINT process E2E: in-flight model request가 끊기면 `model_call_unknown`, effect 0회, 자동 replay 없음
- 기존 `run/resume` 전체 회귀와 Linux/macOS/Windows release process test
- npm global install과 native installer smoke 뒤 bare REPL의 non-network `/status`/`/exit` smoke

## 대안

별도 TUI runtime은 승인과 복구가 이중화되므로 채택하지 않는다. Raw structured planner SSE를 사용자
답변처럼 표시하는 방식은 검증 전 정보와 terminal injection 위험 때문에 채택하지 않는다. Ctrl+C에
process를 즉시 kill하고 Run을 정상 cancelled로 닫는 방식은 possible-send와 NonIdempotent no-replay를
깨뜨리므로 채택하지 않는다.
