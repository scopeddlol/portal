<p align="center">
  <img src="assets/banner.svg" alt="Portal" width="820">
</p>

<p align="center">
  <a href="https://github.com/scopeddlol/portal/actions/workflows/release.yml"><img alt="Build" src="https://img.shields.io/github/actions/workflow/status/scopeddlol/portal/release.yml?style=for-the-badge&label=build&labelColor=0B1020&color=22D3EE"></a>
  <img alt="Status" src="https://img.shields.io/badge/status-beta%20v0.1-818CF8?style=for-the-badge&labelColor=0B1020">
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-MIT-C084FC?style=for-the-badge&labelColor=0B1020"></a>
  <img alt="Rust" src="https://img.shields.io/badge/built%20with-Rust-CE422B?style=for-the-badge&logo=rust&logoColor=white&labelColor=0B1020">
  <img alt="Docker" src="https://img.shields.io/badge/runs%20with-Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white&labelColor=0B1020">
  <img alt="Linux" src="https://img.shields.io/badge/agent-Linux-FCC624?style=for-the-badge&logo=linux&logoColor=white&labelColor=0B1020">
</p>

<p align="center">
  <b>Publishes game servers on a private network through a rented server — no port forwarding, no exposed home address.</b>
</p>

---

## Overview

Running a game server at home normally means forwarding a port on the router and
handing players an IP address that identifies the household.

Portal replaces both. A small agent on the home network holds an encrypted
WireGuard tunnel open to a rented server with a public IP. Players connect to a
normal subdomain such as `mc.example.com`, which resolves to the **rented**
server. The home address never appears in DNS, and no inbound port is opened on
the router.

<p align="center">
  <img src="assets/how-it-works.svg" alt="Players resolve a subdomain to a rented server, which passes traffic down an encrypted tunnel to servers on a home network" width="820">
</p>

Each forward names the address it targets, so **one agent covers an entire
network**. Ten servers on ten machines need one container, not ten. Public ports
are allocated automatically so servers that all use the same local port do not
collide, and Minecraft Java services can publish an `SRV` record so players
still connect to a bare hostname.

## Requirements

| | |
| --- | --- |
| 🌐 **Domain** | Managed by Cloudflare, with an API token scoped to DNS edit on that zone |
| 🖥️ **Rented server** | Any Linux VPS with a public IPv4 address and Docker |
| 🏠 **Home machine** | Linux with Docker, on the same network as the servers being published |

## Setup

Four files on the rented server, one on the home network. No clone, no
toolchain, no build step.

### 1. Gateway

In Cloudflare, add an `A` record for `portal` pointing at the rented server,
with the proxy (orange cloud) **off**. That subdomain serves the control panel.

Create a directory on the server containing these four files.

<details open>
<summary><b>compose.yml</b></summary>

```yaml
services:
  gateway:
    image: ghcr.io/scopeddlol/portal-gateway:beta
    container_name: portal-gateway
    restart: unless-stopped
    network_mode: host
    cap_add: [NET_ADMIN]
    volumes:
      - portal-data:/var/lib/portal
      - ./config.toml:/etc/portal/config.toml:ro
    env_file: [.env]

  caddy:
    image: caddy:2-alpine
    container_name: portal-caddy
    restart: unless-stopped
    network_mode: host
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data

volumes:
  portal-data:
  caddy-data:
```
</details>

<details open>
<summary><b>config.toml</b> — four values to change</summary>

```toml
[gateway]
public_ip = "203.0.113.10"        # the rented server's public IP
zone = "example.com"              # the Cloudflare zone
reserved_tcp_ports = [80, 443]    # left free for the control panel

[tunnel]
endpoint = "203.0.113.10:51820"   # the same IP; agents dial this

[cloudflare]
zone_id = "0123456789abcdef"      # Cloudflare dashboard → zone → overview
```
</details>

<details open>
<summary><b>Caddyfile</b> — TLS for the control panel</summary>

```caddyfile
portal.example.com {
	reverse_proxy 127.0.0.1:8080
}
```
</details>

<details open>
<summary><b>.env</b> — secrets, kept out of config.toml</summary>

```ini
PORTAL_CF_API_TOKEN=cloudflare-api-token
PORTAL_ADMIN_TOKEN=a-long-random-string
```

The Cloudflare token comes from **My Profile → API Tokens** and needs one
permission: **Zone → DNS → Edit**.
</details>

Start it:

```bash
chmod 600 .env

# Required. Without it the kernel drops every forwarded packet.
echo 'net.ipv4.ip_forward=1' | sudo tee /etc/sysctl.d/99-portal.conf
sudo sysctl --system

docker compose up -d
```

Ports **80**, **443** and **UDP 51820** must be open on the server's firewall —
the first two for the control panel's certificate, the last for the tunnel.

### 2. Control panel

Open `https://portal.example.com` and sign in with `PORTAL_ADMIN_TOKEN`.

### 3. Agent

Click **Add node**, give it a name, and copy the key it returns. The panel
renders the compose file below with the key already filled in.

<details open>
<summary><b>agent-compose.yml</b></summary>

```yaml
services:
  agent:
    image: ghcr.io/scopeddlol/portal-agent:beta
    container_name: portal-agent
    restart: unless-stopped
    network_mode: host
    cap_add: [NET_ADMIN]
    environment:
      PORTAL_URL: https://portal.example.com
      PORTAL_KEY: the-key-from-the-control-panel
```
</details>

```bash
docker compose -f agent-compose.yml up -d
```

That is the entire home-side setup. The agent stores nothing: it generates a
tunnel key on each start and registers it, so there is no volume to preserve
and no enrollment step. One agent serves every machine on its network.

### 4. Services and ports

Two steps per server:

1. **Add service** — choose the node and a subdomain such as `mc`.
2. **Add port** on that service — the server's address on the local network
   (`192.168.1.50`) and its port (`25565`). For Minecraft Java, ticking the SRV
   box lets players omit the port.

Servers may share the same local port; each service receives its own public
port, and the panel shows the exact address players use.

## What can be published

| | |
| --- | --- |
| 🟩 **Minecraft (Java)** | With the SRV box ticked, players connect to `mc.example.com` |
| 🟦 **Minecraft (Bedrock)** | Bedrock ignores SRV, so the address includes the port |
| 🎙️ **Voice chat** | An extra UDP port on the same subdomain |
| 🎮 **Anything else** | Any TCP or UDP port; Portal is indifferent to the protocol above it |

## Limitations

- The agent requires Linux with kernel WireGuard and `CAP_NET_ADMIN`.
- Traffic is masqueraded into the tunnel, so servers observe the tunnel address
  rather than individual player addresses. IP bans and IP-based plugins behave
  accordingly.
- IPv4 only.
- Servers that advertise their own address to clients — voice chat among them —
  must be configured with the public address by hand. Portal does not modify
  server files.

## Documentation

- [Deployment](deploy/README.md) — configuration reference, building from
  source, running without Docker, verification commands
- [How it works](docs/how-it-works.md) — architecture and design decisions

## License

MIT. See [LICENSE](LICENSE).
