Phase 4 of 6: the job executor (one item, end to end).

Read CLAUDE.md, SPEC.md (sections 6.2-6.6) and reference/factory.sh first.

Build in `singlelane-exec` and wire into the `singlelane` bin:
1. The `ProcessRunner` trait plus the real implementation (tokio::process, argument vectors,
   secrets only in env, captured output with redaction) and a recording fake that returns
   scripted results.
2. Workspace management (SPEC 6.2) and cleanup rules.
3. Git operations (SPEC 6.3) with authentication that never touches disk. Test G2 asserts
   that after clone, `.git/config`, the remote URL, and every argument vector recorded by the
   fake contain no token.
4. Compose rendering (SPEC 6.5) as a typed struct serialized to YAML, with snapshot tests
   (insta). Test G2 asserts the rendered compose has no GitHub token anywhere, and only the
   claude env file is referenced. Run, timeout and mandatory teardown on every path,
   including a test that teardown happens when the job future is dropped or panics.
5. Publish (SPEC 6.6): status handling, the commit count, the G4 guard, push rules (G3:
   assert the executor can only ever push to `agent/` refs, and only force-pushes on fresh
   issue branches), PR create or comment, labels, run stats from result.json, screenshots via
   `gh issue comment --attach` with the filters from the spec (plus the orphan-branch
   fallback from SPEC 6.6, selectable by config, because `--attach` with an app token is
   unverified), and the failure path: classify
   with the SPEC 6.8 function and apply that class's row (network and fatal classes requeue
   the item without counting an attempt; agent_failed and factory_bug go to blocked).
   Return the class to the caller so the daemon (phase 5) can pause and notify.
6. The revise flow (PR head branch checkout, fast-forward push, reply comment).
7. `singlelane job --repo R (--issue N | --pr N) [--dry-run]` running one item through the real
   implementations.
8. Integration tests with the GitHub fake plus the ProcessRunner fake, covering: happy issue
   flow, blocked with question, no commits, protected path touched, agent timeout, screenshot
   upload failure (non-fatal), revise flow, release handling (PR merged, PR closed unmerged,
   issue closed by the owner closes the agent PR), a PR already existing for the branch, an expired
   Claude token (requeued, class claude_auth), a GitHub 401 (class github_auth), and a DNS
   failure during clone (requeued, class network, no comment posted). Every
   path must end in a visible state (G6).

Done when: all checks pass. Commit, summarize, and give me the exact commands to try
`singlelane job --dry-run` and then a real run against a throwaway test repo, including a
manual test of `gh issue comment --attach` with an installation token so we know whether the
fallback is needed.
Do not start phase 5.
