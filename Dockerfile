# Build stage
FROM rust:1.88-trixie AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*
COPY . .
RUN RUSTFLAGS="-C link-arg=-Wl,--allow-multiple-definition" cargo build --release --bin nestweaver

# Runtime stage
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y git ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/nestweaver /usr/local/bin/nestweaver
VOLUME /data
# 9377 web UI + Prometheus metrics, 9378 gRPC, 9379 MCP-over-HTTP + webhook + admin
EXPOSE 9377 9378 9379
ENTRYPOINT ["nestweaver"]
# Argument order matches docker-compose.yml. (`--db` is a global flag on the
# daemon command, so it is accepted before or after `run`.)
CMD ["daemon", "run", "--server", "--bind", "0.0.0.0:9378", "--db", "/data/nestweaver/brain.lbug", "--config", "/etc/nestweaver/instance.toml"]
