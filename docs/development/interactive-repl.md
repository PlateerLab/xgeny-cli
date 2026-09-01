# XGENy 대화형 REPL

## 빠른 시작

Model profile을 한 번 설정한 뒤 프로젝트 root에서 subcommand 없이 실행한다. TTY에서 profile이 없으면
bare `xgeny`가 같은 온보딩을 먼저 시작한다.

```bash
xgeny model setup
cd my-project
xgeny
```

기본 approval mode는 모두 `ask`다. Model prompt에는 현재 goal, 직전 durable result와 이후 tool
observation이 포함될 수 있다. Read, write와 process execute는 각각 별도로 묻는다.

```text
xgeny> 프로젝트 구조를 보고 테스트 실패를 수정해줘.
Allow sending the goal, session context, and tool observations to the model? [y/N] y
progress: model_call_starting
progress: plan_committed
progress: approval_required effect=read
Allow read for this durable continuation? [y/N] y
```

줄 끝에 unescaped `\`를 쓰면 다음 줄을 같은 goal로 입력한다.

```text
xgeny> src를 탐색하고 \
...> 실패한 테스트만 수정해줘.
```

## 명령

```text
/model [PROFILE]
/status
/permissions
/permissions model|read|write|execute ask|allow|deny
/resume [RUN_ID]
/clear
/exit
```

`allow`는 현재 REPL session에서 사용자가 명시한 선택이다. Process executable은 PATH에서 발견한 고정된
공통 개발 도구 allowlist의 absolute native executable만 catalog한다. `/permissions`는 model에 보이는
logical ID만 출력하며 host path는 출력하지 않는다. Catalog는 실행 권한이 아니고, 실행은 별도 approval과
Core authorization을 거친다. Catalog snapshot은 첫 실제 goal에서 만들어 같은 REPL process의 resume에
재사용하며, 선택된 binary는 매 실행 직전에 다시 검증한다.

## 세션과 재개

한 goal은 하나의 durable Run이다. 완료 summary는 다음 goal에 비신뢰 context로 이어지지만 전체 transcript
또는 장기 memory는 저장하지 않는다. `/clear`는 이 연결과 active/last pointer만 제거하며 SQLite Run을
삭제하지 않는다.

중단 뒤 stderr의 `XGENY_STARTED run_id=...` 값을 사용해 재개한다.

```text
xgeny> /resume run-0123456789abcdef0123456789abcdef
```

완료된 Run은 model, workspace 또는 tool effect 없이 summary를 offline replay한다. Approval 대기 Run은
동일한 physical workspace와 자동 catalog snapshot이 필요하다. 도구 binary, PATH 또는 safe environment가
바뀌어 execution profile이 달라지면 configuration mismatch로 fail-closed할 수 있다.

## Progress와 Ctrl+C

`progress:` line은 fake spinner나 model token이 아니라 runtime의 redacted lifecycle event다. Strict JSON
proposal은 검증 전 표시하지 않으며 최종 summary는 durable completion 검증 뒤 출력한다.

Ctrl+C는 다음 안전한 durable 경계에서 멈춘다. Model request나 process outcome이 이미 불확정해졌으면
`user_cancelled`보다 `model_call_unknown`/`effect_outcome_unknown`이 우선하고 자동 재실행하지 않는다.
Network/process call이 진행 중이면 configured timeout까지 기다릴 수 있다.

## Headless

Pipe 입력에서는 first-run hidden prompt를 자동으로 열지 않는다. Profile이나 환경변수를 먼저 공급하면
결정적 smoke script를 실행할 수 있다. Secret은 script text나 argv에 넣지 않는다.

```bash
printf '/status\n/exit\n' | xgeny
```

실제 goal을 pipe로 실행할 때 원격 HTTPS credential은 `XGENY_OPENAI_API_KEY` 같은 외부 secret injection을
사용한다. 일반 자동화는 exit code와 고정 stderr 계약이 더 단순한 기존 `xgeny run/resume`을 권장한다.
