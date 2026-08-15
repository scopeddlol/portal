# How it works

Architecture and the reasoning behind it. The [README](../README.md) covers
what Portal does; this covers why it is built this way.

## Cloudflare's role

The Cloudflare proxy handles HTTP/HTTPS on a fixed set of ports. It will not
carry TCP 25565 or UDP 24454. Cloudflare **Spectrum** does proxy raw TCP/UDP —
with a Minecraft-specific offering on Pro/Business plans — but generic TCP/UDP
applications, which is what a voice-chat port is, sit behind an Enterprise
add-on. Spectrum is also not a tunnel: it still requires a reachable origin.

So Cloudflare is a **DNS control plane, not a data plane**. The API token writes
DNS-only (grey-cloud) `A` records plus `SRV` records. The rented server is what
players connect to, and the private network's address is never published.

## Shape

```
   private network                     VPS (public IP)                players
┌────────────────────┐          ┌──────────────────────────┐
│ servers            │          │ nftables DNAT            │
│  192.168.1.50      │          │  :25565/tcp ─┐           │  ──▶ mc.example.com
│  192.168.1.51      │          │  :24454/udp ─┤           │
│  192.168.1.52      │          │              ▼           │
│                    │          │ wg0  10.99.0.1           │
│ agent  10.99.0.2   │◀════════▶│ (kernel WireGuard)       │
│  one container     │ WireGuard│                          │
│                    │  UDP/51820│ gateway (Rust)          │
└────────────────────┘          │  web UI, API,            │
                                │  port allocator,         │
                                │  Cloudflare reconciler   │
                                └──────────────────────────┘
```

The gateway process never touches a packet of game traffic. Publishing a port is
an nftables DNAT rule into the tunnel subnet; the kernel does the rest. Latency
overhead is the WireGuard encryption and nothing else, and restarting the
gateway does not drop established connections.

The agent's tunnel sits behind a `TunnelBackend` trait, and the forwarding
engine asks it only for an address to bind on. The implemented backend is kernel
WireGuard, which is close to free on Linux and in Docker: the agent accepts on
its tunnel address and opens an ordinary socket to whichever address on its
network the mapping names.

## The model

Three types:

```
Node (a machine running the agent)
 └── Service (one subdomain)
      └── PortMapping (public port -> address:port reachable from the node)
```

A mapping carries its own `local_host`. That single field is what allows one
container to front an entire network — the agent bridges to any address it can
reach, so ten servers on ten machines need one agent.

```
mc.example.com  →  A 203.0.113.10                    (DNS-only)
  ├─ tcp 25565  →  _minecraft._tcp SRV  →  192.168.1.50:25565
  └─ udp 24454  →                          192.168.1.50:24454
```

Public ports are allocated and need not equal the local port, because many
servers can sensibly listen on 25565 on their own machines while only one can
hold 25565 on the public IP. The `SRV` record makes that invisible to clients
that follow it: Java clients resolve it and connect to the bare hostname
whatever public port the service received. Clients that ignore SRV see the
allocated port, which the panel displays verbatim.

No component holds game-specific configuration. A port is a port.

## Design decisions

- A mapping is given its local port number on the public side when nothing else
  holds it, so a single server looks exactly like one behind a forwarded port.
  Subsequent ones fall back to `30000-32767` — below Linux's ephemeral range, so
  an allocated port cannot collide with the source port of an outbound
  connection the server makes itself.
- DNS reconciliation touches only a service's own name and the names beneath it.
  Records elsewhere in the zone — `MX` for mail, the apex `A` — are invisible to
  it, so tidying up a service cannot delete them.
- The nftables ruleset is replaced wholesale through a single `nft -f`
  transaction. Rules cannot drift from the database, and a crash part-way
  through leaves the previous ruleset serving players.
- Return traffic is masqueraded into the tunnel, so servers observe the tunnel
  address rather than player addresses.
- Tokens are stored as SHA-256 hashes; a copy of `portal.db` yields no usable
  credentials. Each node's WireGuard peer is allowed exactly its own `/32`, so a
  compromised machine cannot source traffic as another node.
- Agents hold no state. A fresh keypair is generated on each start and
  registered, while the node's tunnel address is fixed at creation — which is
  why an agent needs no volume and no enrollment step, and why restarting one
  does not disturb the forwards aimed at it.
- Assignments are sent in full rather than as deltas, and applied declaratively.
  A missed update heals on the next poll instead of leaving an agent
  permanently out of step.

## Layout

| Path | Contents |
| --- | --- |
| `crates/proto` | Shared model and wire types |
| `crates/gateway` | VPS side: API, web UI, WireGuard peers, nftables, Cloudflare |
| `crates/agent-core` | Tunnel client and forwarding engine |
| `crates/agent-cli` | Agent binary |
| `deploy/` | Compose files, example configuration, deployment notes |
| `scripts/` | End-to-end smoke test |

## Scope

The agent depends on kernel WireGuard, so it runs on Linux with
`CAP_NET_ADMIN`. Addressing is IPv4 throughout. Servers are observed by the
tunnel address rather than by player address, a consequence of masquerading
return traffic.

## Building and testing

Rust 1.82+. Without a local toolchain:

```bash
docker run --rm -v "$PWD:/w" -w /w rust:1-slim cargo test --workspace
```

The test suite runs entirely offline — no VPS, no Cloudflare account, no root.
The forwarding tests bind real sockets on loopback and push bytes through them.

`scripts/smoke.sh` exercises the compiled binaries end to end: it starts a
gateway, adds a node, runs an agent with nothing but a URL and a key, publishes
two services pointing at two different addresses, and pushes bytes through
both. It requires root for the loopback aliases standing in for the tunnel and
the LAN, and skips what needs a public IP — DNS writes and DNAT.
