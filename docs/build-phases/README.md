# Build phases

singlelane is built spec-first with Claude Code: `SPEC.md` is the single source of truth, and
each phase below is a prompt that builds one part of it.

| Phase | Prompt | Builds |
|---|---|---|
| 1 | [01-core.md](01-core.md) | Workspace, config, state machine, in-flight rules, input filtering, failure classes, working hours |
| 2 | [02-github.md](02-github.md) | GitHub App auth, GitHub layer, claim logic, `setup-labels`, GitHub part of `doctor`, CI for this repo |
| 3 | [03-agent-runner-image.md](03-agent-runner-image.md) | `agent-runner` and the agent image (Claude Code from the signed apt repo, Playwright Chromium + MCP) |
| 4 | [04-job-executor.md](04-job-executor.md) | One issue end to end: git, compose stack, publish, screenshots, revise, release; `singlelane job` |
| 5 | [05-daemon.md](05-daemon.md) | Daemon, scheduling, breaker, recovery, Telegram, `image-update`, systemd unit, install docs |
| 6 | [06-hardening.md](06-hardening.md) | Guarantee audit, threat model, independent review, first-run checklist |

## How to run a phase

1. Read `SPEC.md` once and change anything you disagree with **before** phase 1. Changing the
   spec is much cheaper than changing the code.
2. Open Claude Code in the repo root on your laptop (not on the machine that will run singlelane).
3. For each phase:
   - start a fresh session (`/clear`), so the phase starts from the spec and the git log
   - switch to plan mode, paste the phase prompt, review the plan, then let it build
   - review the summary and the diff, answer its open questions, update `SPEC.md` if needed
   - commit, then move to the next phase
4. After phase 4 you can run single jobs with `singlelane job`. After phase 5 the daemon runs on its own.

## Effort

Phases 1, 2 and 5 are medium, phase 4 is the largest. Phases 3 and 6 are smaller but need
manual checks on real hardware (Docker, Playwright, a real GitHub App, a real Telegram bot).
