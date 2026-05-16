#!/bin/bash
# Prepara Ubuntu 22.04 con SSH y docker CLI.
# No arranca dockerd — usa el socket del host montado por ec2-start.sh.
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive

apt-get update -qq
apt-get install -y openssh-server docker.io curl rsync

# SSH: permitir root con clave
sed -i 's/#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
sed -i 's/#PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config
mkdir -p /run/sshd
/usr/sbin/sshd

touch /tmp/ready
