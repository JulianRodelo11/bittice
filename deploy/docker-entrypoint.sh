#!/bin/sh
set -e

# Ensure the TUN device exists for OpenVPN (needed in some Docker environments like Docker Desktop).
if [ ! -c /dev/net/tun ]; then
    mkdir -p /dev/net
    mknod /dev/net/tun c 10 200
    chmod 600 /dev/net/tun
fi

exec bittice "$@"
