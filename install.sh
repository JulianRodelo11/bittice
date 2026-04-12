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

# 1. Detectar Sistema Operativo
OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH_TYPE=$(uname -m)

case "$OS_TYPE" in
    linux*)     OS="linux" ;;
    darwin*)    OS="macos" ;;
    *)          echo -e "${RED}Error: Sistema operativo no soportado por este script: $OS_TYPE${NC}"; exit 1 ;;
esac

# 2. Detectar Arquitectura
case "$ARCH_TYPE" in
    x86_64)     ARCH="x86_64" ;;
    arm64|aarch64) ARCH="aarch64" ;;
    *)          echo -e "${RED}Error: Arquitectura no soportada: $ARCH_TYPE${NC}"; exit 1 ;;
esac

TARGET="bittice-${OS}-${ARCH}"

# 3. Obtener la última versión de GitHub (incluyendo Betas)
echo -e "Buscando la última versión en GitHub..."
LATEST_TAG=$(curl -s "https://api.github.com/repos/$REPO/releases" | grep '"tag_name":' | head -n 1 | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$LATEST_TAG" ] || [ "$LATEST_TAG" == "null" ]; then
    echo -e "${RED}No se encontró ninguna versión publicada en GitHub.${NC}"
    exit 1
fi

echo -e "Instalando versión ${GREEN}$LATEST_TAG${NC} para ${GREEN}$OS ($ARCH)${NC}..."

# 4. Descargar el binario
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/$TARGET"
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
