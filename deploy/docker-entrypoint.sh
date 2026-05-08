#!/bin/sh
set -e

# Only require /dev/net/tun when VPN mode is enabled.
# Standard production deploys do not need this device.
if [ -n "${BITTICE_VPN_DIR:-}" ]; then
    if [ ! -c /dev/net/tun ]; then
        mkdir -p /dev/net
        if mknod /dev/net/tun c 10 200 2>/dev/null; then
            chmod 600 /dev/net/tun
        else
            echo "WARN: BITTICE_VPN_DIR is set but /dev/net/tun is not available."
            echo "WARN: Start with deploy/docker-compose.vpn.yaml (privileged + NET_ADMIN + /dev/net/tun)."
        fi
    fi
fi

exec bittice "$@"
