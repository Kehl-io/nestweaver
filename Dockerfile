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
EXPOSE 9377 9378 9379 9380
ENTRYPOINT ["nestweaver"]
CMD ["daemon", "run", "--server", "--config", "/etc/nestweaver/instance.toml"]
