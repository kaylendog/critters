# Builds one binary from the workspace. Pass `--build-arg BIN=critter-discord`
# for the pusher. The default is the server.
ARG BIN=critter-server

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG BIN
# sqlx::query!() checks against the committed `.sqlx/` data instead of a live database.
ENV SQLX_OFFLINE=true
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --bin "$BIN" --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin "$BIN" && cp "target/release/$BIN" /app/entrypoint

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/entrypoint /usr/local/bin/entrypoint
VOLUME /data
ENV DATA_DIR=/data
EXPOSE 8080 8081
ENTRYPOINT ["entrypoint"]
