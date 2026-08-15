# Deploying

Two halves. The gateway runs on a rented server with a public IP; the agent
runs at home next to the game servers. Nothing at home needs a port forward.

The quick path is in the [main README](../README.md#setup) — copy four small
files, no clone and no compiler. This page is the detail behind it.

## What you need first

- A server with a public IPv4 and root.
- A domain on Cloudflare, and its **Zone ID** (zone overview page).
- A Cloudflare API token with **Zone → DNS → Edit** on that zone and nothing
  else. Cloudflare cannot scope a token tighter than a zone, which is why the
  gateway refuses to touch records outside the names it created.

## Files

| File | What it is |
| --- | --- |
| `compose.yml` | Gateway + Caddy, pulling published images. The normal way to run it. |
| `agent-compose.yml` | The agent, for the machine at home. |
| `Caddyfile` | HTTPS in front of the control panel. |
| `config.example.toml` | Every setting, annotated. Copy to `config.toml`. |
| `compose.build.yml` | Builds the gateway from source instead. For working on Portal itself. |

## Two things Compose cannot do for you

**IP forwarding** has to be set on the host. Docker refuses namespaced network
sysctls when a container shares the host's network namespace, which the gateway
must do, so this cannot live in `compose.yml`:

```bash
echo 'net.ipv4.ip_forward=1' | sudo tee /etc/sysctl.d/99-portal.conf
sudo sysctl --system
```

Without it, DNAT silently drops every game packet and everything else looks fine.

**The panel's own DNS record.** Portal creates records for the game servers you
add, but not for itself — it would have to be running and reachable to do that.
Add an `A` record for `portal` pointing at the server, orange cloud off.

## Why HTTPS is not optional here

The agent at home reaches the gateway's API over the internet, so the API has
to be publicly reachable. The admin token protects your DNS, so it must not
cross the internet in the clear. Caddy fetches and renews a certificate on its
own, which makes the secure path the easy one.

If you would rather not expose the panel at all, bind it to localhost, reach it
over an SSH tunnel, and put only the API behind a proxy — but the agent still
needs a way in.

## Checking it works

```bash
# The rules the gateway wrote.
nft list table ip portal

# Peers and handshakes. A handshake under ~2 minutes old means the agent is up.
wg show wg0

# What players will resolve.
dig +short mc.example.com
dig +short SRV _minecraft._tcp.mc.example.com
```

If a service shows **dns pending** in the panel, the Cloudflare call failed; the
gateway retries once a minute and the reason is in `docker compose logs`.

## Not using the published images

To build from source instead — because you are changing Portal, or you want to
audit what you run:

```bash
git clone https://github.com/scopeddlol/portal.git
cd portal/deploy
cp config.example.toml config.toml
$EDITOR config.toml
docker compose -f compose.build.yml up -d --build
```

## Without Docker at all

Build with `cargo build --release`, put `portal-gateway` on the box, install
`nftables` and `wireguard-tools`, enable IP forwarding, and run it with
`PORTAL_CONFIG` pointing at your config. It needs `CAP_NET_ADMIN`; a systemd
unit with `AmbientCapabilities=CAP_NET_ADMIN` is enough — it does not need to
be root.

## Adding a server

In the panel: pick the agent, name it, choose a subdomain, tick the games. A
Minecraft server with proximity voice is `minecraft-java` + `simple-voice-chat`.

Two things the gateway cannot do for you:

- **Config keys.** Simple Voice Chat has to be told its public address or
  clients show a red plug icon. The service list shows exactly what to set and
  where. Setting it is your job — silently rewriting someone's server config is
  a good way to lose their world settings.
- **Firewalls at home.** The agent connects out, so no inbound rule is needed,
  but an outbound block on UDP 51820 will stop it.

## Why nothing here is proxied through Cloudflare

The orange cloud carries HTTP/HTTPS on a fixed set of ports. It will not carry
TCP 25565 or UDP 24454. Every record this gateway writes is DNS-only, on
purpose: proxying a game record would not hide anything, it would break the
game. Cloudflare is the control plane; your server is the data plane.
