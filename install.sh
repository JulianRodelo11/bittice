#!/bin/bash
set -e

# Configuración del repositorio
REPO="JulianRodelo11/bittice"
BINARY_NAME="bittice"

# 1. Función para detectar si estamos en la nube
is_cloud() {
    # Verificar AWS
    if curl -s -m 2 http://169.254.169.254/latest/meta-data/ > /dev/null; then return 0; fi
    # Verificar GCP
    if curl -s -m 2 -H "Metadata-Flavor: Google" http://metadata.google.internal/computeMetadata/v1/ > /dev/null; then return 0; fi
    # Verificar Azure
    if curl -s -m 2 -H "Metadata: true" "http://169.254.169.254/metadata/instance?api-version=2021-02-01" > /dev/null; then return 0; fi
    return 1
}

# 2. Detección de arquitectura
ARCH=$(uname -m)
case $ARCH in
    x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
    aarch64) TARGET="aarch64-unknown-linux-musl" ;;
    arm64)   TARGET="aarch64-unknown-linux-musl" ;; # Mac M1/M2 o ARM cloud
    *) echo "Arquitectura no soportada: $ARCH"; exit 1 ;;
esac

# 3. Lógica de instalación según el entorno
if is_cloud; then
    echo "--- [CLAVE] Entorno Cloud detectado: Instalación Automática vía Docker ---"
    
    # Asegurar que Docker y OpenVPN están instalados
    if ! command -v docker &> /dev/null || ! command -v openvpn &> /dev/null; then
        echo "Instalando dependencias (Docker/OpenVPN)..."
        sudo apt-get update && sudo apt-get install -y openvpn
        curl -fsSL https://get.docker.com | sh
        sudo usermod -aG docker $USER
    fi

    # Iniciar Bittice (Modo Servidor por defecto en Docker)
    echo "Levantando Bittice (Modo Beta)..."
    docker run -d \
        --name bittice \
        --restart always \
        -p 3000:3000 \
        -p 50051:50051 \
        -v bittice_data:/app/data \
        ghcr.io/$REPO:beta

    # Configurar Watchtower para Actualizaciones Automáticas
    echo "Configurando Watchtower (Auto-Update)..."
    docker run -d \
        --name watchtower \
        --restart always \
        -v /var/run/docker.sock:/var/run/docker.sock \
        containrrr/watchtower --interval 3600 bittice --cleanup

    echo "Bittice está instalado, corriendo y configurado para auto-actualizarse."

else
    echo "--- Entorno Local detectado: Instalando Binario ---"
    
    # Instalar OpenVPN si no existe
    if ! command -v openvpn &> /dev/null; then
        echo "OpenVPN no detectado. Intentando instalar..."
        if command -v apt-get &> /dev/null; then
            sudo apt-get update && sudo apt-get install -y openvpn
        elif command -v brew &> /dev/null; then
            brew install openvpn
        else
            echo "Aviso: No se pudo instalar OpenVPN automáticamente. Por favor instálelo manualmente si usará conexiones VPN."
        fi
    fi

    # Obtener última versión desde GitHub API
    RELEASE_URL=$(curl -s https://api.github.com/repos/$REPO/releases/latest | grep "browser_download_url" | grep "$TARGET" | cut -d '"' -f 4)
    
    if [ -z "$RELEASE_URL" ]; then
        echo "No se encontró un binario para $TARGET en la última release de GitHub."
        exit 1
    fi

    echo "Descargando bittice para $TARGET..."
    curl -L "$RELEASE_URL" -o "$BINARY_NAME"
    chmod +x "$BINARY_NAME"
    
    sudo mv "$BINARY_NAME" /usr/local/bin/
    echo "Bittice instalado en /usr/local/bin/$BINARY_NAME"
    
    # Inicializar automáticamente
    echo "Inicializando Bittice..."
    bittice
fi
