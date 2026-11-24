#!/usr/bin/env bash
# Agent factory dispatcher - one run = claim one work item, work it in a container, publish result, exit.
# Usage: factory.sh <slot-number>      (called by factory-run.sh on each timer tick)
# Exit codes: 0 = item processed, 10 = nothing to do (queue empty or paused), other = failure
#
# Work sources, in priority order, across all REPOS:
#   1. open PRs labeled  agent-revise  -> apply the owner's review comments on the existing branch
#   2. open issues labeled agent-ready, authored by TRUSTED_USER -> implement, open a PR
#
# The container never gets a GitHub token. This script (on the host) does every GitHub action.
set -euo pipefail

SLOT=${1:?slot number required}
CONF=${CONF:-/etc/agent-factory/factory.env}
# shellcheck source=/dev/null
source "$CONF"

: "${REPOS:?set REPOS=\"owner/repo owner/repo2\" in $CONF}"
: "${TRUSTED_USER:?set TRUSTED_USER=<your github login> in $CONF}"
: "${GH_TOKEN:?set GH_TOKEN=<bot fine-grained PAT> in $CONF}"
export GH_TOKEN

FACTORY_ROOT=${FACTORY_ROOT:-/srv/factory}
WORK_ROOT="$FACTORY_ROOT/work"
LOG_ROOT="$FACTORY_ROOT/logs"
LOCK_DIR="$FACTORY_ROOT/locks"
IMAGE=${IMAGE:-agent-factory:latest}
BASE_BRANCH=${BASE_BRANCH:-main}
PROMPT_DIR=${PROMPT_DIR:-/opt/agent-factory/prompts}
CLAUDE_ENV=${CLAUDE_ENV:-/etc/agent-factory/claude.env}
CONTAINER_MEMORY=${CONTAINER_MEMORY:-6g}
CONTAINER_CPUS=${CONTAINER_CPUS:-2}
REQUEST_REVIEW=${REQUEST_REVIEW:-true}

mkdir -p "$WORK_ROOT" "$LOG_ROOT" "$LOCK_DIR"
log() { echo "[$(date -Is)] [slot $SLOT] $*"; }

# Kill switch: touch /srv/factory/PAUSE to stop picking up new work.
if [[ -e "$FACTORY_ROOT/PAUSE" ]]; then log "paused"; exit 10; fi

# ---------------------------------------------------------------- claim (atomic across slots)
claim() {
  exec {cl}>"$LOCK_DIR/claim.lock"
  flock "$cl"
  local repo n
  for repo in $REPOS; do
    n=$(gh pr list -R "$repo" --label agent-revise --state open --json number --jq '.[0].number // empty')
    if [[ -n "$n" ]]; then
      gh pr edit "$n" -R "$repo" --remove-label agent-revise --add-label agent-working >/dev/null
      echo "revise $repo $n"; return
    fi
  done
  for repo in $REPOS; do
    n=$(gh issue list -R "$repo" --label agent-ready --author "$TRUSTED_USER" --state open \
          --json number --jq 'sort_by(.number) | .[0].number // empty')
    if [[ -n "$n" ]]; then
      gh issue edit "$n" -R "$repo" --remove-label agent-ready --add-label agent-working >/dev/null
      echo "issue $repo $n"; return
    fi
  done
}

KIND='' REPO='' NUM=''
read -r KIND REPO NUM < <(claim) || true
[[ -n "$KIND" ]] || exit 10

TARGET='issue'; [[ $KIND == revise ]] && TARGET='pr'
JOB="${REPO//\//__}-$NUM"
JOB_DIR="$WORK_ROOT/$JOB"
SRC="$JOB_DIR/src"; TASK="$JOB_DIR/task"; OUT="$JOB_DIR/out"
LOG="$LOG_ROOT/$JOB-$(date +%Y%m%d-%H%M%S).log"

fail() {
  trap - ERR
  log "FAILED $KIND $REPO#$NUM: $*"
  gh "$TARGET" edit "$NUM" -R "$REPO" --remove-label agent-working --add-label agent-blocked >/dev/null 2>&1 || true
  gh "$TARGET" comment "$NUM" -R "$REPO" \
    --body "Agent factory could not finish: $*"$'\n\n'"Log on the Pi: \`$LOG\`" >/dev/null 2>&1 || true
  docker rm -f "factory-$SLOT" >/dev/null 2>&1 || true
  exit 1
}
trap 'fail "unexpected error at factory.sh line $LINENO"' ERR

rm -rf "$JOB_DIR"; mkdir -p "$TASK" "$OUT"
log "start $KIND $REPO#$NUM"

# ---------------------------------------------------------------- prepare workspace + task text
if [[ $KIND == issue ]]; then
  BRANCH="agent/issue-$NUM"
  gh repo clone "$REPO" "$SRC" -- --quiet
  git -C "$SRC" switch --quiet -c "$BRANCH"
  # Only the owner's text reaches the agent (prompt-injection guard).
  gh issue view "$NUM" -R "$REPO" --json number,title,body,comments \
    | jq -r --arg u "$TRUSTED_USER" '
        "# Issue #\(.number): \(.title)\n\n\(.body // "")\n\n## Follow-up comments from the owner\n"
        + ([.comments[] | select(.author.login == $u) | "- " + .body] | join("\n"))' \
    > "$TASK/task.md"
  sed "s/{{NUM}}/$NUM/g; s|{{BRANCH}}|$BRANCH|g" "$PROMPT_DIR/implement.md" > "$TASK/prompt.md"
else
  BRANCH=$(gh pr view "$NUM" -R "$REPO" --json headRefName --jq .headRefName)
  [[ $BRANCH == agent/* ]] || fail "PR branch '$BRANCH' is not an agent/* branch"
  gh repo clone "$REPO" "$SRC" -- --quiet --branch "$BRANCH"
  {
    gh pr view "$NUM" -R "$REPO" --json number,title,body,reviews,comments \
      | jq -r --arg u "$TRUSTED_USER" '
          "# PR #\(.number): \(.title)\n\n\(.body // "")\n\n## Review summaries from the owner\n"
          + ([.reviews[] | select(.author.login == $u and .body != "") | "- " + .body] | join("\n"))
          + "\n\n## PR conversation comments from the owner\n"
          + ([.comments[] | select(.author.login == $u) | "- " + .body] | join("\n"))'
    printf '\n## Inline review comments from the owner\n'
    gh api "repos/$REPO/pulls/$NUM/comments" --paginate \
      | jq -r --arg u "$TRUSTED_USER" \
          '.[] | select(.user.login == $u) | "- \(.path):\(.line // .original_line // "?"): \(.body)"'
  } > "$TASK/task.md"
  sed "s/{{NUM}}/$NUM/g; s|{{BRANCH}}|$BRANCH|g" "$PROMPT_DIR/revise.md" > "$TASK/prompt.md"
fi
BASE_SHA=$(git -C "$SRC" rev-parse HEAD)

# ---------------------------------------------------------------- run the agent (isolated)
set +e
docker run --rm --name "factory-$SLOT" \
  --memory "$CONTAINER_MEMORY" --cpus "$CONTAINER_CPUS" --pids-limit 2048 \
  --security-opt no-new-privileges --cap-drop ALL \
  -v "$SRC:/work" -v "$TASK:/task:ro" -v "$OUT:/out" \
  -v factory-gradle:/home/agent/.gradle -v factory-npm:/home/agent/.npm \
  --env-file "$CLAUDE_ENV" \
  "$IMAGE" > "$LOG" 2>&1
RC=$?
set -e
docker rm -f "factory-$SLOT" >/dev/null 2>&1 || true

STATUS=$(tr -d '[:space:]' < "$OUT/status" 2>/dev/null || echo missing)
NEW_COMMITS=$(git -C "$SRC" rev-list --count "$BASE_SHA..HEAD")
log "agent exit=$RC status=$STATUS new_commits=$NEW_COMMITS"

# ---------------------------------------------------------------- publish
if [[ $STATUS == blocked ]]; then
  [[ -s "$OUT/question.md" ]] || echo "Agent is blocked but left no question. See log: $LOG" > "$OUT/question.md"
  gh "$TARGET" comment "$NUM" -R "$REPO" --body-file "$OUT/question.md" >/dev/null
  gh "$TARGET" edit "$NUM" -R "$REPO" --remove-label agent-working --add-label agent-blocked >/dev/null
  log "blocked, question posted"; exit 0
fi

[[ $STATUS == 'done' && $NEW_COMMITS -gt 0 ]] \
  || fail "no usable result (exit=$RC, status=$STATUS, new commits=$NEW_COMMITS)"

# Guardrails the agent cannot talk its way around.
CHANGED=$(git -C "$SRC" diff --name-only "$BASE_SHA..HEAD")
if grep -qE '^\.github/(workflows|actions)/|^CODEOWNERS$|^\.github/CODEOWNERS$' <<<"$CHANGED"; then
  fail "agent changed CI or CODEOWNERS files - refusing to push"
fi

if [[ $KIND == issue ]]; then
  git -C "$SRC" push --quiet --force -u origin "$BRANCH"
  [[ -s "$OUT/pr.md" ]] || echo "Automated implementation of #$NUM." > "$OUT/pr.md"
  printf '\n\nCloses #%s\n\n_Generated by the agent factory. Human review required._\n' "$NUM" >> "$OUT/pr.md"
  TITLE=$(head -n1 "$OUT/pr-title.txt" 2>/dev/null || true)
  [[ -n "$TITLE" ]] || TITLE=$(gh issue view "$NUM" -R "$REPO" --json title --jq .title)
  PR=$(gh pr list -R "$REPO" --head "$BRANCH" --state open --json number --jq '.[0].number // empty')
  if [[ -z "$PR" ]]; then
    args=(--base "$BASE_BRANCH" --head "$BRANCH" --title "$TITLE" --body-file "$OUT/pr.md" --label agent-pr)
    [[ $REQUEST_REVIEW == true ]] && args+=(--reviewer "$TRUSTED_USER")
    gh pr create -R "$REPO" "${args[@]}" >/dev/null
  else
    gh pr comment "$PR" -R "$REPO" --body-file "$OUT/pr.md" >/dev/null
  fi
  gh issue edit "$NUM" -R "$REPO" --remove-label agent-working --add-label agent-in-review >/dev/null
else
  git -C "$SRC" push --quiet origin "$BRANCH"
  [[ -s "$OUT/pr.md" ]] || echo "Addressed review comments." > "$OUT/pr.md"
  gh pr comment "$NUM" -R "$REPO" --body-file "$OUT/pr.md" >/dev/null
  gh pr edit "$NUM" -R "$REPO" --remove-label agent-working >/dev/null
fi

# ---------------------------------------------------------------- screenshots -> issue comment
# Needs gh >= 2.99 (--attach). Branch name agent/issue-N maps a revise run back to its issue.
ISSUE_NUM=${BRANCH#agent/issue-}
PR_NUM=$(gh pr list -R "$REPO" --head "$BRANCH" --state open --json number --jq '.[0].number // empty')
mapfile -t SHOTS < <(find "$OUT/screenshots" -maxdepth 1 -type f \
  \( -iname '*.png' -o -iname '*.jpg' -o -iname '*.jpeg' -o -iname '*.gif' -o -iname '*.webm' -o -iname '*.mp4' \) \
  -size -10M 2>/dev/null | sort | head -n "${MAX_SCREENSHOTS:-10}")
if (( ${#SHOTS[@]} > 0 )) && [[ $ISSUE_NUM =~ ^[0-9]+$ ]]; then
  {
    echo "### Agent UI verification${PR_NUM:+ for PR #$PR_NUM}"
    if [[ -s "$OUT/screenshots/README.md" ]]; then echo; cat "$OUT/screenshots/README.md"; fi
  } > "$OUT/shots-comment.md"
  attach=()
  for f in "${SHOTS[@]}"; do attach+=(--attach "$f#$(basename "$f")"); done
  gh issue comment "$ISSUE_NUM" -R "$REPO" --body-file "$OUT/shots-comment.md" "${attach[@]}" >/dev/null \
    || log "screenshot upload failed (non-fatal)"
fi

trap - ERR
rm -rf "$JOB_DIR"
log "done $KIND $REPO#$NUM"
