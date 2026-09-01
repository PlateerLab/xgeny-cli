# RC3 Developer Preview 파일럿

이 문서는 게시된 `v0.1.0-rc.3` artifact가 일반 개발자의 작은 코딩 작업을 끝까지 수행하는지 확인하는
제한 파일럿 runbook이다. Release 이전 source checkout이나 local build 결과를 사용자 결과로 대체하지
않는다. 파일럿 project와 state는 폐기 가능한 전용 위치에서 만들고 실제 업무 source나 credential을
사용하지 않는다.

## 사전 조건과 중단 조건

- GitHub Release `v0.1.0-rc.3`과 `@xgen/cli@0.1.0-rc.3`이 게시됐고 provenance, checksum과 package
  integrity 검증이 끝나야 한다.
- 각 참여자는 repository checkout이나 Rust compiler 없이 먼저 설치·온보딩 smoke를 통과한다. 언어별
  compiler/interpreter는 해당 coding fixture 실행을 위해서만 사용한다.
- Fixture의 실패 test와 acceptance condition은 model 실행 전에 고정한다. 실행 결과를 보고 task를
  바꾸거나 성공 사례만 남기지 않는다.
- API key, endpoint 전체 URL, prompt, model 원문 응답, source, tool stdout/stderr 또는 state DB가 결과
  기록에 들어가면 해당 기록을 폐기하고 보안 사고 절차를 우선한다.
- 예상하지 못한 외부 변경, credential 노출, workspace 밖 mutation 또는 자동 replay 징후가 있으면 즉시
  파일럿을 중단한다.

## 사전 고정 사용자 matrix

| Pilot | Project | 진입 경로 | 허용 executable | 필수 관찰 |
| --- | --- | --- | --- | --- |
| `rust-bare` | 작은 Cargo project | bare `xgeny` | `cargo` | 탐색, 수정, 실패 test 관찰, 교정, test/build 성공, `/status`, `/exit` |
| `node-resume` | Node.js built-in test fixture | `run` → `resume` | `node` | read/write와 분리된 execute 승인, `node --test`, `node --check`, offline replay |
| `python-resume` | 표준 `unittest` fixture | `run` → `resume` | `python3` 또는 `python` | 별도 execute 승인, 실패 분석, `-m unittest`, `-m compileall`, offline replay |

각 fixture는 작은 논리 오류 하나와 이를 검출하는 기존 test를 가진다. Model이 test를 삭제·완화하거나
acceptance를 바꾸면 실패다. Rust scenario는 대화형 UX를, Node.js와 Python scenario는 process를 실행하기
직전 pause한 durable Run을 별도 `resume --allow-execute`로 계속하는 경계를 담당한다.

## 공통 설치와 온보딩

각 OS/architecture에서 native installer와 npm 중 배정된 공개 채널만 사용한다. 설치 후 아래 순서를
repository 밖의 깨끗한 home/state에서 실행한다.

```text
install exact version
  -> xgeny --version
  -> xgeny model setup
  -> xgeny model check --compatibility
  -> bare xgeny
  -> /status
  -> /exit
  -> same-version reinstall
  -> remove
```

온보딩 key는 숨김 입력 또는 secret manager의 stdin으로만 전달한다. 일반 설정, shell script, command
argument나 결과 ledger에 복사하지 않는다. `model setup`이 catalog와 실제 structured inference를 모두
통과하지 않으면 coding pilot을 시작하지 않는다.

## 대화형 Rust 절차

1. 폐기 가능한 Rust fixture root에서 `xgeny`를 실행하고 `/status`로 active model과 idle 상태를 확인한다.
2. 기존 실패 test를 유지하면서 project를 탐색하고 원인을 수정한 뒤 test/build하도록 요청한다.
3. Model egress, read, write와 execute 승인이 각각 별도로 나타나는지 확인한다.
4. 첫 실행 실패가 있으면 bounded durable output을 다음 turn이 관찰해 교정하는지 확인한다.
5. 성공 후 `/status`, `/exit`으로 정상 종료하고 새 process에서 완료 Run을 offline replay한다.

자유 형식 token streaming이나 prompt 품질을 평가하지 않는다. 합격 기준은 acceptance test/build와 durable
상태 전이이며, 최종 source나 output은 결과 ledger에 복사하지 않는다.

## 비대화형 Node.js와 Python 절차

각 fixture에서 처음에는 `--allow-execute`를 빼고 실행한다. `RUNTIME_ID`는 Node.js의 `node`, Python의
`python`이고 `EXE`는 operator가 신뢰한 executable의 absolute path다. Model에는 logical ID만 보인다.

```bash
xgeny run \
  --workspace . \
  --allow-dir . \
  --allow-executable RUNTIME_ID="EXE" \
  --allow-remote-model-egress \
  --allow-read \
  --allow-write \
  '기존 테스트를 유지하며 실패 원인을 수정하고 테스트와 구문 검사를 통과시켜줘.'
```

`execute_approval_required`로 pause한 exact Run ID를 같은 workspace, directory scope와 executable binding으로
재개한다.

```bash
xgeny resume RUN_ID \
  --workspace . \
  --allow-dir . \
  --allow-executable RUNTIME_ID="EXE" \
  --allow-remote-model-egress \
  --allow-read \
  --allow-write \
  --allow-execute
```

Node.js fixture는 shell script나 package lifecycle hook 대신 `node --test`와 `node --check`만 사용한다.
Python fixture는 추가 package 설치 없이 `python -m unittest`와 `python -m compileall`만 사용한다. 완료 뒤
model credential, workspace와 executable 없이 `xgeny resume RUN_ID`가 같은 completion을 offline
replay해야 한다.

## 복구·중단 안전성 확인

사용자 파일럿과 같은 release SHA의 필수 CI가 다음 회귀를 모두 통과한 상태여야 한다.

- read/write/execute 승인 분리와 승인 전 물리 I/O 0회
- idle 및 model call 중 Ctrl+C의 safe pause 또는 Unknown 종결
- timeout과 정상 leader 종료 뒤 descendant process tree 정리
- process/model outcome commit 장애와 process exit 뒤 effect/model call 자동 replay 0회
- 완료 Run offline replay와 journal 불변성

파일럿에서는 한 대화형 Run을 model call 중 Ctrl+C로 중단하고 `/status`와 `/resume`의 redacted 상태가
위 계약과 일치하는지 확인한다. 불확정 call/effect가 Unknown이면 성공으로 바꾸거나 다시 실행하지 않는다.
Fault injection이나 process-tree sentinel 검증은 동일 SHA의 자동화 test 결과를 권위로 사용하며 일반
사용자 PC에서 임의로 장애를 주입하지 않는다.

## 비민감 결과 ledger

한 행은 사전 고정 pilot 한 번이다. 다음 필드만 기록한다.

| 필드 | 허용 값 |
| --- | --- |
| release | tag, immutable release commit SHA, npm/native channel |
| host | OS family/version, architecture, 설치 성공 여부 |
| runtime | Rust/Node.js/Python version과 scenario ID |
| model | 공개 가능한 served model ID만 기록하거나 `redacted` |
| mode | `bare` 또는 `run-resume` |
| outcome | `pass`, `product-failure`, `model-variance`, `environment-failure`, `security-stop` |
| stage | `install`, `onboarding`, `explore`, `edit`, `test`, `correct`, `build`, `replay`, `remove` |
| metrics | elapsed seconds, model call 수, tool step 수, approval 수, retry 수 |
| invariants | Unknown 수, duplicate effect 수, process-tree leak 여부, offline replay 여부 |
| diagnostic | 고정된 redacted error code만 허용 |

Run ID, path, filename, goal, prompt, endpoint, credential, response, source diff, tool output과 state digest는
기록하지 않는다. Aggregate 문서에는 scenario별 시도/성공 수, 중앙 elapsed time, 실패 stage 빈도와
invariant 위반 수만 남긴다. 표본이 작으므로 모델 품질의 일반적 우월성이나 통계적 유의성을 주장하지
않는다.

## 합격 기준과 결과 처리

- 세 사전 고정 scenario가 각각 최소 한 번 성공하고 실패 실행도 삭제하지 않고 분류한다.
- `bare`와 `run-resume`, 설치·온보딩·재설치·제거, 실제 model 연결과 offline replay가 모두 관찰된다.
- Model/read/write/execute 승인 분리가 유지되고 duplicate effect, process-tree leak과 민감정보 기록은 0이다.
- Unknown은 자동 재실행 0회를 유지해야 하며, Unknown 자체를 억지로 성공 처리하지 않는다.
- 제품 결함은 재현 test와 별도 PR로 수정한 뒤 전체 필수 CI와 해당 pilot을 다시 수행한다.
- 치명적 공개 결함은 기존 immutable release나 npm version을 수정하지 않고 새 version으로 교정한다.

결과 집계는 게시 artifact와 commit을 명시하되 원문 실행 자료를 포함하지 않는다. 이 기준을 모두 만족한
경우에만 RC3 제한 파일럿을 통과했다고 기록한다.
