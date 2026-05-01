#!/bin/bash

# Bittice Installer Script
# Detects OS/Arch and downloads the latest binary from GitHub Releases

set -e

REPO="JulianRodelo11/bittice"
BINARY_NAME="bittice"
DEFAULT_CLOUD_APP_DIR="${HOME}/.bittice"

# Cloud installer behavior controls (env vars)
# - BITTICE_SETUP_DOCKER: true/false (forces docker setup decision on cloud)
# - BITTICE_CLOUD_AUTO: true/false (if true, skips prompts on cloud)
# - BITTICE_APP_DIR: cloud working directory (compose/data). Default: ~/.bittice
# - BITTICE_VPN_MODE: host|container (default on cloud: container)
# - BITTICE_VERSION: install a specific release tag (e.g. v0.1.56)
# - BITTICE_USE_LEGACY_ASSET: if true, download standalone bittice-{os}-{arch} instead of OS bundle (.zip / .tar.gz)
# - BITTICE_USE_SYSTEM_INSTALL=1: force /usr/local/bin on a normal machine (may prompt sudo once; binary is chowned to you)
# - BITTICE_INSTALL_DIR / BITTICE_LIBEXEC_DIR: full override of install paths

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

# --- Cloud Instance Detection ---
is_cloud_instance() {
    if [ -f /sys/class/dmi/id/sys_vendor ]; then
        vendor=$(cat /sys/class/dmi/id/sys_vendor)
        if [[ "$vendor" == *"Amazon"* ]] || [[ "$vendor" == *"Google"* ]] || [[ "$vendor" == *"Microsoft"* ]]; then
            return 0
        fi
    fi
    if curl -s -m 1 http://169.254.169.254/latest/meta-data/ > /dev/null 2>&1; then
        return 0
    fi
    if curl -s -m 1 -H "Metadata-Flavor: Google" http://metadata.google.internal/computeMetadata/v1/instance/ > /dev/null 2>&1; then
        return 0
    fi
    return 1
}

# Workstations: ~/.local/bin (no sudo). Cloud or BITTICE_USE_SYSTEM_INSTALL: /usr/local.
if [ -n "${BITTICE_INSTALL_DIR:-}" ]; then
    INSTALL_DIR="$BITTICE_INSTALL_DIR"
elif is_true "${BITTICE_USE_SYSTEM_INSTALL:-0}"; then
    INSTALL_DIR="/usr/local/bin"
elif is_cloud_instance; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="${HOME}/.local/bin"
fi

if [ -n "${BITTICE_LIBEXEC_DIR:-}" ]; then
    LIBEXEC_DIR="$BITTICE_LIBEXEC_DIR"
elif is_true "${BITTICE_USE_SYSTEM_INSTALL:-0}"; then
    LIBEXEC_DIR="/usr/local/lib/bittice"
elif is_cloud_instance; then
    LIBEXEC_DIR="/usr/local/lib/bittice"
else
    LIBEXEC_DIR="${HOME}/.local/lib/bittice"
fi

install_owner() {
    if [ -n "${SUDO_USER:-}" ]; then
        echo "$SUDO_USER"
    else
        id -un
    fi
}

install_primary_group() {
    local u
    u="$(install_owner)"
    id -gn "$u" 2>/dev/null || id -gn
}

file_owned_by_root() {
    local f="$1"
    [ -e "$f" ] || return 1
    [ "$(ls -nd "$f" | awk '{print $3}')" = "0" ]
}

# chmod +x and, if root owns the file, chown to the real user so `bittice update` / uninstall work without sudo.
finalize_binary_permissions() {
    local f owner grp
    owner="$(install_owner)"
    grp="$(install_primary_group)"
    for f in "$INSTALL_DIR/$BINARY_NAME" "$LIBEXEC_DIR/bittice-host"; do
        [ -f "$f" ] || continue
        chmod +x "$f" 2>/dev/null || sudo chmod +x "$f"
        if file_owned_by_root "$f"; then
            sudo chown "$owner:$grp" "$f" 2>/dev/null || true
        fi
    done
}

# Add INSTALL_DIR to PATH when missing (no sudo).
configure_path_for_workstation() {
    if is_cloud_instance; then
        return 0
    fi
    case ":${PATH:-}:" in *:"$INSTALL_DIR":*)
        echo -e "${GREEN}$INSTALL_DIR is already in your PATH.${NC}"
        return 0
        ;;
    esac
    local marker="# bittice installer PATH"
    append_hook() {
        local rcfile="$1"
        local create="$2"
        if [ ! -f "$rcfile" ]; then
            if [ "$create" != "1" ]; then
                return 0
            fi
            touch "$rcfile"
        fi
        if grep -qF "$marker" "$rcfile" 2>/dev/null; then
            return 0
        fi
        printf '\n%s\nexport PATH="%s:$PATH"\n' "$marker" "$INSTALL_DIR" >>"$rcfile"
        echo -e "${GREEN}Added ${BLUE}$INSTALL_DIR${NC} to PATH via ${BLUE}$rcfile${NC}"
        echo -e "  Open a ${BLUE}new terminal${NC}, or run: ${BLUE}source \"$rcfile\"${NC}"
    }
    if [ "$(uname -s)" = "Darwin" ]; then
        append_hook "${HOME}/.zshrc" 1
    else
        append_hook "${HOME}/.bashrc" 1
        append_hook "${HOME}/.profile" 1
    fi
}

echo -e "${BLUE}--- Bittice Installer ---${NC}"
echo -e "Install directory: ${GREEN}$INSTALL_DIR${NC}  (libexec: ${GREEN}$LIBEXEC_DIR${NC})"

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

# Map uname-style arch to bundle member filename inside OS archives (see release workflow)
bundle_member_for_arch() {
    case "$OS" in
        linux)
            case "$ARCH" in
                x86_64)  echo "bittice-x86_64-unknown-linux-musl" ;;
                aarch64) echo "bittice-aarch64-unknown-linux-gnu" ;;
                *) return 1 ;;
            esac
            ;;
        macos)
            case "$ARCH" in
                x86_64)  echo "bittice-x86_64-apple-darwin" ;;
                aarch64) echo "bittice-aarch64-apple-darwin" ;;
                *) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
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

# 4. Download binary (prefer per-OS bundle; fallback to legacy standalone asset name)
TEMP_FILE=$(mktemp)

download_via_bundle() {
    local member extract_dir zf
    member=$(bundle_member_for_arch) || return 1
    extract_dir=$(mktemp -d)

    case "$OS" in
        linux)
            if ! curl -sSLf "https://github.com/$REPO/releases/download/$LATEST_TAG/bittice-${LATEST_TAG}-linux.tar.gz" | tar -xzf - -C "$extract_dir"; then
                rm -rf "$extract_dir"
                return 1
            fi
            ;;
        macos)
            zf=$(mktemp)
            if ! curl -sSLf "https://github.com/$REPO/releases/download/$LATEST_TAG/bittice-${LATEST_TAG}-macos.zip" -o "$zf"; then
                rm -f "$zf"
                rm -rf "$extract_dir"
                return 1
            fi
            unzip -q "$zf" -d "$extract_dir"
            rm -f "$zf"
            ;;
        *)
            rm -rf "$extract_dir"
            return 1
            ;;
    esac

    if [ ! -f "$extract_dir/$member" ]; then
        rm -rf "$extract_dir"
        return 1
    fi
    cp "$extract_dir/$member" "$TEMP_FILE"
    chmod +x "$TEMP_FILE"
    rm -rf "$extract_dir"
    return 0
}

if is_true "${BITTICE_USE_LEGACY_ASSET:-0}"; then
    DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/$TARGET"
    if ! curl -sSLf "$DOWNLOAD_URL" -o "$TEMP_FILE"; then
        echo -e "${RED}Error: Failed to download binary from $DOWNLOAD_URL${NC}"
        exit 1
    fi
    chmod +x "$TEMP_FILE"
else
    if download_via_bundle; then
        echo -e "Downloaded OS bundle for ${GREEN}$OS${NC} (${GREEN}$ARCH${NC})."
    else
        echo -e "${BLUE}Bundle not available; trying standalone asset ${TARGET}...${NC}"
        DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/$TARGET"
        if ! curl -sSLf "$DOWNLOAD_URL" -o "$TEMP_FILE"; then
            echo -e "${RED}Error: Failed to download from bundle and from $DOWNLOAD_URL${NC}"
            echo -e "Ensure release $LATEST_TAG finished building, or set BITTICE_USE_LEGACY_ASSET=1."
            exit 1
        fi
        chmod +x "$TEMP_FILE"
    fi
fi

# 5. Install binary (chmod is applied here and in finalize; sudo only if the directory is not writable)
ensure_dir "$INSTALL_DIR"
echo -e "${BLUE}Installing ${BINARY_NAME} to ${GREEN}$INSTALL_DIR${NC}..."
if mv "$TEMP_FILE" "$INSTALL_DIR/$BINARY_NAME" 2>/dev/null; then
    :
else
    echo -e "${BLUE}Requesting elevated permissions once (sudo) to write under ${INSTALL_DIR}...${NC}"
    sudo mv "$TEMP_FILE" "$INSTALL_DIR/$BINARY_NAME"
fi
chmod +x "$INSTALL_DIR/$BINARY_NAME" 2>/dev/null || sudo chmod +x "$INSTALL_DIR/$BINARY_NAME"

# Keep a stable host copy even when /usr/local/bin/bittice is replaced by a wrapper in cloud mode
ensure_dir "$LIBEXEC_DIR"
copy_to_path "$INSTALL_DIR/$BINARY_NAME" "$LIBEXEC_DIR/bittice-host"
finalize_binary_permissions

configure_path_for_workstation

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
            sudo usermod -aG docker "$(install_owner)"
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
EXPOSE 3000 8080 50051
ENTRYPOINT ["bittice"]
EOF
                        cp "$LIBEXEC_DIR/bittice-host" ./bittice_bin
            docker build -t bittice:local -f Dockerfile.local .
            rm bittice_bin Dockerfile.local
            IMAGE_NAME="bittice:local"
        fi

                # 2. Create or Update cloud docker-compose.yml (Permanent Service)
                APP_DIR="${BITTICE_APP_DIR:-$DEFAULT_CLOUD_APP_DIR}"
                VPN_MODE_RAW="${BITTICE_VPN_MODE:-container}"
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
            - BITTICE_ENGINE_ONLY=1
            - BITTICE_VPN_DIR=/app/vpn
            - BITTICE_VPN_SPLIT_TUNNEL=true
        ports:
            - "3000:3000"
            - "8080:8080"
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
            - BITTICE_ENGINE_ONLY=1
            - BITTICE_VPN_DIR=/app/vpn
            - BITTICE_VPN_SPLIT_TUNNEL=true
        ports:
            - "3000:3000"
            - "8080:8080"
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
                echo -e "Please check if ports 3000, 8080, or 50051 are already in use."
                exit 1
            fi
        else
                        docker compose -f "$COMPOSE_FILE" down &> /dev/null || true
                        if ! docker compose -f "$COMPOSE_FILE" up -d --remove-orphans; then
                echo -e "${RED}Error: Failed to start Bittice Engine with docker compose.${NC}"
                echo -e "Please check if ports 3000, 8080, or 50051 are already in use."
                exit 1
            fi
        fi

        # 4. Create the 'bittice' command wrapper on the host
        echo -e "${BLUE}Creating bittice command wrapper...${NC}"
                cat <<EOF | sudo tee /usr/local/bin/bittice > /dev/null
#!/bin/bash
# Bittice Docker Wrapper (host → container). Engine runs as PID 1; no interactive wizard on the server.
if [ "\$#" -eq 0 ]; then
    docker logs -f bittice
else
    docker exec -it bittice bittice "\$@"
fi
EOF
        sudo chmod +x /usr/local/bin/bittice
        if file_owned_by_root /usr/local/bin/bittice; then
            sudo chown "$(install_owner):$(install_primary_group)" /usr/local/bin/bittice 2>/dev/null || true
        fi

        echo -e "\n${GREEN}Bittice Engine is now running in the background!${NC}"
        echo -e "To watch CDC and HTTP logs like on your PC: ${BLUE}bittice${NC} (runs docker logs -f)."
        echo -e "Configure and sync databases from your workstation, then redeploy — not inside this container."
                echo -e "Compose file: ${BLUE}$COMPOSE_FILE${NC}"
                echo -e "Data dir: ${BLUE}$APP_DIR/data${NC}"
                echo -e "VPN dir: ${BLUE}$APP_DIR/vpn${NC}"
        else
                echo -e "Skipping Docker background setup on cloud instance."
    fi
fi

# 7. Finalize
echo -e "\n${GREEN}Bittice ($LATEST_TAG) installed successfully!${NC}"
echo -e "Binary: ${BLUE}$INSTALL_DIR/$BINARY_NAME${NC}"
if command -v "$BINARY_NAME" &>/dev/null; then
    echo -e "Run: ${BLUE}bittice${NC}"
else
    echo -e "Open a ${BLUE}new terminal${NC} (PATH was updated), then run: ${BLUE}bittice${NC}"
fi
