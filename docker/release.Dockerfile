# The released dsf image: the same static binary as the release tarball,
# so the proxy sidecar runs exactly the reviewed, released code. The
# release workflow puts the binary next to this file. For a local image
# built from source, use dsf.Dockerfile.
FROM debian:bookworm-slim
COPY dsf /usr/local/bin/dsf
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/dsf"]
