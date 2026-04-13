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
    # Detect if we are in an interactive terminal
    if [ -t 0 ]; then
        TTY_RED="<&0"
    else
        TTY_RED="< /dev/tty"
    fi

    echo -ne "Would you like to set up Docker for background execution? [Y/n]: "
    if [ -e /dev/tty ]; then
        read -r setup_docker < /dev/tty
    else
        read -r setup_docker
    fi
    
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
RUN apt-get update && apt-get install -y ca-certificates libc6 openvpn iproute2 curl && rm -rf /var/lib/apt/lists/*
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

        # 2. Create or Update docker-compose.yml (Permanent Service)
        if [ -f "docker-compose.yml" ]; then
            # Heavy cleaning: ensure we have a clean service definition named 'bittice'
            # We recreate it to avoid naming conflicts in service keys vs container names
            cat > docker-compose.yml <<EOF
services:
  bittice:
    image: $IMAGE_NAME
    container_name: bittice
    restart: always
    environment:
      - BITTICE_HOST=0.0.0.0
    ports:
      - "3000:3000"
      - "50051:50051"
    volumes:
      - ./data:/app/data
EOF
            echo -e "${GREEN}Updated docker-compose.yml to use unified 'bittice' service.${NC}"
        else
            cat > docker-compose.yml <<EOF
services:
  bittice:
    image: $IMAGE_NAME
    container_name: bittice
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

        # 3. Start/Restart Bittice Service immediately
        echo -e "${BLUE}Starting Bittice Engine...${NC}"
        # Stop everything first to ensure a clean state
        if command -v docker-compose &> /dev/null; then
            docker-compose down &> /dev/null || true
            if ! docker-compose up -d --remove-orphans; then
                echo -e "${RED}Error: Failed to start Bittice Engine with docker-compose.${NC}"
                echo -e "Please check if ports 3000 or 50051 are already in use."
                exit 1
            fi
        else
            docker compose down &> /dev/null || true
            if ! docker compose up -d --remove-orphans; then
                echo -e "${RED}Error: Failed to start Bittice Engine with docker compose.${NC}"
                echo -e "Please check if ports 3000 or 50051 are already in use."
                exit 1
            fi
        fi

        # 4. Create the 'bittice' command wrapper on the host
        echo -e "${BLUE}Creating bittice command wrapper...${NC}"
        cat <<EOF | sudo tee /usr/local/bin/bittice > /dev/null
#!/bin/bash
# Bittice Docker Wrapper
docker exec -it bittice bittice "\$@"
EOF
        sudo chmod +x /usr/local/bin/bittice

        echo -e "\n${GREEN}Bittice is now running in the background!${NC}"
        
        # Wait for container to be ready
        echo -e "Waiting for Bittice Engine to initialize..."
        MAX_RETRIES=10
        COUNT=0
        while [ $COUNT -lt $MAX_RETRIES ]; do
            if [ "$(docker inspect -f '{{.State.Running}}' bittice 2>/dev/null)" == "true" ]; then
                break
            fi
            echo -n "."
            sleep 1
            COUNT=$((COUNT + 1))
        done
        echo -e "\n"

        echo -e "Launching setup wizard...\n"
        
        # 5. Launch Setup Wizard (Ensuring TTY for piped installs)
        # We use 'docker exec -it' and redirect /dev/tty (or stdin if terminal) to ensure interaction works
        if eval "docker exec -it bittice bittice setup $TTY_RED"; then
            echo -e "\n${BLUE}Reloading Bittice Engine to activate new configuration...${NC}"
            if command -v docker-compose &> /dev/null; then
                docker-compose restart bittice
            else
                docker compose restart bittice
            fi
            echo -e "${GREEN}✓ Bittice is now running and your database is synchronized!${NC}"
            echo -e "Try it: ${BLUE}curl http://localhost:3000/_config${NC}"
        else
            echo -e "${RED}Setup was not completed.${NC}"
        fi
    fi
fi

# 7. Finalize
echo -e "\n${GREEN}Bittice ($LATEST_TAG) installed successfully!${NC}"
echo -e "Type '${BLUE}bittice --help${NC}' to get started."
