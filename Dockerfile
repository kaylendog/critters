# Builds one binary from the workspace. The target picks the final image and
# BIN picks the binary, and they must match:
#   docker build --target critter-discord --build-arg BIN=critter-discord .
# The default is the server.
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
ENTRYPOINT ["entrypoint"]

FROM runtime AS critter-discord
EXPOSE 8081

# Last, so it is the default target.
FROM runtime AS critter-server
# The labels and model this image was built with. Synced into the database on
# every start. Set IMPORT_FILE empty to label on the deployment instead.
COPY weights.json /usr/share/critters/weights.json
ENV IMPORT_FILE=/usr/share/critters/weights.json
VOLUME /data
ENV DATA_DIR=/data
EXPOSE 8080
