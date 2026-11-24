# syntax=docker/dockerfile:1

# singlelane agent image (SPEC 7): Claude Code, agent-runner, JDK 21, Node 22, and headless
# Chromium behind the Playwright MCP server. One container of this image runs one job.
#
# The build context is the repository root:
#
#   docker buildx build --platform linux/arm64 -f image/Dockerfile -t singlelane-agent:latest --load .
#
# Add your project's toolchains near the end of the "toolchains" step, not after the Claude
# Code step: everything after CLAUDE_CACHE_BUST is rebuilt every night.

ARG RUST_IMAGE=rust:1-bookworm
ARG NODE_IMAGE=node:22-bookworm-slim
ARG BASE_IMAGE=eclipse-temurin:21-jdk-noble

# ---- agent-runner ---------------------------------------------------------------------------
# Built for the target platform. Debian bookworm has an older glibc (2.36) than the Ubuntu
# noble runtime (2.39), so the binary runs there.
FROM ${RUST_IMAGE} AS builder
WORKDIR /src
# rust-toolchain.toml is left out on purpose: it would make rustup download a second toolchain.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --package agent-runner --bin agent-runner \
 && cp target/release/agent-runner /usr/local/bin/agent-runner \
 && /usr/local/bin/agent-runner --version

# ---- Node 22 --------------------------------------------------------------------------------
# From the official image instead of a third-party apt repository set up with `curl | bash`.
FROM ${NODE_IMAGE} AS node

# ---- runtime --------------------------------------------------------------------------------
FROM ${BASE_IMAGE}
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# Toolchains. Versions follow the base image's Ubuntu release, so they are not pinned.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git gpg jq procps ripgrep unzip \
 && rm -rf /var/lib/apt/lists/*

COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && ln -s ../lib/node_modules/corepack/dist/corepack.js /usr/local/bin/corepack \
 && node --version \
 && npm --version

# Non-root user with uid 1000, so files in the mounted checkout match the host's `singlelane`
# user. Ubuntu images ship an `ubuntu` user with that uid.
RUN (userdel --remove ubuntu 2>/dev/null || true) \
 && useradd --create-home --uid 1000 --shell /bin/bash agent \
 && install -d -o agent -g agent \
      /work /task /out \
      /home/agent/.claude /home/agent/.claude/agents /home/agent/.gradle /home/agent/.npm

# Playwright MCP and the Chromium build that belongs to it. The browser is installed by the
# Playwright bundled with the MCP server (`install-browser`): a plain `npx playwright install`
# takes whatever Playwright is current, whose Chromium revision the MCP server does not find.
# `--no-shell`: the MCP server runs the full Chromium in headless mode, not the headless shell.
ARG PLAYWRIGHT_MCP_VERSION=0.0.86
ENV PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright
RUN npm install --global "@playwright/mcp@${PLAYWRIGHT_MCP_VERSION}" \
 && playwright-mcp install-browser --with-deps --no-shell chromium \
 && chmod -R a+rX "${PLAYWRIGHT_BROWSERS_PATH}" \
 && npm cache clean --force \
 && rm -rf /var/lib/apt/lists/* /root/.npm

ENV DISABLE_AUTOUPDATER=1 \
    GRADLE_OPTS="-Dorg.gradle.daemon=false -Xmx2g"

COPY --from=builder /usr/local/bin/agent-runner /usr/local/bin/agent-runner
# The settings.json deny rules are part of the sandbox: keep them.
COPY --chown=agent:agent assets/settings.json /home/agent/.claude/settings.json
COPY --chown=agent:agent assets/agents/ /home/agent/.claude/agents/

# ---- Claude Code ----------------------------------------------------------------------------
# From Anthropic's signed apt repository (https://code.claude.com/docs/en/setup, "Install with
# Linux package managers"). The build fails unless the downloaded key is exactly the Claude
# Code release signing key; apt then verifies the repository and the package against it.
#
# `singlelane image-update` sets CLAUDE_CACHE_BUST to the current date, so a nightly rebuild
# redoes this step and the ones below, and nothing above.
ARG CLAUDE_CHANNEL=stable
ARG CLAUDE_CACHE_BUST=initial
# hadolint ignore=DL3008
RUN echo "Claude Code channel: ${CLAUDE_CHANNEL} (cache bust: ${CLAUDE_CACHE_BUST})" \
 && case "${CLAUDE_CHANNEL}" in \
      stable|latest) ;; \
      *) echo "CLAUDE_CHANNEL must be stable or latest, not '${CLAUDE_CHANNEL}'" >&2; exit 1 ;; \
    esac \
 && install -d -m 0755 /etc/apt/keyrings \
 && curl -fsSL https://downloads.claude.ai/keys/claude-code.asc -o /etc/apt/keyrings/claude-code.asc \
 && GNUPGHOME="$(mktemp -d)" \
 && export GNUPGHOME \
 && fingerprints="$(gpg --show-keys --with-colons /etc/apt/keyrings/claude-code.asc \
      | awk -F: '$1 == "pub" { primary = 1; next } $1 == "fpr" && primary { print $10; primary = 0 }')" \
 && rm -rf "${GNUPGHOME}" \
 && if [ "${fingerprints}" != "31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE" ]; then \
      echo "unexpected Claude Code signing key fingerprint(s): '${fingerprints}'" >&2; exit 1; \
    fi \
 && echo "deb [signed-by=/etc/apt/keyrings/claude-code.asc] https://downloads.claude.ai/claude-code/apt/${CLAUDE_CHANNEL} ${CLAUDE_CHANNEL} main" \
      > /etc/apt/sources.list.d/claude-code.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends claude-code \
 && rm -rf /var/lib/apt/lists/* \
 && claude --version

# The `agent` user, by number, so that a runtime can check it is not root.
USER 1000:1000
WORKDIR /work

# Playwright MCP at user scope (https://code.claude.com/docs/en/mcp, "User scope": the entry
# lands in ~/.claude.json and applies in every project, without an approval prompt).
#   --browser chromium                 Chrome, the default, does not exist for arm64
#   --headless --isolated              no display; no browser profile on disk
#   --no-sandbox                       the container runs with all capabilities dropped
#   --allow-unrestricted-file-access   screenshots go to /out/screenshots, outside /work
#   --output-dir /tmp/playwright-mcp   keeps auto-named snapshots out of the checkout
RUN claude mcp add --scope user playwright -- \
      playwright-mcp --browser chromium --headless --isolated --no-sandbox \
      --allow-unrestricted-file-access --output-dir /tmp/playwright-mcp \
 && test -s /home/agent/.claude.json \
 && claude --version \
 && agent-runner --version

# Labels last: they change on every build without invalidating a layer. CLAUDE_CODE_VERSION
# can only be known after the install step, so it is filled in by a second, fully cached
# build (see "Building the agent image" in the README).
ARG SINGLELANE_VERSION=unknown
ARG BUILD_DATE=unknown
ARG CLAUDE_CODE_VERSION=unknown
LABEL org.opencontainers.image.title="singlelane-agent" \
      org.opencontainers.image.description="Agent image for singlelane: Claude Code, agent-runner, JDK 21, Node 22, headless Chromium" \
      org.opencontainers.image.source="https://github.com/ichanzhar/singlelane" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="${SINGLELANE_VERSION}" \
      org.opencontainers.image.created="${BUILD_DATE}" \
      io.github.ichanzhar.singlelane.claude-code.version="${CLAUDE_CODE_VERSION}" \
      io.github.ichanzhar.singlelane.claude-code.channel="${CLAUDE_CHANNEL}" \
      io.github.ichanzhar.singlelane.playwright-mcp.version="${PLAYWRIGHT_MCP_VERSION}"

ENTRYPOINT ["/usr/local/bin/agent-runner"]
# No default arguments (the base image's CMD is jshell): no arguments means "run the job".
CMD []
