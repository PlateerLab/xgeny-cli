# ADR-0046: XGEN 이름 통일과 기존 저장 데이터 호환

- 상태: Accepted
- 날짜: 2026-10-04

## 결정

일반적인 제품 이름 변경이다. GitHub repository는 `PlateerLab/xgen-cli`, 실행 명령은 `xgen`, 제품
표시는 `XGEN`으로 통일한다. Rust crate와 directory는 `xgen-*`, Rust import는 `xgen_*`를 사용한다.
Native release binary와 installer, npm launcher와 문서, CI/release workflow도 같은 이름에서 파생한다.
이미 XGEN 이름인 npm package scope `@xgen/cli`와 platform package 이름은 유지한다.

공개 환경변수는 `XGEN_*`를 사용한다. Runtime configuration은 새 변수가 없을 때만 기존 `XGENY_*`
변수를 읽는다. 새 변수가 빈 문자열이나 invalid Unicode이면 기존 변수로 우회하지 않고 기존 검증
규칙을 적용한다. Credential 값을 출력하거나 tool 환경에 추가하지 않는다.

다른 XGEN application의 설정과 충돌하지 않도록 CLI의 app ID를 구분한다.
새 기본 config/state directory는 `xgen-cli`(Linux), `XGEN CLI`(macOS/Windows)이다. 명시적 root override가
없고 새 directory가 아직 없으면 기존 `xgeny`/`XGENy` directory를 그대로 재사용한다. 파일이나 live
SQLite를 자동 이동·병합하지 않는다. 새 root의 broken symlink도 legacy fallback으로 숨기지 않는다.

기존 Receipt, journal, manifest와 credential을 계속 읽기 위해 다음은 branding 대상에서 제외한다.

- `xgeny.io/v1alpha1`, schema URI·response schema 이름과 기존 capability ID
- digest domain, profile revision, digest에 결합된 planner prompt bytes, actor/executor ID와 authority/policy identity
- OS credential service `com.plateer.xgeny.model`

이 식별자는 durable format의 일부이며 사용자 실행 명령과 구분된다. 변경하려면 별도의 protocol
migration이 필요하다. 기존 server 제품의 고유명사 `Agent-XGeny`도 이 CLI의 이름 변경 대상이 아니다.

## 검증

Rust workspace, npm package contract/test와 native installer smoke, CI/release/doc checks를 실행한다.
구버전 executable로 만든 profile과 completed/approval-paused Run을 새 executable에서 읽고,
completed offline replay와 pending Run 재개를 별도로 확인한다. 새 환경변수 우선순위와 legacy
root 재사용은 config/state 두 종류에서 검증한다.
