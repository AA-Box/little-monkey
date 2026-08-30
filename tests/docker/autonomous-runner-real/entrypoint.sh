#!/bin/sh
set -eu

home=/tmp/little-monkey-home
data=/tmp/little-monkey-data
mkdir -p "$home" "$data/com.littlemonkey.app"
cp /opt/little-monkey-fixture/verify_configs.json "$data/com.littlemonkey.app/verify_configs.json"
export HOME="$home"
export XDG_DATA_HOME="$data"

python3 /opt/model_fixture.py &
fixture_pid=$!
trap 'kill "$fixture_pid" 2>/dev/null || true' EXIT INT TERM

for _ in $(seq 1 50); do
    if python3 -c 'import socket; socket.create_connection(("127.0.0.1", 18080), 0.1).close()' 2>/dev/null; then
        break
    fi
    sleep 0.1
done

exec /usr/local/bin/monkey-cli "$@"
