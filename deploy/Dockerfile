# Builds both binaries. The gateway image is the default target; the agent
# image is the same build with a different entrypoint, which keeps one
# dependency compile serving both.
FROM rust:1-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --bin portal-gateway --bin portal-agent

# ---- gateway ---------------------------------------------------------------
# nftables and wireguard-tools are the gateway's hands: it shells out to `nft`
# and `wg`. The container needs NET_ADMIN and the host network namespace to
# use them — see docker-compose.yml.
FROM debian:stable-slim AS gateway
RUN apt-get update && apt-get install -y --no-install-recommends \
        nftables wireguard-tools iproute2 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/portal-gateway /usr/local/bin/
ENV PORTAL_CONFIG=/etc/portal/config.toml
VOLUME ["/var/lib/portal"]
ENTRYPOINT ["/usr/local/bin/portal-gateway"]

# ---- agent -----------------------------------------------------------------
FROM debian:stable-slim AS agent
RUN apt-get update && apt-get install -y --no-install-recommends \
        wireguard-tools iproute2 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/portal-agent /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/portal-agent"]
