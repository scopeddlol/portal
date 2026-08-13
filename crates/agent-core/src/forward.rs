//! The forwarding engine.
//!
//! For each assigned port the agent accepts on its tunnel address and bridges
//! to the game server on the local machine. That is the whole job: decrypted
//! traffic arrives on one socket and leaves on another, both ordinary sockets,
//! which is what lets the Windows build work without a TUN driver or
//! administrator rights.
//!
//! Listeners are keyed by protocol and port so an assignment change only
//! disturbs what actually changed. Adding a second server does not interrupt
//! the game already in progress on the first.

use portal_proto::api::Forward;
use portal_proto::model::Protocol;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

/// How long a UDP flow with no traffic is kept in the session table.
///
/// UDP has no close, so the only way to release a session is to time it out.
/// Minecraft voice sends continuously while connected, and a player who stops
/// sending for a full minute has left; a longer timeout only wastes sockets.
const UDP_SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// Buffer for a single datagram. Larger than any sane game packet, and far
/// under the 64 KiB a UDP datagram can theoretically be.
const UDP_BUFFER: usize = 8 * 1024;

/// Runs the listeners for the current assignment.
#[derive(Default)]
pub struct Forwarder {
    running: HashMap<(Protocol, u16), Listener>,
}

struct Listener {
    forward: Forward,
    task: Option<JoinHandle<()>>,
}

impl Listener {
    /// Stop accepting and wait for the socket to actually be released.
    ///
    /// `abort` only schedules cancellation, so rebinding the same port right
    /// after it returns fails with "address already in use". Awaiting the
    /// handle is what makes a port move deterministic rather than a race.
    ///
    /// Connections already established are left alone: they are players
    /// mid-game, the gateway has already stopped sending new traffic this way,
    /// and they end when the player does.
    async fn stop(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Forwarder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ports currently being served, for logging and tests.
    pub fn active(&self) -> Vec<(Protocol, u16)> {
        let mut keys: Vec<_> = self.running.keys().copied().collect();
        keys.sort_by_key(|(protocol, port)| (protocol.as_str(), *port));
        keys
    }

    /// Make the running listeners match `forwards`.
    ///
    /// Applied declaratively rather than as a diff sent by the gateway, so a
    /// missed update heals on the next poll instead of leaving the agent
    /// permanently out of step.
    pub async fn apply(&mut self, bind_ip: IpAddr, forwards: &[Forward]) -> io::Result<()> {
        let wanted: HashMap<(Protocol, u16), Forward> = forwards
            .iter()
            .map(|f| ((f.protocol, f.tunnel_port), f.clone()))
            .collect();

        // Stop listeners that are gone or whose target moved, and wait for
        // each one before binding anything: a port that moved must be free by
        // the time it is claimed again.
        let stale: Vec<(Protocol, u16)> = self
            .running
            .iter()
            .filter(|(key, listener)| {
                !wanted
                    .get(*key)
                    .is_some_and(|w| same_target(w, &listener.forward))
            })
            .map(|(key, _)| *key)
            .collect();
        for key in stale {
            if let Some(listener) = self.running.remove(&key) {
                tracing::info!(protocol = %key.0, tunnel_port = key.1, "no longer serving");
                listener.stop().await;
            }
        }

        for (key, forward) in wanted {
            if self.running.contains_key(&key) {
                continue;
            }
            let addr = SocketAddr::new(bind_ip, forward.tunnel_port);
            let task = match forward.protocol {
                Protocol::Tcp => spawn_tcp(addr, forward.clone()).await?,
                Protocol::Udp => spawn_udp(addr, forward.clone()).await?,
            };
            tracing::info!(
                protocol = %forward.protocol,
                tunnel_port = forward.tunnel_port,
                local = %format!("{}:{}", forward.local_host, forward.local_port),
                "listening"
            );
            self.running.insert(
                key,
                Listener {
                    forward,
                    task: Some(task),
                },
            );
        }
        Ok(())
    }

    /// Stop everything, without waiting. Used on the way out of the process,
    /// where there is nothing left to rebind and nothing to race with.
    pub fn shutdown(&mut self) {
        self.running.clear();
    }
}

fn same_target(a: &Forward, b: &Forward) -> bool {
    a.local_host == b.local_host && a.local_port == b.local_port
}

fn local_target(forward: &Forward) -> String {
    format!("{}:{}", forward.local_host, forward.local_port)
}

async fn spawn_tcp(addr: SocketAddr, forward: Forward) -> io::Result<JoinHandle<()>> {
    // Bound here rather than inside the task so a port conflict is reported to
    // the caller instead of disappearing into a background failure.
    let listener = TcpListener::bind(addr).await?;
    Ok(tokio::spawn(async move {
        loop {
            let (mut inbound, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            };
            let target = local_target(&forward);
            tokio::spawn(async move {
                match TcpStream::connect(&target).await {
                    Ok(mut outbound) => {
                        // Errors here are ordinary: players disconnect, and
                        // servers restart under them.
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                    Err(e) => tracing::warn!(
                        error = %e, %peer, %target,
                        "game server refused the connection"
                    ),
                }
            });
        }
    }))
}

async fn spawn_udp(addr: SocketAddr, forward: Forward) -> io::Result<JoinHandle<()>> {
    let socket = Arc::new(UdpSocket::bind(addr).await?);
    Ok(tokio::spawn(async move {
        // UDP has no connections, so the agent keeps its own session table:
        // one socket per remote address, reused for that player's datagrams
        // and reaped when they go quiet. Without this, replies from the game
        // server would have nowhere to be sent back to.
        let mut sessions: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
        let mut last_seen: HashMap<SocketAddr, tokio::time::Instant> = HashMap::new();
        let mut buf = vec![0u8; UDP_BUFFER];

        loop {
            let (len, peer) = match socket.recv_from(&mut buf).await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "udp receive failed");
                    continue;
                }
            };

            let now = tokio::time::Instant::now();
            sessions.retain(|peer, _| {
                last_seen
                    .get(peer)
                    .is_some_and(|seen| now.duration_since(*seen) < UDP_SESSION_TIMEOUT)
            });
            last_seen.retain(|_, seen| now.duration_since(*seen) < UDP_SESSION_TIMEOUT);

            let session = match sessions.get(&peer) {
                Some(existing) => existing.clone(),
                None => {
                    let target = local_target(&forward);
                    match new_udp_session(&socket, peer, &target).await {
                        Ok(session) => {
                            sessions.insert(peer, session.clone());
                            session
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, %peer, "could not open a udp session");
                            continue;
                        }
                    }
                }
            };
            last_seen.insert(peer, now);

            if let Err(e) = session.send(&buf[..len]).await {
                tracing::warn!(error = %e, %peer, "forwarding a datagram failed");
                sessions.remove(&peer);
            }
        }
    }))
}

/// Open a socket to the game server for one remote peer, and pump replies back.
async fn new_udp_session(
    inbound: &Arc<UdpSocket>,
    peer: SocketAddr,
    target: &str,
) -> io::Result<Arc<UdpSocket>> {
    // Port 0: let the OS pick. Binding to the unspecified address keeps this
    // working whether the game server is on loopback or elsewhere on the LAN.
    let session = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    session.connect(target).await?;

    let reply_socket = inbound.clone();
    let session_reader = session.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; UDP_BUFFER];
        // Ends when the session socket closes or the player is unreachable,
        // both of which mean this flow is over.
        while let Ok(len) = session_reader.recv(&mut buf).await {
            if reply_socket.send_to(&buf[..len], peer).await.is_err() {
                break;
            }
        }
    });
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    /// A stand-in game server that echoes what it is sent.
    async fn tcp_echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = socket.read(&mut buf).await {
                        if n == 0 || socket.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }

    async fn udp_echo_server() -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok((len, peer)) = socket.recv_from(&mut buf).await {
                let _ = socket.send_to(&buf[..len], peer).await;
            }
        });
        port
    }

    fn forward(protocol: Protocol, tunnel_port: u16, local_port: u16) -> Forward {
        Forward {
            protocol,
            tunnel_port,
            local_host: "127.0.0.1".into(),
            local_port,
        }
    }

    /// Ask the OS for a free port, then let go of it.
    async fn free_port(protocol: Protocol) -> u16 {
        match protocol {
            Protocol::Tcp => TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap()
                .local_addr()
                .unwrap()
                .port(),
            Protocol::Udp => UdpSocket::bind("127.0.0.1:0")
                .await
                .unwrap()
                .local_addr()
                .unwrap()
                .port(),
        }
    }

    #[tokio::test]
    async fn tcp_traffic_reaches_the_game_server_and_comes_back() {
        let game = tcp_echo_server().await;
        let edge = free_port(Protocol::Tcp).await;
        let mut forwarder = Forwarder::new();
        forwarder
            .apply(LOCALHOST, &[forward(Protocol::Tcp, edge, game)])
            .await
            .unwrap();

        let mut client = TcpStream::connect(("127.0.0.1", edge)).await.unwrap();
        client.write_all(b"hello minecraft").await.unwrap();
        let mut buf = [0u8; 15];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello minecraft");
    }

    #[tokio::test]
    async fn udp_datagrams_round_trip_to_the_right_player() {
        let game = udp_echo_server().await;
        let edge = free_port(Protocol::Udp).await;
        let mut forwarder = Forwarder::new();
        forwarder
            .apply(LOCALHOST, &[forward(Protocol::Udp, edge, game)])
            .await
            .unwrap();

        let player = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        player.connect(("127.0.0.1", edge)).await.unwrap();
        player.send(b"voice packet").await.unwrap();

        let mut buf = [0u8; 64];
        let len = tokio::time::timeout(Duration::from_secs(5), player.recv(&mut buf))
            .await
            .expect("a reply should come back")
            .unwrap();
        assert_eq!(&buf[..len], b"voice packet");
    }

    #[tokio::test]
    async fn two_players_on_one_udp_port_do_not_get_each_others_replies() {
        let game = udp_echo_server().await;
        let edge = free_port(Protocol::Udp).await;
        let mut forwarder = Forwarder::new();
        forwarder
            .apply(LOCALHOST, &[forward(Protocol::Udp, edge, game)])
            .await
            .unwrap();

        let alice = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bob = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        alice.connect(("127.0.0.1", edge)).await.unwrap();
        bob.connect(("127.0.0.1", edge)).await.unwrap();

        alice.send(b"from-alice").await.unwrap();
        bob.send(b"from-bob").await.unwrap();

        let mut buf = [0u8; 64];
        let len = tokio::time::timeout(Duration::from_secs(5), alice.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..len], b"from-alice");

        let len = tokio::time::timeout(Duration::from_secs(5), bob.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..len], b"from-bob");
    }

    #[tokio::test]
    async fn a_new_assignment_only_disturbs_what_changed() {
        let game = tcp_echo_server().await;
        let kept = free_port(Protocol::Tcp).await;
        let removed = free_port(Protocol::Tcp).await;
        let added = free_port(Protocol::Tcp).await;

        let mut forwarder = Forwarder::new();
        forwarder
            .apply(
                LOCALHOST,
                &[
                    forward(Protocol::Tcp, kept, game),
                    forward(Protocol::Tcp, removed, game),
                ],
            )
            .await
            .unwrap();

        // A connection through the port that survives must stay usable across
        // the reconfiguration — this is a player mid-game.
        let mut player = TcpStream::connect(("127.0.0.1", kept)).await.unwrap();

        forwarder
            .apply(
                LOCALHOST,
                &[
                    forward(Protocol::Tcp, kept, game),
                    forward(Protocol::Tcp, added, game),
                ],
            )
            .await
            .unwrap();

        assert_eq!(
            forwarder.active(),
            vec![
                (Protocol::Tcp, kept.min(added)),
                (Protocol::Tcp, kept.max(added))
            ]
        );

        player.write_all(b"still here").await.unwrap();
        let mut buf = [0u8; 10];
        player.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"still here");
    }

    #[tokio::test]
    async fn a_removed_port_stops_accepting() {
        let game = tcp_echo_server().await;
        let edge = free_port(Protocol::Tcp).await;
        let mut forwarder = Forwarder::new();
        forwarder
            .apply(LOCALHOST, &[forward(Protocol::Tcp, edge, game)])
            .await
            .unwrap();
        assert!(TcpStream::connect(("127.0.0.1", edge)).await.is_ok());

        forwarder.apply(LOCALHOST, &[]).await.unwrap();
        assert!(forwarder.active().is_empty());
        assert!(
            TcpStream::connect(("127.0.0.1", edge)).await.is_err(),
            "a deleted service must stop answering"
        );
    }

    #[tokio::test]
    async fn a_moved_local_port_rebinds_the_listener() {
        let first = tcp_echo_server().await;
        let second = tcp_echo_server().await;
        let edge = free_port(Protocol::Tcp).await;

        let mut forwarder = Forwarder::new();
        forwarder
            .apply(LOCALHOST, &[forward(Protocol::Tcp, edge, first)])
            .await
            .unwrap();
        forwarder
            .apply(LOCALHOST, &[forward(Protocol::Tcp, edge, second)])
            .await
            .unwrap();

        assert_eq!(forwarder.active(), vec![(Protocol::Tcp, edge)]);
        let mut client = TcpStream::connect(("127.0.0.1", edge)).await.unwrap();
        client.write_all(b"moved").await.unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"moved");
    }

    #[tokio::test]
    async fn applying_the_same_assignment_twice_changes_nothing() {
        let game = tcp_echo_server().await;
        let edge = free_port(Protocol::Tcp).await;
        let assignment = [forward(Protocol::Tcp, edge, game)];

        let mut forwarder = Forwarder::new();
        forwarder.apply(LOCALHOST, &assignment).await.unwrap();
        forwarder
            .apply(LOCALHOST, &assignment)
            .await
            .expect("a rebind would fail here with address-in-use");
        assert_eq!(forwarder.active().len(), 1);
    }

    #[tokio::test]
    async fn tcp_and_udp_can_share_a_port_number() {
        let tcp_game = tcp_echo_server().await;
        let udp_game = udp_echo_server().await;
        let port = free_port(Protocol::Tcp).await;

        let mut forwarder = Forwarder::new();
        forwarder
            .apply(
                LOCALHOST,
                &[
                    forward(Protocol::Tcp, port, tcp_game),
                    forward(Protocol::Udp, port, udp_game),
                ],
            )
            .await
            .expect("the two protocols are independent");
        assert_eq!(forwarder.active().len(), 2);
    }
}
