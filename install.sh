#!/bin/bash

# Bittice Installer Script
# Detects OS/Arch and downloads the latest binary from GitHub Releases

set -e

REPO="JulianRodelo11/bittice"
BINARY_NAME="bittice"
INSTALL_DIR="/usr/local/bin"
LIBEXEC_DIR="/usr/local/lib/bittice"
DEFAULT_CLOUD_APP_DIR="${HOME}/.bittice"

# Cloud installer behavior controls (env vars)
# - BITTICE_SETUP_DOCKER: true/false (forces docker setup decision on cloud)
# - BITTICE_CLOUD_AUTO: true/false (if true, skips prompts on cloud)
# - BITTICE_APP_DIR: cloud working directory (compose/data). Default: ~/.bittice
# - BITTICE_VPN_MODE: host|container (default: host)
# - BITTICE_VERSION: install a specific release tag (e.g. v0.1.56)

# Terminal colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m' # No Color

is_true() {
    case "${1:-}" in
        1|true|TRUE|yes|YES|y|Y|on|ON) return 0 ;;
        *) return 1 ;;
    esac
}

is_false() {
    case "${1:-}" in
        0|false|FALSE|no|NO|n|N|off|OFF) return 0 ;;
        *) return 1 ;;
    esac
}

ensure_dir() {
    local dir="$1"
    if [ -d "$dir" ]; then
        return 0
    fi
    if mkdir -p "$dir" 2>/dev/null; then
        return 0
    fi
    sudo mkdir -p "$dir"
}

copy_to_path() {
    local src="$1"
    local dst="$2"
    if cp "$src" "$dst" 2>/dev/null; then
        return 0
    fi
    sudo cp "$src" "$dst"
}

write_text_file() {
    local dst="$1"
    local content="$2"
    if printf "%s\n" "$content" > "$dst" 2>/dev/null; then
        return 0
    fi
    printf "%s\n" "$content" | sudo tee "$dst" > /dev/null
}

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

# 3. Resolve version tag
if [ -n "${BITTICE_VERSION:-}" ]; then
    LATEST_TAG="$BITTICE_VERSION"
    echo -e "Using requested version tag: ${GREEN}$LATEST_TAG${NC}"
else
    echo -e "Checking for latest version on GitHub..."
    # Robust extraction of the first tag_name found in the releases list
    LATEST_TAG=$(curl -s "https://api.github.com/repos/$REPO/releases" | grep '"tag_name":' | head -n 1 | sed -E 's/.*"tag_name": "([^"]+)".*/\1/')
fi

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

# Keep a stable host copy even when /usr/local/bin/bittice is replaced by a wrapper in cloud mode
ensure_dir "$LIBEXEC_DIR"
copy_to_path "$INSTALL_DIR/$BINARY_NAME" "$LIBEXEC_DIR/bittice-host"
if [ ! -x "$LIBEXEC_DIR/bittice-host" ]; then
    sudo chmod +x "$LIBEXEC_DIR/bittice-host"
fi

# 6. Instance Flow (Cloud Detection)
if is_cloud_instance; then
    echo -e "\n${BLUE}--- Cloud Instance Detected ---${NC}"
    echo -e "This system looks like a cloud server (AWS, GCP, or Azure)."
    SETUP_DOCKER=""
    if [ -n "${BITTICE_SETUP_DOCKER:-}" ]; then
        SETUP_DOCKER="$BITTICE_SETUP_DOCKER"
    elif is_true "${BITTICE_CLOUD_AUTO:-}" || [ ! -t 0 ]; then
        SETUP_DOCKER="true"
        echo -e "Cloud auto mode enabled: Docker setup selected by default."
    else
        echo -ne "Would you like to set up Docker for background execution? [Y/n]: "
        if [ -e /dev/tty ]; then
            read -r setup_docker < /dev/tty
        else
            read -r setup_docker
        fi
        if [[ "$setup_docker" =~ ^([nN][oO]|[nN])$ ]]; then
            SETUP_DOCKER="false"
        else
            SETUP_DOCKER="true"
        fi
    fi

    if is_true "$SETUP_DOCKER"; then
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
                        cp "$LIBEXEC_DIR/bittice-host" ./bittice_bin
            docker build -t bittice:local -f Dockerfile.local .
            rm bittice_bin Dockerfile.local
            IMAGE_NAME="bittice:local"
        fi

                # 2. Create or Update cloud docker-compose.yml (Permanent Service)
                APP_DIR="${BITTICE_APP_DIR:-$DEFAULT_CLOUD_APP_DIR}"
                VPN_MODE_RAW="${BITTICE_VPN_MODE:-host}"
                VPN_MODE=$(echo "$VPN_MODE_RAW" | tr '[:upper:]' '[:lower:]')
                if [ "$VPN_MODE" != "container" ]; then
                        VPN_MODE="host"
                fi

                ensure_dir "$APP_DIR"
                ensure_dir "$APP_DIR/data"
                ensure_dir "$APP_DIR/vpn"
                COMPOSE_FILE="$APP_DIR/docker-compose.yml"

                if [ "$VPN_MODE" = "container" ]; then
                        COMPOSE_CONTENT=$(cat <<EOF
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
            - $APP_DIR/data:/app/data
            - $APP_DIR/vpn:/app/vpn
        privileged: true
        cap_add:
            - NET_ADMIN
        devices:
            - "/dev/net/tun:/dev/net/tun"
EOF
)
                else
                        COMPOSE_CONTENT=$(cat <<EOF
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
            - $APP_DIR/data:/app/data
            - $APP_DIR/vpn:/app/vpn
EOF
)
                fi

                write_text_file "$COMPOSE_FILE" "$COMPOSE_CONTENT"
                echo -e "${GREEN}Cloud compose ready at $COMPOSE_FILE (vpn_mode=$VPN_MODE).${NC}"

        # 3. Start/Restart Bittice Service immediately
        echo -e "${BLUE}Starting Bittice Engine...${NC}"
        # Stop everything first to ensure a clean state
        if command -v docker-compose &> /dev/null; then
                        docker-compose -f "$COMPOSE_FILE" down &> /dev/null || true
                        if ! docker-compose -f "$COMPOSE_FILE" up -d --remove-orphans; then
                echo -e "${RED}Error: Failed to start Bittice Engine with docker-compose.${NC}"
                echo -e "Please check if ports 3000 or 50051 are already in use."
                exit 1
            fi
        else
                        docker compose -f "$COMPOSE_FILE" down &> /dev/null || true
                        if ! docker compose -f "$COMPOSE_FILE" up -d --remove-orphans; then
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
# If no arguments are provided, launch the interactive setup/monitor
if [ "\$#" -eq 0 ]; then
    docker exec -it bittice bittice
else
    docker exec -it bittice bittice "\$@"
fi
EOF
        sudo chmod +x /usr/local/bin/bittice

        echo -e "\n${GREEN}Bittice Engine is now running in the background!${NC}"
        echo -e "To configure your database or monitor events, simply type: ${BLUE}bittice${NC}"
                echo -e "Compose file: ${BLUE}$COMPOSE_FILE${NC}"
                echo -e "Data dir: ${BLUE}$APP_DIR/data${NC}"
                echo -e "VPN dir: ${BLUE}$APP_DIR/vpn${NC}"
        else
                echo -e "Skipping Docker background setup on cloud instance."
    fi
fi

# 7. Finalize
echo -e "\n${GREEN}Bittice ($LATEST_TAG) installed successfully!${NC}"
echo -e "Type '${BLUE}bittice${NC}' to get started."
