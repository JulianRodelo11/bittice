# Stage 1: Build
FROM rust:bookworm as builder

# Install system dependencies
RUN apt-get update && apt-get install -y \
    protobuf-compiler \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 1. Create a dummy project and build only dependencies (for caching)
# This layer will only be re-run if Cargo.toml or Cargo.lock change
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# 2. Now copy the actual source code and build the real project
# This layer will be re-run on every code change, but it will be FAST
# because dependencies are already compiled in the previous layer.
COPY . .
RUN touch src/main.rs && cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

WORKDIR /app

# Install necessary runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from the builder stage
COPY --from=builder /app/target/release/bittice .

# Create the data directory and copy data
RUN mkdir -p /app/data
COPY data/ /app/data/

# Set environment variable to allow external access
ENV BITTICE_HOST=0.0.0.0

# Expose the ports
EXPOSE 3000
EXPOSE 50051

# Default command
CMD ["./bittice", "server"]
