# Build context must be the repo root (so the workspace Cargo.toml is reachable).
# fly.toml at the repo root sets this up automatically.
FROM rust:1.87-slim AS builder
WORKDIR /build

# Copy workspace manifest + server crate manifest.
# overlay/Cargo.toml is needed for workspace resolution but overlay is never compiled.
COPY Cargo.toml ./
COPY server/Cargo.toml  server/Cargo.toml
COPY overlay/Cargo.toml overlay/Cargo.toml

# Stub the server source so cargo can compile all dependencies without real code.
# This layer is cached as long as the manifests don't change.
RUN mkdir -p server/src && echo 'fn main(){}' > server/src/main.rs
RUN cargo build --release -p backseat-server

# Copy real source and rebuild — only the server crate recompiles.
COPY server/src server/src
RUN touch server/src/main.rs && cargo build --release -p backseat-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/backseat-server /usr/local/bin/backseat-server
EXPOSE 3000
ENV PORT=3000
CMD ["backseat-server"]
