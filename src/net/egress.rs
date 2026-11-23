use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(unix)]
use std::os::fd::AsFd as AsSocket;
#[cfg(windows)]
use std::os::windows::io::AsSocket;
use std::time::Duration;

use socket2::{Domain, Protocol, SockRef, Type};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::task::JoinSet;

use crate::config::{EgressConfig, EgressPolicy};
use crate::daemon::stats::DEFAULT_IFACE;
use crate::net::dns::Resolver;
use crate::net::netif::NetSnapshot;

const HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(250);
const MAX_CANDIDATES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fam {
    V4,
    V6,
}

impl Fam {
    pub fn of(ip: &IpAddr) -> Fam {
        match ip {
            IpAddr::V4(_) => Fam::V4,
            IpAddr::V6(_) => Fam::V6,
        }
    }

    pub fn other(self) -> Fam {
        match self {
            Fam::V4 => Fam::V6,
            Fam::V6 => Fam::V4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Fam::V4 => "IPv4",
            Fam::V6 => "IPv6",
        }
    }
}

#[derive(Clone, Debug)]
pub enum TargetAddr {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl TargetAddr {
    pub fn from_host(host: &str, port: u16) -> TargetAddr {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        match bare.parse::<IpAddr>() {
            Ok(ip) => TargetAddr::Ip(SocketAddr::new(ip.to_canonical(), port)),
            Err(_) => TargetAddr::Domain(host.to_string(), port),
        }
    }

    /// Parses `host`, `host:port`, `[v6]`, `[v6]:port` or a bare IPv6 literal.
    pub fn parse_authority(text: &str, default_port: u16) -> Option<TargetAddr> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if let Some(rest) = text.strip_prefix('[') {
            let (host, after) = rest.split_once(']')?;
            let port = match after {
                "" => default_port,
                p => p.strip_prefix(':')?.parse().ok()?,
            };
            let ip: Ipv6Addr = host.parse().ok()?;
            return Some(TargetAddr::Ip(SocketAddr::new(
                IpAddr::V6(ip).to_canonical(),
                port,
            )));
        }
        if text.matches(':').count() > 1 {
            let ip: Ipv6Addr = text.parse().ok()?;
            return Some(TargetAddr::Ip(SocketAddr::new(
                IpAddr::V6(ip).to_canonical(),
                default_port,
            )));
        }
        let (host, port) = match text.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (text, default_port),
        };
        if host.is_empty() {
            return None;
        }
        Some(TargetAddr::from_host(host, port))
    }
}

impl fmt::Display for TargetAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetAddr::Ip(addr) => write!(f, "{addr}"),
            TargetAddr::Domain(host, port) => write!(f, "{host}:{port}"),
        }
    }
}

/// Where and how outbound traffic for one client connection leaves the host.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Egress {
    /// Interface to pin sockets to; `None` means "let the OS route".
    pub iface: Option<String>,
    pub index: u32,
    /// Source addresses available on the egress interface (used for binding).
    pub src_v4: Option<Ipv4Addr>,
    pub src_v6: Option<Ipv6Addr>,
    /// Address families allowed for the target, most preferred first.
    pub order: Vec<Fam>,
}

impl Egress {
    pub fn label(&self) -> &str {
        self.iface.as_deref().unwrap_or(DEFAULT_IFACE)
    }

    pub fn allows(&self, fam: Fam) -> bool {
        self.order.contains(&fam)
    }

    pub fn src(&self, fam: Fam) -> Option<IpAddr> {
        match fam {
            Fam::V4 => self.src_v4.map(IpAddr::V4),
            Fam::V6 => self.src_v6.map(IpAddr::V6),
        }
    }

    /// Whether a socket of this family can be opened through the egress at all.
    pub fn can_use(&self, fam: Fam) -> bool {
        self.iface.is_none() || self.src(fam).is_some()
    }

    pub fn unbound_any() -> Egress {
        Egress {
            iface: None,
            index: 0,
            src_v4: None,
            src_v6: None,
            order: vec![Fam::V4, Fam::V6],
        }
    }
}

/// Picks the egress for a connection that arrived on local address `entry`.
pub fn select(entry: IpAddr, snap: &NetSnapshot, cfg: &EgressConfig) -> Egress {
    let entry = entry.to_canonical();
    let entry_fam = Fam::of(&entry);
    let routes_known = snap.routes_known;
    let src_iface = snap.iface_of(entry);
    let out_iface = src_iface
        .and_then(|i| match cfg.iface_override.get(&i.name) {
            Some(target) => snap.by_name(target),
            None => Some(i),
        })
        .filter(|i| i.egress_capable(routes_known));

    let (iface, index, src_v4, src_v6, can_v4, can_v6) = match out_iface {
        Some(out) => {
            let same = src_iface.map(|s| s.name == out.name).unwrap_or(false);
            let pair = src_iface
                .and_then(|s| match entry {
                    IpAddr::V4(a) => s.v4.iter().position(|x| *x == a),
                    IpAddr::V6(a) => s.v6.iter().position(|x| *x == a),
                })
                .unwrap_or(0);
            let v4_route = out.v4_egress(routes_known);
            let src_v4 = match entry {
                IpAddr::V4(a) if same && v4_route => Some(a),
                _ if v4_route => Some(out.v4[pair % out.v4.len()]),
                _ => None,
            };
            // IPv6 egress only when the interface's IPv6 really works (probed),
            // even for clients that came in over IPv6.
            let v6_ok = out.v6_egress(routes_known);
            let src_v6 = match entry {
                IpAddr::V6(a) if same && v6_ok && !is_ula(&a) => Some(a),
                _ if v6_ok => Some(pick_v6(out, pair)),
                _ => None,
            };
            (
                Some(out.name.clone()),
                out.index,
                src_v4,
                src_v6,
                src_v4.is_some(),
                src_v6.is_some(),
            )
        }
        None => (
            None,
            0,
            None,
            None,
            snap.any_v4_egress(),
            snap.any_v6_egress(),
        ),
    };

    let allowed = |fam: Fam| match fam {
        Fam::V4 => can_v4,
        Fam::V6 => can_v6,
    };
    let preferred = match cfg.policy {
        EgressPolicy::SameIface | EgressPolicy::Strict => entry_fam,
        EgressPolicy::PreferV4 => Fam::V4,
        EgressPolicy::PreferV6 => Fam::V6,
    };
    let mut order = Vec::with_capacity(2);
    if allowed(preferred) {
        order.push(preferred);
    }
    if cfg.policy != EgressPolicy::Strict && allowed(preferred.other()) {
        order.push(preferred.other());
    }
    Egress {
        iface,
        index,
        src_v4,
        src_v6,
        order,
    }
}

/// Unique local address (fc00::/7): reachable on the LAN only.
fn is_ula(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// A source for IPv6 egress, preferring globally routable addresses (a client
/// that came in over a ULA still needs a global source to reach the internet).
fn pick_v6(iface: &crate::net::netif::Iface, pair: usize) -> Ipv6Addr {
    let global: Vec<Ipv6Addr> = iface.v6.iter().copied().filter(|a| !is_ula(a)).collect();
    let pool = if global.is_empty() {
        &iface.v6
    } else {
        &global
    };
    pool[pair % pool.len()]
}

fn no_family_error(fam: Fam, eg: &Egress) -> io::Error {
    io::Error::new(
        io::ErrorKind::NetworkUnreachable,
        format!("no {} egress available via {}", fam.label(), eg.label()),
    )
}

/// Resolves the target into candidate socket addresses ordered per RFC 8305.
pub async fn candidates(
    target: &TargetAddr,
    eg: &Egress,
    resolver: &Resolver,
) -> io::Result<Vec<SocketAddr>> {
    match target {
        TargetAddr::Ip(addr) => {
            let ip = addr.ip().to_canonical();
            let fam = Fam::of(&ip);
            if !eg.allows(fam) && !ip.is_loopback() {
                return Err(no_family_error(fam, eg));
            }
            Ok(vec![SocketAddr::new(ip, addr.port())])
        }
        TargetAddr::Domain(host, port) => {
            let (v4, v6) = resolver.resolve(host, eg).await?;
            let v4: Vec<IpAddr> = v4.into_iter().map(IpAddr::V4).collect();
            let v6: Vec<IpAddr> = v6.into_iter().map(IpAddr::V6).collect();
            let (first, second) = match eg.order.first() {
                Some(Fam::V6) => (v6, v4),
                _ => (v4, v6),
            };
            let mut out = Vec::new();
            let mut a = first.into_iter();
            let mut b = second.into_iter();
            loop {
                let x = a.next();
                let y = b.next();
                if x.is_none() && y.is_none() {
                    break;
                }
                out.extend(x.into_iter().chain(y).map(|ip| SocketAddr::new(ip, *port)));
            }
            out.retain(|a| eg.allows(Fam::of(&a.ip())));
            out.truncate(MAX_CANDIDATES);
            if out.is_empty() {
                let fams: Vec<&str> = eg.order.iter().map(|f| f.label()).collect();
                return Err(io::Error::new(
                    io::ErrorKind::HostUnreachable,
                    format!(
                        "{host} has no usable address (allowed: {}) via {}",
                        if fams.is_empty() {
                            "none".to_string()
                        } else {
                            fams.join("/")
                        },
                        eg.label()
                    ),
                ));
            }
            Ok(out)
        }
    }
}

/// Connects to the target through the egress using Happy Eyeballs between candidates.
pub async fn connect_tcp(
    target: &TargetAddr,
    eg: &Egress,
    resolver: &Resolver,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let work = async {
        let cands = candidates(target, eg, resolver).await?;
        happy_eyeballs(cands, eg).await
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(res) => res,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("connect to {target} timed out"),
        )),
    }
}

async fn happy_eyeballs(cands: Vec<SocketAddr>, eg: &Egress) -> io::Result<TcpStream> {
    if cands.len() == 1 {
        return connect_via(cands[0], eg).await;
    }
    let mut pending = cands.into_iter();
    let mut set = JoinSet::new();
    let mut last_err = None;
    let spawn = |set: &mut JoinSet<io::Result<TcpStream>>, addr: SocketAddr| {
        let eg = eg.clone();
        set.spawn(async move { connect_via(addr, &eg).await });
    };
    if let Some(addr) = pending.next() {
        spawn(&mut set, addr);
    }
    loop {
        let more = pending.len() > 0;
        tokio::select! {
            res = set.join_next(), if !set.is_empty() => {
                match res {
                    Some(Ok(Ok(stream))) => return Ok(stream),
                    Some(Ok(Err(e))) => last_err = Some(e),
                    Some(Err(e)) => last_err = Some(io::Error::other(e.to_string())),
                    None => {}
                }
                if let Some(addr) = pending.next() {
                    spawn(&mut set, addr);
                } else if set.is_empty() {
                    break;
                }
            }
            _ = tokio::time::sleep(HAPPY_EYEBALLS_DELAY), if more => {
                if let Some(addr) = pending.next() {
                    spawn(&mut set, addr);
                }
            }
            else => break,
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("no connection candidates")))
}

pub async fn connect_via(addr: SocketAddr, eg: &Egress) -> io::Result<TcpStream> {
    let fam = Fam::of(&addr.ip());
    connect_bound(addr, eg.iface.as_deref(), eg.index, eg.src(fam)).await
}

pub async fn connect_bound(
    addr: SocketAddr,
    iface: Option<&str>,
    index: u32,
    src: Option<IpAddr>,
) -> io::Result<TcpStream> {
    let fam = Fam::of(&addr.ip());
    let sock = match fam {
        Fam::V4 => TcpSocket::new_v4()?,
        Fam::V6 => TcpSocket::new_v6()?,
    };
    prepare(&sock, fam, iface, index, src, true)?;
    let stream = sock.connect(addr).await?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Opens a UDP socket pinned to the egress for the given family.
pub fn bound_udp(fam: Fam, eg: &Egress) -> io::Result<UdpSocket> {
    let domain = match fam {
        Fam::V4 => Domain::IPV4,
        Fam::V6 => Domain::IPV6,
    };
    let sock = socket2::Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if fam == Fam::V6 {
        sock.set_only_v6(true)?;
    }
    let src = eg.src(fam);
    prepare(&sock, fam, eg.iface.as_deref(), eg.index, src, false)?;
    if src.is_none() {
        let any: IpAddr = match fam {
            Fam::V4 => Ipv4Addr::UNSPECIFIED.into(),
            Fam::V6 => Ipv6Addr::UNSPECIFIED.into(),
        };
        sock.bind(&SocketAddr::new(any, 0).into())?;
    }
    sock.set_nonblocking(true)?;
    UdpSocket::from_std(sock.into())
}

fn prepare<S: AsSocket>(
    sock: &S,
    fam: Fam,
    iface: Option<&str>,
    index: u32,
    src: Option<IpAddr>,
    tcp: bool,
) -> io::Result<()> {
    let sref = SockRef::from(sock);
    if let Some(name) = iface {
        bind_device(&sref, fam, name, index)?;
    }
    if let Some(src) = src {
        if tcp {
            bind_address_no_port(&sref);
        }
        sref.bind(&SocketAddr::new(src, 0).into())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn bind_device(sref: &SockRef<'_>, _fam: Fam, name: &str, _index: u32) -> io::Result<()> {
    match sref.bind_device(Some(name.as_bytes())) {
        Ok(()) => Ok(()),
        // Without CAP_NET_RAW (kernels < 5.7) rely on the source address + policy routing.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn bind_device(sref: &SockRef<'_>, fam: Fam, _name: &str, index: u32) -> io::Result<()> {
    let index = std::num::NonZeroU32::new(index);
    if index.is_none() {
        return Ok(());
    }
    match fam {
        Fam::V4 => sref.bind_device_by_index_v4(index),
        Fam::V6 => sref.bind_device_by_index_v6(index),
    }
}

/// IP_UNICAST_IF / IPV6_UNICAST_IF. The source address alone already pins the
/// interface under Windows' default strong host model; this makes it explicit.
#[cfg(windows)]
fn bind_device(sref: &SockRef<'_>, fam: Fam, _name: &str, index: u32) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, IP_UNICAST_IF, SOCKET_ERROR,
    };
    if index == 0 {
        return Ok(());
    }
    // IPv4 takes the index in network byte order, IPv6 in host byte order.
    let (level, name, value) = match fam {
        Fam::V4 => (IPPROTO_IP, IP_UNICAST_IF, index.to_be()),
        Fam::V6 => (IPPROTO_IPV6, IPV6_UNICAST_IF, index),
    };
    let ret = unsafe {
        setsockopt(
            sref.as_raw_socket() as usize,
            level,
            name,
            (&value as *const u32).cast(),
            std::mem::size_of::<u32>() as i32,
        )
    };
    if ret == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
fn bind_device(_sref: &SockRef<'_>, _fam: Fam, _name: &str, _index: u32) -> io::Result<()> {
    Ok(())
}

/// Defers ephemeral port selection to connect() so bind-before-connect does not
/// exhaust ports (Linux IP_BIND_ADDRESS_NO_PORT).
#[cfg(target_os = "linux")]
fn bind_address_no_port(sref: &SockRef<'_>) {
    use std::os::fd::AsRawFd;
    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            sref.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_BIND_ADDRESS_NO_PORT,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn bind_address_no_port(_sref: &SockRef<'_>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::netif::Iface;

    fn iface(name: &str, v4: &[&str], v6: &[&str], d4: bool, d6: bool, ok6: bool) -> Iface {
        Iface {
            name: name.into(),
            index: 1,
            v4: v4.iter().map(|s| s.parse().unwrap()).collect(),
            v6: v6.iter().map(|s| s.parse().unwrap()).collect(),
            default_v4: d4,
            default_v6: d6,
            v6_ok: ok6,
        }
    }

    fn snap() -> NetSnapshot {
        NetSnapshot::new(
            vec![
                iface(
                    "eth0",
                    &["1.1.1.10", "1.1.1.11"],
                    &["2001:db8::10", "2001:db8::11"],
                    true,
                    true,
                    true,
                ),
                iface("eth1", &["2.2.2.20"], &[], true, false, false),
                iface("docker0", &["172.17.0.1"], &[], false, false, false),
            ],
            true,
        )
    }

    #[test]
    fn same_iface_keeps_entry_ip_and_pairs_v6() {
        let cfg = EgressConfig::default();
        let eg = select("1.1.1.11".parse().unwrap(), &snap(), &cfg);
        assert_eq!(eg.iface.as_deref(), Some("eth0"));
        assert_eq!(eg.src_v4, Some("1.1.1.11".parse().unwrap()));
        assert_eq!(eg.src_v6, Some("2001:db8::11".parse().unwrap()));
        assert_eq!(eg.order, vec![Fam::V4, Fam::V6]);
    }

    #[test]
    fn v6_entry_prefers_v6_then_falls_back() {
        let cfg = EgressConfig::default();
        let eg = select("2001:db8::10".parse().unwrap(), &snap(), &cfg);
        assert_eq!(eg.src_v4, Some("1.1.1.10".parse().unwrap()));
        assert_eq!(eg.order, vec![Fam::V6, Fam::V4]);
    }

    #[test]
    fn v6_entry_without_working_v6_uses_v4() {
        let snap = NetSnapshot::new(
            vec![iface(
                "eth0",
                &["1.1.1.10"],
                &["2001:db8::10"],
                true,
                true,
                false,
            )],
            true,
        );
        let eg = select(
            "2001:db8::10".parse().unwrap(),
            &snap,
            &EgressConfig::default(),
        );
        assert_eq!(eg.src_v6, None);
        assert_eq!(eg.order, vec![Fam::V4]);
        let strict = EgressConfig {
            policy: EgressPolicy::Strict,
            ..Default::default()
        };
        assert!(select("2001:db8::10".parse().unwrap(), &snap, &strict)
            .order
            .is_empty());
    }

    #[test]
    fn ula_entry_egresses_from_a_global_address() {
        let snap = NetSnapshot::new(
            vec![iface(
                "eth0",
                &["1.1.1.10"],
                &["2001:db8::10", "fd00::10"],
                true,
                true,
                true,
            )],
            true,
        );
        let eg = select("fd00::10".parse().unwrap(), &snap, &EgressConfig::default());
        assert_eq!(eg.src_v6, Some("2001:db8::10".parse().unwrap()));
        assert_eq!(eg.order, vec![Fam::V6, Fam::V4]);
    }

    #[test]
    fn iface_without_v6_never_uses_v6() {
        let cfg = EgressConfig::default();
        let eg = select("2.2.2.20".parse().unwrap(), &snap(), &cfg);
        assert_eq!(eg.iface.as_deref(), Some("eth1"));
        assert_eq!(eg.order, vec![Fam::V4]);
    }

    #[test]
    fn strict_only_allows_entry_family() {
        let cfg = EgressConfig {
            policy: EgressPolicy::Strict,
            ..Default::default()
        };
        let eg = select("1.1.1.10".parse().unwrap(), &snap(), &cfg);
        assert_eq!(eg.order, vec![Fam::V4]);
    }

    #[test]
    fn non_egress_iface_uses_default_routing() {
        let cfg = EgressConfig::default();
        let eg = select("172.17.0.1".parse().unwrap(), &snap(), &cfg);
        assert!(eg.iface.is_none());
        assert_eq!(eg.order, vec![Fam::V4, Fam::V6]);
    }

    #[test]
    fn override_maps_iface() {
        let mut cfg = EgressConfig::default();
        cfg.iface_override.insert("docker0".into(), "eth1".into());
        let eg = select("172.17.0.1".parse().unwrap(), &snap(), &cfg);
        assert_eq!(eg.iface.as_deref(), Some("eth1"));
        assert_eq!(eg.src_v4, Some("2.2.2.20".parse().unwrap()));
    }

    #[test]
    fn parse_authority_forms() {
        assert!(matches!(
            TargetAddr::parse_authority("example.com:443", 80),
            Some(TargetAddr::Domain(h, 443)) if h == "example.com"
        ));
        assert!(matches!(
            TargetAddr::parse_authority("[2001:db8::1]:8443", 80),
            Some(TargetAddr::Ip(a)) if a.port() == 8443 && a.is_ipv6()
        ));
        assert!(matches!(
            TargetAddr::parse_authority("1.2.3.4", 80),
            Some(TargetAddr::Ip(a)) if a.port() == 80
        ));
        assert!(TargetAddr::parse_authority(":80", 80).is_none());
    }
}
