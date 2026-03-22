# Stage 1: Build
# Use rust:bookworm to match the glibc version of the runtime image (debian:bookworm-slim)
FROM rust:bookworm as builder

# Install system dependencies required for compilation
# protobuf-compiler is needed for tonic/prost
RUN apt-get update && apt-get install -y \
    protobuf-compiler \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Build the release binary
RUN cargo build --release

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

# Create the data directory and copy data for performance testing
RUN mkdir -p /app/data
COPY data/ /app/data/

# Set environment variable to allow external access
ENV BITTICE_HOST=0.0.0.0

# Expose the port
EXPOSE 3000

# Run the server command by default (HTTP mode)
CMD ["./bittice", "server", "--type", "http"]
