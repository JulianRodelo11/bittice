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
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
DIM='\033[0;90m'
BOLD='\033[1m'
NC='\033[0m' # No Color

print_rule() {
    printf '%b\n' "${DIM}============================================================${NC}"
}

installer_banner() {
    print_rule
    printf '%b\n' "${BOLD}${BLUE}BITTICE${NC} ${DIM}installer${NC}"
    printf '%b\n' "${DIM}Fast setup for local machines and cloud hosts${NC}"
    print_rule
    printf '%b\n' "${DIM}destination${NC} ${GREEN}$INSTALL_DIR${NC}"
    printf '%b\n' "${DIM}libexec    ${NC} ${GREEN}$LIBEXEC_DIR${NC}"
    print_rule
}

installer_step() {
    printf '\n%b\n' "${BOLD}${BLUE}==>${NC} ${BOLD}$1${NC}"
}

installer_info() {
    printf '%b\n' "${BLUE}  [..]${NC} $1"
}

installer_ok() {
    printf '%b\n' "${GREEN}  [ok]${NC} $1"
}

installer_warn() {
    printf '%b\n' "${YELLOW}  [!!]${NC} $1"
}

installer_error() {
    printf '%b\n' "${RED}  [x]${NC} $1"
}

installer_success() {
    print_rule
    printf '%b\n' "${BOLD}${GREEN}Bittice is ready.${NC}"
    printf '%b\n' "${DIM}binary${NC}      ${BLUE}$1${NC}"
    printf '%b\n' "${DIM}next${NC}        $2"
    print_rule
}

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
        installer_ok "PATH already includes $INSTALL_DIR."
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
        installer_ok "Added $INSTALL_DIR to PATH via $rcfile."
        installer_info "Open a new terminal, or run: source \"$rcfile\""
    }
    if [ "$(uname -s)" = "Darwin" ]; then
        append_hook "${HOME}/.zshrc" 1
    else
        append_hook "${HOME}/.bashrc" 1
        append_hook "${HOME}/.profile" 1
    fi
}

installer_banner
installer_step "Inspecting host platform"

# 1. Detect Operating System
OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH_TYPE=$(uname -m)

case "$OS_TYPE" in
    linux*)     OS="linux" ;;
    darwin*)    OS="macos" ;;
    *)          installer_error "Operating system not supported by this script: $OS_TYPE"; exit 1 ;;
esac

# 2. Detect Architecture
case "$ARCH_TYPE" in
    x86_64)     ARCH="x86_64" ;;
    arm64|aarch64) ARCH="aarch64" ;;
    *)          installer_error "Architecture not supported: $ARCH_TYPE"; exit 1 ;;
esac

installer_ok "Target platform: $OS / $ARCH"

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
installer_step "Resolving release"
if [ -n "${BITTICE_VERSION:-}" ]; then
    LATEST_TAG="$BITTICE_VERSION"
    installer_info "Using requested version tag: $LATEST_TAG"
else
    installer_info "Checking for latest version on GitHub..."
    # Robust extraction of the first tag_name found in the releases list
    LATEST_TAG=$(curl -s "https://api.github.com/repos/$REPO/releases" | grep '"tag_name":' | head -n 1 | sed -E 's/.*"tag_name": "([^"]+)".*/\1/')
fi

if [ -z "$LATEST_TAG" ] || [ "$LATEST_TAG" == "null" ] || [[ "$LATEST_TAG" == http* ]]; then
    installer_error "Could not determine the latest version tag ($LATEST_TAG)."
    exit 1
fi

installer_ok "Installing $LATEST_TAG for $OS ($ARCH)."

# 4. Download binary (prefer per-OS bundle; fallback to legacy standalone asset name)
installer_step "Downloading package"
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
        installer_error "Failed to download binary from $DOWNLOAD_URL"
        exit 1
    fi
    chmod +x "$TEMP_FILE"
    installer_ok "Downloaded standalone asset $TARGET."
else
    if download_via_bundle; then
        installer_ok "Downloaded OS bundle for $OS ($ARCH)."
    else
        installer_warn "Bundle not available; trying standalone asset $TARGET."
        DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/$TARGET"
        if ! curl -sSLf "$DOWNLOAD_URL" -o "$TEMP_FILE"; then
            installer_error "Failed to download from bundle and from $DOWNLOAD_URL"
            installer_info "Ensure release $LATEST_TAG finished building, or set BITTICE_USE_LEGACY_ASSET=1."
            exit 1
        fi
        chmod +x "$TEMP_FILE"
        installer_ok "Downloaded standalone asset $TARGET."
    fi
fi

# 5. Install binary (chmod is applied here and in finalize; sudo only if the directory is not writable)
ensure_dir "$INSTALL_DIR"
installer_step "Installing binary"
installer_info "Copying $BINARY_NAME into $INSTALL_DIR"
if mv "$TEMP_FILE" "$INSTALL_DIR/$BINARY_NAME" 2>/dev/null; then
    :
else
    installer_warn "Requesting elevated permissions once (sudo) to write under $INSTALL_DIR."
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
    installer_step "Configuring cloud runtime"
    installer_info "This system looks like a cloud server (AWS, GCP, or Azure)."
    SETUP_DOCKER=""
    if [ -n "${BITTICE_SETUP_DOCKER:-}" ]; then
        SETUP_DOCKER="$BITTICE_SETUP_DOCKER"
    elif is_true "${BITTICE_CLOUD_AUTO:-}" || [ ! -t 0 ]; then
        SETUP_DOCKER="true"
        installer_info "Cloud auto mode enabled: Docker setup selected by default."
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
            installer_warn "Docker not found. Installing Docker..."
            curl -fsSL https://get.docker.com | sh
            sudo usermod -aG docker "$(install_owner)"
            installer_ok "Docker installed. You may need to re-login for group permissions."
        fi

        # Install Docker Compose if missing
        if ! command -v docker-compose &> /dev/null; then
             installer_info "Installing docker-compose..."
             sudo curl -L "https://github.com/docker/compose/releases/latest/download/docker-compose-$(uname -s)-$(uname -m)" -o /usr/local/bin/docker-compose
             sudo chmod +x /usr/local/bin/docker-compose
        fi

        # 1. Pull official image from GHCR
        IMAGE_NAME="ghcr.io/julianrodelo11/bittice:${LATEST_TAG}"
        installer_info "Pulling official Bittice image: $IMAGE_NAME"
        if ! docker pull "$IMAGE_NAME"; then
            installer_warn "Could not pull image $IMAGE_NAME. Using local build as fallback."
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
        labels:
            - com.centurylinklabs.watchtower.enable=true
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
        labels:
            - com.centurylinklabs.watchtower.enable=true
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
        installer_ok "Cloud compose ready at $COMPOSE_FILE (vpn_mode=$VPN_MODE)."

        # 3. Start/Restart Bittice Service immediately
    installer_info "Starting Bittice Engine..."
        # Stop everything first to ensure a clean state
        if command -v docker-compose &> /dev/null; then
                        docker-compose -f "$COMPOSE_FILE" down &> /dev/null || true
                        docker-compose -f "$COMPOSE_FILE" pull || true
                        if ! docker-compose -f "$COMPOSE_FILE" up -d --remove-orphans; then
        installer_error "Failed to start Bittice Engine with docker-compose."
        installer_info "Please check if ports 3000, 8080, or 50051 are already in use."
                exit 1
            fi
        else
                        docker compose -f "$COMPOSE_FILE" down &> /dev/null || true
                        docker compose -f "$COMPOSE_FILE" pull || true
                        if ! docker compose -f "$COMPOSE_FILE" up -d --remove-orphans; then
        installer_error "Failed to start Bittice Engine with docker compose."
        installer_info "Please check if ports 3000, 8080, or 50051 are already in use."
                exit 1
            fi
        fi

        # 4. Create the 'bittice' command wrapper on the host
    installer_info "Creating bittice command wrapper..."
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

        installer_ok "Bittice Engine is now running in the background."
        installer_info "To watch CDC and HTTP logs like on your PC: bittice"
        installer_info "Configure and sync databases from your workstation, then redeploy."
        installer_info "Compose file: $COMPOSE_FILE"
        installer_info "Data dir: $APP_DIR/data"
        installer_info "VPN dir: $APP_DIR/vpn"
        else
                installer_warn "Skipping Docker background setup on cloud instance."
    fi
fi

# 7. Finalize
installer_step "Final summary"
if command -v "$BINARY_NAME" &>/dev/null; then
    installer_success "$INSTALL_DIR/$BINARY_NAME" "Run: bittice"
else
    installer_success "$INSTALL_DIR/$BINARY_NAME" "Open a new terminal, then run: bittice"
fi
