//! 守护进程：持有运行时状态（配置、统计、网卡、监听），处理控制请求并热更新。

pub mod control;
pub mod logging;
pub mod stats;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::api::{IfaceStatus, Request, Response, RunMode, Status, Update};
use crate::config::{self, Config};
use crate::net::dns::Resolver;
use crate::net::netif::NetState;
use crate::net::routing;
use crate::proxy::pool::Pool;
use crate::proxy::tls;
use crate::proxy::{bind_listeners, bind_socket, Listener, Listeners};
use stats::{Stats, DEFAULT_IFACE};

const NET_REFRESH: Duration = Duration::from_secs(10);
const ROUTING_REFRESH: Duration = Duration::from_secs(60);
const MAX_CONNS_REPORTED: usize = 500;

/// Set by the systemd unit / launchd plist so the daemon knows it runs as a service.
pub const SERVICE_ENV: &str = "PROXY_TUI_SERVICE";

pub struct Ctx {
    config: RwLock<Arc<Config>>,
    pub config_path: PathBuf,
    pub stats: Arc<Stats>,
    pub net: Arc<NetState>,
    resolver: RwLock<Arc<Resolver>>,
    tls: RwLock<Option<TlsAcceptor>>,
    started: Instant,
    mode: RunMode,
    /// Whether `Request::Shutdown` (`proxy-tui stop`) may stop this daemon.
    stoppable: bool,
    stop: tokio::sync::Notify,
    /// Serialises every config change (read-modify-write, save, publish).
    update_lock: tokio::sync::Mutex<()>,
    listeners: tokio::sync::Mutex<Listeners>,
    /// Idle origin connections of plain HTTP forwarding.
    pub pool: Pool,
}

impl Ctx {
    pub fn config(&self) -> Arc<Config> {
        self.config.read().unwrap().clone()
    }

    pub fn resolver(&self) -> Arc<Resolver> {
        self.resolver.read().unwrap().clone()
    }

    pub fn tls_acceptor(&self) -> Option<TlsAcceptor> {
        self.tls.read().unwrap().clone()
    }

    fn init_tls(&self, cfg: &Config) {
        let cert = cfg.cert_path(&self.config_path);
        let key = cfg.key_path(&self.config_path);
        let mut sans = vec!["localhost".to_string()];
        if let Ok(host) = nix::unistd::gethostname() {
            if let Some(host) = host.to_str() {
                if !host.is_empty() && host != "localhost" {
                    sans.push(host.to_string());
                }
            }
        }
        sans.push("127.0.0.1".into());
        sans.push("::1".into());
        for ip in self.net.snapshot().all_ips() {
            sans.push(ip.to_string());
        }
        let acceptor = match tls::ensure_cert(&cert, &key, sans) {
            Ok(generated) => {
                if generated {
                    self.stats.event(format!(
                        "generated self-signed certificate {}",
                        cert.display()
                    ));
                }
                match tls::load_acceptor(&cert, &key) {
                    Ok(acceptor) => Some(acceptor),
                    Err(e) => {
                        self.stats.event(format!("TLS disabled: {e:#}"));
                        None
                    }
                }
            }
            Err(e) => {
                self.stats.event(format!("TLS disabled: {e:#}"));
                None
            }
        };
        *self.tls.write().unwrap() = acceptor;
    }

    pub async fn handle(self: &Arc<Self>, req: Request) -> Response {
        let result = match req {
            Request::Status => return Response::Status(Box::new(self.status().await)),
            Request::Update(update) => self.apply_update(update).await,
            Request::Reload => self.reload().await,
            Request::Shutdown => self.request_stop(),
        };
        match result {
            Ok(message) => Response::Ok { message },
            Err(e) => Response::Error {
                message: format!("{e:#}"),
            },
        }
    }

    async fn apply_update(self: &Arc<Self>, update: Update) -> Result<String> {
        let _guard = self.update_lock.lock().await;
        let mut new = (*self.config()).clone();
        update.apply(&mut new)?;
        self.apply_config(new, true).await?;
        Ok("设置已保存并生效".into())
    }

    fn request_stop(&self) -> Result<String> {
        if self.mode == RunMode::Service {
            bail!("服务模式请用 sudo proxy-tui service stop 停止");
        }
        if !self.stoppable {
            bail!("该代理运行在管理面板中，请在面板里停止或退出面板");
        }
        self.stats.event("stop requested via control socket");
        self.stop.notify_one();
        Ok("正在停止".into())
    }

    async fn reload(self: &Arc<Self>) -> Result<String> {
        let _guard = self.update_lock.lock().await;
        let new = Config::load(&self.config_path)?;
        self.apply_config(new, false).await?;
        Ok(format!("已重新读取 {}", self.config_path.display()))
    }

    /// Applies a new config; the caller holds `update_lock`. Either everything
    /// takes effect (listeners, file, in-memory config) or nothing does.
    async fn apply_config(self: &Arc<Self>, new: Config, persist: bool) -> Result<()> {
        new.validate()?;
        new.validate_paths(&self.config_path)?;
        let old = self.config();
        let mut listeners = self.listeners.lock().await;
        let wanted = new.listen.addrs()?;
        // Bind added addresses before touching the current ones, so a failure
        // leaves the proxy exactly as it was.
        let mut staged = Vec::new();
        let mut closed: Vec<SocketAddr> = Vec::new();
        let mut errors = Vec::new();
        if new.listen != old.listen {
            let mut in_use = Vec::new();
            for addr in wanted.iter().filter(|a| !listeners.has(a)) {
                match bind_socket(*addr) {
                    Ok(socket) => staged.push((*addr, socket)),
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => in_use.push(*addr),
                    Err(e) => errors.push(format!("{addr}: {e}")),
                }
            }
            let mut taken = false;
            if !in_use.is_empty() {
                // Likely held by a listener that is going away (e.g. a wildcard
                // address replaced by a specific one on the same port).
                for listener in listeners.take_except(&wanted) {
                    closed.push(listener.addr);
                    listener.close().await;
                }
                for addr in in_use {
                    match bind_socket(addr) {
                        Ok(socket) => staged.push((addr, socket)),
                        Err(e) => {
                            taken |= e.kind() == std::io::ErrorKind::AddrInUse;
                            errors.push(format!("{addr}: {e}"));
                        }
                    }
                }
            }
            let kept = wanted.iter().filter(|a| listeners.has(a)).count();
            // A port taken by someone else is a mistake in the requested
            // change: refuse it as a whole. (Other failures, such as no IPv6 on
            // this host, are tolerated as at startup.)
            if taken || (staged.is_empty() && kept == 0) {
                drop(staged);
                self.restore_listeners(&mut listeners, &closed);
                bail!("无法监听：{}", errors.join("; "));
            }
        }
        if persist {
            if let Err(e) = new.save(&self.config_path) {
                drop(staged);
                self.restore_listeners(&mut listeners, &closed);
                return Err(e);
            }
        }
        *self.config.write().unwrap() = Arc::new(new.clone());
        if new.listen != old.listen {
            for listener in listeners.take_except(&wanted) {
                listener.close().await;
            }
            for (addr, socket) in staged {
                listeners.list.push(Listener::start(self, addr, socket));
            }
            listeners.errors = errors;
            self.stats
                .event(format!("listening on {}", join_addrs(&listeners.bound())));
            for err in &listeners.errors {
                self.stats.event(format!("listen failed: {err}"));
            }
        }
        drop(listeners);
        if old.log != new.log {
            self.stats.event("日志设置在代理重启后生效");
        }
        *self.resolver.write().unwrap() = Arc::new(Resolver::new(&new.dns));
        let tls_changed = new.tls.cert != old.tls.cert || new.tls.key != old.tls.key;
        let policy_routing_off = old.egress.auto_policy_routing && !new.egress.auto_policy_routing;
        let routing_changed = old.egress.auto_policy_routing != new.egress.auto_policy_routing
            || old.egress.table_base != new.egress.table_base
            || old.egress.rule_priority != new.egress.rule_priority;
        let changes = describe_changes(&old, &new);
        if tls_changed || (new.protocols.https && self.tls_acceptor().is_none()) {
            self.init_tls(&new);
        }
        if policy_routing_off {
            routing::cleanup().await;
        } else if routing_changed {
            // Re-sync now instead of at the next periodic run.
            self.routing_step().await;
        }
        if !changes.is_empty() {
            self.stats.event(format!("config: {}", changes.join(", ")));
        }
        Ok(())
    }

    /// Re-binds listeners closed during a failed update (their ports were just freed).
    fn restore_listeners(self: &Arc<Self>, listeners: &mut Listeners, addrs: &[SocketAddr]) {
        for addr in addrs {
            match bind_socket(*addr) {
                Ok(socket) => listeners.list.push(Listener::start(self, *addr, socket)),
                Err(e) => self.stats.event(format!("恢复监听 {addr} 失败: {e}")),
            }
        }
    }

    pub async fn status(&self) -> Status {
        let cfg = self.config();
        let totals = self.stats.totals();
        let (conns, active_tcp, active_udp) = self.stats.connections(MAX_CONNS_REPORTED);
        let snap = self.net.snapshot();
        let (listening, listen_errors) = {
            let l = self.listeners.lock().await;
            (
                l.bound().iter().map(|a| a.to_string()).collect(),
                l.errors.clone(),
            )
        };
        let mut ifaces: Vec<IfaceStatus> = snap
            .ifaces
            .iter()
            .map(|i| {
                let t = totals.per_iface.get(&i.name);
                IfaceStatus {
                    name: i.name.clone(),
                    index: i.index,
                    ipv4: i.v4.iter().map(|a| a.to_string()).collect(),
                    ipv6: i.v6.iter().map(|a| a.to_string()).collect(),
                    default_v4: i.default_v4,
                    default_v6: i.default_v6,
                    v6_ok: i.v6_ok,
                    routing: self.net.routing_status(&i.name),
                    up: t.map_or(0, |t| t.up),
                    down: t.map_or(0, |t| t.down),
                    rate_up: t.map_or(0, |t| t.rate_up),
                    rate_down: t.map_or(0, |t| t.rate_down),
                    active: t.map_or(0, |t| t.active),
                }
            })
            .collect();
        for (name, t) in &totals.per_iface {
            if ifaces.iter().any(|i| &i.name == name) {
                continue;
            }
            ifaces.push(IfaceStatus {
                name: name.clone(),
                routing: if name == DEFAULT_IFACE {
                    "os routing".into()
                } else {
                    "gone".into()
                },
                up: t.up,
                down: t.down,
                rate_up: t.rate_up,
                rate_down: t.rate_down,
                active: t.active,
                ..Default::default()
            });
        }
        Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            mode: self.mode,
            stoppable: self.stoppable,
            uptime_secs: self.started.elapsed().as_secs(),
            config_path: self.config_path.display().to_string(),
            log_path: logging::active_path()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| cfg.log_path(&self.config_path))
                .display()
                .to_string(),
            settings: cfg.settings(),
            listening,
            listen_errors,
            tls_ready: self.tls_acceptor().is_some(),
            cert_path: cfg.cert_path(&self.config_path).display().to_string(),
            auto_policy_routing: cfg.egress.auto_policy_routing,
            total_up: totals.up,
            total_down: totals.down,
            rate_up: totals.rate_up,
            rate_down: totals.rate_down,
            active_tcp,
            active_udp,
            total_conns: totals.total_conns,
            history_up: totals.history_up,
            history_down: totals.history_down,
            ifaces,
            conns,
            events: self.stats.events(),
        }
    }

    async fn routing_step(&self) {
        let cfg = self.config();
        let snap = self.net.snapshot();
        if cfg!(target_os = "linux") {
            if !cfg.egress.auto_policy_routing {
                self.net.set_routing_status(
                    snap.ifaces
                        .iter()
                        .map(|i| (i.name.clone(), "off".to_string()))
                        .collect(),
                );
                return;
            }
            if !config::is_root() {
                self.net.set_routing_status(
                    snap.ifaces
                        .iter()
                        .map(|i| (i.name.clone(), "needs root".to_string()))
                        .collect(),
                );
                return;
            }
        }
        let report = routing::sync(&snap.ifaces, snap.routes_known, &cfg.egress).await;
        for event in report.events {
            self.stats.event(event);
        }
        self.net.set_routing_status(report.status);
    }
}

fn join_addrs(addrs: &[SocketAddr]) -> String {
    addrs
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn describe_changes(old: &Config, new: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if old.listen.port != new.listen.port {
        out.push(format!("port {} -> {}", old.listen.port, new.listen.port));
    }
    if old.auth != new.auth {
        out.push("credentials changed".into());
    }
    if old.protocols != new.protocols {
        out.push(format!("protocols {}", new.protocols.list()));
    }
    if old.egress.policy != new.egress.policy {
        out.push(format!("policy {}", new.egress.policy.as_str()));
    }
    if old.dns != new.dns {
        out.push("dns changed".into());
    }
    out
}

pub struct Daemon {
    pub ctx: Arc<Ctx>,
    pub control_path: PathBuf,
    tasks: Vec<JoinHandle<()>>,
}

impl Daemon {
    pub async fn listening(&self) -> Vec<SocketAddr> {
        self.ctx.listeners.lock().await.bound()
    }

    pub async fn shutdown(self) {
        for task in &self.tasks {
            task.abort();
        }
        self.ctx.listeners.lock().await.close_all().await;
        routing::cleanup().await;
        control::remove_socket(&self.control_path);
    }
}

/// Starts the daemon. `stoppable` lets `proxy-tui stop` end it (headless
/// one-shot); services and daemons embedded in the TUI refuse.
pub async fn start(
    config_path: PathBuf,
    control_path: PathBuf,
    mode: RunMode,
    stoppable: bool,
) -> Result<Daemon> {
    raise_nofile_limit();
    let (cfg, created) = Config::load_or_create(&config_path)?;
    let stats = Stats::new();
    if created {
        // The generated password is shown by status / TUI, never logged.
        stats.event(format!("created {}", config_path.display()));
    }
    let net = NetState::new();
    net.refresh().await;
    let ctx = Arc::new(Ctx {
        resolver: RwLock::new(Arc::new(Resolver::new(&cfg.dns))),
        config: RwLock::new(Arc::new(cfg.clone())),
        config_path: config_path.clone(),
        stats: stats.clone(),
        net,
        tls: RwLock::new(None),
        started: Instant::now(),
        mode,
        stoppable,
        stop: tokio::sync::Notify::new(),
        update_lock: tokio::sync::Mutex::new(()),
        listeners: tokio::sync::Mutex::new(Listeners::default()),
        pool: Pool::default(),
    });
    ctx.init_tls(&cfg);
    let resolver = ctx.resolver();
    if resolver.servers().is_empty() {
        stats.event("dns: no usable nameserver in config/resolv.conf, using system resolver");
    } else {
        let list: Vec<String> = resolver.servers().iter().map(|s| s.to_string()).collect();
        stats.event(format!("dns: per-egress queries via {}", list.join(", ")));
    }

    let control = control::serve(control_path.clone(), ctx.clone())
        .await
        .context("starting control socket")?;

    let listeners = bind_listeners(&ctx, &cfg.listen.addrs()?);
    if listeners.list.is_empty() {
        control.abort();
        control::remove_socket(&control_path);
        bail!("无法监听：{}", listeners.errors.join("; "));
    }
    stats.event(format!("listening on {}", join_addrs(&listeners.bound())));
    for err in &listeners.errors {
        stats.event(format!("listen failed: {err}"));
    }
    *ctx.listeners.lock().await = listeners;

    let mut tasks = vec![control];
    let sampler = ctx.clone();
    tasks.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            sampler.stats.sample();
            sampler.pool.sweep();
        }
    }));
    tasks.push(tokio::spawn(net_monitor(ctx.clone())));

    Ok(Daemon {
        ctx,
        control_path,
        tasks,
    })
}

async fn net_monitor(ctx: Arc<Ctx>) {
    let mut last_probe: Option<Instant> = None;
    let mut last_routing: Option<Instant> = None;
    let mut first = true;
    loop {
        let changed = ctx.net.refresh().await;
        if changed && !first {
            ctx.stats.event("network interfaces changed");
        }
        let cfg = ctx.config();
        let probe_every = Duration::from_secs(cfg.egress.probe_interval_secs.max(10));
        if first || changed || last_probe.is_none_or(|t| t.elapsed() >= probe_every) {
            for (name, ok) in ctx.net.probe_v6().await {
                ctx.stats.event(format!(
                    "{name}: IPv6 egress {}",
                    if ok { "available" } else { "unavailable" }
                ));
            }
            last_probe = Some(Instant::now());
        }
        if first || changed || last_routing.is_none_or(|t| t.elapsed() >= ROUTING_REFRESH) {
            ctx.routing_step().await;
            last_routing = Some(Instant::now());
        }
        first = false;
        tokio::time::sleep(NET_REFRESH).await;
    }
}

/// Runs the daemon in this process until Ctrl+C, SIGTERM, SIGHUP or (one-shot
/// only) `proxy-tui stop`. `on_ready` gets the status once it is listening.
pub async fn run(config_path: PathBuf, on_ready: impl FnOnce(&Status)) -> Result<()> {
    let control_path = config::control_socket_for(&config_path);
    let mode = if std::env::var_os(SERVICE_ENV).is_some() {
        RunMode::Service
    } else {
        RunMode::Oneshot
    };
    let log_cfg = Config::load(&config_path).unwrap_or_default();
    let tag = if mode == RunMode::Service {
        "service"
    } else {
        "oneshot"
    };
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stderr());
    logging::init(&log_cfg, &config_path, tag, interactive);
    // Service managers restart a stopped service, so only one-shot honours `stop`.
    let started = start(config_path, control_path, mode, mode == RunMode::Oneshot).await;
    let daemon = match started {
        Ok(daemon) => daemon,
        Err(e) => {
            tracing::error!("{e:#}");
            logging::flush();
            return Err(e);
        }
    };
    tracing::info!("control socket {}", daemon.control_path.display());
    on_ready(&daemon.ctx.status().await);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = hup.recv() => {}
        _ = daemon.ctx.stop.notified() => {}
    }
    tracing::info!("shutting down");
    daemon.shutdown().await;
    logging::flush();
    Ok(())
}

fn raise_nofile_limit() {
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        for target in [lim.rlim_max, 1_048_576, 262_144, 65_536, 10_240] {
            let target = target.min(lim.rlim_max);
            if target <= lim.rlim_cur {
                continue;
            }
            let new = libc::rlimit {
                rlim_cur: target,
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                return;
            }
        }
    }
}
