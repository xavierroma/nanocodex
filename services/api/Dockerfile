# syntax=docker/dockerfile:1.7
FROM rust:1.97-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --locked --profile nightly -p nanocodex-api && \
    cp target/nightly/nanocodex-api /nanocodex-api

FROM build AS check
RUN rustup component add clippy
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo clippy --locked --profile nightly -p nanocodex-api -p nanocodex-memory --no-deps -- -D warnings

FROM check AS test
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo test --locked --profile nightly -p nanocodex-memory --lib

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /nanocodex-api /usr/local/bin/nanocodex-api
USER 10001:10001
WORKDIR /tmp
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/nanocodex-api"]
