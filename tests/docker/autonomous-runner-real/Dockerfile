FROM ubuntu:22.04

RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates \
        git \
        libappindicator3-1 \
        libgtk-3-0 \
        librsvg2-2 \
        libwebkit2gtk-4.1-0 \
        python3 \
    && rm -rf /var/lib/apt/lists/*

COPY src-tauri/target/debug/monkey-cli /usr/local/bin/monkey-cli
COPY tests/docker/autonomous-runner-real/model_fixture.py /opt/model_fixture.py
COPY tests/docker/autonomous-runner-real/entrypoint.sh /usr/local/bin/docker-autonomous-entrypoint
COPY tests/docker/autonomous-runner-real/verify_configs.json /root/.local/share/com.littlemonkey.app/verify_configs.json

RUN chmod 0755 /usr/local/bin/monkey-cli /usr/local/bin/docker-autonomous-entrypoint \
    && mkdir -p /root/.littlemonkey /root/.local/share/com.littlemonkey.app

ENV XDG_DATA_HOME=/root/.local/share
ENTRYPOINT ["/usr/local/bin/docker-autonomous-entrypoint"]
