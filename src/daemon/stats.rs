use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::api::ConnStatus;

const HISTORY_LEN: usize = 120;
const EVENT_LEN: usize = 200;
pub const DEFAULT_IFACE: &str = "default";

#[derive(Default)]
pub struct Traffic {
    pub up: AtomicU64,
    pub down: AtomicU64,
}

impl Traffic {
    #[inline]
    pub fn add_up(&self, n: u64) {
        self.up.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_down(&self, n: u64) {
        self.down.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> (u64, u64) {
        (
            self.up.load(Ordering::Relaxed),
            self.down.load(Ordering::Relaxed),
        )
    }
}

#[derive(Default)]
pub struct IfaceCounters {
    pub traffic: Traffic,
    pub active: AtomicU64,
}

pub struct ConnMeta {
    pub proto: String,
    pub client: SocketAddr,
    pub entry: SocketAddr,
    pub target: String,
    pub iface: String,
    pub local: Option<SocketAddr>,
    pub remote: Option<SocketAddr>,
    pub udp: bool,
}

struct ConnRecord {
    meta: ConnMeta,
    traffic: Arc<Traffic>,
    started: Instant,
}

#[derive(Default)]
struct Rates {
    last: HashMap<String, (u64, u64)>,
    rates: HashMap<String, (u64, u64)>,
    last_total: (u64, u64),
    total: (u64, u64),
    history_up: VecDeque<u64>,
    history_down: VecDeque<u64>,
    last_sample: Option<Instant>,
}

#[derive(Default)]
pub struct Stats {
    next_id: AtomicU64,
    total_conns: AtomicU64,
    conns: Mutex<HashMap<u64, ConnRecord>>,
    ifaces: Mutex<HashMap<String, Arc<IfaceCounters>>>,
    rates: Mutex<Rates>,
    events: Mutex<VecDeque<String>>,
}

pub struct ConnGuard {
    stats: Arc<Stats>,
    id: u64,
    iface: Arc<IfaceCounters>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.iface.active.fetch_sub(1, Ordering::Relaxed);
        let record = self.stats.conns.lock().unwrap().remove(&self.id);
        if let Some(rec) = record {
            let (up, down) = rec.traffic.get();
            let m = &rec.meta;
            let local = m.local.map(|a| format!(" {a}")).unwrap_or_default();
            tracing::info!(
                "close {} {} -> {} via {}{} up {} down {} {:.1}s",
                m.proto,
                m.client,
                m.target,
                m.iface,
                local,
                crate::fmt_bytes(up),
                crate::fmt_bytes(down),
                rec.started.elapsed().as_secs_f64()
            );
        }
    }
}

pub struct Registered {
    pub guard: ConnGuard,
    pub conn: Arc<Traffic>,
    pub iface: Arc<IfaceCounters>,
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn iface(&self, name: &str) -> Arc<IfaceCounters> {
        let mut map = self.ifaces.lock().unwrap();
        if let Some(c) = map.get(name) {
            return c.clone();
        }
        let c = Arc::new(IfaceCounters::default());
        map.insert(name.to_string(), c.clone());
        c
    }

    pub fn register(self: &Arc<Self>, meta: ConnMeta) -> Registered {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.total_conns.fetch_add(1, Ordering::Relaxed);
        let iface = self.iface(&meta.iface);
        iface.active.fetch_add(1, Ordering::Relaxed);
        let conn = Arc::new(Traffic::default());
        self.conns.lock().unwrap().insert(
            id,
            ConnRecord {
                meta,
                traffic: conn.clone(),
                started: Instant::now(),
            },
        );
        Registered {
            guard: ConnGuard {
                stats: self.clone(),
                id,
                iface: iface.clone(),
            },
            conn,
            iface,
        }
    }

    pub fn event(&self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!("{msg}");
        let line = format!("{} {}", local_time(), msg);
        let mut ev = self.events.lock().unwrap();
        if ev.len() >= EVENT_LEN {
            ev.pop_front();
        }
        ev.push_back(line);
    }

    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().iter().cloned().collect()
    }

    pub fn sample(&self) {
        let now = Instant::now();
        let current: Vec<(String, (u64, u64))> = self
            .ifaces
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.traffic.get()))
            .collect();
        let mut r = self.rates.lock().unwrap();
        let elapsed = r
            .last_sample
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(1.0)
            .max(0.001);
        r.last_sample = Some(now);
        let mut total = (0u64, 0u64);
        let mut rates = HashMap::new();
        for (name, (up, down)) in &current {
            total.0 += up;
            total.1 += down;
            let (lu, ld) = r.last.get(name).copied().unwrap_or((*up, *down));
            rates.insert(
                name.clone(),
                (
                    (up.saturating_sub(lu) as f64 / elapsed) as u64,
                    (down.saturating_sub(ld) as f64 / elapsed) as u64,
                ),
            );
        }
        let rate_total = (
            (total.0.saturating_sub(r.last_total.0) as f64 / elapsed) as u64,
            (total.1.saturating_sub(r.last_total.1) as f64 / elapsed) as u64,
        );
        r.last = current.into_iter().collect();
        r.rates = rates;
        r.last_total = total;
        r.total = rate_total;
        if r.history_up.len() >= HISTORY_LEN {
            r.history_up.pop_front();
            r.history_down.pop_front();
        }
        r.history_up.push_back(rate_total.0);
        r.history_down.push_back(rate_total.1);
    }

    pub fn totals(&self) -> TotalsSnapshot {
        let r = self.rates.lock().unwrap();
        let ifaces = self.ifaces.lock().unwrap();
        let mut per_iface = HashMap::new();
        let mut up = 0;
        let mut down = 0;
        for (name, c) in ifaces.iter() {
            let (u, d) = c.traffic.get();
            up += u;
            down += d;
            let (ru, rd) = r.rates.get(name).copied().unwrap_or((0, 0));
            per_iface.insert(
                name.clone(),
                IfaceTotals {
                    up: u,
                    down: d,
                    rate_up: ru,
                    rate_down: rd,
                    active: c.active.load(Ordering::Relaxed),
                },
            );
        }
        TotalsSnapshot {
            up,
            down,
            rate_up: r.total.0,
            rate_down: r.total.1,
            history_up: r.history_up.iter().copied().collect(),
            history_down: r.history_down.iter().copied().collect(),
            total_conns: self.total_conns.load(Ordering::Relaxed),
            per_iface,
        }
    }

    /// The `limit` busiest connections plus active TCP / UDP counts. Only the
    /// selected entries are formatted, keeping the shared lock short.
    pub fn connections(&self, limit: usize) -> (Vec<ConnStatus>, u64, u64) {
        let conns = self.conns.lock().unwrap();
        let udp = conns.values().filter(|r| r.meta.udp).count() as u64;
        let tcp = conns.len() as u64 - udp;
        // (bytes, id): busiest first, newest first on ties.
        let mut keys: Vec<(u64, u64)> = conns
            .iter()
            .map(|(id, rec)| {
                let (up, down) = rec.traffic.get();
                (up + down, *id)
            })
            .collect();
        let busiest = |a: &(u64, u64), b: &(u64, u64)| b.cmp(a);
        if keys.len() > limit && limit > 0 {
            keys.select_nth_unstable_by(limit - 1, busiest);
        }
        keys.truncate(limit);
        keys.sort_unstable_by(busiest);
        let list = keys
            .iter()
            .filter_map(|(_, id)| {
                let rec = conns.get(id)?;
                let (up, down) = rec.traffic.get();
                Some(ConnStatus {
                    id: *id,
                    proto: rec.meta.proto.clone(),
                    client: rec.meta.client.to_string(),
                    entry: rec.meta.entry.ip().to_string(),
                    target: rec.meta.target.clone(),
                    egress: rec.meta.iface.clone(),
                    local: rec.meta.local.map(|a| a.to_string()).unwrap_or_default(),
                    remote: rec.meta.remote.map(|a| a.to_string()).unwrap_or_default(),
                    up,
                    down,
                    age_secs: rec.started.elapsed().as_secs(),
                })
            })
            .collect();
        (list, tcp, udp)
    }
}

pub struct IfaceTotals {
    pub up: u64,
    pub down: u64,
    pub rate_up: u64,
    pub rate_down: u64,
    pub active: u64,
}

pub struct TotalsSnapshot {
    pub up: u64,
    pub down: u64,
    pub rate_up: u64,
    pub rate_down: u64,
    pub history_up: Vec<u64>,
    pub history_down: Vec<u64>,
    pub total_conns: u64,
    pub per_iface: HashMap<String, IfaceTotals>,
}

fn local_time() -> String {
    let t = super::clock::now();
    format!(
        "{:02}-{:02} {:02}:{:02}:{:02}",
        t.month, t.day, t.hour, t.minute, t.second
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_returns_the_busiest_first() {
        let stats = Stats::new();
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let regs: Vec<Registered> = (0..10)
            .map(|i| {
                let reg = stats.register(ConnMeta {
                    proto: "http".into(),
                    client: addr,
                    entry: addr,
                    target: format!("t{i}"),
                    iface: "eth0".into(),
                    local: None,
                    remote: None,
                    udp: i == 9,
                });
                reg.conn.add_up(i * 100);
                reg
            })
            .collect();
        let (list, tcp, udp) = stats.connections(3);
        assert_eq!((tcp, udp), (9, 1));
        let targets: Vec<&str> = list.iter().map(|c| c.target.as_str()).collect();
        assert_eq!(targets, vec!["t9", "t8", "t7"]);
        drop(regs);
        assert_eq!(stats.connections(3).0.len(), 0);
    }
}
