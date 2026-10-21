# Scanner image for Anthropic's OSS Scanner (https://github.com/anthropics/oss-scanner).
#
# The audit runs offline, so this build installs the toolchain, fetches every
# crate and the pinned LadybugDB source, and compiles the CLI and the workspace
# tests. threat_model.md, next to this file, says how to run them.
# The build context is the repository root; .oss-scanner/Dockerfile.dockerignore
# keeps the whole checkout, .git included.

# The Rust release the repository's Dockerfile builds with; CI uses stable.
FROM rust:1.96-trixie
ENV DEBIAN_FRONTEND=noninteractive
# The same native dependencies the Linux CI job installs (cmake and g++ for the
# LadybugDB C++ build, protoc for tonic, mold as the linker), plus git, which
# indexing and tests use.
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake ninja-build g++ libssl-dev libzstd-dev pkg-config protobuf-compiler mold git curl jq python3 \
    && rm -rf /var/lib/apt/lists/*

# scanner contract: the checkout lives inside the image, at /src
COPY . /src
WORKDIR /src

# The lbug crate builds from a LadybugDB checkout; the script pins and verifies
# the tag and commit against Cargo.lock.
RUN scripts/fetch-lbug-source.sh /opt/ladybug
ENV LBUG_SOURCE_DIR=/opt/ladybug \
    LBUG_BUILD_FROM_SOURCE=1 \
    CFLAGS="-DZSTD_DISABLE_ASM" \
    RUST_BACKTRACE=1 \
    CARGO_PROFILE_DEV_DEBUG=line-tables-only \
    CARGO_PROFILE_TEST_DEBUG=line-tables-only
# Link with mold, as CI does: GNU ld runs out of memory linking the test binaries
# in parallel. On aarch64, gemm-f16 (under the candle embedder) also needs fp16,
# which aarch64-apple-darwin enables by default and aarch64 Linux does not; that
# only affects arm64 hosts, such as tools/check on Apple Silicon.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=mold" \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=mold -C target-feature=+fp16"

# Build the LadybugDB C++ engine optimised and without debug info. At opt-level 0
# it builds as CMake Debug, and bundling that static archive into the lbug rlib
# exhausts memory. LadybugDB is a separate project; NestWeaver's own crates keep
# their debug info. It lives in CARGO_HOME so later cargo runs in the audit reuse
# this build instead of rebuilding the engine.
RUN printf '[profile.dev.package.lbug]\nopt-level = 2\ndebug = false\n' >> "$CARGO_HOME/config.toml"

# An ARG, so that it bounds the memory this build uses without reaching the audit environment.
ARG CARGO_BUILD_JOBS=8

# Every crate the workspace depends on, for all targets.
RUN cargo fetch --locked

# The workspace tests and the CLI they drive, with default features, then the
# same tests in the configuration Linux CI runs them. Both passes resolve features
# the same way (a plain `cargo build` leaves out dev-dependency features and would
# compile about 200 crates a second time); ci-direct-tests only touches the root
# crate. The first pass's binary, which has no CI-only feature, goes on PATH.
# A step that fails fails the build, and primary_contact gets the log.
RUN cargo test --locked --workspace --no-run \
    && install -m 0755 target/debug/nestweaver /usr/local/bin/nestweaver \
    && nestweaver --version \
    && cargo test --locked --workspace --features ci-direct-tests --no-run

# Tests run without a background daemon and must never fetch an embedding model.
ENV NESTWEAVER_ALLOW_NO_DAEMON=1 \
    NESTWEAVER_NO_DAEMON=1

# Run the unit tests so the log shows the image works, without letting a failure abort the build.
# The time cap keeps the whole build inside oss-scanner's 40-minute pull-request check; a cut-short run
# still shows the image works, and the audit can run the full suite.
RUN timeout 240 cargo test --locked --workspace --features ci-direct-tests --lib --no-fail-fast \
    || echo "WARNING: cargo test reported failures"

# Everything is fetched. From here on cargo must not wait for a registry that it cannot reach.
ENV CARGO_NET_OFFLINE=true
