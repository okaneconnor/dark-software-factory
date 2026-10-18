# The image agent and verify stages run in: Claude Code, git and common
# build tools. dsf runs it as the factory user's uid with a read-only root
# filesystem and tmpfs HOME, so nothing here needs to be writable.
FROM node:22-bookworm-slim

# Pinned: a Claude Code upgrade is a deliberate change, re-checked against
# the spike findings (stream format, flags, egress hosts).
ARG CLAUDE_CODE_VERSION=2.1.292

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git make python3 \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && npm cache clean --force

ENV DISABLE_AUTOUPDATER=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
WORKDIR /work
# dsf passes the full command; nothing should wrap it.
ENTRYPOINT []
