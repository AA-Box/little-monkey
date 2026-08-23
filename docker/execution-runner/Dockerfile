ARG BASE_IMAGE=debian:bookworm-slim
FROM ${BASE_IMAGE}

ARG MONKEY_BINARY=src-tauri/target/debug/monkey-cli
RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates git \
        libappindicator3-1 libgtk-3-0 librsvg2-2 libwebkit2gtk-4.1-0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 monkey

COPY ${MONKEY_BINARY} /usr/local/bin/monkey
RUN chmod 0755 /usr/local/bin/monkey \
    && install -d -o monkey -g monkey /var/lib/little-monkey

USER monkey
ENV HOME=/home/monkey \
    XDG_DATA_HOME=/home/monkey/.local/share
WORKDIR /var/lib/little-monkey
ENTRYPOINT ["/usr/local/bin/monkey", "runner", "serve", "--stdio"]
