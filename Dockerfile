# Build stage
FROM rust:1.96-trixie AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y cmake g++ libssl-dev libzstd-dev pkg-config protobuf-compiler && rm -rf /var/lib/apt/lists/*
COPY . .
RUN RUSTFLAGS="-C link-arg=-Wl,--allow-multiple-definition" cargo build --locked --release --bin nestweaver

# Runtime stage
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y git ca-certificates jq && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/nestweaver /usr/local/bin/nestweaver
VOLUME /data
# 9377 web UI, 9378 gRPC, 9379 MCP-over-HTTP + webhook + admin + Prometheus metrics
EXPOSE 9377 9378 9379
ENTRYPOINT ["nestweaver"]
# Default CMD binds loopback so a bare `docker run` boots. A non-loopback bind
# (0.0.0.0) is rejected at startup without TLS (validate_bind_security), so for a
# network-reachable server use docker-compose.yml — it provisions certs via the
# init-tls service and passes --tls-cert/--tls-key — or override this CMD with
# your own --bind 0.0.0.0 + --tls-cert/--tls-key.
CMD ["daemon", "run", "--server", "--bind", "127.0.0.1:9378", "--db", "/data/nestweaver/brain.lbug", "--config", "/etc/nestweaver/instance.toml"]
