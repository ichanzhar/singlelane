Phase 5 of 6: daemon, scheduling and operations.

Read CLAUDE.md and SPEC.md (sections 6.1, 6.7 and 8-13) first.

Build:
1. The history store (SPEC 10) with migrations, and `max_attempts` enforcement in claim.
2. `singlelane run`: single-instance lock (G5), wall-clock aligned ticks in the configured time
   zone, the work-hours gate (G9), the PAUSE file, the single worker and the in-flight rules (G10, SPEC 3.1; no slots, no drain), the
   circuit breaker with the class rules from SPEC 6.8 (G7: immediate pause on fatal classes,
   timed self-clearing pause on usage limits, network never counted), startup recovery
   (G6, SPEC 6.7), and graceful shutdown on SIGTERM/SIGINT with `shutdown_grace`, then
   teardown and recovery labeling.
3. `singlelane tick`, `status`, `pause`, `resume`, `recover`, and the remaining `doctor` checks
   (gh version >= 2.99.0, docker plus compose, the image exists and is arm64, the claude env
   file, disk space, and `--with-claude`).
3b. `image-update` and `image-rollback` (SPEC 7.1) and the nightly schedule, never overlapping a
   job, with tests using the ProcessRunner fake: success switches tags, a failed smoke test keeps
   the current image, a running job skips the update, unchanged version sends no message.
4. Telegram (SPEC 11): the outbox in the history store, the sender task with retry and
   backoff, all events including the daily summary, token-expiry warnings and
   network_restored, the `getUpdates` command poller (/status, /pause, /resume) that accepts
   only TELEGRAM_CHAT_ID, `resume` refusing while the 6.8 check still fails, and
   `singlelane notify-test`. Use the Bot API over reqwest with rustls and the ring
   provider, the same setup as `singlelane-github`'s `http` module; check the current Bot API
   docs for request and response shapes. Mock Telegram with wiremock in tests.
5. Deliverables from SPEC 13: the systemd unit, example config and env files, and a README
   with install steps on Raspberry Pi OS or Ubuntu arm64, a `singlelane` system user, file
   permissions (secrets 600), first run with doctor, and day-to-day operations.
6. Tests with a fake clock: G10 (no second claim while a job runs, ticks during a job are
   skipped, a leftover compose project is torn down and its item recovered, no new issue while
   the PR waits for review or the task is blocked, the next issue is picked on the first tick
   after merge, PR closed unmerged releases the task, review_reminder fires once per period);
   no claims outside work hours; the breaker trips after N failures but not on "blocked with question";
   recovery relabels orphaned `agent-working` items; max_attempts leads to blocked; shutdown
   waits for running jobs and then tears down and relabels; claude_auth pauses on the first
   failure and sends exactly one Telegram message; /resume refuses while the auth probe fails;
   a usage-limit pause clears itself at the reset time; messages queued during a network
   outage are delivered afterwards; commands from another chat id are ignored.

Done when: all checks pass. Commit, summarize, and list what still needs a real-world check.
Do not start phase 6.
