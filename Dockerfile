# syntax=docker/dockerfile:1
# InfinityDB release image (M1-S14): static musl build → `scratch`.
#
# The binary is fully static (musl + crt-static, verified static-pie), so the
# runtime stage carries nothing but the executable: no shell, no libc, no
# distro CVE surface. Size gate: < 30 MB image (the stripped binary is ~3 MB).
#
#   docker build -t infinitydb:dev --build-arg INF_RELEASE_VERSION=v0.1.0-alpha.1 .
#   docker run --rm -p 6379:6379 infinitydb:dev
#
# Multi-arch builds run this same file per-platform under buildx (the rust
# alpine image exists for linux/amd64 and linux/arm64; the native target of
# each is already musl).

ARG RUST_VERSION=1.95
FROM rust:${RUST_VERSION}-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# Tag-derived version + SHA provenance for `infinityd --version` (build.rs;
# .git is excluded from the context, so the pipeline forwards both).
ARG INF_RELEASE_VERSION=""
ARG INF_GIT_SHA=""
ENV INF_RELEASE_VERSION=${INF_RELEASE_VERSION} INF_GIT_SHA=${INF_GIT_SHA}
RUN cargo build --release -p infinityd -p inf --locked \
    && strip -s target/release/infinityd target/release/inf

FROM scratch
COPY --from=build /src/target/release/infinityd /infinityd
COPY --from=build /src/target/release/inf /inf
EXPOSE 6379
ENTRYPOINT ["/infinityd"]
