# XGEN 대화형 REPL

## 빠른 시작

프로젝트 root에서 subcommand 없이 실행한다. TTY에서 profile이 없으면 제공자 선택, 숨김 API key
입력, model 선택과 연결 검증을 진행한다. DeepSeek와 OpenAI는 preset으로 URL과 wire 옵션을 채우며,
다른 OpenAI-compatible endpoint는 URL을 직접 입력한다. 저장된 설정은 다음 실행부터 재사용한다.

```bash
cd my-project
xgen
```

API key는 OS credential store에 저장한다. 저장소를 쓸 수 없으면 숨김 입력한 key는 현재 process의
메모리에만 두고 다음 실행 때 다시 묻는다. 평문 key file이나 tool process 환경변수로 옮기지 않는다.
자동화의 `--store-token`은 여전히 저장 실패를 오류로 반환한다.

기본 approval mode는 모두 `ask`다. Model prompt에는 현재 goal, 제한된 이전 요청·답변과 이후 tool
observation이 포함될 수 있다. Read, write와 process execute는 각각 별도로 묻는다.
한 goal을 처리하는 동안 승인한 종류는 이후 continuation에서도 유지하며, 다음 goal이나 명시적인
`/resume`에서는 다시 기본 approval mode를 적용한다. `deny`는 계속 실행을 차단한다.

```text
xgen> 프로젝트 구조를 보고 테스트 실패를 수정해줘.
Allow sending the goal, session context, and tool observations to the model? [y/N] y
Thinking… · 2s
Allow read for this durable continuation? [y/N] y
```

줄 끝에 unescaped `\`를 쓰면 다음 줄을 같은 goal로 입력한다.

```text
xgen> src를 탐색하고 \
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

한 goal은 하나의 durable Run이다. 일반 질문이나 이전 설명에 대한 후속 질문은 도구 실행 없이 답할 수 있다.
이 경우 `/status`는 `responded`를 표시하고 파일·process 실행 승인을 묻지 않는다. 도구 Step을 계획한 Run은
대화 응답으로 종료할 수 없고, 기존대로 Receipt 검증 뒤 작업 완료 응답을 출력한다.

최근 요청·답변은 최대 8 turn, JSON 기준 12 KiB의 비신뢰 context로 이어진다. 현재 요청은 유지하고 한도에
맞지 않는 오래된 문맥부터 생략한다. 별도 장기 transcript는 저장하지 않지만 다음 Run에 전달한 문맥은
그 Run의 goal에 포함된다. `/clear`는 문맥과 active/last pointer만 제거하며 SQLite Run을 삭제하지 않는다.

같은 세션에서는 `/resume`으로 현재/마지막 Run을 재개한다. 다른 process에서 재개하려면 `/status`의
Run ID를 기록한다. `--debug` 또는 pipe 모드에서는 stderr의 `XGEN_STARTED run_id=...`도 사용할 수 있다.

```text
xgen> /resume run-0123456789abcdef0123456789abcdef
```

작업 완료와 대화 응답을 저장한 Run은 model, workspace 또는 tool effect 없이 summary를 offline replay한다. Approval 대기 Run은
동일한 physical workspace와 자동 catalog snapshot이 필요하다. 도구 binary, PATH 또는 safe environment가
바뀌어 execution profile이 달라지면 configuration mismatch로 fail-closed할 수 있다.

## Progress와 Ctrl+C

일반 TTY 화면은 요청 처리 중 `Thinking…`과 경과 시간을 표시한다. 이 표시는 model 호출뿐 아니라
도구 실행과 결과 검증 시간을 포함하며, model의 내부 reasoning text를 뜻하지 않는다. 승인 입력이나
결과 출력 전에 표시를 지운다. 최종 summary만 durable completion 검증 뒤 control character를 escape해
출력하며, Run ID는 `/status`에서 확인할 수 있다. 이 검증은 Receipt와 결과 저장의 결합·무결성을
확인하며, summary의 모든 자연어 설명이 실제 변경과 일치함을 보장하지 않는다.

```bash
# 기존 durable progress와 Run ID 로그 확인
xgen --debug
```

`--debug`와 pipe 입력에서는 기존 `progress:` event 및 `XGEN_STARTED` 계약을 유지하고 ANSI animation을
출력하지 않는다. Progress는 runtime의 redacted lifecycle event이며 strict JSON proposal은 표시하지 않는다.

Ctrl+C는 다음 안전한 durable 경계에서 멈춘다. Model request나 process outcome이 이미 불확정해졌으면
`user_cancelled`보다 `model_call_unknown`/`effect_outcome_unknown`이 우선하고 자동 재실행하지 않는다.
Network/process call이 진행 중이면 configured timeout까지 기다릴 수 있다.

## Headless

Pipe 입력에서는 first-run hidden prompt를 자동으로 열지 않는다. Profile이나 환경변수를 먼저 공급하면
결정적 smoke script를 실행할 수 있다. Secret은 script text나 argv에 넣지 않는다.

```bash
printf '/status\n/exit\n' | xgen
```

실제 goal을 pipe로 실행할 때 원격 HTTPS credential은 `XGEN_OPENAI_API_KEY` 같은 외부 secret injection을
사용한다. 일반 자동화는 exit code와 고정 stderr 계약이 더 단순한 기존 `xgen run/resume`을 권장한다.

대화 응답의 저장·검증 계약은 [ADR-0047](../adr/0047-conversation-response-and-bounded-session-context.md)을 따른다.
