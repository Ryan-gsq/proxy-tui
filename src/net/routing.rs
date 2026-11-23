//! Linux source-based policy routing so replies and egress for an address leave
//! via the interface that owns it. macOS uses interface-scoped routes
//! (IP_BOUND_IF) and needs no configuration.
//!
//! Only rules and tables this process created are ever changed or removed:
//! other tools may use the same priorities, and a table that already holds
//! someone else's routes is refused instead of flushed.

use std::collections::HashMap;

#[cfg(not(target_os = "linux"))]
use crate::config::EgressConfig;
#[cfg(not(target_os = "linux"))]
use crate::net::netif::Iface;

pub struct SyncReport {
    pub status: HashMap<String, String>,
    pub events: Vec<String>,
}

#[cfg(target_os = "linux")]
pub use linux::{cleanup, sync};

#[cfg(not(target_os = "linux"))]
pub async fn sync(ifaces: &[Iface], _routes_known: bool, _cfg: &EgressConfig) -> SyncReport {
    let status = ifaces
        .iter()
        .map(|i| (i.name.clone(), "scoped".to_string()))
        .collect();
    SyncReport {
        status,
        events: Vec::new(),
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn cleanup() {}

#[cfg(target_os = "linux")]
mod linux {
    use super::SyncReport;
    use crate::config::EgressConfig;
    use crate::net::netif::Iface;
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::net::IpAddr;
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    /// What this process put into the kernel, so it can undo exactly that.
    #[derive(Default)]
    struct Owned {
        /// (family, table) -> signature of the routes written into it.
        tables: HashMap<(u8, u32), String>,
        rules: HashSet<(u8, Rule)>,
    }

    fn owned() -> &'static Mutex<Owned> {
        static OWNED: OnceLock<Mutex<Owned>> = OnceLock::new();
        OWNED.get_or_init(|| Mutex::new(Owned::default()))
    }

    fn flag(fam: u8) -> &'static str {
        if fam == 4 {
            "-4"
        } else {
            "-6"
        }
    }

    async fn ip(args: &[&str]) -> io::Result<String> {
        let out = tokio::process::Command::new("ip")
            .args(args)
            .output()
            .await?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "ip {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    const SKIP_TYPES: [&str; 9] = [
        "unreachable",
        "prohibit",
        "blackhole",
        "local",
        "broadcast",
        "anycast",
        "multicast",
        "throw",
        "nat",
    ];
    const SKIP_FLAGS: [&str; 6] = [
        "linkdown",
        "dead",
        "offload",
        "trap",
        "rt_offload",
        "rt_trap",
    ];

    fn clean_route(line: &str) -> Option<Vec<String>> {
        if line.starts_with(char::is_whitespace) || line.trim().is_empty() {
            return None;
        }
        let mut toks = line.split_whitespace().peekable();
        if SKIP_TYPES.contains(toks.peek()?) {
            return None;
        }
        let mut out = Vec::new();
        while let Some(tok) = toks.next() {
            if tok == "expires" {
                toks.next();
                continue;
            }
            if SKIP_FLAGS.contains(&tok) {
                continue;
            }
            out.push(tok.to_string());
        }
        Some(out)
    }

    fn signature(routes: &[Vec<String>]) -> String {
        routes
            .iter()
            .map(|r| r.join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn table_routes(fam: u8, table: &str) -> Vec<Vec<String>> {
        // A table that does not exist yet reads as empty (or as an error).
        ip(&[flag(fam), "route", "show", "table", table])
            .await
            .map(|out| out.lines().filter_map(clean_route).collect())
            .unwrap_or_default()
    }

    /// Mirrors the interface's main-table routes into its own table.
    async fn sync_table(
        owned: &mut Owned,
        fam: u8,
        iface: &Iface,
        table: u32,
    ) -> io::Result<Option<usize>> {
        let flag = flag(fam);
        let out = ip(&[flag, "route", "show", "table", "main", "dev", &iface.name]).await?;
        let routes: Vec<Vec<String>> = out.lines().filter_map(clean_route).collect();
        if !routes
            .iter()
            .any(|r| r.first().map(String::as_str) == Some("default"))
        {
            return Err(io::Error::other("no default route"));
        }
        let wanted = signature(&routes);
        let table_s = table.to_string();
        match owned.tables.get(&(fam, table)) {
            Some(sig) if *sig == wanted => return Ok(None),
            Some(_) => {}
            None => {
                let current = table_routes(fam, &table_s).await;
                if !current.is_empty() {
                    // Same content means a leftover of ours (e.g. after a crash).
                    if signature(&current) == wanted {
                        owned.tables.insert((fam, table), wanted);
                        return Ok(None);
                    }
                    return Err(io::Error::other(format!(
                        "路由表 {table} 已有其他路由，未修改；请调整 egress.table_base"
                    )));
                }
            }
        }
        let _ = ip(&[flag, "route", "flush", "table", &table_s]).await;
        // From here on the table is ours, even if some routes fail below.
        owned.tables.insert((fam, table), String::new());
        let mut errors = Vec::new();
        for route in &routes {
            let mut args: Vec<&str> = vec![flag, "route", "replace"];
            args.extend(route.iter().map(String::as_str));
            args.extend(["dev", iface.name.as_str(), "table", table_s.as_str()]);
            if let Err(e) = ip(&args).await {
                errors.push(e.to_string());
            }
        }
        if !errors.is_empty() {
            return Err(io::Error::other(errors.join("; ")));
        }
        owned.tables.insert((fam, table), wanted);
        Ok(Some(routes.len()))
    }

    #[derive(Clone, PartialEq, Eq, Hash, Debug)]
    struct Rule {
        priority: u32,
        from: IpAddr,
        lookup: String,
    }

    async fn list_rules(flag: &str, priorities: &[u32]) -> io::Result<Vec<Rule>> {
        let out = ip(&[flag, "rule", "show"]).await?;
        let mut rules = Vec::new();
        for line in out.lines() {
            let Some((prio, rest)) = line.split_once(':') else {
                continue;
            };
            let Ok(priority) = prio.trim().parse::<u32>() else {
                continue;
            };
            if !priorities.contains(&priority) {
                continue;
            }
            let toks: Vec<&str> = rest.split_whitespace().collect();
            let find = |key: &str| {
                toks.iter()
                    .position(|t| *t == key)
                    .and_then(|i| toks.get(i + 1))
                    .copied()
            };
            let (Some(from), Some(lookup)) = (find("from"), find("lookup")) else {
                continue;
            };
            let Ok(from) = from.split('/').next().unwrap_or(from).parse::<IpAddr>() else {
                continue;
            };
            rules.push(Rule {
                priority,
                from,
                lookup: lookup.to_string(),
            });
        }
        Ok(rules)
    }

    async fn add_rule(flag: &str, rule: &Rule) -> io::Result<()> {
        let from = rule.from.to_string();
        let prio = rule.priority.to_string();
        let mut args = vec![flag, "rule", "add", "from", &from, "lookup", &rule.lookup];
        if rule.lookup == "main" {
            args.extend(["suppress_prefixlength", "0"]);
        }
        args.extend(["priority", &prio]);
        ip(&args).await.map(|_| ())
    }

    async fn del_rule(flag: &str, rule: &Rule) -> io::Result<()> {
        let from = rule.from.to_string();
        let prio = rule.priority.to_string();
        ip(&[
            flag,
            "rule",
            "del",
            "priority",
            &prio,
            "from",
            &from,
            "lookup",
            &rule.lookup,
        ])
        .await
        .map(|_| ())
    }

    pub async fn sync(ifaces: &[Iface], routes_known: bool, cfg: &EgressConfig) -> SyncReport {
        let mut owned = owned().lock().await;
        let mut status: HashMap<String, Vec<String>> = HashMap::new();
        let mut events = Vec::new();
        let prio = cfg.rule_priority;
        let mut used_tables: HashSet<(u8, u32)> = HashSet::new();
        let mut desired: HashSet<(u8, Rule)> = HashSet::new();
        for fam in [4u8, 6] {
            let addrs_of = |i: &Iface| -> Vec<IpAddr> {
                if fam == 4 {
                    i.v4.iter().copied().map(IpAddr::V4).collect()
                } else {
                    i.v6.iter().copied().map(IpAddr::V6).collect()
                }
            };
            let has_default = |i: &Iface| if fam == 4 { i.default_v4 } else { i.default_v6 };
            let egress: Vec<&Iface> = ifaces
                .iter()
                .filter(|i| routes_known && has_default(i) && !addrs_of(i).is_empty())
                .collect();
            if egress.len() < 2 {
                continue;
            }
            for iface in egress {
                let table = cfg.table_base + iface.index;
                used_tables.insert((fam, table));
                match sync_table(&mut owned, fam, iface, table).await {
                    Ok(Some(n)) => events.push(format!(
                        "policy routing: IPv{fam} table {table} <- {n} routes of {}",
                        iface.name
                    )),
                    Ok(None) => {}
                    Err(e) => {
                        status
                            .entry(iface.name.clone())
                            .or_default()
                            .push(format!("v{fam}: error"));
                        events.push(format!("policy routing {} IPv{fam}: {e}", iface.name));
                        continue;
                    }
                }
                for addr in addrs_of(iface) {
                    desired.insert((
                        fam,
                        Rule {
                            priority: prio - 1,
                            from: addr,
                            lookup: "main".into(),
                        },
                    ));
                    desired.insert((
                        fam,
                        Rule {
                            priority: prio,
                            from: addr,
                            lookup: table.to_string(),
                        },
                    ));
                }
                status
                    .entry(iface.name.clone())
                    .or_default()
                    .push(format!("v{fam}:t{table}"));
            }
        }

        // Drop only our own rules that are no longer wanted.
        let stale: Vec<(u8, Rule)> = owned.rules.difference(&desired).cloned().collect();
        for (fam, rule) in stale {
            match del_rule(flag(fam), &rule).await {
                Ok(()) => events.push(format!(
                    "policy routing: removed rule from {} lookup {}",
                    rule.from, rule.lookup
                )),
                Err(e) => tracing::debug!("{e}"),
            }
            owned.rules.remove(&(fam, rule));
        }
        let mut added = 0;
        for fam in [4u8, 6] {
            let existing: HashSet<Rule> = match list_rules(flag(fam), &[prio - 1, prio]).await {
                Ok(rules) => rules.into_iter().collect(),
                Err(e) => {
                    events.push(format!("policy routing IPv{fam}: {e}"));
                    continue;
                }
            };
            for (_, rule) in desired.iter().filter(|(f, _)| *f == fam) {
                // An identical rule already there is adopted (ours from a previous run).
                if existing.contains(rule) || add_rule(flag(fam), rule).await.is_ok() {
                    if !existing.contains(rule) {
                        added += 1;
                    }
                    owned.rules.insert((fam, rule.clone()));
                } else {
                    events.push(format!(
                        "policy routing: cannot add rule from {} lookup {}",
                        rule.from, rule.lookup
                    ));
                }
            }
        }
        if added > 0 {
            events.push(format!("policy routing: added {added} rules"));
        }

        // Tables we filled that no interface uses any more.
        let unused: Vec<(u8, u32)> = owned
            .tables
            .keys()
            .filter(|k| !used_tables.contains(k))
            .copied()
            .collect();
        for (fam, table) in unused {
            let _ = ip(&[flag(fam), "route", "flush", "table", &table.to_string()]).await;
            owned.tables.remove(&(fam, table));
        }

        let status = ifaces
            .iter()
            .map(|i| {
                let text = match status.get(&i.name) {
                    Some(parts) => parts.join(" "),
                    None => "main".to_string(),
                };
                (i.name.clone(), text)
            })
            .collect();
        SyncReport { status, events }
    }

    /// Removes every rule and table this process created.
    pub async fn cleanup() {
        let mut owned = owned().lock().await;
        for (fam, rule) in std::mem::take(&mut owned.rules) {
            let _ = del_rule(flag(fam), &rule).await;
        }
        for (fam, table) in std::mem::take(&mut owned.tables).into_keys() {
            let _ = ip(&[flag(fam), "route", "flush", "table", &table.to_string()]).await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::clean_route;

        #[test]
        fn clean_route_filters_flags() {
            let r =
                clean_route("default via fe80::1 proto ra metric 1024 expires 1798sec pref medium")
                    .unwrap();
            assert_eq!(
                r.join(" "),
                "default via fe80::1 proto ra metric 1024 pref medium"
            );
            assert!(clean_route("multicast ff00::/8 proto kernel metric 256").is_none());
            assert!(clean_route("\tnexthop via 1.2.3.4 weight 1").is_none());
            let r =
                clean_route("10.0.0.0/24 proto kernel scope link src 10.0.0.5 linkdown").unwrap();
            assert_eq!(
                r.join(" "),
                "10.0.0.0/24 proto kernel scope link src 10.0.0.5"
            );
        }
    }
}
