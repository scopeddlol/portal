# Portal — self-hosted game server proxy

Run game servers at home without exposing your home IP or forwarding a single
port on your router. A small agent next to the game server holds a WireGuard
tunnel open to a VPS you control; the VPS is the public edge, and Cloudflare
DNS is kept in sync automatically so each server gets a real subdomain.

## What Cloudflare does and does not do here

Cloudflare's proxy (the orange cloud) only handles HTTP/HTTPS on a fixed set of
ports. It will not carry TCP 25565 or UDP 24454. Cloudflare **Spectrum** does
proxy raw TCP/UDP — there is a Minecraft-specific offering on Pro/Business
plans — but generic TCP/UDP applications, which is what a voice-chat UDP port
is, sit behind an Enterprise add-on. Spectrum is also not a tunnel: it still
needs a reachable origin.

So in this design **Cloudflare is a DNS control plane, not a data plane**. The
API token writes DNS-only (grey-cloud) `A` records plus `SRV` records. Your VPS
is what players actually connect to, and your home IP is never published.

## Shape

```
   home / LAN                          VPS (public IP)                players
┌────────────────────┐          ┌──────────────────────────┐
│ game servers       │          │ nftables DNAT            │
│  :25565/tcp        │          │  :25565/tcp ─┐           │  ──▶ mc.example.com
│  :24454/udp        │          │  :24454/udp ─┤           │
│                    │          │              ▼           │
│ agent              │◀════════▶│ wg0  10.99.0.1           │
│  boringtun+smoltcp │ WireGuard│ (kernel WireGuard)       │
│  no TUN driver     │  UDP/51820│                          │
│                    │          │ gateway (Rust)           │
└────────────────────┘          │  web UI, API,            │
                                │  port allocator,         │
                                │  Cloudflare reconciler   │
                                └──────────────────────────┘
```

The gateway process never touches a packet of game traffic. Publishing a port
is an nftables DNAT rule into the tunnel subnet; the kernel does the rest. That
keeps latency overhead to the WireGuard encryption itself and means a gateway
restart does not drop anyone's connection.

On the agent side, `boringtun` runs the WireGuard protocol in userspace and
`smoltcp` provides the TCP/IP stack, so the Windows build needs no TUN driver
and no administrator rights — it decrypts a flow, then opens an ordinary socket
to the game server on `127.0.0.1`. A kernel-TUN backend sits behind the same
trait for the Linux/Docker agent, where it is essentially free.

## Multiple ports on one domain

The model is one `Service` (= one subdomain) owning many `PortMapping`s:

```
mc.example.com  →  A 203.0.113.10          (DNS-only)
  ├─ tcp 25565  →  _minecraft._tcp SRV     (Java clients need no port)
  └─ udp 24454  →  advertised via voice_host
```

Which ports a service needs comes from **composable game profiles** — YAML in
[`profiles/`](profiles/), not code. A Minecraft server with proximity voice
selects `minecraft-java` and `simple-voice-chat`, and the union of their port
templates is allocated. Adding a game is a new YAML file.

Edge ports are allocated per service and do not have to equal the local port,
because two servers on one machine can both want 25565 and only one can have it
on the public IP. Profiles say when that is not allowed: Bedrock's UDP 19132 is
marked `edge_port_fixed`, because console clients often cannot enter a custom
port, so a collision must surface as an error instead of an unjoinable server.

## Layout

| Path | What |
| --- | --- |
| `crates/proto` | Shared model, wire types, profile schema |
| `crates/gateway` | VPS side: API, web UI, WireGuard peers, nftables, Cloudflare |
| `crates/agent-core` | Tunnel client and forwarding engine |
| `crates/agent-cli` | Headless agent (Linux/Docker) |
| `profiles/` | Game profiles |
| `deploy/` | Docker Compose for the VPS |

## Status

Early. `crates/proto` is implemented and tested; the gateway and agent are in
progress.

## Building

Rust 1.82+. If you would rather not install a toolchain:

```bash
docker run --rm -v "$PWD:/w" -w /w rust:1-slim cargo test --workspace
```
