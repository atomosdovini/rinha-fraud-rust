# syntax=docker/dockerfile:1.7

FROM --platform=linux/amd64 alpine:3.20 AS references
WORKDIR /work
RUN apk add --no-cache curl
RUN curl -fsSL -o references.json.gz \
    https://raw.githubusercontent.com/zanfranceschi/rinha-de-backend-2026/main/resources/references.json.gz

# Build index using the C++ build-index tool (same binary, same format)
FROM --platform=linux/amd64 gcc:16 AS index-builder
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends zlib1g-dev libgomp1 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY index-builder/build_index.cpp index-builder/index.hpp ./
RUN mkdir -p /index \
    && g++ -O3 -DNDEBUG -std=c++20 -march=haswell -mtune=haswell -mavx2 -mfma -flto \
       build_index.cpp -lz -fopenmp -o /build-index
COPY --from=references /work/references.json.gz /tmp/references.json.gz
RUN /build-index /tmp/references.json.gz /index/index.bin 1280 65536 6 \
    && ls -lh /index/index.bin

# Build Rust binaries
FROM --platform=linux/amd64 rust:1.87-slim AS rust-builder
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
ENV RUSTFLAGS="-C target-cpu=haswell -C target-feature=+avx2,+fma"
RUN cargo build --release --bins \
    && strip target/release/server target/release/lb

# Runtime image
FROM --platform=linux/amd64 debian:trixie-slim AS runtime
COPY --from=rust-builder /src/target/release/server /server
COPY --from=rust-builder /src/target/release/lb /lb
COPY --from=index-builder /index /index
ENTRYPOINT ["/server"]
