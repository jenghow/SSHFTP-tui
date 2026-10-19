#!/bin/sh
# Generate host keys on first boot (persisted in the /etc/ssh/keys volume) and
# run sshd in the foreground.
set -eu

mkdir -p /etc/ssh/keys /run/sshd

for type in ed25519 rsa; do
    key="/etc/ssh/keys/ssh_host_${type}_key"
    if [ ! -f "$key" ]; then
        ssh-keygen -q -t "$type" -N '' -f "$key"
    fi
done

exec /usr/sbin/sshd -D -e
