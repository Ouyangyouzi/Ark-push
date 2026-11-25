# syntax=docker/dockerfile:1
FROM rust:1.90-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata wget \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/ark-push /usr/local/bin/ark-push
COPY ui /app/ui
ENV TZ=Asia/Shanghai \
    HTTP_PORT=7701 \
    STATE_DIR=/state \
    STRM_ROOT=/src \
    UI_DIR=/app/ui \
    LITEPAN_URL=http://host.docker.internal:5211 \
    LITEPAN_USERNAME=admin \
    LINK_ENABLED=on \
    LINK_115=off \
    LOG_LEVEL=info
EXPOSE 7701
VOLUME ["/state"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=5 \
  CMD wget -qO- "http://127.0.0.1:${HTTP_PORT:-7701}/health" || exit 1
ENTRYPOINT ["/usr/local/bin/ark-push"]
CMD ["serve"]
