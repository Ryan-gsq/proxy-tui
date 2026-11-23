use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{parse_dns_server, DnsConfig};
use crate::net::egress::{self, Egress, Fam};

const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;
const NEGATIVE_TTL: u64 = 30;
const MIN_TTL: u64 = 5;
const SYSTEM_TTL: u64 = 60;
const CACHE_LIMIT: usize = 20_000;

type Lookup = (Vec<Ipv4Addr>, Vec<Ipv6Addr>);

struct CacheEntry {
    addrs: Vec<IpAddr>,
    expires: Instant,
}

/// Stub resolver whose queries leave through the same egress as the proxied
/// connection, so DNS answers match the egress location.
pub struct Resolver {
    servers: Vec<IpAddr>,
    hosts: HashMap<String, Vec<IpAddr>>,
    timeout: Duration,
    ttl_max: u64,
    cache: Mutex<HashMap<(String, u32, u16), CacheEntry>>,
}

impl Resolver {
    pub fn new(cfg: &DnsConfig) -> Self {
        let mut servers: Vec<IpAddr> = cfg
            .servers
            .iter()
            .filter_map(|s| parse_dns_server(s).ok())
            .collect();
        if servers.is_empty() {
            servers = system_nameservers();
        }
        Self {
            servers,
            hosts: read_hosts(),
            timeout: Duration::from_millis(cfg.timeout_ms.max(100)),
            ttl_max: cfg.cache_ttl_max_secs.max(MIN_TTL),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn servers(&self) -> &[IpAddr] {
        &self.servers
    }

    pub async fn resolve(&self, host: &str, eg: &Egress) -> io::Result<Lookup> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(ips) = self.hosts.get(&host) {
            return Ok(split(ips.iter().copied(), eg));
        }
        if self.servers.is_empty() || host == "localhost" || host.ends_with(".localhost") {
            return Ok(split(system_ips(&host).await?.into_iter(), eg));
        }
        let want4 = eg.allows(Fam::V4);
        let want6 = eg.allows(Fam::V6);
        let (r4, r6) = tokio::join!(
            async {
                if want4 {
                    Some(self.query_cached(&host, QTYPE_A, eg).await)
                } else {
                    None
                }
            },
            async {
                if want6 {
                    Some(self.query_cached(&host, QTYPE_AAAA, eg).await)
                } else {
                    None
                }
            }
        );
        let failed = matches!(r4, Some(Err(_))) || matches!(r6, Some(Err(_)));
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for ip in r4
            .and_then(Result::ok)
            .into_iter()
            .flatten()
            .chain(r6.and_then(Result::ok).into_iter().flatten())
        {
            match ip {
                IpAddr::V4(a) => v4.push(a),
                IpAddr::V6(a) => v6.push(a),
            }
        }
        if failed && v4.is_empty() && v6.is_empty() {
            // Egress-bound DNS failed (e.g. resolver only reachable via another NIC);
            // fall back to the OS resolver rather than failing the connection.
            // Cached unfiltered: requests allowing other families share the entry.
            let all = system_ips(&host).await?;
            self.store((host.clone(), eg.index, 0), all.clone(), SYSTEM_TTL);
            return Ok(split(all.into_iter(), eg));
        }
        Ok((v4, v6))
    }

    fn lookup_cache(&self, key: &(String, u32, u16)) -> Option<Vec<IpAddr>> {
        let cache = self.cache.lock().unwrap();
        let entry = cache.get(key)?;
        (entry.expires > Instant::now()).then(|| entry.addrs.clone())
    }

    fn store(&self, key: (String, u32, u16), addrs: Vec<IpAddr>, ttl: u64) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_LIMIT {
            let now = Instant::now();
            cache.retain(|_, v| v.expires > now);
            if cache.len() >= CACHE_LIMIT {
                cache.clear();
            }
        }
        cache.insert(
            key,
            CacheEntry {
                addrs,
                expires: Instant::now() + Duration::from_secs(ttl),
            },
        );
    }

    async fn query_cached(&self, host: &str, qtype: u16, eg: &Egress) -> io::Result<Vec<IpAddr>> {
        let key = (host.to_string(), eg.index, qtype);
        if let Some(hit) = self.lookup_cache(&key) {
            return Ok(hit);
        }
        if let Some(hit) = self.lookup_cache(&(host.to_string(), eg.index, 0)) {
            let want = if qtype == QTYPE_A { Fam::V4 } else { Fam::V6 };
            return Ok(hit.into_iter().filter(|ip| Fam::of(ip) == want).collect());
        }
        let (addrs, ttl) = self.query_servers(host, qtype, eg).await?;
        let ttl = if addrs.is_empty() {
            NEGATIVE_TTL
        } else {
            (ttl as u64).clamp(MIN_TTL, self.ttl_max)
        };
        self.store(key, addrs.clone(), ttl);
        Ok(addrs)
    }

    async fn query_servers(
        &self,
        host: &str,
        qtype: u16,
        eg: &Egress,
    ) -> io::Result<(Vec<IpAddr>, u32)> {
        let preferred = eg.order.first().copied().unwrap_or(Fam::V4);
        let mut servers: Vec<IpAddr> = self
            .servers
            .iter()
            .copied()
            .filter(|s| eg.can_use(Fam::of(s)))
            .collect();
        servers.sort_by_key(|s| Fam::of(s) != preferred);
        let mut last_err = io::Error::new(
            io::ErrorKind::NotFound,
            format!("no DNS server reachable via {}", eg.label()),
        );
        for server in servers {
            match self.query_udp(server, host, qtype, eg).await {
                Ok(answer) => return Ok(answer),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    async fn query_udp(
        &self,
        server: IpAddr,
        host: &str,
        qtype: u16,
        eg: &Egress,
    ) -> io::Result<(Vec<IpAddr>, u32)> {
        let (id, packet) = build_query(host, qtype)?;
        let sock = egress::bound_udp(Fam::of(&server), eg)?;
        let addr = SocketAddr::new(server, 53);
        sock.connect(addr).await?;
        sock.send(&packet).await?;
        let mut buf = vec![0u8; 4096];
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let n = tokio::time::timeout(remaining, sock.recv(&mut buf))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dns timeout"))??;
            let Some(resp) = parse_response(&buf[..n], id, qtype) else {
                continue;
            };
            if resp.truncated {
                return self.query_tcp(addr, &packet, id, qtype, eg).await;
            }
            return resp.into_result();
        }
    }

    async fn query_tcp(
        &self,
        addr: SocketAddr,
        packet: &[u8],
        id: u16,
        qtype: u16,
        eg: &Egress,
    ) -> io::Result<(Vec<IpAddr>, u32)> {
        let work = async {
            let mut stream = egress::connect_via(addr, eg).await?;
            let mut msg = Vec::with_capacity(packet.len() + 2);
            msg.extend_from_slice(&(packet.len() as u16).to_be_bytes());
            msg.extend_from_slice(packet);
            stream.write_all(&msg).await?;
            let len = stream.read_u16().await? as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            parse_response(&buf, id, qtype)
                .ok_or_else(|| io::Error::other("invalid dns response"))?
                .into_result()
        };
        tokio::time::timeout(self.timeout * 2, work)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dns tcp timeout"))?
    }
}

fn split(ips: impl Iterator<Item = IpAddr>, eg: &Egress) -> Lookup {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for ip in ips {
        match ip.to_canonical() {
            IpAddr::V4(a) if eg.allows(Fam::V4) => v4.push(a),
            IpAddr::V6(a) if eg.allows(Fam::V6) => v6.push(a),
            _ => {}
        }
    }
    (v4, v6)
}

/// The OS resolver's answer, every address family.
async fn system_ips(host: &str) -> io::Result<Vec<IpAddr>> {
    let addrs = tokio::net::lookup_host((host, 0)).await?;
    let mut seen = Vec::new();
    for addr in addrs {
        let ip = addr.ip().to_canonical();
        if !seen.contains(&ip) {
            seen.push(ip);
        }
    }
    Ok(seen)
}

struct DnsResponse {
    rcode: u8,
    truncated: bool,
    addrs: Vec<IpAddr>,
    ttl: Option<u32>,
}

impl DnsResponse {
    fn into_result(self) -> io::Result<(Vec<IpAddr>, u32)> {
        match self.rcode {
            0 => Ok((self.addrs, self.ttl.unwrap_or(SYSTEM_TTL as u32))),
            3 => Ok((Vec::new(), NEGATIVE_TTL as u32)),
            code => Err(io::Error::other(format!("dns rcode {code}"))),
        }
    }
}

fn build_query(host: &str, qtype: u16) -> io::Result<(u16, Vec<u8>)> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid hostname {host:?}"),
        )
    };
    if host.is_empty() || host.len() > 253 {
        return Err(invalid());
    }
    let id: u16 = rand::random();
    let mut pkt = Vec::with_capacity(18 + host.len());
    pkt.extend_from_slice(&id.to_be_bytes());
    pkt.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(invalid());
        }
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0);
    pkt.extend_from_slice(&qtype.to_be_bytes());
    pkt.extend_from_slice(&1u16.to_be_bytes());
    Ok((id, pkt))
}

fn skip_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *buf.get(pos)? as usize;
        if len & 0xC0 == 0xC0 {
            return Some(pos + 2);
        }
        if len == 0 {
            return Some(pos + 1);
        }
        pos += 1 + len;
    }
}

fn read_u16(buf: &[u8], pos: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]))
}

fn parse_response(buf: &[u8], id: u16, qtype: u16) -> Option<DnsResponse> {
    if buf.len() < 12 || read_u16(buf, 0)? != id {
        return None;
    }
    let flags = read_u16(buf, 2)?;
    if flags & 0x8000 == 0 {
        return None;
    }
    let rcode = (flags & 0x000F) as u8;
    if flags & 0x0200 != 0 {
        // Truncated: the records may be cut off mid-way; retry over TCP.
        return Some(DnsResponse {
            rcode,
            truncated: true,
            addrs: Vec::new(),
            ttl: None,
        });
    }
    let qd = read_u16(buf, 4)?;
    let an = read_u16(buf, 6)?;
    let mut pos = 12;
    for _ in 0..qd {
        pos = skip_name(buf, pos)? + 4;
    }
    let mut addrs = Vec::new();
    let mut ttl: Option<u32> = None;
    for _ in 0..an {
        pos = skip_name(buf, pos)?;
        let rtype = read_u16(buf, pos)?;
        let rttl = u32::from_be_bytes(buf.get(pos + 4..pos + 8)?.try_into().ok()?);
        let rdlen = read_u16(buf, pos + 8)? as usize;
        pos += 10;
        let rdata = buf.get(pos..pos + rdlen)?;
        pos += rdlen;
        let ip = match (rtype, qtype, rdlen) {
            (QTYPE_A, QTYPE_A, 4) => {
                IpAddr::V4(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]))
            }
            (QTYPE_AAAA, QTYPE_AAAA, 16) => {
                let raw: [u8; 16] = rdata.try_into().ok()?;
                IpAddr::V6(Ipv6Addr::from(raw))
            }
            _ => continue,
        };
        if !addrs.contains(&ip) {
            addrs.push(ip);
        }
        ttl = Some(ttl.map_or(rttl, |t| t.min(rttl)));
    }
    Some(DnsResponse {
        rcode,
        truncated: false,
        addrs,
        ttl,
    })
}

/// Upstream servers from resolv.conf. When it only names a local stub (e.g.
/// systemd-resolved's 127.0.0.53), the stub's own upstream list is used so the
/// queries can still be pinned to an egress.
#[cfg(unix)]
fn system_nameservers() -> Vec<IpAddr> {
    ["/etc/resolv.conf", "/run/systemd/resolve/resolv.conf"]
        .into_iter()
        .map(nameservers_in)
        .find(|list| !list.is_empty())
        .unwrap_or_default()
}

/// The DNS servers configured on adapters that are up, without Windows'
/// deprecated `fec0:0:0:ffff::1..3` placeholders.
#[cfg(windows)]
fn system_nameservers() -> Vec<IpAddr> {
    use ipconfig::OperStatus;
    let mut out: Vec<IpAddr> = Vec::new();
    for adapter in ipconfig::get_adapters().unwrap_or_default() {
        if adapter.oper_status() != OperStatus::IfOperStatusUp {
            continue;
        }
        for ip in adapter.dns_servers() {
            let placeholder =
                matches!(ip, IpAddr::V6(v6) if v6.segments()[..4] == [0xfec0, 0, 0, 0xffff]);
            if !ip.is_loopback() && !ip.is_unspecified() && !placeholder && !out.contains(ip) {
                out.push(*ip);
            }
        }
    }
    out
}

#[cfg(unix)]
fn nameservers_in(path: &str) -> Vec<IpAddr> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("nameserver") {
            continue;
        }
        if let Some(Ok(ip)) = parts.next().map(parse_dns_server) {
            // Local stub resolvers (systemd-resolved, dnsmasq) cannot be pinned to an egress.
            if !ip.is_loopback() && !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

fn hosts_path() -> std::path::PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        std::path::Path::new(&root).join(r"System32\drivers\etc\hosts")
    } else {
        "/etc/hosts".into()
    }
}

fn read_hosts() -> HashMap<String, Vec<IpAddr>> {
    let mut map: HashMap<String, Vec<IpAddr>> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(hosts_path()) else {
        return map;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut parts = line.split_whitespace();
        let Some(Ok(ip)) = parts
            .next()
            .map(|s| s.split('%').next().unwrap_or(s).parse::<IpAddr>())
        else {
            continue;
        };
        for name in parts {
            let list = map.entry(name.to_ascii_lowercase()).or_default();
            if !list.contains(&ip) {
                list.push(ip);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_roundtrip_parses_answers() {
        let (id, query) = build_query("example.com", QTYPE_A).unwrap();
        let mut resp = query.clone();
        resp[2] = 0x81;
        resp[3] = 0x80;
        resp[7] = 2;
        for last in [1u8, 2] {
            resp.extend_from_slice(&[
                0xC0,
                12,
                0,
                1,
                0,
                1,
                0,
                0,
                0,
                100 + last,
                0,
                4,
                93,
                184,
                216,
                last,
            ]);
        }
        let parsed = parse_response(&resp, id, QTYPE_A).unwrap();
        assert_eq!(parsed.rcode, 0);
        assert_eq!(parsed.addrs.len(), 2);
        assert_eq!(parsed.ttl, Some(101));
        assert!(parse_response(&resp, id.wrapping_add(1), QTYPE_A).is_none());
    }

    #[test]
    fn truncated_response_is_reported_before_parsing_records() {
        let (id, query) = build_query("example.com", QTYPE_A).unwrap();
        let mut resp = query.clone();
        resp[2] = 0x83; // QR + TC + RD
        resp[3] = 0x80;
        resp[7] = 3; // claims 3 answers, but the packet stops here
        resp.extend_from_slice(&[0xC0, 12, 0, 1]);
        let parsed = parse_response(&resp, id, QTYPE_A).unwrap();
        assert!(parsed.truncated);
    }

    #[test]
    fn rejects_bad_hostnames() {
        assert!(build_query("a..b", QTYPE_A).is_err());
        assert!(build_query(&"a".repeat(64), QTYPE_A).is_err());
    }
}
