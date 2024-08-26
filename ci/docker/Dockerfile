# Stage 1: Rust Build Stage
FROM ubuntu:20.04 AS rust-build

ARG RUST_VERSION=
ARG RUST_NIGHTLY_VERSION=
ARG SCCACHE_VERSION=v0.8.1
ARG GRCOV_VERSION=v0.8.18

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH="$PATH:/usr/local/cargo/bin"

RUN apt-get update && apt-get install --no-install-recommends -y \
    tzdata apt-transport-https sudo build-essential git vim jq ca-certificates curl gnupg lld cmake mscgen \
    libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang make libprotobuf-dev protobuf-compiler && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN if [ -z "$RUST_VERSION" ]; then echo "ERROR: The RUST_VERSION argument is required!" && exit 1; fi && \
    if [ -z "$RUST_NIGHTLY_VERSION" ]; then echo "ERROR: The RUST_NIGHTLY_VERSION argument is required!" && exit 1; fi && \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs/ | sh -s -- --no-modify-path --profile minimal --default-toolchain $RUST_VERSION -y && \
    rustup component add rustfmt clippy && \
    rustup install $RUST_NIGHTLY_VERSION && \
    rustup component add clippy --toolchain=$RUST_NIGHTLY_VERSION && \
    rustup component add rustfmt --toolchain=$RUST_NIGHTLY_VERSION && \
    rustup component add miri --toolchain=$RUST_NIGHTLY_VERSION && \
    rustup component add llvm-tools-preview --toolchain=$RUST_NIGHTLY_VERSION && \
    rustup target add wasm32-unknown-unknown && \
    cargo install cargo-audit cargo-hack cargo-sort mdbook mdbook-linkcheck svgbob_cli wasm-pack rustfilt && \
    rustup show && rustc --version && cargo --version

# Install sccache and grcov
RUN curl -LOsS "https://github.com/mozilla/sccache/releases/download/$SCCACHE_VERSION/sccache-$SCCACHE_VERSION-x86_64-unknown-linux-musl.tar.gz" && \
    tar -xzf "sccache-$SCCACHE_VERSION-x86_64-unknown-linux-musl.tar.gz" && \
    mv "sccache-$SCCACHE_VERSION-x86_64-unknown-linux-musl/sccache" "$CARGO_HOME/bin/" && \
    rm -rf "sccache-$SCCACHE_VERSION-x86_64-unknown-linux-musl.tar.gz" "sccache-$SCCACHE_VERSION-x86_64-unknown-linux-musl" && \
    curl -LOsS "https://github.com/mozilla/grcov/releases/download/$GRCOV_VERSION/grcov-x86_64-unknown-linux-musl.tar.bz2" && \
    tar -xf grcov-x86_64-unknown-linux-musl.tar.bz2 && mv ./grcov $CARGO_HOME/bin && rm grcov-x86_64-unknown-linux-musl.tar.bz2

# Build Rust application
WORKDIR /app
COPY . /app
RUN cargo build --release && mv target/release/rust-app /app/bin/rust-app

# Stage 2: Golang Build Stage
FROM ubuntu:20.04 AS go-build

ARG GOLANG_VERSION=1.21.3
ENV PATH="/usr/local/go/bin:$PATH"

RUN apt-get update && apt-get install --no-install-recommends -y \
    curl build-essential && \
    rm -rf /var/lib/apt/lists/*

# Install Golang
RUN curl -LOsS "https://golang.org/dl/go$GOLANG_VERSION.linux-amd64.tar.gz" && \
    tar -C /usr/local -xzf "go$GOLANG_VERSION.linux-amd64.tar.gz" && \
    rm "go$GOLANG_VERSION.linux-amd64.tar.gz"

# Build Go application
WORKDIR /go/src/app
COPY . /go/src/app
RUN go build -o /app/bin/go-app .

# Stage 3: Final Runtime Stage
FROM ubuntu:20.04 

ENV DEBIAN_FRONTEND=noninteractive TZ=UTC

# Install runtime dependencies
RUN apt-get update && apt-get install --no-install-recommends -y \
    ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Copy binaries from Rust and Golang build stages
COPY --from=rust-build /app/bin/rust-app /usr/local/bin/rust-app
COPY --from=go-build /app/bin/go-app /usr/local/bin/go-app

# Entrypoint
ENTRYPOINT ["/bin/bash"]
