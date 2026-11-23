#[cfg(unix)]
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

#[cfg(unix)]
use nix::ifaddrs::getifaddrs;
#[cfg(unix)]
use nix::net::if_::{if_nametoindex, InterfaceFlags};

use crate::net::egress;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    pub index: u32,
    pub v4: Vec<Ipv4Addr>,
    pub v6: Vec<Ipv6Addr>,
    pub default_v4: bool,
    pub default_v6: bool,
    pub v6_ok: bool,
}

impl Iface {
    pub fn egress_capable(&self, routes_known: bool) -> bool {
        !routes_known || self.default_v4 || self.default_v6
    }

    pub fn v4_egress(&self, routes_known: bool) -> bool {
        !self.v4.is_empty() && (self.default_v4 || !routes_known)
    }

    pub fn v6_route(&self, routes_known: bool) -> bool {
        !self.v6.is_empty() && (self.default_v6 || !routes_known)
    }

    pub fn v6_egress(&self, routes_known: bool) -> bool {
        self.v6_route(routes_known) && self.v6_ok
    }
}

#[derive(Clone, Debug, Default)]
pub struct NetSnapshot {
    pub ifaces: Vec<Iface>,
    pub routes_known: bool,
    by_ip: HashMap<IpAddr, usize>,
}

impl NetSnapshot {
    pub fn new(ifaces: Vec<Iface>, routes_known: bool) -> Self {
        let mut by_ip = HashMap::new();
        for (idx, iface) in ifaces.iter().enumerate() {
            for ip in &iface.v4 {
                by_ip.insert(IpAddr::V4(*ip), idx);
            }
            for ip in &iface.v6 {
                by_ip.insert(IpAddr::V6(*ip), idx);
            }
        }
        Self {
            ifaces,
            routes_known,
            by_ip,
        }
    }

    pub fn iface_of(&self, ip: IpAddr) -> Option<&Iface> {
        self.by_ip.get(&ip).map(|idx| &self.ifaces[*idx])
    }

    pub fn by_name(&self, name: &str) -> Option<&Iface> {
        self.ifaces.iter().find(|i| i.name == name)
    }

    pub fn any_v4_egress(&self) -> bool {
        self.ifaces.is_empty() || self.ifaces.iter().any(|i| i.v4_egress(self.routes_known))
    }

    pub fn any_v6_egress(&self) -> bool {
        self.ifaces.iter().any(|i| i.v6_egress(self.routes_known))
    }

    pub fn all_ips(&self) -> Vec<IpAddr> {
        self.by_ip.keys().copied().collect()
    }
}

#[derive(Default)]
pub struct NetState {
    snap: RwLock<Arc<NetSnapshot>>,
    v6_probe: Mutex<HashMap<String, bool>>,
    routing: Mutex<HashMap<String, String>>,
}

impl NetState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> Arc<NetSnapshot> {
        self.snap.read().unwrap().clone()
    }

    pub fn set_routing_status(&self, status: HashMap<String, String>) {
        *self.routing.lock().unwrap() = status;
    }

    pub fn routing_status(&self, name: &str) -> String {
        self.routing
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Re-reads interfaces and default routes. Returns true when the topology changed.
    pub async fn refresh(&self) -> bool {
        let mut ifaces = tokio::task::spawn_blocking(enumerate)
            .await
            .unwrap_or_default();
        let defaults = default_route_ifaces().await;
        let routes_known = match &defaults {
            Some((v4, v6)) => !v4.is_empty() || !v6.is_empty(),
            None => false,
        };
        let probes = self.v6_probe.lock().unwrap().clone();
        for iface in &mut ifaces {
            if let Some((v4, v6)) = &defaults {
                iface.default_v4 = v4.contains(&iface.name);
                iface.default_v6 = v6.contains(&iface.name);
            }
            iface.v6_ok = probes.get(&iface.name).copied().unwrap_or(false);
        }
        let new = NetSnapshot::new(ifaces, routes_known);
        let mut guard = self.snap.write().unwrap();
        let changed = guard.ifaces != new.ifaces || guard.routes_known != new.routes_known;
        *guard = Arc::new(new);
        changed
    }

    /// Probes IPv6 reachability through every interface that has a global IPv6 route.
    /// Returns the interfaces whose state changed.
    pub async fn probe_v6(&self) -> Vec<(String, bool)> {
        let snap = self.snapshot();
        let mut tasks = Vec::new();
        for iface in &snap.ifaces {
            if !iface.v6_route(snap.routes_known) {
                continue;
            }
            let iface = iface.clone();
            tasks.push(tokio::spawn(async move {
                let ok = probe_iface_v6(&iface).await;
                (iface.name, ok)
            }));
        }
        let mut results = Vec::new();
        for task in tasks {
            if let Ok(res) = task.await {
                results.push(res);
            }
        }
        let mut changes = Vec::new();
        let probes = {
            let mut probes = self.v6_probe.lock().unwrap();
            let alive: HashSet<&str> = snap.ifaces.iter().map(|i| i.name.as_str()).collect();
            probes.retain(|k, _| alive.contains(k.as_str()));
            for (name, ok) in &results {
                if probes.insert(name.clone(), *ok) != Some(*ok) {
                    changes.push((name.clone(), *ok));
                }
            }
            for iface in &snap.ifaces {
                if !iface.v6_route(snap.routes_known) {
                    probes.insert(iface.name.clone(), false);
                }
            }
            probes.clone()
        };
        let mut guard = self.snap.write().unwrap();
        let mut ifaces = guard.ifaces.clone();
        for iface in &mut ifaces {
            iface.v6_ok = probes.get(&iface.name).copied().unwrap_or(false);
        }
        let routes_known = guard.routes_known;
        *guard = Arc::new(NetSnapshot::new(ifaces, routes_known));
        changes
    }
}

const V6_PROBE_TARGETS: [&str; 3] = [
    "[2400:3200::1]:53",
    "[2606:4700:4700::1111]:53",
    "[2001:4860:4860::8888]:53",
];

async fn probe_iface_v6(iface: &crate::net::netif::Iface) -> bool {
    let Some(src) = iface.v6.first().copied() else {
        return false;
    };
    for target in V6_PROBE_TARGETS {
        let addr: SocketAddr = target.parse().expect("static probe address");
        let attempt = egress::connect_bound(
            addr,
            Some(iface.name.as_str()),
            iface.index,
            Some(IpAddr::V6(src)),
        );
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(3), attempt).await {
            return true;
        }
    }
    false
}

/// Addresses that can carry traffic to the outside.
fn usable_v4(ip: &Ipv4Addr) -> bool {
    !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified())
}

fn usable_v6(ip: &Ipv6Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unicast_link_local()
        || ip.to_ipv4_mapped().is_some())
}

fn is_ula(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

#[cfg(unix)]
pub fn enumerate() -> Vec<Iface> {
    let Ok(addrs) = getifaddrs() else {
        return Vec::new();
    };
    let v6_flags = ipv6_flags();
    let mut map: BTreeMap<String, Iface> = BTreeMap::new();
    for ifa in addrs {
        if ifa.flags.contains(InterfaceFlags::IFF_LOOPBACK)
            || !ifa.flags.contains(InterfaceFlags::IFF_UP)
        {
            continue;
        }
        let Some(addr) = ifa.address else { continue };
        let ip = if let Some(sin) = addr.as_sockaddr_in() {
            let ip = sin.ip();
            if !usable_v4(&ip) {
                continue;
            }
            IpAddr::V4(ip)
        } else if let Some(sin6) = addr.as_sockaddr_in6() {
            let ip = sin6.ip();
            if !usable_v6(&ip) {
                continue;
            }
            if let Some(flags) = v6_flags.get(&ip) {
                if flags & (IFA_F_DEPRECATED | IFA_F_TENTATIVE | IFA_F_DADFAILED) != 0 {
                    continue;
                }
            }
            IpAddr::V6(ip)
        } else {
            continue;
        };
        let name = ifa.interface_name.clone();
        let entry = map.entry(name.clone()).or_insert_with(|| Iface {
            index: if_nametoindex(name.as_str()).unwrap_or(0),
            name,
            v4: Vec::new(),
            v6: Vec::new(),
            default_v4: false,
            default_v6: false,
            v6_ok: false,
        });
        match ip {
            IpAddr::V4(v4) if !entry.v4.contains(&v4) => entry.v4.push(v4),
            IpAddr::V6(v6) if !entry.v6.contains(&v6) => entry.v6.push(v6),
            _ => {}
        }
    }
    let mut out: Vec<Iface> = map.into_values().collect();
    for iface in &mut out {
        iface.v6.sort_by_key(|ip| {
            let ula = is_ula(ip);
            let temporary = v6_flags
                .get(ip)
                .map(|f| f & IFA_F_TEMPORARY != 0)
                .unwrap_or(false);
            (ula, temporary)
        });
    }
    out
}

/// Up, non-loopback adapters with usable addresses, each with whether it has
/// an IPv4 / IPv6 default gateway (GetAdaptersAddresses).
#[cfg(windows)]
fn adapters() -> Vec<(Iface, bool, bool)> {
    use ipconfig::{IfType, OperStatus};
    let Ok(list) = ipconfig::get_adapters() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for adapter in &list {
        if adapter.oper_status() != OperStatus::IfOperStatusUp
            || adapter.if_type() == IfType::SoftwareLoopback
        {
            continue;
        }
        let mut iface = Iface {
            name: adapter.friendly_name().to_string(),
            index: adapter.ipv6_if_index(),
            v4: Vec::new(),
            v6: Vec::new(),
            default_v4: false,
            default_v6: false,
            v6_ok: false,
        };
        for ip in adapter.ip_addresses() {
            match ip {
                IpAddr::V4(v4) if usable_v4(v4) && !iface.v4.contains(v4) => iface.v4.push(*v4),
                IpAddr::V6(v6) if usable_v6(v6) && !iface.v6.contains(v6) => iface.v6.push(*v6),
                _ => {}
            }
        }
        if iface.v4.is_empty() && iface.v6.is_empty() {
            continue;
        }
        iface.v6.sort_by_key(is_ula);
        let gateways = adapter.gateways();
        let gw4 = gateways.iter().any(|g| g.is_ipv4() && !g.is_unspecified());
        let gw6 = gateways.iter().any(|g| g.is_ipv6() && !g.is_unspecified());
        out.push((iface, gw4, gw6));
    }
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    out
}

#[cfg(windows)]
pub fn enumerate() -> Vec<Iface> {
    adapters().into_iter().map(|(iface, _, _)| iface).collect()
}

/// Adapters with a default gateway, per family.
#[cfg(windows)]
pub async fn default_route_ifaces() -> Option<(HashSet<String>, HashSet<String>)> {
    let list = tokio::task::spawn_blocking(adapters).await.ok()?;
    let names = |pick: fn(&(Iface, bool, bool)) -> bool| {
        list.iter()
            .filter(|a| pick(a))
            .map(|a| a.0.name.clone())
            .collect::<HashSet<String>>()
    };
    Some((names(|a| a.1), names(|a| a.2)))
}

#[cfg(unix)]
const IFA_F_TEMPORARY: u32 = 0x01;
#[cfg(unix)]
const IFA_F_DADFAILED: u32 = 0x08;
#[cfg(unix)]
const IFA_F_DEPRECATED: u32 = 0x20;
#[cfg(unix)]
const IFA_F_TENTATIVE: u32 = 0x40;

#[cfg(target_os = "linux")]
fn ipv6_flags() -> HashMap<Ipv6Addr, u32> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string("/proc/net/if_inet6") else {
        return out;
    };
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[0].len() != 32 {
            continue;
        }
        let Ok(raw) = u128::from_str_radix(fields[0], 16) else {
            continue;
        };
        let Ok(flags) = u32::from_str_radix(fields[4], 16) else {
            continue;
        };
        out.insert(Ipv6Addr::from(raw), flags);
    }
    out
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ipv6_flags() -> HashMap<Ipv6Addr, u32> {
    HashMap::new()
}

#[cfg(unix)]
async fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Returns the interface names holding an IPv4 / IPv6 default route.
#[cfg(target_os = "linux")]
pub async fn default_route_ifaces() -> Option<(HashSet<String>, HashSet<String>)> {
    fn devs(text: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut tokens = text.split_whitespace();
        while let Some(tok) = tokens.next() {
            if tok == "dev" {
                if let Some(dev) = tokens.next() {
                    out.insert(dev.to_string());
                }
            }
        }
        out
    }
    let v4 = command_output("ip", &["-4", "route", "show", "default"]).await?;
    let v6 = command_output("ip", &["-6", "route", "show", "default"])
        .await
        .unwrap_or_default();
    Some((devs(&v4), devs(&v6)))
}

#[cfg(all(unix, not(target_os = "linux")))]
pub async fn default_route_ifaces() -> Option<(HashSet<String>, HashSet<String>)> {
    let v4 = command_output("netstat", &["-rn", "-f", "inet"]).await?;
    let v6 = command_output("netstat", &["-rn", "-f", "inet6"])
        .await
        .unwrap_or_default();
    Some((bsd_default_devs(&v4), bsd_default_devs(&v6)))
}

/// Interfaces with a real default route in `netstat -rn` output: a gateway
/// (`G` flag) that is not a link-local clone (`link#N`, as macOS adds for
/// bridges such as `bridge100` without any route out).
#[cfg_attr(any(target_os = "linux", windows), allow(dead_code))]
fn bsd_default_devs(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["default", gateway, flags, netif, ..]
                    if flags.contains('G') && !gateway.starts_with("link#") =>
                {
                    Some(netif.to_string())
                }
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::bsd_default_devs;

    #[test]
    fn macos_link_clone_routes_are_not_defaults() {
        let v4 = "Destination        Gateway            Flags               Netif Expire
default            192.168.2.1        UGScg                 en0
default            link#25            UCSIg           bridge100      !
default            link#27            UCSIg           bridge101      !
default            192.168.0.1        UGScIg               en16
";
        let mut devs: Vec<String> = bsd_default_devs(v4).into_iter().collect();
        devs.sort();
        assert_eq!(devs, vec!["en0", "en16"]);
    }
}
