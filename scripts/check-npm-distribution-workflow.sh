#!/bin/sh

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
if [ "$#" -gt 1 ]; then
    echo "usage: $0 [release-workflow]" >&2
    exit 2
fi
workflow=${1:-"$repo_root/.github/workflows/release.yml"}

python3 - \
    "$workflow" \
    "$repo_root/npm/scripts/publish.mjs" \
    "$repo_root/npm/scripts/smoke.mjs" <<'PY'
from pathlib import Path
import re
import sys

workflow = Path(sys.argv[1])
publisher = Path(sys.argv[2])
smoke = Path(sys.argv[3])
lines = workflow.read_text(encoding="utf-8").splitlines()
text = "\n".join(lines)
publisher_text = publisher.read_text(encoding="utf-8")
smoke_text = smoke.read_text(encoding="utf-8")


def fail(message: str) -> None:
    raise SystemExit(f"{workflow}: {message}")


for secret_marker in ("NODE_AUTH_TOKEN", "NPM_TOKEN", "_authToken", "npm_password"):
    if secret_marker.lower() in text.lower():
        fail(f"long-lived npm credential marker is forbidden: {secret_marker}")

job_starts = [index for index, line in enumerate(lines) if line == "  publish-npm:"]
if len(job_starts) != 1:
    fail(f"expected one publish-npm job, found {len(job_starts)}")
job_start = job_starts[0]
job_end = next(
    (
        index
        for index in range(job_start + 1, len(lines))
        if re.fullmatch(r"  [A-Za-z0-9_-]+:", lines[index])
    ),
    len(lines),
)
job = "\n".join(lines[job_start:job_end])

required_job_fragments = (
    "    needs: publish",
    "      contents: read",
    "      id-token: write",
    "XGENY_NPM_PUBLISH_ENABLED",
    "test \"$NPM_PUBLISH_ENABLED\" = \"true\"",
    "node-version: 24.20.0",
    "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
    "node npm/scripts/verify-release.mjs",
    "node npm/scripts/publish.mjs",
)
for fragment in required_job_fragments:
    if fragment not in job:
        fail(f"publish-npm job is missing required contract: {fragment}")

if "contents: write" in job:
    fail("publish-npm job must not have GitHub contents write permission")
if job.count("node npm/scripts/publish.mjs") != 1:
    fail("publish-npm job must invoke the npm publisher exactly once")
if job.index("node npm/scripts/verify-release.mjs") > job.index("node npm/scripts/publish.mjs"):
    fail("npm release bundle verification must happen before publication")

gate_step = text.find("- name: Require npm Trusted Publishing readiness")
build_job = text.find("\n  build:")
if gate_step < 0 or build_job < 0 or gate_step > build_job:
    fail("npm readiness must fail closed in the release quality gate before native builds")

verify_starts = [index for index, line in enumerate(lines) if line == "  verify-published-npm:"]
if len(verify_starts) != 1:
    fail(f"expected one verify-published-npm job, found {len(verify_starts)}")
verify_start = verify_starts[0]
verify_job = "\n".join(lines[verify_start:])
for fragment in (
    "    needs: publish-npm",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "--include=optional --ignore-scripts",
    '"@xgen/cli@$version"',
    '"@xgen/cli@$Version"',
    "printf '/status\\n/exit\\n'",
    "status: idle",
    'npm uninstall --global --prefix "$test_root/install"',
    "& npm.cmd uninstall --global --prefix $InstallRoot",
    "npm uninstall left package artifacts",
):
    if fragment not in verify_job:
        fail(f"published npm verification is missing: {fragment}")

for fragment in (
    "const { launcher, reports, version } = await verifyRelease(directory, tag);",
    "for (const report of publicationOrder(reports, launcher.name))",
    "...reports.filter(({ name }) => name !== launcherName)",
    "launcherReports[0]",
    "'--provenance'",
    "shell: false",
    "dist.integrity",
    "dist.attestations",
):
    if fragment not in publisher_text:
        fail(f"npm publisher is missing required safety contract: {fragment}")

for fragment in (
    "const uninstall = npmInvocation([",
    "await assert.rejects(lstat(installedLauncher)",
    "await assert.rejects(lstat(installedPlatform)",
    "npm install/reinstall/remove smoke: PASS",
):
    if fragment not in smoke_text:
        fail(f"npm package smoke is missing lifecycle verification: {fragment}")

install_invocation = "await run(install.command, install.args, { env: npmEnvironment });"
if smoke_text.count(install_invocation) != 2:
    fail("npm package smoke must install the exact package twice")

print("npm release workflow contract: PASS")
PY
