Phase 1 of 6: workspace scaffold and singlelane-core.

Read CLAUDE.md and SPEC.md fully first. Look at reference/factory.sh for the current behavior.

Build:
1. The Cargo workspace layout from SPEC section 4 (all five crates; the ones outside this phase
   may be empty stubs that compile). rust-toolchain.toml pinned to stable, rustfmt.toml,
   clippy settings in the workspace Cargo.toml, a .gitignore.
2. In `singlelane-core`:
   - Config types plus TOML parsing and the full startup validation from SPEC section 5,
     including the "exactly one Claude auth variable" check (taking the parsed env file as input).
   - The state machine from SPEC section 3: labels, item kinds (issue, PR), events and
     transitions; invalid transitions are errors, not panics. The in-flight rules from
     SPEC 3.1 as a pure function (input: the open labeled issues and PRs across repos; output:
     claim this issue, run revise on this PR, wait, or conflict).
   - Policy: trusted-author filtering of issue, PR, review and comment data (G1), the
     protected-path guard with globset (G4), and redaction (G8).
   - Working hours and tick alignment in a time zone (G9), with an injected clock.
   - Task and prompt rendering (SPEC 6.4), with templates embedded from assets/agent-prompts.
   - Failure classification (SPEC 6.8) as a pure function, and the Telegram message formatting
     for every event in SPEC 11 (escaping, links, redaction, length limits).
3. Tests: exhaustive state machine tests; G10 in-flight tests for every case in SPEC 3.1
   (none, working, blocked, in review, in review with revise, revise on a foreign PR, two in
   flight, PR closed unmerged, issue closed, an answered blocked issue goes ahead of older queued issues); G1, G4, G8, G9 tests named with their ids; edge
   cases (DST change in Europe/Kyiv, ticks exactly at the work-hours end, empty bodies,
   comments from other users that contain prompt-injection text, globs for CODEOWNERS in the
   repo root and in .github/); one classification fixture per row of the SPEC 6.8 table, and a
   test that an unknown error never classifies as `network`; G8 tests on Telegram messages.

Done when: fmt, clippy -D warnings and tests pass. Commit. Then stop and give me a summary:
the module layout, the public API of singlelane-core, and any spec ambiguities you found.
Do not start phase 2.
