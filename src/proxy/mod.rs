//! 代理协议：mixed 端口监听与协议识别，HTTP / HTTPS / SOCKS5 处理。

pub mod http;
pub mod pool;
pub mod relay;
pub mod socks5;
pub mod tls;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::daemon::stats::{ConnMeta, Registered};
use crate::daemon::Ctx;
use crate::net::egress::{self, Egress, TargetAddr};
use crate::proxy::relay::{relay, Counted, Io, Rewind};

pub struct Session {
    pub client: SocketAddr,
    pub entry: SocketAddr,
    pub tls: bool,
    pub cfg: Arc<Config>,
    pub egress: Egress,
}

impl Session {
    pub fn handshake_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.performance.handshake_timeout_secs.max(1))
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.egress.connect_timeout_secs.max(1))
    }

    pub async fn open_upstream(&self, ctx: &Ctx, target: &TargetAddr) -> io::Result<TcpStream> {
        let res = egress::connect_tcp(
            target,
            &self.egress,
            &ctx.resolver(),
            self.connect_timeout(),
        )
        .await;
        if let Err(e) = &res {
            tracing::info!(
                "connect failed {} -> {} via {}: {e}",
                self.client,
                target,
                self.egress.label()
            );
        }
        res
    }

    pub fn register(
        &self,
        ctx: &Ctx,
        proto: &str,
        target: &TargetAddr,
        remote: &TcpStream,
    ) -> Registered {
        ctx.stats.register(ConnMeta {
            proto: proto.to_string(),
            client: self.client,
            entry: self.entry,
            target: target.to_string(),
            iface: self.egress.label().to_string(),
            local: remote.local_addr().ok(),
            remote: remote.peer_addr().ok(),
            udp: false,
        })
    }

    pub async fn splice<S: Io>(&self, client: S, mut remote: TcpStream, reg: Registered) {
        let mut client = Counted::new(client, reg.conn.clone(), reg.iface.clone());
        relay(&mut client, &mut remote, self.cfg.performance.buffer_size).await;
        drop(reg);
    }
}

/// One listening socket and its accept loop.
pub struct Listener {
    /// Configured address (port 0 resolves to `bound`).
    pub addr: SocketAddr,
    pub bound: SocketAddr,
    task: JoinHandle<()>,
}

impl Listener {
    /// Starts accepting on an already bound socket.
    pub fn start(ctx: &Arc<Ctx>, addr: SocketAddr, socket: TcpListener) -> Listener {
        Listener {
            addr,
            bound: socket.local_addr().unwrap_or(addr),
            task: tokio::spawn(accept_loop(socket, ctx.clone())),
        }
    }

    /// Stops accepting and waits until the socket is closed, so the address can
    /// be bound again right away.
    pub async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// The proxy's listening sockets.
#[derive(Default)]
pub struct Listeners {
    pub list: Vec<Listener>,
    /// Addresses that could not be bound, with the reason.
    pub errors: Vec<String>,
}

impl Listeners {
    pub fn bound(&self) -> Vec<SocketAddr> {
        self.list.iter().map(|l| l.bound).collect()
    }

    pub fn has(&self, addr: &SocketAddr) -> bool {
        self.list.iter().any(|l| l.addr == *addr)
    }

    /// Removes and returns the listeners whose address is not in `keep`.
    pub fn take_except(&mut self, keep: &[SocketAddr]) -> Vec<Listener> {
        let (stay, gone) = std::mem::take(&mut self.list)
            .into_iter()
            .partition(|l| keep.contains(&l.addr));
        self.list = stay;
        gone
    }

    pub async fn close_all(&mut self) {
        for listener in std::mem::take(&mut self.list) {
            listener.close().await;
        }
    }
}

impl Drop for Listeners {
    fn drop(&mut self) {
        for listener in &self.list {
            listener.task.abort();
        }
    }
}

pub fn bind_socket(addr: SocketAddr) -> io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    if addr.is_ipv6() {
        // Separate v4/v6 listeners keep getsockname() exact for egress selection.
        sock.set_only_v6(true)?;
    }
    sock.set_nodelay(true)?;
    sock.bind(&addr.into())?;
    sock.listen(4096)?;
    sock.set_nonblocking(true)?;
    TcpListener::from_std(sock.into())
}

/// Binds and starts every address; failures are recorded, not fatal.
pub fn bind_listeners(ctx: &Arc<Ctx>, addrs: &[SocketAddr]) -> Listeners {
    let mut out = Listeners::default();
    for addr in addrs {
        match bind_socket(*addr) {
            Ok(socket) => out.list.push(Listener::start(ctx, *addr, socket)),
            Err(e) => out.errors.push(format!("{addr}: {e}")),
        }
    }
    out
}

async fn accept_loop(listener: TcpListener, ctx: Arc<Ctx>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, peer, ctx).await {
                        tracing::debug!(%peer, "connection error: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn handle_conn(stream: TcpStream, peer: SocketAddr, ctx: Arc<Ctx>) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let entry = stream.local_addr()?;
    let cfg = ctx.config();
    let egress = egress::select(entry.ip(), &ctx.net.snapshot(), &cfg.egress);
    let mut sess = Session {
        client: peer,
        entry,
        tls: false,
        cfg,
        egress,
    };
    let hs = sess.handshake_timeout();
    let mut first = [0u8; 1];
    match tokio::time::timeout(hs, stream.peek(&mut first)).await {
        Ok(Ok(1)) => {}
        _ => return Ok(()),
    }
    if first[0] == 0x16 {
        if !sess.cfg.protocols.https {
            return Ok(());
        }
        let Some(acceptor) = ctx.tls_acceptor() else {
            return Ok(());
        };
        let mut tls = match tokio::time::timeout(hs, acceptor.accept(stream)).await {
            Ok(Ok(tls)) => tls,
            _ => return Ok(()),
        };
        let mut inner = [0u8; 1];
        match tokio::time::timeout(hs, tls.read_exact(&mut inner)).await {
            Ok(Ok(_)) => {}
            _ => return Ok(()),
        }
        sess.tls = true;
        dispatch(Rewind::new(inner.to_vec(), tls), inner[0], sess, ctx).await
    } else {
        dispatch(stream, first[0], sess, ctx).await
    }
}

async fn dispatch<S: Io>(stream: S, first: u8, sess: Session, ctx: Arc<Ctx>) -> io::Result<()> {
    let protocols = sess.cfg.protocols;
    match first {
        0x05 if protocols.socks5 => socks5::handle(stream, sess, ctx).await,
        // Inside TLS the HTTPS switch already applied; plain HTTP needs its own.
        b'A'..=b'Z' | b'a'..=b'z' if sess.tls || protocols.http => {
            http::handle(stream, sess, ctx).await
        }
        _ => Ok(()),
    }
}
