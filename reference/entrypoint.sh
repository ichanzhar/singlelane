#!/usr/bin/env bash
# Runs inside the container. No GitHub credentials exist here: the agent edits and commits only.
set -euo pipefail

git config --global user.name  "${GIT_AUTHOR_NAME:-agent-factory}"
git config --global user.email "${GIT_AUTHOR_EMAIL:-agent-factory@users.noreply.github.com}"
git config --global --add safe.directory /work

cd /work
exec timeout --kill-after=1m "${JOB_TIMEOUT:-90m}" \
  claude -p "$(cat /task/prompt.md)" \
    --dangerously-skip-permissions \
    --model "${CLAUDE_MODEL:-opus}" \
    --max-turns "${MAX_TURNS:-200}" \
    --output-format stream-json --verbose
