# The image dsf's engine tests use for agent and verify stages: a shell,
# git, and the fake `claude` in place of the real one.
FROM alpine:3.20
RUN apk add --no-cache git
COPY claude /usr/local/bin/claude
WORKDIR /work
