#!/bin/sh

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)

python3 - "$repo_root" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
getting_started = (root / "docs/getting-started.md").read_text(encoding="utf-8")
pilot = (root / "docs/development/rc3-developer-preview-pilot.md").read_text(
    encoding="utf-8"
)
readme = (root / "README.md").read_text(encoding="utf-8")
candidate = (root / "docs/development/rc3-release-candidate.md").read_text(
    encoding="utf-8"
)
security = (root / "SECURITY.md").read_text(encoding="utf-8")
npm_distribution = (root / "docs/development/npm-distribution.md").read_text(
    encoding="utf-8"
)


def require(document: str, fragments: tuple[str, ...], label: str) -> None:
    for fragment in fragments:
        if fragment not in document:
            raise SystemExit(f"{label}: required public contract is missing: {fragment}")


require(
    getting_started,
    (
        "## 5분 빠른 시작",
        "@xgen/cli@0.1.0-rc.3",
        "xgeny model setup",
        "xgeny model check --compatibility",
        "## 업데이트와 RC2 rollback",
        "v0.1.0-rc.2",
        "npm uninstall --global @xgen/cli",
        "## 문제 해결과 지원 정보",
        "credential_store_unavailable",
        "model_call_unknown",
        "## 보안 경계",
        "security/advisories/new",
    ),
    "getting-started",
)

require(
    pilot,
    (
        "## 사전 조건과 중단 조건",
        "## 사전 고정 사용자 matrix",
        "rust-bare",
        "node-resume",
        "python-resume",
        "## 공통 설치와 온보딩",
        "## 복구·중단 안전성 확인",
        "## 비민감 결과 ledger",
        "## 합격 기준과 결과 처리",
        "duplicate effect",
        "process-tree leak",
        "offline replay",
    ),
    "rc3-pilot",
)

link = "docs/development/rc3-developer-preview-pilot.md"
require(readme, (link,), "README")
require(readme, ("SECURITY.md",), "README")
require(candidate, ("rc3-developer-preview-pilot.md",), "rc3-release-candidate")
require(
    security,
    (
        "## 지원 범위",
        "## 비공개 신고",
        "security/advisories/new",
        "## 주요 보안 경계",
    ),
    "SECURITY",
)
require(
    npm_distribution,
    (
        "granular access token",
        "`bypass 2FA`",
        "GitHub Actions repository secret `NPM_TOKEN`",
        "publish 단일 step의 `NODE_AUTH_TOKEN`",
        "`--provenance`",
        "npm 인증이 아니라",
    ),
    "npm-distribution",
)
for stale_fragment in (
    "Require two-factor authentication and disallow tokens",
    "npm trust github",
    "npm trust list",
):
    if stale_fragment in npm_distribution:
        raise SystemExit(
            f"{npm_distribution_path}: stale Trusted Publishing instruction remains: {stale_fragment}"
        )

print("RC3 public documentation contract: PASS")
PY
