# Security policy

singlelane runs an autonomous coding agent and handles GitHub App keys and Claude credentials,
so security reports are very welcome.

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private vulnerability reporting instead:
the **Security** tab of this repository, then **Report a vulnerability**.

Include what you found, how to reproduce it, and the impact you expect. You will get an answer
as soon as possible. This is a personal project, so there is no fixed response time.

## In scope

- Ways for text from someone other than the configured owner to reach the agent
- Ways for the agent container to obtain GitHub credentials or other host secrets
- Ways to make singlelane merge, approve, push outside `agent/*` branches, or change protected paths
- Secret leakage into logs, comments, PRs or Telegram messages

## Out of scope

- Behavior of Claude Code, GitHub, Docker or Telegram themselves
- Setups that ignore the documented requirements (for example, running without branch rules)
