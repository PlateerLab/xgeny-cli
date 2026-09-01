# ADR-0028: shell 없는 로컬 process 실행 경계

- 상태: 채택
- 기준일: 2026-09-01
- 적용 범위: Capability Definition, process adapter, executable catalog, workspace confinement, OS process lifecycle
- 공개 protocol v0.1 schema 변경: `xgeny.process/execute@1.0.0` Definition 추가
- local store schema 변경: 없음(schema 8 유지)

## 문맥

Workspace 탐색·읽기·쓰기·패치만으로는 실제 개발 작업을 완결할 수 없다. 에이전트가 변경 뒤 test,
lint, build를 실행하고 그 결과를 다음 planning turn에 전달해야 한다. 하지만 범용 process 실행은
project code, build script, compiler plugin과 그 하위 process를 사용자의 OS 권한으로 실행한다. 문자열
command를 shell에 넘기거나 model이 임의 실행 경로와 상속 환경을 고르면 quoting 차이, command injection,
PATH hijack, credential 노출과 재시작 뒤 중복 실행 위험이 생긴다.

ADR-0027은 `NonIdempotent + durableToolOutput=true` 결과와 no-replay 복구 의미를 먼저 고정했다. 이번
결정은 그 profile 위에 실제 OS process adapter를 올리되 sandbox나 자동 재실행을 주장하지 않는다.

## 결정

### 1. 공개 계약은 하나의 shell 없는 argv 실행이다

`xgeny.process/execute@1.0.0` input은 다음 여섯 필드만 허용한다.

| 필드 | 의미 | 고정 상한 |
|---|---|---:|
| `executable` | host catalog의 logical ID 또는 정규화 resource | ID 128 bytes, resource 224 bytes |
| `args` | 그대로 전달할 argument vector | 128개, 항목당 4096 bytes, 합계 32 KiB |
| `cwd` | 선택한 workspace 안의 portable relative directory | 4096 bytes |
| `env` | child에 추가할 비보호 환경 변수 | 64개, 합계 32 KiB |
| `timeoutMs` | wall-clock 실행 상한 | 100~600000 ms |
| `maxOutputBytes` | stdout/stderr 각각의 보존 상한 | 1~32 KiB |

구현은 `Command::new(executable).args(args)`를 직접 사용한다. `/bin/sh`, `sh -c`, `cmd.exe`,
PowerShell 또는 command-line 문자열 재조립 경로는 두지 않는다. 따라서 `;`, `&&`, `|`, redirect와
substitution 문자는 shell 문법이 아니라 하나의 literal argument다.

Output은 `outcome`, `success`, nullable `exitCode`, bounded `stdout`/`stderr`, 각 stream의
truncation flag와 `durationMs`를 가진 exact object다. Non-zero exit와 timeout은 adapter transport
실패가 아니라 모델이 다음 행동을 결정할 durable 결과다. 결과 canonical digest는 adapter evidence와
일치해야 하고 read-only verifier가 schema, 조합과 bound를 다시 검사한다.

### 2. 실행 파일 경로와 기본 환경은 host가 소유한다

Model은 ambient path나 `PATH` lookup 결과를 전달하지 않는다. Composition root가 OS-executable file을
portable logical ID에 연결한 `ExecutableCatalog`를 만든다. Catalog 생성 시 launch path, canonical
target과 content SHA-256을 고정하며, prepare 직전에 같은 alias/target/file을 다시 검사한다.
Unix에서는 execute bit가 있는 regular file(명시적 shebang script 포함), Windows에서는 `.exe`와 `.com`만
허용한다. Cataloged shebang은 OS loader가 해석하지만 shell command 문자열을 구성하거나 전달하지 않는다.
Rustup의 `cargo -> rustup`처럼 `argv[0]` alias가 동작을 결정하는 multicall executable을 위해 spawn에는
검증된 launch alias를 보존한다. Launch path 자체는 digest로 binding에 묶여 다른 alias로 조용히 바뀌지
않는다.
정규화된 invocation resource는 `process:<workspace>/executables/<logical-id>`이고 실제 host path는
journal, material, Receipt와 `Debug`에 들어가지 않는다.

Child 환경은 ambient inheritance가 아니라 `env_clear()` 뒤 host가 만든 bounded
`ProcessEnvironment` snapshot과 model env를 합쳐 구성한다. Model env는 host key를 override할 수 없고
`PATH`, OS home/system/temp, Cargo/Rustup home, loader 변수를 설정할 수 없다. Host snapshot에는 token,
credential 또는 다른 secret을 넣지 않는다. Snapshot과 executable catalog의 digest는 exact Instance
binding에 들어가므로 재개 시 다른 실행 경계로 조용히 바뀌지 않는다.

### 3. cwd는 선택한 workspace 안의 기존 실제 directory만 허용한다

`cwd`는 `.` 또는 `/`로 구분된 portable relative path다. Absolute path, `..`, backslash, Windows
device name, empty component, symlink와 reparse point를 거부한다. Directory capability로 각 component를
nofollow 방식으로 확인한 뒤 같은 workspace의 canonical ambient path를 child의 current directory로
사용한다.

이 검사는 실수와 model path escape를 막지만, 같은 OS 계정이 검사와 spawn 사이에 filesystem을
교체하는 공격까지 격리하는 sandbox는 아니다. Developer Preview에서는 XGENy와 사용자를 같은 local
trust boundary로 두며, 강한 hostile-code isolation은 별도 설계 없이는 주장하지 않는다.

### 4. timeout과 정상 종료 모두 process tree를 닫는다

Unix에서는 새 process group, Windows에서는 Job Object로 top-level process와 descendants를 묶는다.
Timeout이면 group/job 전체를 종료하고 leader를 wait한다. Top-level process가 먼저 끝나도 one-shot
Capability 밖으로 background child가 남지 않도록 group/job을 종료하고 회수한다.

stdout/stderr pipe는 두 host reader가 동시에 계속 drain한다. 각 reader는 처음 `maxOutputBytes`만
메모리에 보존하고 나머지는 버려 child가 가득 찬 pipe에서 멈추거나 host disk를 무제한 소비하지 않게
한다. Durable UTF-8 문자열로 바꾼 뒤에도 같은 byte 상한을 지키며, 잘못된 UTF-8의 치환 때문에 상한을
넘으면 유효한 문자 경계에서 다시 절단한다. Group/job 종료 뒤 bounded grace 안에 EOF가 오지 않으면
exact output을 추측하지 않고
`ResponseUnverifiable`로 닫는다. 따라서 scope를 벗어난 detached process가 stdio handle을 유지해도
agent loop가 무기한 기다리지 않는다.

`process-wrap 10.0.0`의 std ProcessGroup/JobObject wrapper를 사용한다. 우리 workspace의
`unsafe_code = forbid`를 유지하면서 두 OS의 process-tree primitive를 동일한 adapter 경계에 둔다.

### 5. 실행 승인은 filesystem read/write와 분리한다

Capability scope는 `process.execute`다. CLI composition은 반복 가능한
`--allow-executable ID=ABSOLUTE_PATH`로 top-level executable catalog를 만들고, 별도
`--allow-execute`가 있을 때만 exact one-shot process request를 승인한다. `--allow-dir`,
`--allow-read`, `--allow-write`는 process 권한으로 승격되지 않는다. Catalog만 주고 실행 승인을 주지
않으면 plan과 material을 durable하게 보존한 뒤 `execute_approval_required`로 pause한다.

Planner constraint에는 logical ID만 전달한다. 실제 path, content digest와 safe child-environment snapshot은
local Instance binding에 결합하고 local execution profile에 포함한다. 미완료 Run 재개는 원래와 동일한
executable content/catalog, workspace와 safe environment가 아니면 `configuration_mismatch`로 닫는다.
`--allow-execute`만 주고 catalog를 생략하는 새 Run도 configuration 오류다.

Process는 `NonIdempotent`, `durableToolOutput=true`, `SinkGuarantee::None` 의미를 사용한다. Durable
idempotency key는 identity이지 replay 허가가 아니다. `Executing` 상태에서 재시작하면 adapter를 다시
prepare/execute하지 않고 `EffectUnknown`으로 닫는다. Exact output bundle commit이 끝난 경우에만
`Validating`에서 verifier와 Receipt 생성을 이어간다.

## 검증

- public fixture의 schema/round-trip/manifest conformance
- executable catalog 밖 resource, executable drift와 다른 workspace resource 거부
- cwd absolute/traversal/symlink/reparse 및 portable grammar 거부
- shell metacharacter가 literal argv이며 marker file을 만들지 않음
- non-zero exit, stdout/stderr 개별 절단과 truncation flag 보존
- timeout 뒤 descendant가 지연 marker를 만들지 못함
- protected environment override와 material/path/환경의 `Debug` 노출 거부
- exact output shape/evidence digest verifier 검증
- CLI plan → 실행 승인 pause → 별도 process 재개 → durable output → 다음 model turn과 단일 Receipt
- 변경된 executable catalog로 재개 실패와 완료된 process의 no-replay
- 실제 process 적용 뒤 outcome/output transaction commit 실패 → `Executing` cold resume →
  `EffectUnknown`; 반복 재개·재승인에도 외부 marker 1회, start event 1개, Receipt 0개 유지
- Linux x86-64/ARM64, macOS Intel/Apple Silicon, Windows x86-64의 workspace test
- ADR-0027의 SQLite crash/lost-ack no-replay 회귀 suite 유지

## 포함하지 않는 범위

- shell command string 또는 shell builtin 실행
- hostile project code의 filesystem/network/credential sandbox
- XGEN 의존성, MCP, 장기 메모리, browser, TUI
- interactive approval UI와 executable 자동 발견
- detached/background service lifecycle

## 결과

Core는 host가 고정한 executable과 workspace 안에서 하나의 bounded OS process를 실행하고 결과를
장기 WorkGraph에 전달할 수 있다. 실제 coding loop의 test/build 기반은 생기지만, 실행 권한과 격리는
과장하지 않으며 실행 시작 뒤 장애에는 결과를 얻기 위한 blind replay보다 manual recovery를 택한다.
