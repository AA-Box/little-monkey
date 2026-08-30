ARG BASE_IMAGE=debian:bookworm-slim
FROM ${BASE_IMAGE}

ARG MONKEY_BINARY=src-tauri/target/debug/monkey-cli
RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates git openssh-server \
        libappindicator3-1 libgtk-3-0 librsvg2-2 libwebkit2gtk-4.1-0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /bin/bash monkey \
    && install -d -m 0755 /run/sshd \
    && install -d -m 0700 -o monkey -g monkey /home/monkey/.ssh \
    && install -d -m 0755 -o monkey -g monkey /var/lib/little-monkey

COPY ${MONKEY_BINARY} /usr/local/bin/monkey
COPY docker/execution-runner-ssh/entrypoint.sh /usr/local/bin/little-monkey-ssh-entrypoint
RUN chmod 0755 /usr/local/bin/monkey /usr/local/bin/little-monkey-ssh-entrypoint \
    && printf '%s\n' \
      'PasswordAuthentication no' \
      'KbdInteractiveAuthentication no' \
      'PermitRootLogin no' \
      'PubkeyAuthentication yes' \
      'AllowUsers monkey' \
      'X11Forwarding no' \
      'AllowTcpForwarding no' \
      'PermitTunnel no' \
      'GatewayPorts no' \
      'PermitUserEnvironment no' \
      'ClientAliveInterval 30' \
      'ClientAliveCountMax 2' \
      >> /etc/ssh/sshd_config

ENV HOME=/home/monkey \
    XDG_DATA_HOME=/home/monkey/.local/share
EXPOSE 22
ENTRYPOINT ["/usr/local/bin/little-monkey-ssh-entrypoint"]
