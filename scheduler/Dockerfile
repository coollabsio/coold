# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS builder

ARG SCHEDULER_VERSION=0.1.0-dev

WORKDIR /workspace

RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

COPY . .

RUN SCHEDULER_VERSION="$SCHEDULER_VERSION" cargo build -p scheduler --release --locked \
    && cp target/release/scheduler /usr/local/bin/scheduler

FROM debian:bookworm-slim AS runtime

ARG SCHEDULER_VERSION=0.1.0-dev

LABEL org.opencontainers.image.title="Coolify scheduler" \
      org.opencontainers.image.description="Central-side gRPC stream scheduler for coold agents." \
      org.opencontainers.image.version="$SCHEDULER_VERSION" \
      org.opencontainers.image.source="https://github.com/coollabsio/coold" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates passwd \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 scheduler \
    && useradd --system --uid 10001 --gid scheduler --home-dir /nonexistent --shell /usr/sbin/nologin scheduler \
    && mkdir -p /etc/coolify /run/coolify \
    && chown -R scheduler:scheduler /run/coolify

COPY --from=builder /usr/local/bin/scheduler /usr/local/bin/scheduler
COPY scheduler/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ENV SCHEDULER_UNIX_SOCKET_PATH=/run/coolify/scheduler.sock \
    SCHEDULER_JWT_PUBLIC_KEY_PATH=/etc/coolify/jwt.pub \
    SCHEDULER_LOG_LEVEL=info

EXPOSE 6443
VOLUME ["/run/coolify", "/etc/coolify"]

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["/usr/local/bin/scheduler"]
