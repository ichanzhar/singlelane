Phase 6 of 6: hardening review.

Read CLAUDE.md and SPEC.md first. This phase is a review, not new features.

1. Guarantee audit: for each of G1-G10, find the code that enforces it and the tests that
   prove it. Write a table to docs/guarantees.md (guarantee, enforcement points with file and
   line, tests). Any gap: add the missing test, then fix.
2. Threat-model pass from the agent's point of view. Assume the agent is fully compromised
   by a prompt injection hidden in a dependency or repo file. What can it reach from inside
   the container: env vars, mounts, the network, docker, the host workspace, the Gradle and
   npm cache volumes shared between jobs, the out/ files the host trusts (pr.md, question.md,
   status, screenshots, pr-title.txt, failure.json)? Could the agent forge output that the
   host classifies as `network` to dodge the breaker, or that ends up in a Telegram message? Make sure host-side parsing of out/ is defensive: size
   limits, symlinks resolved and rejected if they leave out/, no path traversal in screenshot
   names, and PR text with redaction applied. Fix what you find. Document the residual risks
   in docs/threat-model.md.
3. Use a fresh subagent with no context of this implementation to review the full diff
   against SPEC.md and report BLOCKER / SHOULD-FIX / NIT. Fix the blockers and should-fix items.
4. Delete reference/. Make sure the README is complete. Run fmt, clippy and tests.
5. Write docs/first-run-checklist.md: an ordered checklist for the first real run on the Pi
   against a throwaway repo, covering every outcome (PR, blocked, revise, protected path,
   timeout, restart in the middle of a job, a deliberately broken Claude token, pulling the
   network cable during a job, /status /pause /resume from Telegram).

Commit and give me a final summary with known limitations.
