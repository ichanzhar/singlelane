# singlelane

Turn GitHub issues into pull requests on a small dedicated machine (built for a Raspberry Pi 5),
using [Claude Code](https://code.claude.com) headless in an isolated container stack.
One task at a time, from pickup until you merge its PR. You write issues, answer questions,
review, and merge. singlelane does the rest.

> **Status: under construction.** The code is being built in phases from the specification.
> See [`SPEC.md`](SPEC.md) and [`docs/build-phases/`](docs/build-phases/). Nothing here is
> ready to run yet.

## How it works

```
You open an issue (label: agent-ready)
  -> singlelane picks it up within 15 min (working hours only)
  -> a fresh container stack: your app + database + Claude Code + headless Chromium
  -> plan, code, tests until green, independent review, UI screenshots
  -> PR opened, review requested from you, screenshots attached to the issue
You review: add `agent-revise` for changes, or approve and merge
  -> the next issue starts only after the merge
```

Questions from the agent land as issue comments. Status, failures, and expired credentials
are reported to Telegram. `/status`, `/pause` and `/resume` work from your phone.

## Design principles

- **One lane.** Exactly one task in flight until its PR is merged. No parallel work, no pile of
  stale PRs.
- **The agent never touches GitHub.** The container has no GitHub credentials. The host does
  every push, comment and PR, and refuses changes to CI files and CODEOWNERS.
- **Only the owner's words reach the agent.** Issues and comments from anyone else are dropped,
  which closes the obvious prompt-injection path on public repos.
- **A human merges.** singlelane has no code path that merges or approves. Pair it with branch
  rules on your default branch.
- **Fails loudly, not repeatedly.** Failures are classified. Expired credentials pause it at
  once, network outages stay silent, and three agent failures in a row stop it.

## Requirements (planned)

- A dedicated Linux arm64 or x86_64 machine with Docker (tested target: Raspberry Pi 5, 16 GB, NVMe)
- A GitHub App you own, installed on the target repository
- A Claude Code account: an Anthropic API key, or your own Claude subscription
- Optional: a Telegram bot for notifications

## Building the agent image

The agent image holds Claude Code, `agent-runner` (the container entrypoint), JDK 21, Node 22
and headless Chromium behind the Playwright MCP server. The Dockerfile is
[`image/Dockerfile`](image/Dockerfile) and the build context is the repository root. It needs
Docker with BuildKit (the default since Docker 23). Nothing here needs `make`.

Natively on the Pi (the first build takes a while: the Chromium download and the Rust build):

```bash
cd /opt/singlelane
```

```bash
docker build -f image/Dockerfile -t singlelane-agent:latest \
  --build-arg SINGLELANE_VERSION="$(git describe --tags --always)" \
  --build-arg BUILD_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  .
```

From a laptop, for the Pi. On an Apple Silicon Mac this is a native build. On an x86_64
machine it runs under QEMU emulation and is slow; register the emulator once with
`docker run --privileged --rm tonistiigi/binfmt --install arm64`.

```bash
docker buildx build --platform linux/arm64 -f image/Dockerfile -t singlelane-agent:latest --load .
```

```bash
docker save singlelane-agent:latest | ssh pi docker load
```

Build arguments:

| Argument | Default | Meaning |
|---|---|---|
| `CLAUDE_CHANNEL` | `stable` | Claude Code apt channel: `stable` (about a week old) or `latest` |
| `CLAUDE_CACHE_BUST` | `initial` | change it (for example to today's date) to reinstall Claude Code; the layers above stay cached |
| `PLAYWRIGHT_MCP_VERSION` | pinned | `@playwright/mcp` version; its bundled Playwright installs the matching Chromium |
| `SINGLELANE_VERSION`, `BUILD_DATE`, `CLAUDE_CODE_VERSION` | `unknown` | image labels only |

Check the result without spending tokens. The smoke test runs `claude --version`, checks the
Claude Code settings and the Playwright MCP configuration, and opens a page in headless
Chromium through the MCP server:

```bash
docker run --rm --shm-size=1g --cap-drop=ALL --security-opt no-new-privileges:true \
  singlelane-agent:latest --smoke-test
```

The Claude Code version is only known after it is installed, so its label is filled in by
building again with the same arguments plus the version. That build is fully cached and only
rewrites the labels:

```bash
docker build -f image/Dockerfile -t singlelane-agent:latest \
  --build-arg SINGLELANE_VERSION="$(git describe --tags --always)" \
  --build-arg BUILD_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --build-arg CLAUDE_CODE_VERSION="$(docker run --rm --entrypoint claude singlelane-agent:latest --version | cut -d' ' -f1)" \
  .
```

`singlelane image-update` (planned, see SPEC 7.1) will do the nightly rebuild, smoke test and
tag switch by itself. Project toolchains (a different JDK, Python, ...) go into the
"toolchains" step of the Dockerfile, above the Claude Code step, so they stay cached.

## Responsible use

singlelane runs an AI agent with all permission prompts turned off inside a container.
Run it on dedicated hardware, keep the target repositories under branch rules, and review every
PR before merging. Use your own credentials only, and check that your use fits the terms of
your Claude plan and GitHub account. singlelane is an independent project and is not affiliated
with or endorsed by Anthropic or GitHub.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
