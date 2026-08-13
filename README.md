<p align="center">
  <img src="assets/banner.svg" alt="Portal" width="820">
</p>

<p align="center">
  <a href="https://github.com/scopeddlol/portal/actions/workflows/release.yml"><img alt="Build" src="https://img.shields.io/github/actions/workflow/status/scopeddlol/portal/release.yml?style=for-the-badge&label=build&labelColor=0B1020&color=22D3EE"></a>
  <img alt="Status" src="https://img.shields.io/badge/status-beta%20v0.1-818CF8?style=for-the-badge&labelColor=0B1020">
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-MIT-C084FC?style=for-the-badge&labelColor=0B1020"></a>
  <img alt="Rust" src="https://img.shields.io/badge/built%20with-Rust-CE422B?style=for-the-badge&logo=rust&logoColor=white&labelColor=0B1020">
  <img alt="Docker" src="https://img.shields.io/badge/runs%20with-Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white&labelColor=0B1020">
  <img alt="Linux" src="https://img.shields.io/badge/home%20PC-Linux-FCC624?style=for-the-badge&logo=linux&logoColor=white&labelColor=0B1020">
</p>

<p align="center">
  <b>Let friends join game servers on your home PC — without touching your router or sharing your home address.</b>
</p>

---

## What is this?

You want to run a Minecraft server on your own PC and have friends join. Normally that means two uncomfortable things: opening a port on your home router, and handing out your home IP address — which quietly tells people roughly where you live.

Portal removes both. You rent a small server somewhere (about **$5 a month**), and Portal joins your home PC to it through a private, encrypted tunnel. Friends connect to a normal address like `mc.yourdomain.com`, which points at the **rented** server. Your home address is never published, and nothing on your router changes.

<p align="center">
  <img src="assets/how-it-works.svg" alt="Friends connect to your domain, which points at a small rented server, which passes traffic down an encrypted tunnel to your PC at home" width="820">
</p>

Portal also handles the fiddly parts for you: it picks the port numbers, writes the DNS records so your address just works, and knows that a Minecraft server with voice chat needs two ports rather than one.

## What you need

| | |
| --- | --- |
| 🌐 **A domain name** | Managed by Cloudflare. Roughly $10 a year. |
| 🖥️ **A small rented server** | Any cheap Linux VPS with a public IP. About $5 a month. |
| 🏠 **A home PC** | Running Linux or Docker, where your game servers live. |

## Setup

### 1. Set up the rented server

```bash
git clone https://github.com/scopeddlol/portal.git
cd portal/deploy
cp config.example.toml config.toml
nano config.toml     # fill in your domain and the server's IP address
```

Add your two secrets, then start it:

```bash
cat > .env <<'EOF'
PORTAL_CF_API_TOKEN=<your Cloudflare token>
PORTAL_ADMIN_TOKEN=<any long random password you make up>
EOF
chmod 600 .env

docker compose up -d --build
```

The Cloudflare token comes from your Cloudflare dashboard and needs one permission: **Zone → DNS → Edit**.

### 2. Connect your home PC

Open the web page (see [the setup notes](deploy/README.md) for how to reach it safely), sign in with your admin password, and click **Create enrollment token**. Then on the PC with your game servers:

```bash
cd portal/deploy
docker compose -f agent-compose.yml run --rm agent \
  enroll --gateway https://portal.yourdomain.com --token <token> --name my-pc
docker compose -f agent-compose.yml up -d
```

### 3. Add your game server

Back on the web page: pick your PC, give the server a name, choose an address like `mc`, tick the games it runs, and press **Create**.

That's it. Your friends can now connect to `mc.yourdomain.com`.

## Games it knows about

| Game | Notes |
| --- | --- |
| 🟩 **Minecraft (Java)** | Friends type just `mc.yourdomain.com`, no port number needed |
| 🟦 **Minecraft (Bedrock)** | Consoles and phones can join too |
| 🎙️ **Simple Voice Chat** | Add-on for Java — tick it alongside Minecraft |
| 🧪 **Hytale** | Placeholder — the port numbers are guesses until the game exists |

Adding another game is one small text file, not a code change. See [`profiles/`](profiles/).

## Good to know

This is a **beta**. It works, but a few things are worth knowing before you rely on it:

- **Your home PC needs Linux or Docker.** A native Windows version is planned but not built yet.
- **Your game server sees one address for everyone.** Player IP addresses arrive looking identical, so IP bans and IP-based plugins won't behave as you'd expect.
- **IPv4 only** for now.
- **You still set your own game config.** If you use voice chat, the web page tells you exactly which line to change and where — Portal won't edit your server files behind your back.

## Learn more

- 📘 [**Full setup and troubleshooting**](deploy/README.md) — safer ways to reach the web page, running without Docker, checking it works
- 🔧 [**How it works inside**](docs/how-it-works.md) — the architecture, why Cloudflare can't carry game traffic, and the design decisions
- 🧩 [**Game profiles**](profiles/) — add support for another game

## Licence

MIT. See [LICENSE](LICENSE).
