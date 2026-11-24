# singlelane - Specification (v1)

Single source of truth for the Rust implementation. If the implementation must deviate,
update this file and add an entry to the Decision log at the bottom in the same commit.

## 1. Purpose

Personal tool: one owner, the owner's own repos, low volume, and exactly one task in flight
from pickup until the owner merges its PR (G10).

Turn GitHub issues into pull requests autonomously on a dedicated Raspberry Pi 5
(16 GB RAM, 4 cores, aarch64, Linux, NVMe). Claude Code runs headless inside a per-job
container stack. The only human steps are: writing the issue, answering questions when the
agent is blocked, reviewing, and merging.

Target repo shape: a monorepo with a Kotlin/Spring Boot backend, a Next.js storefront and a
Next.js admin panel. The agent must be able to start the stack and drive a headless browser
(Playwright) to verify UI changes.

## 2. Non-negotiable guarantees

Each guarantee must have at least one automated test whose name starts with its id
(for example `g2_container_env_has_no_github_token`).

- **G1 Trusted input only.** Only issues authored by `trusted_user` are picked up. Only text
  authored by `trusted_user` is passed to the agent: issue body, issue comments, PR review
  summaries, inline review comments, PR conversation comments. Everything else is dropped.
- **G2 No GitHub credentials in the agent stack.** No GitHub App private key, JWT or
  installation token in any container env, mounted file, or the cloned repo's `.git/config` /
  remote URL. All GitHub operations happen in the host process.
- **G3 Never merge, never approve.** The factory never merges, approves, or pushes to the
  base branch or to any branch outside `agent/`. Force push is allowed only when creating a
  fresh `agent/issue-<n>` branch. Revise runs push fast-forward only.
- **G4 Protected paths.** If the diff since the job's base commit touches any path matching
  the repo's `protected_paths` globs, nothing is pushed and the item goes to blocked with an
  explanation.
- **G5 Single claim.** One daemon instance per host (lock file). An item is claimed by at most
  one job. Claim = label swap, then re-read to verify.
- **G6 Always a visible end state.** Every job ends with a label change and a comment:
  in-review, blocked (question), or blocked (failure). This includes timeouts, panics, and
  daemon restarts (startup recovery, section 6.7).
- **G7 Circuit breaker.** After `circuit_breaker_failures` consecutive counted failures
  (section 6.8), the factory pauses itself and notifies. Fatal failures (expired or invalid
  credentials, missing image, disk full) pause it immediately on the first occurrence.
  Network failures and "blocked with question" outcomes never count. This stops a broken
  setup from marking the whole queue as blocked.
- **G8 No secret leakage.** Tokens never appear in logs, comments, PR bodies, Telegram
  messages, or error messages. Redact known secret values and secret-shaped strings (`ghs_`, `ghp_`, `github_pat_`,
  `sk-ant-`, JWTs, PEM blocks) before writing any externally visible text.
- **G9 Working hours.** No new job starts outside working hours in the configured time zone.
  Running jobs finish normally.
- **G10 One task in flight.** Across all configured repos, at most one task is in flight
  (section 3.1), from pickup until its PR is merged or the owner releases it. No new issue is
  picked up while a task is in flight, including while its PR waits for review or while it is
  blocked. Only the in-flight task's own revise requests run. At most one agent stack exists on
  the host at any moment. Fixed in code, not configurable. Tests prove that no new issue is
  claimed while a task is running, blocked, or in review, and that overlapping ticks cannot
  start a second job.

## 3. Workflow and state machine

Labels (created by `singlelane setup-labels`):

| Label | Meaning |
|---|---|
| `agent-ready` | Issue queued for the factory |
| `agent-working` | A job is running on this issue or PR |
| `agent-blocked` | Needs a human: a question from the agent, or a failure |
| `agent-in-review` | Issue has an open agent PR waiting for human review |
| `agent-revise` | On a PR: apply the owner's review comments |
| `agent-pr` | Marks PRs opened by the factory |

Transitions:

- Issue: `agent-ready` -> `agent-working` -> `agent-in-review` (PR opened) or `agent-blocked`.
- PR: `agent-revise` -> `agent-working` -> label removed (done) or `agent-blocked`.
- Human: answers a blocked issue and sets `agent-ready` again; reviews a PR and sets
  `agent-revise`; approves and merges (merge closes the issue via `Closes #n`).
- PR closed without merge: the factory removes the agent labels from the issue and comments
  "PR closed without merge. Add `agent-ready` to retry." The task is no longer in flight.
- Issue closed by the owner (with or without a PR): the task is no longer in flight. The
  factory closes its open agent PR for that issue, if any, with a comment, and never deletes
  branches.

### 3.1 In-flight task (G10)
On every tick, before claiming anything, the factory derives the in-flight task from GitHub,
which is the source of truth (no local state is trusted for this):
- an open issue labeled `agent-working`, `agent-blocked` or `agent-in-review`, or
- an open PR labeled `agent-pr`, which belongs to the issue in its `agent/issue-<n>` branch.

Rules:
- None in flight: claim the next `agent-ready` issue. The issue that was in flight most
  recently (per the history store) goes first if it is back in `agent-ready`, for example after
  you answered its question. Otherwise take the lowest-numbered issue, across repos in config
  order. The history store only decides this priority; whether something is in flight always
  comes from GitHub.
- One in flight: the only allowed job is a revise run on that task's PR when it has
  `agent-revise`. `agent-ready` issues stay queued. A blocked task waits for the owner.
- More than one in flight (should never happen, for example after manual label edits): claim
  nothing, pause, and notify with the list. The owner resolves it.
- The task is released when its PR is merged (the issue closes via `Closes #n`), its PR is
  closed without merge, or the owner closes the issue. The next tick then picks the next issue.
- An `agent-revise` label on any PR other than the in-flight task's PR is ignored and
  reported once in the daily summary.
- Revise jobs only run on PRs whose head branch starts with `agent/`.
- An issue carrying both `agent-ready` and `agent-blocked` counts as queued, not blocked: the
  owner answered and requeued it without removing `agent-blocked`. The claim removes both.
  Any other combination of two workflow labels counts as in flight and is reported.

Model the transitions as a pure state machine in `singlelane-core` with exhaustive unit tests.

## 4. Architecture

Cargo workspace:

| Crate | Kind | Responsibility |
|---|---|---|
| `singlelane-core` | lib | Config types and validation, state machine, policy (trusted-author filter, protected-path guard, redaction), failure classification, schedule and working-hours logic, prompt and task rendering, notification message formatting, `doctor` check result format. Pure, no I/O. |
| `singlelane-github` | lib | GitHub App auth and `TokenProvider`, `GitHub` trait plus an octocrab-based implementation and an in-memory fake for tests, claim logic, the GitHub `doctor` checks. |
| `singlelane-exec` | lib | `ProcessRunner` trait (git, docker compose, gh) plus a real implementation and a recording fake. Workspace management, compose rendering, job executor. |
| `singlelane` | bin | CLI and daemon (`singlelane run`, etc.). History store. |
| `agent-runner` | bin | Runs inside the agent container, replaces the old entrypoint script. |

Rules:
- All external I/O goes through traits so the job flow can be tested end to end with fakes.
- External processes are only `git`, `docker` (compose plugin) and `gh` (attachments only).
  Always pass arguments as a vector, never through a shell.
- No `unsafe`. No `unwrap`/`expect` outside tests and startup config validation.
- TLS via rustls only (no OpenSSL), so cross-compiling to aarch64 stays simple.
- Use current stable crate versions. Check APIs on docs.rs instead of guessing.

## 5. Configuration

File: `/etc/singlelane/config.toml` (path overridable with `--config`).
Secrets never live in the TOML:
- GitHub App private key: a PEM file at `github.private_key_path`, mode 600, owned by the
  `singlelane` user. Refuse to start if it is readable by group or others.
- `claude_env_file`: env file passed to the agent container. It must contain exactly one of
  `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`. Refuse to start if both or neither are set.
- `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`: optional. If either is missing, Telegram is off
  and `doctor` reports WARN.

```toml
trusted_user = "your-github-login"
timezone = "Europe/Kyiv"
work_hours = { start = "07:00", end = "23:00" }
poll_interval = "15m"          # ticks aligned to wall clock: :00 :15 :30 :45
max_attempts = 2               # per item, counted in the history store; then blocked
circuit_breaker_failures = 3
shutdown_grace = "10m"
keep_failed_workspaces = "3d"
data_dir = "/var/lib/singlelane" # work/, logs/, history.db, PAUSE, singlelane.lock
claude_env_file = "/etc/singlelane/claude.env"

[image]                        # nightly agent image update, see 7.1
update_at = "03:00"            # in `timezone`; omit to disable
claude_channel = "stable"      # "stable" (about a week old, skips bad releases) or "latest"
dockerfile_dir = "/opt/singlelane/image"
probe_claude = false           # true = one-turn Claude call in the smoke test

[github]                       # GitHub App, see 5.1
app_id = 123456
installation_id = 12345678
private_key_path = "/etc/singlelane/github-app.pem"
claude_token_created = "2026-11-23"  # optional; date you ran `claude setup-token` (tokens last 1 year)

[telegram]                     # secrets come from env, see above
notify = ["pr_opened", "blocked", "failed", "paused", "resumed", "token_expiry", "network_restored", "review_reminder", "task_released", "image_updated", "image_update_failed", "daily_summary"]
daily_summary_at = "21:00"     # in `timezone`; omit to disable
review_reminder_after = "2d"   # remind once per period while the in-flight PR waits for review
commands = true                # accept /status /pause /resume from TELEGRAM_CHAT_ID only
network_alert_after = "2h"     # report an outage longer than this once connectivity is back

[agent]
image = "singlelane-agent:latest"
model = "opus"
max_turns = 200
job_timeout = "90m"
memory = "12g"                 # one job at a time, so it can use most of the Pi
cpus = 3.5
shm_size = "1g"
max_screenshots = 10
max_screenshot_bytes = 10_000_000
# implement_prompt = "/etc/singlelane/prompts/implement.md"  # optional template overrides (6.4)
# revise_prompt = "/etc/singlelane/prompts/revise.md"

[[repo]]
name = "owner/monorepo"
base_branch = "main"
request_review = true
protected_paths = [".github/workflows/**", ".github/actions/**", "CODEOWNERS", ".github/CODEOWNERS"]

  [[repo.sidecar]]               # defined here, NOT in the repo, so the agent cannot change its own sandbox
  name = "db"
  image = "postgres:17"
  env = { POSTGRES_PASSWORD = "dev", POSTGRES_DB = "app" }
  tmpfs = ["/var/lib/postgresql/data"]
```

Validation at startup: time zone exists, `start < end`, repo names are
`owner/name`, globs compile, durations parse, `data_dir` is writable.
Durations use the friendly format (`15m`, `90m`, `2h`, `1h30m`, `3d`; a day is 24 h).
`poll_interval` must be whole minutes and divide 24 h. Unknown keys are errors. An omitted
`[image]` or `[telegram]` section gets the defaults shown above (`update_at = "03:00"`,
`daily_summary_at = "21:00"`); inside a present section, omitting `update_at` or
`daily_summary_at` disables that job. An omitted `protected_paths` gets the four defaults above.

### 5.1 GitHub App

singlelane authenticates as a GitHub App owned by the user, installed only on the target
repositories. Fine-grained PATs cannot be used: they don't work for an account that is only a
collaborator on repos owned by another personal account, and a PAT of the owner would make the
owner the PR author, who cannot then approve the PR.

App settings (documented in the README with screenshots-free step by step text):
- Repository permissions: Metadata read (mandatory), Contents read and write, Issues read and
  write, Pull requests read and write. **Nothing else.** In particular no Workflows,
  Administration, Actions, Secrets, Environments or Webhooks permissions.
- Webhook: inactive (singlelane polls).
- Installable only on the owner's account; installed with "Only select repositories".

Token handling:
- Sign a short-lived JWT with the private key (RS256, `iat` = now - 60 s, `exp` = now + 9 min,
  `iss` = app id), exchange it for an installation access token
  (valid about 1 hour), cache it in memory only, and refresh it when less than 10 minutes remain.
  Mint a fresh token right before publish, since jobs can run longer than an hour.
- REST calls retry 5xx on idempotent methods (GET, PUT, PATCH, DELETE) with exponential backoff,
  and rate limits (429, or 403 with `Retry-After`, `x-ratelimit-remaining: 0` or a rate-limit
  message) on any method, waiting `Retry-After`, else until `x-ratelimit-reset`, else 60 s. A POST
  that failed with 5xx is not retried (it may have been applied). A wait longer than 90 s is not
  taken: the error is returned as a rate limit (`network`, 6.8). Logs carry the
  `x-github-request-id`, never a credential. Pagination follows `Link: rel="next"` only to the API
  host.
- Comment bodies and PR titles and bodies are redacted (G8) inside the GitHub client, as a
  backstop to redaction by the caller.
- The token is used for the REST API, for git over HTTPS (`x-access-token` basic auth, passed
  per command as in 6.3) and as `GH_TOKEN` in the env of the `gh` child process only.
- Git identity for agent commits and pushes is the app's bot user: name `<app-slug>[bot]`,
  email `<bot-user-id>+<app-slug>[bot]@users.noreply.github.com`. Both are looked up at
  startup (`GET /app`, then `GET /users/<app-slug>[bot]`) and passed to the agent container
  as plain env vars.
- `trusted_user` must not be the app's bot user (startup check).

## 6. Job lifecycle (host side)

### 6.1 Claim
Swap the queue label for `agent-working`, re-read the item, and abort the job if the label
set is not as expected. Record the attempt in the history store. If attempts exceed
`max_attempts`, move to `agent-blocked` with a comment instead of running.

Details: the claim first checks the item is open and in its queue state (issue: `agent-ready`,
authored by `trusted_user`; PR: `agent-revise`, head on an `agent/` branch of the same repo). It
then removes the queue label first: GitHub answers 404 when the label is already gone, so of two
racing claimers only one proceeds (the other stops before adding anything). It adds
`agent-working`, removes a leftover `agent-blocked`, and re-reads: the item must be open and carry
`agent-working` and no other workflow label (`agent-pr` aside). If not, the job does not run and no
labels are rolled back; the in-flight rules (3.1) report the conflict.

### 6.2 Workspace
`<data_dir>/work/<job-id>/{src,task,out}`. Job id: `<owner>__<repo>-<n>-<timestamp>`.

### 6.3 Git (G2)
- Clone over HTTPS without credentials in the URL. Authenticate each git command with the
  current installation token via `-c http.extraHeader=...` or a `GIT_ASKPASS` helper set only
  in the child process env. After cloning, assert that `.git/config` contains no token. This
  is test G2.
- Issue jobs: fresh clone of the base branch, create `agent/issue-<n>`.
- Revise jobs: clone the PR head branch.
- Record the base commit SHA before the agent runs.

### 6.4 Task and prompt rendering (G1)
- `task/task.md`: issue or PR text filtered to `trusted_user` only (formats as in the
  reference script: title, body, owner comments; for revise also review summaries and inline
  comments as `- path:line: body`).
- `task/prompt.md`: rendered from `assets/agent-prompts/implement.md` or `revise.md`,
  replacing `{{NUM}}` and `{{BRANCH}}`. Templates are embedded with `include_str!`, and
  `agent.implement_prompt` / `agent.revise_prompt` can override them with files.

### 6.5 Agent stack (docker compose)
Render a compose file per job into the workspace. Project name `singlelane-<short-id>`.
Before starting, check that no other `singlelane-*` compose project exists (G10). If one does,
it is a leftover: tear it down, recover its item (6.7), then continue.
- Service `agent`: `agent.image`; mounts `src:/work`, `task:/task:ro`, `out:/out`;
  `env_file: claude_env_file` plus non-secret env (model, max turns, timeout, git identity);
  memory and cpu limits; `shm_size`; `security_opt: [no-new-privileges:true]`;
  `cap_drop: [ALL]`; named volumes for Gradle and npm caches; depends on sidecars.
- Sidecars from repo config on the same private project network.
- Run with `docker compose up --abort-on-container-exit --exit-code-from agent`, wrapped in
  the job timeout plus a margin.
- Teardown is mandatory on every path, including timeout, error, and panic:
  `docker compose down -v --remove-orphans` (use a guard plus explicit calls).

### 6.6 Publish
Read `out/status` (`done` | `blocked` | anything else = failed) and `out/result.json`.
- `blocked`: post `out/question.md` (or a default message) as a comment, set `agent-blocked`.
- `done` requires at least one new commit since the base SHA, otherwise failed.
- Protected-path guard (G4) on `git diff --name-only <base>..HEAD`.
- Issue job: push `agent/issue-<n>` (force allowed), create the PR if none is open for that
  head (title from `out/pr-title.txt` or the issue title; body from `out/pr.md` plus
  `Closes #n`, a run-stats line from `result.json` and a "human review required" footer;
  label `agent-pr`; request review from `trusted_user` if `request_review`). If a PR exists,
  comment instead. Issue label becomes `agent-in-review`.
- Revise job: fast-forward push, comment `out/pr.md` on the PR, remove `agent-working`.
- Screenshots: up to `max_screenshots` image or video files from `out/screenshots/`
  (png, jpg, jpeg, gif, webm, mp4) under `max_screenshot_bytes`, sorted by name. Post ONE
  comment on the issue (issue number taken from the `agent/issue-<n>` branch, so revise jobs
  work too) with `out/screenshots/README.md` as text, via
  `gh issue comment <n> -R <repo> --body-file <f> --attach <file>...` with the installation
  token as `GH_TOKEN`. There is no REST API for attachments, so this is the only use of `gh`.
  It needs gh >= 2.99.0. Whether `--attach` accepts a GitHub App installation token is
  unverified: phase 4 must test it against a real repo. If it does not, use the fallback:
  commit the files to an orphan branch `singlelane-artifacts` under
  `issue-<n>/<job-id>/` and link them in the comment (`/blob/` links, viewable by anyone with
  repo access). Upload failure is logged and non-fatal either way.
- Failure path: classify the failure (6.8) and follow that class's row. For `agent_failed` and
  `factory_bug`: label `agent-blocked`, comment with a short reason and the local log path.
- Success: delete the workspace. Failure: keep it for `keep_failed_workspaces`.

### 6.7 Startup recovery (G6)
On daemon start, any item labeled `agent-working` with no live job goes back to its queue
label (`agent-ready` or `agent-revise`) with a comment that the factory restarted. The
attempt count still applies, so a crash loop ends in blocked.

### 6.8 Failure classification
Every failed job gets exactly one class. Classification is a pure function in
`singlelane-core` (input: the error source, HTTP status, exit codes, `result.json`, the agent log
tail; output: the class) with a fixture test for every row.

| Class | Typical signals | Item goes to | Breaker | Factory | Telegram |
|---|---|---|---|---|---|
| `network` | DNS failure, connection refused or reset, TLS handshake timeout to GitHub, Anthropic, registries; Telegram itself unreachable | back to its queue label, attempt not counted | not counted | keeps running | silent; one `network_restored` message after an outage longer than `network_alert_after` |
| `claude_auth` | Claude Code reports an invalid or expired API key or OAuth token, HTTP 401/403 from the Anthropic API | back to queue, attempt not counted | n/a | **pauses immediately** | immediate, with re-auth steps |
| `claude_usage_limit` | usage or rate limit reached, with a reset time if Claude reports one | back to queue, attempt not counted | not counted | pauses until the reset time (or 1 h if unknown), then resumes by itself | immediate |
| `github_auth` | JWT rejected, installation token exchange fails, installation removed or suspended, 401 from GitHub, or 403 that is not a rate limit | back to queue if possible | n/a | **pauses immediately** | immediate, with steps: check the app is still installed on the repo, its permissions, and that the private key on disk matches one of the app's keys |
| `infra` | docker daemon down, image missing, disk full, compose error | back to queue, attempt not counted | n/a | **pauses immediately** | immediate |
| `agent_failed` | agent ran but produced no usable result, timeout, tests never green | `agent-blocked` + comment | counted | keeps running | `failed` event |
| `factory_bug` | panic or unexpected internal error | `agent-blocked` + comment | counted | keeps running | `failed` event |

Rules:
- The exact Claude Code auth and usage-limit error messages and result-event fields must be
  checked against the current docs and real runs, and kept as fixtures. Unknown errors fall
  back to `agent_failed`, never to `network`, so a wrong guess can't hide a real failure.
- `failure.json` is written inside the agent container, so a compromised agent could forge it.
  The host confirms `network` itself: it probes GitHub and Anthropic from the host, and only
  accepts `network` if a probe fails. As a backstop, an item requeued as `network` more than
  5 times in 24 h becomes `agent_failed`.
- A GitHub rate limit seen by the host (HTTP 429, or 403 with a rate-limit message) is
  classified `network` without a probe: the host observed it directly, so it can't be forged.
- A pause caused by `claude_auth`, `github_auth` or `infra` is cleared only by `resume`
  (CLI or Telegram). Resume first re-runs the matching `doctor` check (for `claude_auth`,
  the `--with-claude` probe) and refuses with the reason if it still fails.

## 7. agent-runner (inside the container)

- Set the git identity from env and `safe.directory=/work`.
- Run `claude -p <prompt.md> --dangerously-skip-permissions --model <m> --max-turns <n>
  --output-format stream-json --verbose` in `/work`, in its own process group.
- Enforce `JOB_TIMEOUT`: on expiry send SIGTERM to the group, then SIGKILL after 60 s.
- Stream stdout to the container log and to `/out/transcript.jsonl`.
- Parse the final `result` event into `/out/result.json` (turns, cost, duration, subtype).
  Check the real field names against an actual run, tolerate missing fields, and never fail
  the job because of a parse problem.
- If Claude exits without writing `/out/status`, write `failed` and a reason to
  `/out/failure.txt`.
- Exit code: 0 for done or blocked, non-zero for failed.
- For failure classification on the host (6.8), also write `/out/failure.json` when Claude
  fails: exit code, the result event's error subtype and message if present, and the last
  50 lines of stderr (redacted). The runner reports the raw facts; the host classifies.

Agent image (`image/Dockerfile`), multi-stage:
- Builder stage compiles `agent-runner` for the image's libc (or musl static).
- Runtime: `eclipse-temurin:21-jdk-noble`, Node 22, git, jq, ripgrep, Playwright Chromium
  (`npx playwright install --with-deps chromium`), non-root user `agent` with uid 1000,
  `DISABLE_AUTOUPDATER=1`.
- Claude Code from Anthropic's signed apt repository (`downloads.claude.ai/claude-code/apt/`),
  not `curl | bash`. The build verifies the signing key fingerprint
  `31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE` and fails if it differs. Build args:
  `CLAUDE_CHANNEL` (`stable` or `latest`) and `CLAUDE_CACHE_BUST`, placed right before the
  Claude Code install step so a nightly rebuild only redoes that step and the ones after it.
  Check the current Claude Code setup docs for the exact repository lines.
- Labels on the image: Claude Code version, build date, singlelane version.
- Playwright MCP configured at user scope for Claude Code with browser `chromium`, headless,
  isolated. Google Chrome is not available on arm64, and Playwright MCP defaults to the
  Chrome channel. Check the current Claude Code docs for how user-scoped MCP servers are
  configured.
- Copy `assets/settings.json` to `~/.claude/settings.json` and `assets/agents/*` to
  `~/.claude/agents/`.

### 7.1 Nightly image update

`singlelane image-update [--force]`, also run by the daemon daily at `image.update_at`
(default 03:00, outside work hours):
1. Skip if a job is running (never swap the image under a running job). `--force` does not
   override this.
2. Build `<image>:candidate` with `--pull` and `CLAUDE_CACHE_BUST` set to the current date, and
   `CLAUDE_CHANNEL` from config.
3. Smoke test the candidate in a throwaway container: `claude --version`, `agent-runner --version`,
   Playwright Chromium launches headless, the Playwright MCP config is present, and, if
   `image.probe_claude = true`, a one-turn Claude prompt (proves auth, costs a few tokens).
4. Pass: tag the current image `<image>:previous`, the candidate `<image>:latest`; record the
   versions in the history store. Fail: keep the current image, delete the candidate.
5. Prune dangling images and build cache older than 7 days so the disk doesn't fill up.
6. Telegram: `image_updated` (old and new Claude Code version) only when the version changed;
   `image_update_failed` with the failing step on failure. Two failed nights in a row also
   show up in `doctor` as WARN.

`singlelane image-rollback` retags `:previous` as `:latest` (refused while a job is running).
A build failure is never counted by the breaker and never pauses the factory.

## 8. Scheduling and daemon

- `singlelane run`: long-running tokio daemon with a single-instance lock file.
- Ticks aligned to wall clock in `timezone` every `poll_interval`. On a tick, if within work
  hours, not paused (`<data_dir>/PAUSE` absent), the breaker is closed and no job is running:
  claim one item and run it.
- Single worker (G10): one job runner task; ticks that arrive while a job runs are skipped.
  Every claim first evaluates the in-flight rules in 3.1. A new issue is picked up on the first
  tick after the previous task is released (at most `poll_interval` after you merge).
- SIGTERM/SIGINT: stop claiming, wait up to `shutdown_grace` for running jobs, then tear
  down remaining stacks and apply recovery labeling to their items.
- Breaker: tracks consecutive counted failures (6.8). On trip, or on a fatal
  class: create `PAUSE` with the reason and class, notify, and log loudly. A usage-limit pause
  records its end time and clears itself. `singlelane resume` runs the checks from 6.8, then
  clears the pause and resets the counter.
- Background tasks next to the scheduler: Telegram outbox sender, Telegram command poller,
  daily summary, token-expiry check (once a day), connectivity tracker for `network_restored`,
  nightly image update (7.1). The image update and a job never run at the same time.

## 9. CLI

| Command | Purpose |
|---|---|
| `singlelane run` | Daemon |
| `singlelane tick [--dry-run] [--force-hours]` | One scheduling pass, then exit |
| `singlelane job --repo R (--issue N \| --pr N) [--dry-run]` | Run one specific item (testing) |
| `singlelane doctor [--with-claude]` | Preflight checks, see below |
| `singlelane setup-labels` | Create or update labels in all repos (name, color, description) |
| `singlelane status` | In-flight task and its state, running job, queue size per repo, last 20 jobs from history, pause and breaker state |
| `singlelane pause [--reason]` / `singlelane resume` | Manage the PAUSE file |
| `singlelane recover` | Run startup recovery manually |
| `singlelane notify-test` | Send a test Telegram message and print the result |
| `singlelane image-update [--force]` | Rebuild, smoke test and switch the agent image (7.1) |
| `singlelane image-rollback` | Switch back to the previous agent image |

`--dry-run` means read-only against GitHub, and no container is started. Print what would happen.

`doctor` checks: config is valid; the private key file exists with mode 600 and parses; the JWT is
accepted (`GET /app`) and the app id matches `github.app_id`; the installation token can be minted;
the installation is not suspended (WARN if it covers all repositories instead of selected ones); the installation covers every
configured repo (`GET /installation/repositories`); the installation's permissions are exactly
the set in 5.1 (FAIL on anything extra, such as workflows or administration, FAIL on anything
missing); `trusted_user` is not the app's bot user; labels exist; the base branch reports
`protected: true` (WARN with a hint about branch rules and plan requirements if not); `gh --version` >= 2.99.0; docker and the compose plugin are available; the
agent image exists and is arm64; the claude env file is valid; time zone; free disk space in
`data_dir` (warn below 20 GB); Telegram token and chat id valid (`getMe`, and the chat is
reachable), WARN if not configured; days until `claude_token_created` turns 1 year old. `--with-claude`: run the image with a one-turn prompt to prove
auth (costs a few tokens). Every check prints PASS, WARN or FAIL with a fix hint.

## 10. History store

SQLite (rusqlite, bundled) at `<data_dir>/history.db`: jobs (id, repo, kind, number,
started, finished, outcome, failure class, attempts, turns, cost, duration, log path),
pause state (reason, class, until), and the Telegram outbox (message, created, sent, tries).
Used for `max_attempts`, `status`, daily summaries, PR run stats and notification delivery.
Labels remain the source of truth for item state.

## 11. Telegram notifications and commands (optional)

Uses the Telegram Bot API directly over HTTPS (`sendMessage`, `getUpdates`). No webhook, no
inbound port on the Pi. Messages are plain text or HTML with all dynamic content escaped,
include direct links to the issue or PR, and are redacted (G8).

Events (each can be switched off with `telegram.notify`):

| Event | When | Content |
|---|---|---|
| `pr_opened` | PR created | repo, issue title, PR link, turns, cost, duration |
| `blocked` | agent asked a question | issue link, first 300 chars of the question |
| `failed` | `agent_failed` or `factory_bug` | issue link, class, short reason, log path |
| `paused` | breaker trip, fatal class, usage limit | reason, the last N failures with class, and what to do: for `claude_auth`, "run `claude setup-token` on your laptop, put the token in claude.env, then /resume"; for `github_auth`, "check the app installation and private key, then /resume"; for `infra`, the failing check |
| `resumed` | resume succeeded, or usage-limit pause ended | who resumed it, queue size |
| `token_expiry` | `claude_token_created` is set and the token is 14, 7 and 1 days from 1 year old | days left and re-auth steps |
| `network_restored` | connectivity back after an outage longer than `network_alert_after` | outage start, end, and duration |
| `review_reminder` | the in-flight PR has waited for review longer than `review_reminder_after` | PR link, how long it has waited, queue size behind it |
| `task_released` | the in-flight task is released (merged, PR closed, issue closed) | what happened, and which issue is next in the queue |
| `image_updated` | nightly update changed the Claude Code version | old and new version, channel |
| `image_update_failed` | nightly update failed | failing step, short reason; current image kept |
| `daily_summary` | at `daily_summary_at` | the in-flight task and its state, PRs opened, blocked, failed, queue size, total cost today, ignored `agent-revise` labels |

Delivery: an outbox table in the history store. Messages are written to the outbox first and
sent by a background task with retry and backoff, so notifications survive network outages and
restarts. Telegram failures never affect jobs.

Commands (if `telegram.commands = true`): a long-polling `getUpdates` loop. Only messages from
`TELEGRAM_CHAT_ID` are accepted; everything else is ignored and logged. Supported:
`/status` (same data as `singlelane status`, short form, starting with the in-flight task), `/pause [reason]`, `/resume`
(with the checks from 6.8). Nothing else, especially no command that starts a job or changes
config.

## 12. Logging

`tracing` with JSON output to stdout (journald). A per-job log file in `<data_dir>/logs/`
with host-side steps and the container output. Redaction (G8) is applied to everything
that leaves the process.

## 13. Deliverables

- Cargo workspace with the crates above. `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings` and `cargo test` all pass.
- `image/Dockerfile` and the assets it copies; the install docs put `image/` at
  `/opt/singlelane/image` (the default `image.dockerfile_dir`).
- `deploy/singlelane.service`: systemd unit for `singlelane run` (`User=singlelane`,
  `EnvironmentFile=/etc/singlelane/secrets.env`, `Restart=on-failure`,
  `TimeoutStopSec` greater than `shutdown_grace`).
- `deploy/config.example.toml`, `deploy/secrets.env.example` (TELEGRAM_BOT_TOKEN,
  TELEGRAM_CHAT_ID), `deploy/claude.env.example`. No example ever contains a real value.
- `README.md`: build (native on the Pi and cross from a laptop), install, GitHub App setup
  (create the app with the permissions in 5.1, generate a private key, install it on the
  target repo only, find the app id and installation id), branch rules for the default branch
  (require a PR with 1 approval, dismiss stale approvals, block force pushes and deletion,
  empty bypass list; note that rulesets on private repos need a paid GitHub plan), Telegram bot setup
  (create the bot with @BotFather, send it a message, get the chat id from `getUpdates`,
  then `singlelane notify-test`), first run with `doctor`, and operations (pause, resume, status,
  logs, re-authenticating Claude, rotating the GitHub App private key).

## 14. Out of scope for v1

GitHub Projects board sync, CI-failure feedback loop, multiple trusted users, GitHub
webhooks (polling only), web UI, network egress allowlist for the agent stack, Telegram
commands beyond /status /pause /resume.

## Decision log

| Date | Decision | Why |
|---|---|---|
| 2026-11-24 | The app JWT is signed with `ring` (already in the tree through rustls) instead of `jsonwebtoken` or octocrab's `jwt` features; octocrab is built without its auth and retry layers, and singlelane sets the `Authorization` header, retries and paginates itself (5.1) | `ring` already does RS256 signing for rustls, so no second RSA implementation (`jsonwebtoken` needs either the pure-Rust `rsa` stack or `aws-lc-rs` with its C build); the JWT itself is three base64url parts. singlelane needs the raw installation token for git and `gh` anyway, and octocrab's retry layer retries 5xx immediately for every method including POST (duplicate comments or PRs), never caps a rate-limit wait, and can't take a test clock |
| 2026-11-24 | `GitHub` trait has `close_pull_request` (used when the owner closes the task's issue, section 3); claim removes the queue label first and treats a 404 as a lost race, no rollback when unconfirmed (6.1); comment and PR text is redacted inside the GitHub client | Section 3 requires closing the agent PR; the 404 on label removal is the only compare-and-swap the label API offers; redaction at the API boundary is a backstop for G8 |
| 2026-11-24 | Time handling uses `jiff` with its bundled tz database instead of chrono, chrono-tz and humantime; the cargo-deny ignores for them are removed | All three were declared unmaintained (RUSTSEC-2026-0542/0543/0555); stale tz rules would break G9. The bundled database keeps `singlelane-core` free of filesystem reads, and tz updates arrive as `jiff-tzdb` releases through Dependabot |
| 2026-11-24 | CI builds the aarch64 binaries on GitHub's native ARM runner (`ubuntu-22.04-arm`) instead of `cross`; `cargo-deny` enforces the rustls-only rule; the chrono, chrono-tz and humantime "unmaintained" advisories are ignored until core moves off them | Native builds are simpler and also run the tests on aarch64; 22.04's glibc 2.35 keeps the binary runnable on Raspberry Pi OS bookworm |
| 2026-11-24 | Phase 1 fills gaps: `agent.implement_prompt`/`revise_prompt` override options; omitted daily times inside a present section disable the job; `poll_interval` must divide 24 h; `agent-ready` + `agent-blocked` counts as queued; host-observed GitHub rate limits classify as `network` | The spec named these behaviors without fixing the details; these are the conservative readings |
| 2026-11-24 | Nightly image update in v1 (7.1), Claude Code from the signed apt repo | Keeps Claude Code current without changing it under a running job; a bad release is caught by the smoke test and the previous image stays one command away |
| 2026-11-24 | GitHub App instead of a PAT (5.1) | Fine-grained PATs don't work for collaborators on another personal account's repos; app tokens are short-lived, nothing to renew yearly, PRs authored by `app[bot]` so the owner can approve them |
| 2026-11-23 | Exactly one task in flight until its PR is merged, not configurable (G10). No `drain` option. | Personal project, low volume, keeps subscription usage ordinary and individual, and the full stack (backend, two Next.js apps, Chromium, Postgres) needs most of the Pi's 16 GB |
