#!/bin/sh

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
if [ "$#" -gt 1 ]; then
    echo "usage: $0 [release-workflow]" >&2
    exit 2
fi
workflow=${1:-"$repo_root/.github/workflows/release.yml"}

python3 - "$workflow" <<'PY'
from pathlib import Path
import sys

workflow = Path(sys.argv[1])
lines = workflow.read_text(encoding="utf-8").splitlines()

commands = [
    index
    for index, line in enumerate(lines)
    if line.lstrip().startswith('gh release create ')
]
if len(commands) != 1:
    raise SystemExit(
        f"{workflow}: expected exactly one gh release create command, found {len(commands)}"
    )

command_index = commands[0]
command = lines[command_index]
indent = len(command) - len(command.lstrip())
publish_steps = [
    index
    for index, line in enumerate(lines)
    if line.strip() == "- name: Publish tag-bound release assets"
]
if len(publish_steps) != 1:
    raise SystemExit(
        f"{workflow}: expected exactly one publish step, found {len(publish_steps)}"
    )
publish_step_index = publish_steps[0]
publish_step_line = lines[publish_step_index]
publish_step_indent = len(publish_step_line) - len(publish_step_line.lstrip())
if (
    "\t" in publish_step_line[:publish_step_indent]
    or publish_step_line != publish_step_line.rstrip(" \t")
):
    raise SystemExit(
        f"{workflow}:{publish_step_index + 1}: publish step indentation must use spaces "
        "without trailing whitespace"
    )
run_index = next(
    (
        index
        for index in range(command_index - 1, -1, -1)
        if lines[index].strip() == "run: |"
    ),
    None,
)
if run_index is None:
    raise SystemExit(f"{workflow}:{command_index + 1}: publisher run block was not found")
run_line = lines[run_index]
run_indent = len(run_line) - len(run_line.lstrip())
if "\t" in run_line[:run_indent] or run_line != run_line.rstrip(" \t"):
    raise SystemExit(
        f"{workflow}:{run_index + 1}: publisher run block indentation must use spaces "
        "without trailing whitespace"
    )
if not (
    publish_step_index < run_index < command_index
    and publish_step_indent == 6
    and run_indent == 8
    and indent == 10
):
    raise SystemExit(
        f"{workflow}:{command_index + 1}: publish step, run block, and release command "
        "must retain their fixed YAML indentation"
    )
content_indent = 10
for index, raw_line in enumerate(
    lines[publish_step_index + 1 : run_index], publish_step_index + 1
):
    if not raw_line.strip():
        continue
    leading_length = len(raw_line) - len(raw_line.lstrip(" \t"))
    if "\t" in raw_line[:leading_length] or leading_length <= publish_step_indent:
        raise SystemExit(
            f"{workflow}:{index + 1}: publish run block moved into another workflow step"
        )
for index, raw_line in enumerate(lines[run_index + 1 : command_index], run_index + 1):
    if not raw_line.strip():
        continue
    leading_length = len(raw_line) - len(raw_line.lstrip(" \t"))
    if "\t" in raw_line[:leading_length] or leading_length < content_indent:
        raise SystemExit(
            f"{workflow}:{index + 1}: publisher run block ended before the release command"
        )

expected_invocation = [
    'gh release create "$RELEASE_TAG" release/* \\',
    '--repo "$GITHUB_REPOSITORY" \\',
    "--verify-tag \\",
    "--generate-notes \\",
    '--title "XGENy ${RELEASE_TAG#v}" \\',
    '"${release_flags[@]}"',
]
actual_lines = lines[command_index : command_index + len(expected_invocation)]
if len(actual_lines) != len(expected_invocation):
    raise SystemExit(f"{workflow}:{command_index + 1}: truncated release invocation")

actual_invocation = []
for offset, raw_line in enumerate(actual_lines):
    if raw_line != raw_line.rstrip(" \t"):
        raise SystemExit(
            f"{workflow}:{command_index + offset + 1}: trailing whitespace is not "
            "allowed in the release invocation"
        )
    leading_length = len(raw_line) - len(raw_line.lstrip(" \t"))
    leading = raw_line[:leading_length]
    if "\t" in leading:
        raise SystemExit(
            f"{workflow}:{command_index + offset + 1}: release indentation must use spaces"
        )
    if leading_length < content_indent or (offset > 0 and leading_length < indent):
        raise SystemExit(
            f"{workflow}:{command_index + offset + 1}: release invocation escaped its "
            "YAML run block"
        )
    actual_invocation.append(raw_line[leading_length:])

if actual_invocation != expected_invocation:
    raise SystemExit(
        f"{workflow}:{command_index + 1}: release invocation must exactly select the "
        "repository, verify the remote tag, generate notes, set the title, and pass only "
        "the classified release flags"
    )

print("release publisher context contract: PASS")
PY
