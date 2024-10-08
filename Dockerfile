FROM rust:1.80-bookworm AS builder

# Get cmake for zlib-ng
#
# Required to build: <https://github.com/rust-lang/libz-sys?tab=readme-ov-file#zlib-ng>
RUN apt-get update -y && apt-get install -y cmake

# Copy in source.
WORKDIR /usr/src/glaredb
COPY . .

RUN cargo install just

# Build release binary.
RUN just build --release

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update -y && \
    apt-get install -y --no-install-recommends \
        openssl \
        ca-certificates \
        openssh-client \
        postgresql-client && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/glaredb/target/release/glaredb /usr/local/bin/glaredb

CMD ["glaredb"]
