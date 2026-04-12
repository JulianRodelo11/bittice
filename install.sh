#!/bin/bash

# Bittice Installer Script
# Detects OS/Arch and downloads the latest binary from GitHub Releases

set -e

REPO="JulianRodelo11/bittice"
BINARY_NAME="bittice"
INSTALL_DIR="/usr/local/bin"

# Colores para la terminal
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${BLUE}--- Instalador de Bittice ---${NC}"

# 1. Detectar Sistema Operativo (Tu workflow actual solo genera para Linux Musl)
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
if [ "$OS" != "linux" ]; then
    echo -e "${RED}Error: Tu workflow de GitHub actualmente solo genera binarios para Linux (musl).${NC}"
    echo -e "Para otros sistemas, instala desde el código fuente:"
    echo -e "${BLUE}cargo install --path .${NC}"
    exit 1
fi

# 2. Detectar Arquitectura
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)     TARGET="x86_64-unknown-linux-musl" ;;
    arm64|aarch64) TARGET="aarch64-unknown-linux-musl" ;;
    *)          echo -e "${RED}Error: Arquitectura no soportada: $ARCH${NC}"; exit 1 ;;
esac

# 3. Obtener la última versión de GitHub
echo -e "Buscando la última versión en GitHub..."
LATEST_TAG=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$LATEST_TAG" ] || [ "$LATEST_TAG" == "null" ]; then
    echo -e "${RED}No se encontró ninguna versión publicada (tag) en GitHub.${NC}"
    echo -e "Una vez que hagas un 'git tag v0.1.0 && git push --tags', este script funcionará."
    echo -e "Mientras tanto, puedes instalar localmente con: ${BLUE}cargo install --path .${NC}"
    exit 1
fi

echo -e "Instalando versión ${GREEN}$LATEST_TAG${NC} para ${GREEN}$TARGET${NC}..."

# 4. Descargar el binario (Tu workflow sube el binario directamente, no comprimido)
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/${BINARY_NAME}-${TARGET}"
TEMP_FILE=$(mktemp)

curl -L "$DOWNLOAD_URL" -o "$TEMP_FILE"
chmod +x "$TEMP_FILE"

# 5. Mover al directorio de binarios
echo -e "Moviendo binario a $INSTALL_DIR (puede requerir sudo)..."
if [ -w "$INSTALL_DIR" ]; then
    mv "$TEMP_FILE" "$INSTALL_DIR/$BINARY_NAME"
else
    sudo mv "$TEMP_FILE" "$INSTALL_DIR/$BINARY_NAME"
fi

# 6. Finalizar
echo -e "${GREEN}¡Bittice ($LATEST_TAG) instalado correctamente!${NC}"
echo -e "Escribe '${BLUE}bittice --help${NC}' para comenzar."
