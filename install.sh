#!/bin/bash

# Bittice Installer Script
# Detects OS/Arch and downloads the latest binary from GitHub Releases

set -e

REPO="JulianRodelo11/bittice"
BINARY_NAME="bittice"
INSTALL_DIR="/usr/local/bin"

# Terminal colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${BLUE}--- Bittice Installer ---${NC}"

# 1. Detect Operating System
OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH_TYPE=$(uname -m)

case "$OS_TYPE" in
    linux*)     OS="linux" ;;
    darwin*)    OS="macos" ;;
    *)          echo -e "${RED}Error: Operating system not supported by this script: $OS_TYPE${NC}"; exit 1 ;;
esac

# 2. Detect Architecture
case "$ARCH_TYPE" in
    x86_64)     ARCH="x86_64" ;;
    arm64|aarch64) ARCH="aarch64" ;;
    *)          echo -e "${RED}Error: Architecture not supported: $ARCH_TYPE${NC}"; exit 1 ;;
esac

TARGET="bittice-${OS}-${ARCH}"

# --- Cloud Instance Detection ---
is_cloud_instance() {
    # 1. Check DMI/BIOS vendors
    if [ -f /sys/class/dmi/id/sys_vendor ]; then
        vendor=$(cat /sys/class/dmi/id/sys_vendor)
        if [[ "$vendor" == *"Amazon"* ]] || [[ "$vendor" == *"Google"* ]] || [[ "$vendor" == *"Microsoft"* ]]; then
            return 0
        fi
    fi
    # 2. Check metadata endpoints (timeout 1s)
    if curl -s -m 1 http://169.254.169.254/latest/meta-data/ > /dev/null 2>&1; then
        return 0 # AWS
    fi
    if curl -s -m 1 -H "Metadata-Flavor: Google" http://metadata.google.internal/computeMetadata/v1/instance/ > /dev/null 2>&1; then
        return 0 # GCP
    fi
    return 1
}

# 3. Get latest version from GitHub (including Betas)
echo -e "Checking for latest version on GitHub..."

# Robust extraction of the first tag_name found in the releases list
LATEST_TAG=$(curl -s "https://api.github.com/repos/$REPO/releases" | grep '"tag_name":' | head -n 1 | sed -E 's/.*"tag_name": "([^"]+)".*/\1/')

if [ -z "$LATEST_TAG" ] || [ "$LATEST_TAG" == "null" ] || [[ "$LATEST_TAG" == http* ]]; then
    echo -e "${RED}Error: Could not determine the latest version tag ($LATEST_TAG).${NC}"
    exit 1
fi

echo -e "Installing version ${GREEN}$LATEST_TAG${NC} for ${GREEN}$OS ($ARCH)${NC}..."

# 4. Download binary
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/$TARGET"
TEMP_FILE=$(mktemp)

# Use -f to fail on 404
if ! curl -sSLf "$DOWNLOAD_URL" -o "$TEMP_FILE"; then
    echo -e "${RED}Error: Failed to download binary from $DOWNLOAD_URL${NC}"
    echo -e "Please ensure the release $LATEST_TAG has finished building on GitHub."
    exit 1
fi

chmod +x "$TEMP_FILE"

# 5. Move to bin directory
echo -e "Moving binary to $INSTALL_DIR (may require sudo)..."
if [ -w "$INSTALL_DIR" ]; then
    mv "$TEMP_FILE" "$INSTALL_DIR/$BINARY_NAME"
else
    sudo mv "$TEMP_FILE" "$INSTALL_DIR/$BINARY_NAME"
fi

# 6. Instance Flow (Cloud Detection)
if is_cloud_instance; then
    echo -e "\n${BLUE}--- Cloud Instance Detected ---${NC}"
    echo -e "This system looks like a cloud server (AWS, GCP, or Azure)."
    echo -ne "Would you like to set up Docker for background execution? [Y/n]: "
    read -r setup_docker
    
    if [[ ! "$setup_docker" =~ ^([nN][oO]|[nN])$ ]]; then
        # Install Docker if missing
        if ! command -v docker &> /dev/null; then
            echo -e "Docker not found. ${BLUE}Installing Docker...${NC}"
            curl -fsSL https://get.docker.com | sh
            sudo usermod -aG docker "$USER"
            echo -e "${GREEN}Docker installed.${NC} (Note: you may need to re-login for group permissions)."
        fi

        # Install Docker Compose if missing
        if ! command -v docker-compose &> /dev/null; then
             echo -e "${BLUE}Installing docker-compose...${NC}"
             sudo curl -L "https://github.com/docker/compose/releases/latest/download/docker-compose-$(uname -s)-$(uname -m)" -o /usr/local/bin/docker-compose
             sudo chmod +x /usr/local/bin/docker-compose
        fi

        # 1. Pull official image from GHCR
        IMAGE_NAME="ghcr.io/julianrodelo11/bittice:${LATEST_TAG}"
        echo -e "${BLUE}Pulling official Bittice image: $IMAGE_NAME...${NC}"
        if ! docker pull "$IMAGE_NAME"; then
            echo -e "${RED}Error: Could not pull image $IMAGE_NAME. Using local build as fallback...${NC}"
            # Fallback to local build if tag is not yet available in GHCR
            cat > Dockerfile.local <<EOF
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libc6 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY bittice_bin /usr/local/bin/bittice
RUN chmod +x /usr/local/bin/bittice
EXPOSE 3000 50051
ENTRYPOINT ["bittice"]
EOF
            cp "$INSTALL_DIR/$BINARY_NAME" ./bittice_bin
            docker build -t bittice:local -f Dockerfile.local .
            rm bittice_bin Dockerfile.local
            IMAGE_NAME="bittice:local"
        fi

        # 2. Create docker-compose.yml
        if [ ! -f "docker-compose.yml" ]; then
            cat > docker-compose.yml <<EOF
services:
  bittice:
    image: $IMAGE_NAME
    container_name: bittice-engine
    restart: always
    environment:
      - BITTICE_HOST=0.0.0.0
    ports:
      - "3000:3000"
      - "50051:50051"
    volumes:
      - ./data:/app/data
EOF
            echo -e "${GREEN}Created docker-compose.yml.${NC}"
        fi

        echo -e "\n${GREEN}Instance Setup Complete!${NC}"
        echo -e "Let's configure your first database connection now (running inside Docker)..."
        
        # Ejecutar el asistente de configuración DENTRO de Docker
        # Redireccionamos /dev/tty para permitir interactividad en scripts pipeados (curl | bash)
        if docker run -it --rm \
            -v "$(pwd)/data:/app/data" \
            -v "$HOME:/app/host_home:ro" \
            -e "BITTICE_HOST=0.0.0.0" \
            "$IMAGE_NAME" < /dev/tty; then
            
            echo -e "\n${BLUE}Starting Bittice engine in the background...${NC}"
            if command -v docker-compose &> /dev/null; then
                docker-compose up -d
            elif docker compose version &> /dev/null; then
                docker compose up -d
            fi
            echo -e "${GREEN}✓ Bittice is now running in the background!${NC}"
            echo -e "You can see the logs with: ${BLUE}docker-compose logs -f${NC}"
        else
            echo -e "${RED}Configuration cancelled or failed.${NC}"
            echo -e "You can try again later by running: ${BLUE}docker-compose run --rm bittice${NC}"
        fi
    fi
fi

# 7. Finalize
echo -e "\n${GREEN}Bittice ($LATEST_TAG) installed successfully!${NC}"
echo -e "Type '${BLUE}bittice --help${NC}' to get started."
