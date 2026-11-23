use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::task::JoinSet;

use super::Session;
use crate::config::AuthConfig;
use crate::daemon::stats::{ConnMeta, IfaceCounters, Registered, Traffic};
use crate::daemon::Ctx;
use crate::net::egress::{self, Fam, TargetAddr};
use crate::proxy::relay::Io;

const CMD_CONNECT: u8 = 1;
const CMD_UDP_ASSOCIATE: u8 = 3;
const UDP_HEADER_MAX: usize = 22;
/// How long a domain resolved for a UDP association is reused.
const UDP_RESOLVE_TTL: Duration = Duration::from_secs(60);
const UDP_RESOLVE_MAX: usize = 1024;

/// Writes a handshake message and flushes it: over TLS the bytes otherwise
/// sit in the session buffer while we wait for the client's answer.
async fn send<S: Io>(s: &mut S, bytes: &[u8]) -> io::Result<()> {
    s.write_all(bytes).await?;
    s.flush().await
}

pub async fn handle<S: Io>(mut s: S, sess: Session, ctx: Arc<Ctx>) -> io::Result<()> {
    let negotiated = tokio::time::timeout(
        sess.handshake_timeout(),
        negotiate(&mut s, &sess.cfg.auth, sess.client),
    )
    .await;
    let (cmd, target) = match negotiated {
        Ok(Ok(Some(req))) => req,
        Ok(Err(e)) => return Err(e),
        _ => return Ok(()),
    };
    match cmd {
        CMD_CONNECT => connect(s, sess, ctx, target).await,
        CMD_UDP_ASSOCIATE if sess.cfg.protocols.udp => udp_associate(s, sess, ctx, target).await,
        _ => send(&mut s, &reply(0x07, None)).await,
    }
}

async fn negotiate<S: Io>(
    s: &mut S,
    auth: &AuthConfig,
    client: SocketAddr,
) -> io::Result<Option<(u8, TargetAddr)>> {
    let mut hdr = [0u8; 2];
    s.read_exact(&mut hdr).await?;
    let mut methods = vec![0u8; hdr[1] as usize];
    s.read_exact(&mut methods).await?;
    if auth.enabled() {
        if !methods.contains(&0x02) {
            send(s, &[0x05, 0xFF]).await?;
            return Ok(None);
        }
        send(s, &[0x05, 0x02]).await?;
        let mut ver_len = [0u8; 2];
        s.read_exact(&mut ver_len).await?;
        let mut user = vec![0u8; ver_len[1] as usize];
        s.read_exact(&mut user).await?;
        let plen = s.read_u8().await? as usize;
        let mut pass = vec![0u8; plen];
        s.read_exact(&mut pass).await?;
        if ver_len[0] != 0x01 || !auth.verify(&user, &pass) {
            tracing::warn!("auth failed (socks5) from {client}");
            send(s, &[0x01, 0x01]).await?;
            return Ok(None);
        }
        send(s, &[0x01, 0x00]).await?;
    } else if methods.contains(&0x00) {
        send(s, &[0x05, 0x00]).await?;
    } else {
        send(s, &[0x05, 0xFF]).await?;
        return Ok(None);
    }

    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[0] != 0x05 {
        return Ok(None);
    }
    let target = match req[3] {
        0x01 => {
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await?;
            let port = s.read_u16().await?;
            TargetAddr::Ip(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(b)), port))
        }
        0x04 => {
            let mut b = [0u8; 16];
            s.read_exact(&mut b).await?;
            let port = s.read_u16().await?;
            TargetAddr::Ip(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(b)).to_canonical(),
                port,
            ))
        }
        0x03 => {
            let len = s.read_u8().await? as usize;
            let mut b = vec![0u8; len];
            s.read_exact(&mut b).await?;
            let port = s.read_u16().await?;
            let Ok(host) = String::from_utf8(b) else {
                send(s, &reply(0x01, None)).await?;
                return Ok(None);
            };
            TargetAddr::from_host(&host, port)
        }
        _ => {
            send(s, &reply(0x08, None)).await?;
            return Ok(None);
        }
    };
    Ok(Some((req[1], target)))
}

/// ATYP + address + port, as in replies and UDP headers (no allocation).
fn addr_bytes(addr: SocketAddr) -> ([u8; 19], usize) {
    let mut out = [0u8; 19];
    let len = match addr.ip().to_canonical() {
        IpAddr::V4(ip) => {
            out[0] = 0x01;
            out[1..5].copy_from_slice(&ip.octets());
            5
        }
        IpAddr::V6(ip) => {
            out[0] = 0x04;
            out[1..17].copy_from_slice(&ip.octets());
            17
        }
    };
    out[len..len + 2].copy_from_slice(&addr.port().to_be_bytes());
    (out, len + 2)
}

fn encode_addr(out: &mut Vec<u8>, addr: SocketAddr) {
    let (bytes, len) = addr_bytes(addr);
    out.extend_from_slice(&bytes[..len]);
}

fn reply(code: u8, bound: Option<SocketAddr>) -> Vec<u8> {
    let mut out = vec![0x05, code, 0x00];
    encode_addr(
        &mut out,
        bound.unwrap_or_else(|| SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
    );
    out
}

fn error_code(err: &io::Error) -> u8 {
    match err.kind() {
        io::ErrorKind::ConnectionRefused => 0x05,
        io::ErrorKind::NetworkUnreachable => 0x03,
        io::ErrorKind::HostUnreachable | io::ErrorKind::TimedOut | io::ErrorKind::NotFound => 0x04,
        _ => 0x01,
    }
}

async fn connect<S: Io>(
    mut s: S,
    sess: Session,
    ctx: Arc<Ctx>,
    target: TargetAddr,
) -> io::Result<()> {
    let remote = match sess.open_upstream(&ctx, &target).await {
        Ok(r) => r,
        Err(e) => return send(&mut s, &reply(error_code(&e), None)).await,
    };
    send(&mut s, &reply(0x00, remote.local_addr().ok())).await?;
    let proto = if sess.tls { "socks5+tls" } else { "socks5" };
    let reg = sess.register(&ctx, proto, &target, &remote);
    sess.splice(s, remote, reg).await;
    Ok(())
}

async fn udp_associate<S: Io>(
    mut ctrl: S,
    sess: Session,
    ctx: Arc<Ctx>,
    declared: TargetAddr,
) -> io::Result<()> {
    let relay = match UdpSocket::bind(SocketAddr::new(sess.entry.ip(), 0)).await {
        Ok(sock) => Arc::new(sock),
        Err(_) => return send(&mut ctrl, &reply(0x01, None)).await,
    };
    let expected_port = declared_port(&declared, sess.client.ip());
    let bound = relay.local_addr()?;
    ctrl.write_all(&reply(0x00, Some(bound))).await?;
    ctrl.flush().await?;
    let reg = ctx.stats.register(ConnMeta {
        proto: if sess.tls { "udp+tls" } else { "udp" }.to_string(),
        client: sess.client,
        entry: sess.entry,
        target: "UDP ASSOCIATE".to_string(),
        iface: sess.egress.label().to_string(),
        local: Some(bound),
        remote: None,
        udp: true,
    });
    let mut sink = [0u8; 64];
    tokio::select! {
        _ = async {
            loop {
                match ctrl.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        } => {}
        res = udp_loop(relay, &sess, &ctx, &reg, expected_port) => {
            if let Err(e) = res {
                tracing::debug!(client = %sess.client, "udp relay ended: {e}");
            }
        }
    }
    Ok(())
}

fn parse_udp_header(buf: &[u8]) -> Option<(TargetAddr, usize)> {
    if buf.len() < 4 || buf[2] != 0 {
        return None;
    }
    match buf[3] {
        0x01 => {
            let b: [u8; 4] = buf.get(4..8)?.try_into().ok()?;
            let port = u16::from_be_bytes(buf.get(8..10)?.try_into().ok()?);
            Some((
                TargetAddr::Ip(SocketAddr::new(Ipv4Addr::from(b).into(), port)),
                10,
            ))
        }
        0x04 => {
            let b: [u8; 16] = buf.get(4..20)?.try_into().ok()?;
            let port = u16::from_be_bytes(buf.get(20..22)?.try_into().ok()?);
            let ip = IpAddr::V6(Ipv6Addr::from(b)).to_canonical();
            Some((TargetAddr::Ip(SocketAddr::new(ip, port)), 22))
        }
        0x03 => {
            let len = *buf.get(4)? as usize;
            let host = std::str::from_utf8(buf.get(5..5 + len)?).ok()?;
            let port = u16::from_be_bytes(buf.get(5 + len..7 + len)?.try_into().ok()?);
            Some((TargetAddr::from_host(host, port), 7 + len))
        }
        _ => None,
    }
}

/// The client's UDP port from its UDP ASSOCIATE request, when it can be
/// trusted: non-zero and sent from the address it claims (no NAT in between).
fn declared_port(declared: &TargetAddr, client_ip: IpAddr) -> Option<u16> {
    match declared {
        TargetAddr::Ip(addr)
            if addr.port() != 0 && addr.ip().to_canonical() == client_ip.to_canonical() =>
        {
            Some(addr.port())
        }
        _ => None,
    }
}

fn transient(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::WouldBlock
    )
}

async fn udp_loop(
    relay: Arc<UdpSocket>,
    sess: &Session,
    ctx: &Arc<Ctx>,
    reg: &Registered,
    expected_port: Option<u16>,
) -> io::Result<()> {
    let resolver = ctx.resolver();
    let client_ip = sess.client.ip().to_canonical();
    let mut client: Option<SocketAddr> = None;
    let mut outs: [Option<Arc<UdpSocket>>; 2] = [None, None];
    let mut back_tasks = JoinSet::new();
    let mut resolved: HashMap<String, (IpAddr, Instant)> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, from) = match relay.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) if transient(&e) => continue,
            Err(e) => return Err(e),
        };
        if from.ip().to_canonical() != client_ip
            || expected_port.is_some_and(|port| from.port() != port)
        {
            continue;
        }
        // Validate before binding the association to this sender, so a stray
        // packet from another host behind the same NAT cannot claim it.
        let Some((target, hdr_len)) = parse_udp_header(&buf[..n]) else {
            continue;
        };
        match client {
            Some(c) if c != from => continue,
            None => client = Some(from),
            _ => {}
        }
        let dst = match &target {
            TargetAddr::Ip(addr) => {
                if !sess.egress.allows(Fam::of(&addr.ip())) {
                    continue;
                }
                *addr
            }
            TargetAddr::Domain(host, port) => match resolved.get(host) {
                Some((ip, at)) if at.elapsed() < UDP_RESOLVE_TTL => SocketAddr::new(*ip, *port),
                _ => match egress::candidates(&target, &sess.egress, &resolver).await {
                    Ok(cands) => {
                        if resolved.len() >= UDP_RESOLVE_MAX {
                            resolved.retain(|_, (_, at)| at.elapsed() < UDP_RESOLVE_TTL);
                            if resolved.len() >= UDP_RESOLVE_MAX {
                                resolved.clear();
                            }
                        }
                        resolved.insert(host.clone(), (cands[0].ip(), Instant::now()));
                        cands[0]
                    }
                    Err(_) => continue,
                },
            },
        };
        let fam = Fam::of(&dst.ip());
        let slot = if fam == Fam::V4 { 0 } else { 1 };
        if outs[slot].is_none() {
            let sock = match egress::bound_udp(fam, &sess.egress) {
                Ok(sock) => Arc::new(sock),
                Err(e) => {
                    tracing::debug!("udp egress socket failed: {e}");
                    continue;
                }
            };
            outs[slot] = Some(sock.clone());
            back_tasks.spawn(udp_back(
                sock,
                relay.clone(),
                from,
                reg.conn.clone(),
                reg.iface.clone(),
            ));
        }
        let payload = &buf[hdr_len..n];
        if let Some(out) = &outs[slot] {
            if out.send_to(payload, dst).await.is_ok() {
                reg.conn.add_up(payload.len() as u64);
                reg.iface.traffic.add_up(payload.len() as u64);
            }
        }
    }
}

async fn udp_back(
    sock: Arc<UdpSocket>,
    relay: Arc<UdpSocket>,
    client: SocketAddr,
    conn: Arc<Traffic>,
    iface: Arc<IfaceCounters>,
) {
    let mut buf = vec![0u8; 65536 + UDP_HEADER_MAX];
    loop {
        let (n, from) = match sock.recv_from(&mut buf[UDP_HEADER_MAX..]).await {
            Ok(v) => v,
            Err(e) if transient(&e) => continue,
            Err(_) => break,
        };
        // RSV(2) + FRAG(1) + address, written just before the payload.
        let (addr, len) = addr_bytes(from);
        let start = UDP_HEADER_MAX - 3 - len;
        buf[start..start + 3].fill(0);
        buf[start + 3..UDP_HEADER_MAX].copy_from_slice(&addr[..len]);
        if relay
            .send_to(&buf[start..UDP_HEADER_MAX + n], client)
            .await
            .is_ok()
        {
            conn.add_down(n as u64);
            iface.traffic.add_down(n as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_udp_port_is_used_only_without_nat() {
        let client: IpAddr = "10.0.0.5".parse().unwrap();
        let ip = |s: &str| TargetAddr::Ip(s.parse().unwrap());
        assert_eq!(declared_port(&ip("10.0.0.5:5000"), client), Some(5000));
        assert_eq!(declared_port(&ip("0.0.0.0:0"), client), None);
        assert_eq!(declared_port(&ip("192.168.1.9:5000"), client), None);
        assert_eq!(declared_port(&ip("0.0.0.0:5000"), client), None);
    }

    #[test]
    fn udp_header_roundtrip() {
        let mut pkt = vec![0, 0, 0];
        encode_addr(&mut pkt, "[2001:db8::1]:53".parse().unwrap());
        pkt.extend_from_slice(b"data");
        let (target, len) = parse_udp_header(&pkt).unwrap();
        assert_eq!(len, 22);
        assert_eq!(target.to_string(), "[2001:db8::1]:53");
        let domain = [0, 0, 0, 3, 3, b'a', b'b', b'c', 0, 80, 1];
        let (target, len) = parse_udp_header(&domain).unwrap();
        assert_eq!((target.to_string().as_str(), len), ("abc:80", 10));
        assert!(parse_udp_header(&[0, 0, 1, 1, 0, 0, 0, 0, 0, 0]).is_none());
    }
}
