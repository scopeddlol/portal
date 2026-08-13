# Deploying

Two halves. The gateway runs on a VPS with a public IP; the agent runs at home
next to the game servers. Nothing at home needs a port forward.

## What you need first

- A VPS with a public IPv4 and root.
- A domain on Cloudflare, and its **Zone ID** (zone overview page).
- A Cloudflare API token with **Zone → DNS → Edit** on that zone and nothing
  else. Cloudflare cannot scope a token tighter than a zone, which is why the
  gateway refuses to touch records outside the names it created.

## VPS

```bash
git clone <this repo> portal && cd portal/deploy
cp config.example.toml config.toml
$EDITOR config.toml          # public_ip, zone, zone_id, endpoint

cat > .env <<'EOF'
PORTAL_CF_API_TOKEN=<your cloudflare token>
PORTAL_ADMIN_TOKEN=<a long random string you invent>
EOF
chmod 600 .env

docker compose up -d --build
docker compose logs -f gateway
```

Open the UI on `listen` and sign in with `PORTAL_ADMIN_TOKEN`. If you leave
that variable unset the gateway generates a token, logs it once, and forgets it
on restart — fine for a first look, not for anything you want to keep.

The example binds the UI to `127.0.0.1:8080`. Reach it over an SSH tunnel
(`ssh -L 8080:127.0.0.1:8080 vps`) or put a reverse proxy with TLS in front.
Do not expose it directly: the admin token is the only thing between the
internet and control of your DNS.

UDP `51820` must be open on the VPS firewall or the tunnel never comes up.

### Not using Docker

Build with `cargo build --release`, put `portal-gateway` on the box, install
`nftables` and `wireguard-tools`, set `net.ipv4.ip_forward=1`, and run it with
`PORTAL_CONFIG` pointing at your config. It needs `CAP_NET_ADMIN`; a systemd
unit with `AmbientCapabilities=CAP_NET_ADMIN` is enough — it does not need to
be root.

## Home

In the web UI, **Create enrollment token**, then on the machine with the game
servers:

```bash
cd portal/deploy
docker compose -f agent-compose.yml run --rm agent \
  enroll --gateway https://portal.example.com --token <token> --name basement-box
docker compose -f agent-compose.yml up -d
```

The token is single-use and expires in an hour. The agent generates its own
WireGuard key at enrollment; the private half never leaves the machine.

## Adding a server

In the UI: pick the agent, name it, choose a subdomain, tick the games. A
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
game. Cloudflare is the control plane; your VPS is the data plane.

## Checking it works

```bash
# On the VPS: the rules the gateway wrote.
nft list table ip portal

# Peers and handshakes. A handshake under ~2 minutes old means the agent is up.
wg show wg0

# From anywhere: what players will resolve.
dig +short mc.example.com
dig +short SRV _minecraft._tcp.mc.example.com
```

If a service shows **dns pending** in the UI, the Cloudflare call failed; the
gateway retries once a minute and the reason is in `docker compose logs`.
