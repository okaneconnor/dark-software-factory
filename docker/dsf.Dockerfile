# dsf itself in a container. dsf runs its egress proxy from this image as a
# sidecar (`dsf proxy`), so the proxy is the same reviewed code as the rest.
FROM rust:1.99.1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked --package dsf --bin dsf

FROM debian:bookworm-slim
COPY --from=build /src/target/release/dsf /usr/local/bin/dsf
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/dsf"]
