# stream-json fixtures

Samples of `claude -p ... --output-format stream-json --verbose` output, one event per line.
`agent-runner` replays them through the fake `claude` test binary, and the host classifier
(`singlelane_core::failure`) is tested on the `failure.json` they produce (SPEC 6.8).

Checked on 2026-11-24 against the Claude Code docs:

- stream-json, `system/init`, `system/api_retry`: <https://code.claude.com/docs/en/headless>
- `SDKResultMessage`, `SDKAssistantMessage` (`error` categories), `SDKRateLimitEvent`,
  `startup_failure_reason`: <https://code.claude.com/docs/en/agent-sdk/typescript>
- authentication and usage-limit messages: <https://code.claude.com/docs/en/errors>

No file is a log of a real run. The ones marked "observed" copy the event shapes that Claude
Code 2.1.331 printed for a placeholder credential; ids, paths and tool lists are placeholders
and the bulky `system/init` lists are shortened.

| File | Case | Source |
|---|---|---|
| `success.jsonl` | a normal run: tool calls, then `result` with `subtype: success` | docs |
| `auth_invalid_api_key.jsonl` | `ANTHROPIC_API_KEY` rejected by the API | observed |
| `auth_not_logged_in.jsonl` | no usable credential; also what an invalid `CLAUDE_CODE_OAUTH_TOKEN` produced | observed |
| `auth_oauth_expired.jsonl` | expired OAuth token | message from the docs, event shape as observed |
| `usage_limit.jsonl` | subscription usage limit hit mid-run, with a `rate_limit_event` carrying the reset time | docs, **not observed** |
| `error_max_turns.jsonl` | `--max-turns` reached | docs |
| `startup_failure.jsonl` | Claude Code refuses to start (`CLAUDE_CODE_STARTUP_FAILURE_RESULTS=1`) | observed |

Things the observed runs showed that are easy to get wrong:

- An API failure ends with `"subtype":"success"` and `"is_error":true`. The failure is in
  `is_error`, `api_error_status`, `api_error_type` and `terminal_reason: "api_error"`, and the
  message is in `result`. Claude Code exits with code 1.
- The assistant event with the error text is synthetic (`"model":"<synthetic>"`) and has
  `error`, `api_error_status` and `api_error_type` at the top level.
- Nothing is written to stderr in these cases.

To verify on a real run (see the phase 3 notes): the usage-limit events, and whether
`num_turns` is per result event or cumulative when a run has more than one.
