Phase 3 of 6: agent-runner and the agent image.

Read CLAUDE.md, SPEC.md (especially section 7) and reference/entrypoint.sh first.

Build:
1. The `agent-runner` binary exactly as specified in SPEC section 7: git identity,
   safe.directory, spawning Claude Code in its own process group, timeout with SIGTERM then
   SIGKILL, teeing stream-json to stdout and /out/transcript.jsonl, parsing the final result
   event into /out/result.json, writing `failed` plus a reason when Claude leaves no status,
   /out/failure.json with the raw failure facts (SPEC 7), and exit codes.
   Make it testable: the claude command path is configurable, and the tests use a small fake
   executable (a Rust test binary, not a shell script) that emits sample stream-json, sleeps,
   or exits without writing a status.
2. Before writing the result parser, check the current Claude Code docs for the stream-json
   output format, the fields of the final result event, and how authentication errors
   (expired or invalid API key or OAuth token) and usage-limit errors show up in headless
   mode. Add fixtures for each so phase 4 can classify them. Put a real-looking sample in the
   test fixtures and cite the doc page in a comment.
3. `image/Dockerfile` per SPEC section 7: a multi-stage build for arm64, Playwright Chromium
   plus system deps, Playwright MCP at user scope configured for chromium, headless and
   isolated (check the current Claude Code docs for the user-scope MCP config mechanism, and
   the Playwright MCP docs for config options), Claude Code via the native installer, user
   `agent` uid 1000, Claude Code from the signed apt repository with fingerprint verification and the
   `CLAUDE_CHANNEL` / `CLAUDE_CACHE_BUST` build args placed as SPEC 7 describes (check the current
   Claude Code setup docs for the repository lines), image labels, the assets copied in. Use assets/Dockerfile.bash-kit-reference as the
   starting point. The settings.json deny rules stay.
4. A `make`-free build note in the README section "Building the agent image", including
   `docker buildx build --platform linux/arm64` from a laptop and a native build on the Pi.

You cannot run docker here: validate the Dockerfile with hadolint if available and review it
carefully. List every assumption I should verify on the Pi.

Also add `agent-runner --version` and a `--smoke-test` mode used by `image-update` (SPEC 7.1):
it checks claude, Chromium headless and the MCP config without calling the Claude API.

Done when: agent-runner tests pass, clippy is clean, the Dockerfile is reviewed. Commit,
summarize, and give me a 5-step manual smoke test for the image on the Pi.
Do not start phase 4.
