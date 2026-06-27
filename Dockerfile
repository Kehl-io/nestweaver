# Build stage
FROM rust:1.87-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --bin nestweaver

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y git ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/nestweaver /usr/local/bin/nestweaver
VOLUME /data
EXPOSE 9378 9379
ENTRYPOINT ["nestweaver"]
CMD ["daemon", "--db", "/data/nestweaver/brain.lbug", "run", "--server", "--bind", "0.0.0.0:9378", "--config", "/etc/nestweaver/instance.toml"]
