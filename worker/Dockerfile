# The image agent and verify stages run in: Claude Code, git, common build
# tools and Terraform. dsf runs it as the factory user's uid with a
# read-only root filesystem and tmpfs HOME, so nothing here needs to be
# writable.

# Terraform, fetched and checked in its own stage so curl and unzip stay out
# of the final image. Pinned by version and by HashiCorp's SHA-256 for each
# architecture, from terraform_<version>_SHA256SUMS, whose signature was
# checked against HashiCorp's key C874 011F 0AB4 0511 0D02 1055 3436 5D94
# 72D7 468F. Bump all three together.
FROM debian:bookworm-slim AS terraform
ARG TERRAFORM_VERSION=1.16.6
ARG TERRAFORM_SHA256_AMD64=38034b38d03096399a7a1264a14561a8029b2b496fd1f169452656e53968f1b7
ARG TERRAFORM_SHA256_ARM64=a750b5a11038f165846b2658c40c88cd7070aa67a61bab431a14ba27928ea9ea
ARG TARGETARCH
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip \
 && case "$TARGETARCH" in \
      amd64) sum="$TERRAFORM_SHA256_AMD64" ;; \
      arm64) sum="$TERRAFORM_SHA256_ARM64" ;; \
      *) echo "no Terraform checksum for $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && curl -fsSLo /tmp/terraform.zip \
      "https://releases.hashicorp.com/terraform/${TERRAFORM_VERSION}/terraform_${TERRAFORM_VERSION}_linux_${TARGETARCH}.zip" \
 && echo "$sum  /tmp/terraform.zip" | sha256sum -c - \
 && unzip -q /tmp/terraform.zip terraform -d /usr/local/bin

FROM node:22-bookworm-slim

# Pinned: a Claude Code upgrade is a deliberate change, re-checked against
# the spike findings (stream format, flags, egress hosts).
ARG CLAUDE_CODE_VERSION=2.1.292

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git make python3 \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && npm cache clean --force
COPY --from=terraform /usr/local/bin/terraform /usr/local/bin/terraform

# Terraform's update check would only be refused by the egress proxy, and
# TF_IN_AUTOMATION keeps its output free of "now run terraform apply" hints.
ENV DISABLE_AUTOUPDATER=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    CHECKPOINT_DISABLE=1 \
    TF_IN_AUTOMATION=1 \
    TF_INPUT=0
WORKDIR /work
# dsf passes the full command; nothing should wrap it.
ENTRYPOINT []
