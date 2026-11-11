# The image agent and verify stages run in: Claude Code, git, common build
# tools, Terraform and Rust. dsf runs it as the factory user's uid with a
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

# tflint and its azurerm rules, for Terraform repos' verify, in the same
# way: pinned by version and by the SHA-256 in each release's checksums.txt
# on GitHub. The ruleset is installed where tflint finds it with no network.
FROM debian:bookworm-slim AS tflint
ARG TFLINT_VERSION=0.64.0
ARG TFLINT_SHA256_AMD64=cca9d13e2e1d7a2c627af60ff899a3c9b74212899416aeb96ec764d2ef954537
ARG TFLINT_SHA256_ARM64=560da89aacf59389d4eb029730dd5b109b7288096c32f2726a0d9e783a5ea8eb
ARG TFLINT_AZURERM_VERSION=0.32.0
ARG TFLINT_AZURERM_SHA256_AMD64=f1150b2dea7aef001ad07002671c75b5495f4521ddf3538ededf00896f7b0aaa
ARG TFLINT_AZURERM_SHA256_ARM64=70510c58a2d550c548645c42a5ff8acc93940a7dd9ce18a5de207c326f5f3cd3
ARG TARGETARCH
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip \
 && case "$TARGETARCH" in \
      amd64) lint="$TFLINT_SHA256_AMD64"; rules="$TFLINT_AZURERM_SHA256_AMD64" ;; \
      arm64) lint="$TFLINT_SHA256_ARM64"; rules="$TFLINT_AZURERM_SHA256_ARM64" ;; \
      *) echo "no tflint checksum for $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && base=https://github.com/terraform-linters \
 && curl -fsSLo /tmp/tflint.zip \
      "$base/tflint/releases/download/v${TFLINT_VERSION}/tflint_linux_${TARGETARCH}.zip" \
 && echo "$lint  /tmp/tflint.zip" | sha256sum -c - \
 && unzip -q /tmp/tflint.zip tflint -d /usr/local/bin \
 && curl -fsSLo /tmp/azurerm.zip \
      "$base/tflint-ruleset-azurerm/releases/download/v${TFLINT_AZURERM_VERSION}/tflint-ruleset-azurerm_linux_${TARGETARCH}.zip" \
 && echo "$rules  /tmp/azurerm.zip" | sha256sum -c - \
 && mkdir -p /usr/local/share/tflint/plugins \
 && unzip -q /tmp/azurerm.zip tflint-ruleset-azurerm -d /usr/local/share/tflint/plugins

# Rust, for Rust repos' agents and verify, dsf's own among them: the
# toolchain dsf's rust-toolchain.toml pins, with clippy and rustfmt, from the
# official image pinned by its multi-arch digest. Stages can't download a
# toolchain (the egress proxy doesn't allow rustup's host), so a repo that
# pins a different version fails at once rather than mid-build. Bump the
# tag, the digest and rust-toolchain.toml together.
FROM rust:1.99.1-slim-bookworm@sha256:f2c004e3d2d8b547e8cf7ffbc29205385f4e71d23e53e4e383065db5e9f822cd AS rust
RUN rustup component add clippy rustfmt \
 && chmod -R go-w "$RUSTUP_HOME"

FROM node:22-bookworm-slim

# Pinned: a Claude Code upgrade is a deliberate change, re-checked against
# the spike findings (stream format, flags, egress hosts).
ARG CLAUDE_CODE_VERSION=2.1.292

# gcc and libc6-dev are Rust's linker and what crates with C code (bundled
# SQLite, ring) build with; openssl is the command dsf's tests make keys with.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git make python3 \
      gcc libc6-dev openssl \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && npm cache clean --force
COPY --from=terraform /usr/local/bin/terraform /usr/local/bin/terraform
COPY --from=tflint /usr/local/bin/tflint /usr/local/bin/tflint
COPY --from=tflint /usr/local/share/tflint /usr/local/share/tflint
# Only rustup itself; the commands that run the toolchain are links to it.
COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo/bin/rustup /usr/local/cargo/bin/rustup
RUN for tool in cargo cargo-clippy cargo-fmt clippy-driver rustc rustdoc rustfmt; do \
      ln -s rustup "/usr/local/cargo/bin/$tool"; \
    done

# Terraform's update check would only be refused by the egress proxy, and
# TF_IN_AUTOMATION keeps its output free of "now run terraform apply" hints.
# Cargo keeps its downloads in the default ~/.cargo, on the tmpfs HOME, as
# the toolchain under /usr/local is read-only when a stage runs.
ENV RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH \
    DISABLE_AUTOUPDATER=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    CHECKPOINT_DISABLE=1 \
    TF_IN_AUTOMATION=1 \
    TF_INPUT=0 \
    TFLINT_PLUGIN_DIR=/usr/local/share/tflint/plugins
WORKDIR /work
# dsf passes the full command; nothing should wrap it.
ENTRYPOINT []
