FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# sqlx::query!() checks against the committed `.sqlx/` data instead of a live database.
ENV SQLX_OFFLINE=true
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin daily-critters

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/daily-critters /usr/local/bin/daily-critters
VOLUME /data
ENV DATA_DIR=/data BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["daily-critters"]
CMD ["serve"]
