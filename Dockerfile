# Container image for @bitefoundry/obsidian (stdio MCP server, CLI, and TUI).
#
# Installs the verified Linux release binary (GLIBC <= 2.28) directly from
# GitHub Releases instead of compiling Rust from source, so builds complete in
# seconds and match the published release artifact.
#
# Examples:
#   docker run -i --rm -v ~/MyVault:/data/vault ghcr.io/bitefoundry/obsidian:0.4.5
#
#   docker run -i --rm -v ~/MyVault:/vault -e OBSIDIAN_VAULT=/vault \
#     ghcr.io/bitefoundry/obsidian:0.4.5
FROM debian:bookworm-slim

ARG OBSIDIAN_VERSION=0.4.5
ARG TARGETARCH

LABEL org.opencontainers.image.title="obsidian" \
      org.opencontainers.image.description="Single Rust binary pairing a stdio MCP server, CLI, local HTTP API, and Ratatui TUI with an interactive Braille knowledge graph for local Obsidian vaults over SQLite WAL + FTS5." \
      org.opencontainers.image.source="https://github.com/BiteFoundry/obsidian" \
      org.opencontainers.image.documentation="https://github.com/BiteFoundry/obsidian#readme" \
      org.opencontainers.image.version="${OBSIDIAN_VERSION}" \
      org.opencontainers.image.licenses="MIT" \
      io.modelcontextprotocol.server.name="io.github.BiteFoundry/obsidian"

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      amd64) target="x86_64-unknown-linux-gnu" ;; \
      arm64) target="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    base="https://github.com/BiteFoundry/obsidian/releases/download/v${OBSIDIAN_VERSION}"; \
    archive="bitefoundry-obsidian-${OBSIDIAN_VERSION}-${target}.tar.gz"; \
    cd /tmp; \
    curl -fsSLO "${base}/${archive}"; \
    curl -fsSLO "${base}/SHA256SUMS.txt"; \
    grep -F "  ${archive}" SHA256SUMS.txt > expected.sha256; \
    test "$(wc -l < expected.sha256)" -eq 1; \
    sha256sum -c expected.sha256; \
    tar -xzf "${archive}" bitefoundry-obsidian; \
    install -m 0755 bitefoundry-obsidian /usr/local/bin/bitefoundry-obsidian; \
    rm -f "/tmp/${archive}" /tmp/SHA256SUMS.txt /tmp/expected.sha256 /tmp/bitefoundry-obsidian; \
    bitefoundry-obsidian --version

ENV OBSIDIAN_CONFIG_DIR=/data/config \
    OBSIDIAN_DATA_DIR=/data/state \
    OBSIDIAN_APP_CONFIG_PATH=/data/obsidian.json \
    OBSIDIAN_VAULT=/data/vault

RUN mkdir -p /data/config /data/state /data/vault \
 && printf '%s\n' '{"vaults":{}}' > /data/obsidian.json \
 && bitefoundry-obsidian init /data/vault \
 && chmod -R 0777 /data

WORKDIR /data

ENTRYPOINT ["bitefoundry-obsidian"]
CMD ["mcp"]
