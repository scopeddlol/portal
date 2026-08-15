# How it works

The detail behind the [README](../README.md). This is the version for people
who want to know why it is built this way.

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
│  kernel WireGuard  │ WireGuard│ (kernel WireGuard)       │
│  + TCP/UDP relay   │  UDP/51820│                          │
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

On the agent side, the tunnel sits behind a `TunnelBackend` trait and the
forwarding engine only ever asks it for an address to bind on. Today that is
kernel WireGuard, which is essentially free on Linux and in Docker: the agent
accepts on its tunnel address and opens an ordinary socket to whichever
address on its network the mapping names.

The intended second backend runs `boringtun` and `smoltcp` in userspace, so a
Windows build would need no TUN driver and no administrator rights. **That one
is not written yet.**

## The model

Three things:

```
Node (a machine running the agent)
 └── Service (one subdomain)
      └── PortMapping (public port -> address:port behind the node)
```

A mapping names its own `local_host`, which is the piece that matters most in
practice: the agent bridges to any address it can reach, so one container
fronts a whole LAN. Ten Minecraft servers on ten different machines need one
agent, not ten.

```
mc.example.com  →  A 203.0.113.10                    (DNS-only)
  ├─ tcp 25565  →  _minecraft._tcp SRV  →  192.168.1.50:25565
  └─ udp 24454  →                          192.168.1.50:24454
```

Edge ports are allocated and need not equal the local port, because ten servers
can all sensibly listen on 25565 on their own machines while only one of them
can hold 25565 on the public IP. The SRV record is what makes that invisible:
Java clients look it up and connect to the bare hostname regardless of which
public port the service actually got. Games that ignore SRV (Bedrock among
them) show the port to the player instead.

There is no game-specific configuration anywhere in the system. A port is a
port.

## Decisions worth knowing

- A service is given its game's well-known public port when nothing else holds
  it, so the first Minecraft server on a VPS looks exactly like one with a
  forwarded port. Later services fall back to `30000-32767` — below Linux's
  ephemeral range, so an allocated port cannot collide with an outbound
  connection the VPS makes itself.
- Java clients follow the `SRV` record, so a relocated edge port stays
  invisible: players still type `smp.example.com`. Games that ignore SRV show
  the allocated port instead, which the UI displays verbatim.
- DNS reconciliation only ever touches a service's own name and the records
  beneath it, so it cannot delete the `MX` records for your mail while
  tidying up a game server.
- Return traffic is masqueraded into the tunnel, so the game server sees
  connections from the tunnel address rather than the player's real IP. Ban
  lists and IP-based plugins will see one address for everyone.
- Tokens are stored as hashes, so a copy of `portal.db` yields no working
  credentials. Each agent's WireGuard peer is allowed exactly its own `/32`,
  so one compromised home machine cannot source traffic as another.
- The nftables ruleset is replaced wholesale through a single `nft -f`
  transaction, so the rules cannot drift from the database and a crash
  part-way leaves the old ruleset serving players.

## Layout

| Path | What |
| --- | --- |
| `crates/proto` | Shared model and wire types |
| `crates/gateway` | VPS side: API, web UI, WireGuard peers, nftables, Cloudflare |
| `crates/agent-core` | Tunnel client and forwarding engine |
| `crates/agent-cli` | Headless agent (Linux/Docker) |
| `deploy/` | Docker Compose, example config, deployment notes |
| `scripts/` | End-to-end smoke test |

## Status

**Built and tested**

- `crates/proto` — shared model and WireGuard keys.
- `crates/gateway` — port allocation, DNS reconciliation, SQLite storage,
  HTTP API, web UI, nftables, WireGuard peer management, Cloudflare client.
- `crates/agent-core` / `crates/agent-cli` — stateless registration, assignment
  polling, and the TCP/UDP forwarding engine, over kernel WireGuard.
- `deploy/` — Compose files for both halves.

**Not built**

- The userspace tunnel backend. The Windows agent is meant to run `boringtun`
  and `smoltcp` so it needs no TUN driver and no administrator rights; today
  the agent uses kernel WireGuard via `wg-quick`, so it wants `CAP_NET_ADMIN`
  and a Linux host.
- IPv6. Everything is IPv4 end to end.

## Building and testing

Rust 1.82+. If you would rather not install a toolchain:

```bash
docker run --rm -v "$PWD:/w" -w /w rust:1-slim cargo test --workspace
```

The test suite is all offline — no VPS, no Cloudflare account, no root. The
forwarding tests bind real sockets on loopback and push bytes through them.

`scripts/smoke.sh` goes further and exercises the real binaries end to end: it
starts a gateway, adds a node, runs an agent with nothing but a URL and a key,
publishes two services pointing at two different addresses, and pushes bytes
through both. It needs root for the loopback aliases standing in for the tunnel
and the LAN, and skips the parts that require a public IP — DNS writes and
DNAT.
