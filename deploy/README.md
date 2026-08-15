# Deployment

The gateway runs on a rented server with a public IP; the agent runs on the
private network holding the servers to be published. Nothing on that network
needs an inbound port.

The short path is in the [main README](../README.md#setup). This page covers the
configuration in detail.

## Files

| File | Purpose |
| --- | --- |
| `compose.yml` | Gateway and Caddy, using the published images |
| `agent-compose.yml` | The agent, for the private network |
| `Caddyfile` | TLS termination in front of the control panel |
| `config.example.toml` | Every gateway setting, annotated |
| `compose.build.yml` | Builds the gateway from source instead of pulling it |

## Prerequisites

- A server with a public IPv4 address and root access.
- A Cloudflare-managed zone and its **Zone ID**, shown on the zone overview.
- A Cloudflare API token with **Zone → DNS → Edit** on that zone and nothing
  more. Cloudflare cannot scope a token below zone level, which is why
  reconciliation refuses to touch records outside the names Portal created.

## Two things Compose cannot set

**IP forwarding** belongs on the host. Docker rejects namespaced network
sysctls when a container shares the host's network namespace, which the gateway
requires, so this cannot live in `compose.yml`:

```bash
echo 'net.ipv4.ip_forward=1' | sudo tee /etc/sysctl.d/99-portal.conf
sudo sysctl --system
```

Without it, DNAT drops every forwarded packet while the rest of the system
appears healthy.

**The control panel's own DNS record.** Portal writes records for the services
it manages, but not for itself — doing so would require it to already be
running and reachable. Add an `A` record for `portal` pointing at the server,
with the Cloudflare proxy off.

## Why the panel is served over TLS

The agent reaches the gateway's API across the internet, so the API is publicly
reachable by necessity. The admin token controls DNS for the whole zone and
must not cross the internet in cleartext. Caddy obtains and renews a
certificate without configuration beyond the hostname.

An alternative is binding the panel to localhost and reaching it over an SSH
tunnel, with only the API exposed through a proxy. The agent still requires a
route in.

## Verification

```bash
# Rules written by the gateway.
nft list table ip portal

# Peers and handshake ages. Under ~2 minutes means the agent is connected.
wg show wg0

# What players resolve.
dig +short mc.example.com
dig +short SRV _minecraft._tcp.mc.example.com
```

A service marked **dns pending** in the panel means the Cloudflare call failed.
The gateway retries once a minute; the reason appears in `docker compose logs`.

## Not using the published images

To build from source — for development, or to audit what runs:

```bash
git clone https://github.com/scopeddlol/portal.git
cd portal/deploy
cp config.example.toml config.toml
$EDITOR config.toml
docker compose -f compose.build.yml up -d --build
```

## Without Docker

Build with `cargo build --release` and install `portal-gateway` alongside
`nftables` and `wireguard-tools`. Enable IP forwarding, and run the binary with
`PORTAL_CONFIG` pointing at the configuration file. It needs `CAP_NET_ADMIN`
but not root; a systemd unit with `AmbientCapabilities=CAP_NET_ADMIN` is
sufficient.

## Agent configuration

The agent reads two variables and stores nothing:

| Variable | Meaning |
| --- | --- |
| `PORTAL_URL` | Base URL of the gateway, e.g. `https://portal.example.com` |
| `PORTAL_KEY` | Node key, issued when the node is created in the panel |
| `PORTAL_INTERFACE` | WireGuard interface name (default `portal0`) |
| `PORTAL_NO_TUNNEL` | Skip tunnel setup when WireGuard is managed elsewhere |

A fresh tunnel keypair is generated on every start and registered with the
gateway. The node's tunnel address is fixed when the node is created, so
restarts do not disturb the forwards pointing at it.

## Publishing a server

In the panel: **Add service** (node plus subdomain), then **Add port** on it
(the server's address on the local network, and its port). One node serves any
number of servers.

Two things the gateway does not do:

- **Server-side configuration.** Servers that advertise their own address —
  Simple Voice Chat, for instance — must be told the public address by hand.
  Portal does not modify server files.
- **Outbound firewall rules.** The agent connects out, so no inbound rule is
  required, but a block on outbound UDP 51820 prevents the tunnel forming.

## Cloudflare's role

The Cloudflare proxy carries HTTP/HTTPS on a fixed set of ports and will not
carry TCP 25565 or UDP 24454. Every record Portal writes is DNS-only by
construction: proxying a game record would break the game rather than conceal
anything. Cloudflare is the control plane; the rented server is the data plane.
