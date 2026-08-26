#!/bin/sh
set -eu

if [ -z "${AUTHORIZED_KEY:-}" ]; then
  echo "AUTHORIZED_KEY is required" >&2
  exit 64
fi

printf '%s\n' "$AUTHORIZED_KEY" > /home/monkey/.ssh/authorized_keys
chown monkey:monkey /home/monkey/.ssh/authorized_keys
chmod 0600 /home/monkey/.ssh/authorized_keys

ssh-keygen -A >/dev/null 2>&1
exec /usr/sbin/sshd -D -e
