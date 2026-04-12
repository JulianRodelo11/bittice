# Dockerfile optimizado para Bittice
# Usa una imagen ligera de Debian para compatibilidad con GNU/Linux
FROM debian:bookworm-slim

# Instalar dependencias mínimas (Certificados CA para HTTPS y librerías de C)
RUN apt-get update && apt-get install -y ca-certificates libc6 && rm -rf /var/lib/apt/lists/*

# Argumento para recibir el binario específico de la arquitectura
ARG TARGETARCH
ARG BINARY_PATH

WORKDIR /app

# Copiar el binario pre-compilado desde el host al contenedor
COPY ${BINARY_PATH} /usr/local/bin/bittice
RUN chmod +x /usr/local/bin/bittice

# Puertos por defecto (REST y gRPC)
EXPOSE 3000 50051

# Ejecutar bittice por defecto
ENTRYPOINT ["bittice"]
