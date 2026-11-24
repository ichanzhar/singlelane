Phase 2 of 6: GitHub layer.

Read CLAUDE.md, SPEC.md and the phase 1 summary in the git log first.

Build in `singlelane-github`:
0. GitHub App authentication per SPEC 5.1: JWT signing from the PEM key, installation token
   exchange, in-memory caching with refresh, the app identity lookup, and a `TokenProvider`
   trait so git (phase 4) and `gh` can get a fresh token. octocrab has app and installation
   auth support: check its current API on docs.rs before using it. The private key and tokens
   must never appear in logs or errors (G8).
1. A `GitHub` trait covering exactly what the factory needs:
   list queued issues and PRs by label and author; get an issue or PR with its labels;
   add and remove labels; post a comment; list issue comments, PR reviews and inline review
   comments (paginated); find an open PR by head branch; create a PR (title, body, base,
   head, labels, reviewers); get the app identity and bot user; list the installation's repositories and permissions;
   get a branch's `protected` flag; create or update labels.
   No merge or approve methods exist on the trait at all (G3 by construction).
2. A real implementation with octocrab (rustls). Add retries with backoff for 5xx and
   secondary rate limits, and respect `Retry-After`. Log the request id, never the token.
3. An in-memory fake implementing the trait, so later phases can test full flows.
4. Claim logic (SPEC 6.1): swap the label, re-read, verify; plus a G5 test with two
   concurrent claimers against the fake.
5. CLI commands in the `singlelane` bin: `setup-labels` and the GitHub-related part of `doctor`
   (key file mode, JWT accepted, installation token minted, installation covers the repos,
   permissions exactly as in SPEC 5.1, trusted_user is not the app bot, labels exist, base
   branch protected).
5b. CI for this repository already exists (`.github/workflows/ci.yml`, `security.yml`,
   `dependabot.yml`, `deny.toml`, added after phase 1). Keep it green; when you add
   dependencies, make sure `cargo deny check` still passes (rustls only, no OpenSSL), and add
   any guarantee you implement with tests to `scripts/guarantees.txt`.
   Output format: PASS / WARN / FAIL lines with a fix hint.
6. Tests: JWT claims and expiry, token refresh before expiry, a permissions check that fails
   on an extra `workflows` permission, G8 redaction of `ghs_` tokens and PEM blocks; the octocrab implementation against wiremock with recorded-style JSON for each call,
   including pagination and a 403 secondary rate limit; G1 end to end (the fake returns
   comments from other users and they never reach the rendered task.md); G5.

Done when: all checks pass. Commit, summarize, list anything about the GitHub API you were
unsure of. Do not start phase 3.
