#!/usr/bin/env bash
# End-to-end smoke test against real binaries, on one machine.
#
# Runs a gateway with Cloudflare and nftables disabled, enrolls an agent
# against it, publishes a service, and pushes bytes through the forwarder to a
# stand-in game server. What it does not cover is the parts that need a public
# IP: DNS writes and DNAT.
#
# Needs root (or CAP_NET_ADMIN) and iproute2, only for the loopback alias that
# stands in for the tunnel address WireGuard would normally provide.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
PORT=18080
TUNNEL_IP=10.99.0.2
GAME_PORT=25599
ADMIN_TOKEN=smoke-test-admin-token
GATEWAY_PID=""
AGENT_PID=""
GAME_PID=""

cleanup() {
  for pid in "$AGENT_PID" "$GATEWAY_PID" "$GAME_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  ip addr del "$TUNNEL_IP/32" dev lo 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

say() { printf '\n=== %s\n' "$1"; }
fail() { printf '!!! %s\n' "$1" >&2; exit 1; }

say "building"
cargo build --quiet --bin portal-gateway --bin portal-agent

# The agent binds listeners on its tunnel address, so that address has to
# exist. On a real agent WireGuard provides it.
ip addr add "$TUNNEL_IP/32" dev lo 2>/dev/null || \
  fail "could not add $TUNNEL_IP to lo (need root?)"

cat > "$WORK/config.toml" <<EOF
[gateway]
public_ip = "203.0.113.10"
zone = "example.test"
listen = "127.0.0.1:$PORT"
data_dir = "$WORK/data"
profiles_dir = "$ROOT/profiles"

[tunnel]
endpoint = "127.0.0.1:51820"
private_key_file = "$WORK/wg.key"

[cloudflare]
zone_id = "smoke"
enabled = false

[nftables]
enabled = false
EOF

say "starting gateway"
PORTAL_CONFIG="$WORK/config.toml" PORTAL_ADMIN_TOKEN="$ADMIN_TOKEN" \
  RUST_LOG=portal_gateway=info "$ROOT/target/debug/portal-gateway" \
  > "$WORK/gateway.log" 2>&1 &
GATEWAY_PID=$!

for _ in $(seq 1 50); do
  curl -sf -H "Authorization: Bearer $ADMIN_TOKEN" \
    "http://127.0.0.1:$PORT/api/status" > /dev/null && break
  sleep 0.2
done
curl -sf -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$PORT/api/status" \
  > /dev/null || { cat "$WORK/gateway.log"; fail "gateway did not come up"; }

say "rejecting a bad admin token"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer wrong" \
  "http://127.0.0.1:$PORT/api/status")
[ "$code" = "401" ] || fail "expected 401 for a bad token, got $code"

say "enrolling an agent"
TOKEN=$(curl -sf -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"label":"smoke"}' \
  "http://127.0.0.1:$PORT/api/agents/tokens" | grep -o '"token":"[^"]*"' | cut -d'"' -f4)
[ -n "$TOKEN" ] || fail "no enrollment token issued"

"$ROOT/target/debug/portal-agent" --state "$WORK/agent.json" \
  enroll --gateway "http://127.0.0.1:$PORT" --token "$TOKEN" --name smoke-box

say "refusing to reuse the enrollment token"
if "$ROOT/target/debug/portal-agent" --state "$WORK/agent2.json" \
     enroll --gateway "http://127.0.0.1:$PORT" --token "$TOKEN" --name dupe 2>/dev/null; then
  fail "a single-use token was accepted twice"
fi

AGENT_ID=$(curl -sf -H "Authorization: Bearer $ADMIN_TOKEN" \
  "http://127.0.0.1:$PORT/api/agents" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)

say "creating a service"
curl -sf -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{\"agent_id\":\"$AGENT_ID\",\"name\":\"Smoke\",\"subdomain\":\"mc\",
       \"profiles\":[\"minecraft-java\"],
       \"local_port_overrides\":{\"minecraft-java/game\":$GAME_PORT}}" \
  "http://127.0.0.1:$PORT/api/services" > "$WORK/service.json"
grep -q '"fqdn":"mc.example.test"' "$WORK/service.json" || \
  { cat "$WORK/service.json"; fail "service was not created as expected"; }

say "rejecting a duplicate subdomain"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' \
  -d "{\"agent_id\":\"$AGENT_ID\",\"name\":\"Dupe\",\"subdomain\":\"mc\",\"profiles\":[\"minecraft-java\"]}" \
  "http://127.0.0.1:$PORT/api/services")
[ "$code" = "409" ] || fail "expected 409 for a duplicate subdomain, got $code"

say "starting a stand-in game server on $GAME_PORT"
# Echoes whatever it is sent, which is all the forwarder needs to prove itself.
python3 - "$GAME_PORT" > "$WORK/game.log" 2>&1 <<'PY' &
import socketserver, sys
class Echo(socketserver.BaseRequestHandler):
    def handle(self):
        while True:
            data = self.request.recv(1024)
            if not data:
                return
            self.request.sendall(data)
socketserver.ThreadingTCPServer.allow_reuse_address = True
socketserver.ThreadingTCPServer(("127.0.0.1", int(sys.argv[1])), Echo).serve_forever()
PY
GAME_PID=$!

for _ in $(seq 1 40); do
  (exec 3<>/dev/tcp/127.0.0.1/"$GAME_PORT") 2>/dev/null && break
  sleep 0.25
done
(exec 3<>/dev/tcp/127.0.0.1/"$GAME_PORT") 2>/dev/null || \
  { cat "$WORK/game.log"; fail "the stand-in game server did not start"; }

say "running the agent"
"$ROOT/target/debug/portal-agent" --state "$WORK/agent.json" run --no-tunnel \
  > "$WORK/agent.log" 2>&1 &
AGENT_PID=$!

for _ in $(seq 1 60); do
  grep -q "assignment applied" "$WORK/agent.log" && break
  sleep 0.5
done
grep -q "assignment applied" "$WORK/agent.log" || \
  { cat "$WORK/agent.log"; fail "the agent never applied its assignment"; }

say "pushing bytes through the tunnel address to the game server"
# 25565 is the edge port the gateway allocated, and the forwarder listens on
# it inside the tunnel; the game server is on $GAME_PORT via the override.
reply=$(python3 - "$TUNNEL_IP" <<'PY'
import socket, sys, time
for _ in range(40):
    try:
        s = socket.create_connection((sys.argv[1], 25565), timeout=2)
        s.sendall(b"hello from a player")
        print(s.recv(64).decode())
        break
    except OSError:
        time.sleep(0.25)
PY
)
[ "$reply" = "hello from a player" ] || \
  { cat "$WORK/agent.log"; fail "no echo through the forwarder (got '$reply')"; }

say "PASS — gateway, enrollment, service creation and forwarding all work"
