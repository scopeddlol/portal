#!/usr/bin/env bash
# End-to-end smoke test against real binaries, on one machine.
#
# Walks the whole flow: start a gateway, add a node, start an agent with only
# a URL and a key, add two services pointing at two different addresses on the
# "LAN", and push bytes through both. What it does not cover is the parts that
# need a public IP: DNS writes and DNAT.
#
# Needs root (or CAP_NET_ADMIN) and iproute2, only for the loopback aliases
# standing in for the tunnel address and two LAN machines.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
PORT=18080
TUNNEL_IP=10.99.0.2
LAN_A=10.77.0.11
LAN_B=10.77.0.12
ADMIN_TOKEN=smoke-test-admin-token
GATEWAY_PID=""; AGENT_PID=""; GAME_A_PID=""; GAME_B_PID=""

cleanup() {
  for pid in "$AGENT_PID" "$GATEWAY_PID" "$GAME_A_PID" "$GAME_B_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  for ip in "$TUNNEL_IP" "$LAN_A" "$LAN_B"; do
    ip addr del "$ip/32" dev lo 2>/dev/null || true
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

say() { printf '\n=== %s\n' "$1"; }
fail() { printf '!!! %s\n' "$1" >&2; exit 1; }
api() { curl -sf -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' "$@"; }
jget() { python3 -c "import sys,json;print(json.load(sys.stdin)$1)"; }

say "building"
cargo build --quiet --bin portal-gateway --bin portal-agent

# The agent binds on its tunnel address; the two "LAN machines" stand in for
# other boxes on the home network. WireGuard and a real LAN provide these.
for ip in "$TUNNEL_IP" "$LAN_A" "$LAN_B"; do
  ip addr add "$ip/32" dev lo 2>/dev/null || fail "could not add $ip to lo (need root?)"
done

# A stand-in game server on each "machine", echoing whatever it is sent.
start_echo() {
  python3 - "$1" > /dev/null 2>&1 <<'PY' &
import socketserver, sys
class Echo(socketserver.BaseRequestHandler):
    def handle(self):
        while True:
            data = self.request.recv(1024)
            if not data:
                return
            self.request.sendall(data)
socketserver.ThreadingTCPServer.allow_reuse_address = True
socketserver.ThreadingTCPServer((sys.argv[1], 25565), Echo).serve_forever()
PY
  echo $!
}

cat > "$WORK/config.toml" <<EOF
[gateway]
public_ip = "203.0.113.10"
zone = "example.test"
listen = "127.0.0.1:$PORT"
data_dir = "$WORK/data"

[tunnel]
endpoint = "127.0.0.1:51820"

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
  api "http://127.0.0.1:$PORT/api/status" > /dev/null 2>&1 && break
  sleep 0.2
done
api "http://127.0.0.1:$PORT/api/status" > /dev/null \
  || { cat "$WORK/gateway.log"; fail "gateway did not come up"; }

say "rejecting a bad admin token"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer wrong" \
  "http://127.0.0.1:$PORT/api/status")
[ "$code" = "401" ] || fail "expected 401 for a bad token, got $code"

say "adding a node"
NODE=$(api -X POST -d '{"name":"basement-pc"}' "http://127.0.0.1:$PORT/api/nodes")
NODE_KEY=$(echo "$NODE" | jget "['key']")
[ -n "$NODE_KEY" ] || fail "no node key issued"

say "starting the agent with only a URL and a key"
PORTAL_URL="http://127.0.0.1:$PORT" PORTAL_KEY="$NODE_KEY" PORTAL_NO_TUNNEL=1 \
  RUST_LOG=portal_agent_core=info "$ROOT/target/debug/portal-agent" \
  > "$WORK/agent.log" 2>&1 &
AGENT_PID=$!
for _ in $(seq 1 40); do
  grep -q "tunnel up" "$WORK/agent.log" && break
  sleep 0.25
done
grep -q "tunnel up" "$WORK/agent.log" \
  || { cat "$WORK/agent.log"; fail "the agent never registered"; }

say "starting two game servers on two addresses"
GAME_A_PID=$(start_echo "$LAN_A")
GAME_B_PID=$(start_echo "$LAN_B")
for ip in "$LAN_A" "$LAN_B"; do
  for _ in $(seq 1 40); do
    (exec 3<>/dev/tcp/"$ip"/25565) 2>/dev/null && break
    sleep 0.25
  done
  (exec 3<>/dev/tcp/"$ip"/25565) 2>/dev/null || fail "stand-in server on $ip did not start"
done

say "publishing both through one node"
NODE_ID=$(echo "$NODE" | jget "['id']")
for pair in "mc1:$LAN_A" "mc2:$LAN_B"; do
  sub="${pair%%:*}"; host="${pair##*:}"
  SVC=$(api -X POST -d "{\"node_id\":\"$NODE_ID\",\"name\":\"$sub\",\"subdomain\":\"$sub\"}" \
    "http://127.0.0.1:$PORT/api/services")
  SID=$(echo "$SVC" | jget "['id']")
  api -X POST -d "{\"protocol\":\"tcp\",\"local_host\":\"$host\",\"local_port\":25565,\"minecraft_srv\":true}" \
    "http://127.0.0.1:$PORT/api/services/$SID/ports" > /dev/null
done

api "http://127.0.0.1:$PORT/api/services" | python3 -c "
import sys, json
for s in json.load(sys.stdin):
    for p in s['ports']:
        print(f\"    {p['connect']:22} -> {p['local_host']}:{p['local_port']}\")"

say "rejecting a duplicate subdomain"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' \
  -d "{\"node_id\":\"$NODE_ID\",\"name\":\"dupe\",\"subdomain\":\"mc1\"}" \
  "http://127.0.0.1:$PORT/api/services")
[ "$code" = "409" ] || fail "expected 409 for a duplicate subdomain, got $code"

say "pushing bytes to both servers through the one agent"
# Wait for the agent's next poll to pick the new forwards up.
for _ in $(seq 1 60); do
  grep -q "serving 2 port" "$WORK/agent.log" && break
  sleep 0.5
done
grep -q "serving 2 port" "$WORK/agent.log" \
  || { cat "$WORK/agent.log"; fail "the agent never picked up both forwards"; }

EDGE_PORTS=$(api "http://127.0.0.1:$PORT/api/services" | python3 -c "
import sys, json
print(' '.join(str(p['edge_port']) for s in json.load(sys.stdin) for p in s['ports']))")

i=0
for port in $EDGE_PORTS; do
  i=$((i + 1))
  reply=$(python3 - "$TUNNEL_IP" "$port" "hello-from-$i" <<'PY'
import socket, sys, time
host, port, msg = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode()
for _ in range(40):
    try:
        s = socket.create_connection((host, port), timeout=2)
        s.sendall(msg)
        print(s.recv(64).decode())
        break
    except OSError:
        time.sleep(0.25)
PY
)
  [ "$reply" = "hello-from-$i" ] \
    || { cat "$WORK/agent.log"; fail "no echo through edge port $port (got '$reply')"; }
  printf '    edge %s reached its server\n' "$port"
done

say "PASS — one node, two machines on its network, both reachable"
