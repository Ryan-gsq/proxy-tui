//! 命令行输出：文本给人看，`--json` 给 Agent / 脚本解析。

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::json;

use crate::api::{RunMode, Status};
use crate::config::{Settings, Update};
use crate::manage::{self, Applied, Manager, ServiceStatus};
use crate::{fmt_bytes, fmt_duration};

/// What an agent needs to use the proxy.
#[derive(Serialize)]
struct Info {
    ok: bool,
    running: bool,
    mode: Option<RunMode>,
    pid: Option<u32>,
    /// `proxy-tui stop` can stop it.
    stoppable: bool,
    settings: Settings,
    proxy_urls: BTreeMap<&'static str, String>,
    #[serde(skip)]
    host: String,
    /// HTTPS is enabled and its certificate loaded.
    #[serde(skip)]
    https_ready: bool,
    listening: Vec<String>,
    /// Addresses that could not be bound (the proxy still runs on the others).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    listen_errors: Vec<String>,
    data_dir: PathBuf,
    config_path: PathBuf,
    log_path: PathBuf,
}

/// The address a client on this machine should use: loopback when listening on
/// a wildcard address, otherwise the first address actually bound.
fn connect_host(ips: &[IpAddr]) -> String {
    if ips.contains(&IpAddr::V4(Ipv4Addr::UNSPECIFIED)) {
        return "127.0.0.1".into();
    }
    if ips.iter().any(IpAddr::is_unspecified) {
        return "::1".into();
    }
    ips.first()
        .map(IpAddr::to_string)
        .unwrap_or_else(|| "127.0.0.1".into())
}

/// Ready-to-use URLs; HTTPS only when its certificate is usable.
fn urls(settings: &Settings, host: &str, https_ready: bool) -> BTreeMap<&'static str, String> {
    let mut usable = settings.clone();
    usable.protocols.https &= https_ready;
    usable.proxy_urls(host).into_iter().collect()
}

impl Info {
    /// From the running daemon alone, so it works even while config.toml is
    /// being edited by hand.
    fn live(dir: &Path, config_path: &Path, s: &Status) -> Info {
        let ips: Vec<IpAddr> = s
            .listening
            .iter()
            .filter_map(|a| a.parse::<SocketAddr>().ok())
            .map(|a| a.ip())
            .collect();
        let host = connect_host(&ips);
        let https_ready = s.tls_ready;
        Info {
            ok: true,
            running: true,
            mode: Some(s.mode),
            pid: Some(s.pid),
            stoppable: s.stoppable,
            proxy_urls: urls(&s.settings, &host, https_ready),
            host,
            https_ready,
            settings: s.settings.clone(),
            listening: s.listening.clone(),
            listen_errors: s.listen_errors.clone(),
            data_dir: dir.to_path_buf(),
            config_path: config_path.to_path_buf(),
            log_path: PathBuf::from(&s.log_path),
        }
    }

    /// From the config file, when nothing runs.
    fn saved(manager: &Manager) -> Result<Info> {
        let cfg = manager.load_config()?;
        let ips: Vec<IpAddr> = cfg
            .listen
            .addrs()
            .unwrap_or_default()
            .iter()
            .map(SocketAddr::ip)
            .collect();
        let host = connect_host(&ips);
        let settings = cfg.settings();
        Ok(Info {
            ok: true,
            running: false,
            mode: None,
            pid: None,
            stoppable: false,
            proxy_urls: urls(&settings, &host, true),
            host,
            https_ready: true,
            settings,
            listening: Vec::new(),
            listen_errors: Vec::new(),
            data_dir: manager.dir().to_path_buf(),
            config_path: manager.config_path().to_path_buf(),
            log_path: cfg.log_path(manager.config_path()),
        })
    }

    fn collect(manager: &Manager, status: Option<&Status>) -> Result<Info> {
        match status {
            Some(s) => Ok(Info::live(manager.dir(), manager.config_path(), s)),
            None => Info::saved(manager),
        }
    }

    fn print(&self, show_password: bool) {
        let s = &self.settings;
        let auth = if !s.auth_enabled() {
            "无认证".to_string()
        } else if show_password {
            format!("{} / {}", s.username, s.password)
        } else {
            format!("{} / ****（--show-password 显示）", s.username)
        };
        row("端口", format!("{}", s.port));
        if !self.listening.is_empty() {
            row("监听", self.listening.join(", "));
        }
        for err in &self.listen_errors {
            row("监听失败", err.clone());
        }
        row("认证", auth);
        row("协议", s.protocols.list());
        row(
            "出口策略",
            format!("{}（{}）", s.policy.as_str(), s.policy.describe()),
        );
        println!("代理地址");
        let urls = if show_password || !s.auth_enabled() {
            self.proxy_urls.clone()
        } else {
            let masked = Settings {
                password: "PASSWORD".into(),
                ..s.clone()
            };
            urls(&masked, &self.host, self.https_ready)
        };
        for (name, url) in urls {
            println!("  {name:<8}{url}");
        }
        row("数据目录", self.data_dir.display().to_string());
        row("日志", self.log_path.display().to_string());
    }
}

fn row(label: &str, value: String) {
    println!("{}{value}", crate::pad(label, 10));
}

fn mode_text(status: &Status) -> String {
    match status.mode {
        RunMode::Service => "服务模式（常驻后台、开机自启）".into(),
        RunMode::Oneshot if status.stoppable => {
            "一次性模式（后台进程，proxy-tui stop 停止）".into()
        }
        RunMode::Oneshot => "一次性模式（运行在管理面板或前台终端中）".into(),
    }
}

pub fn message(message: &str, json: bool) {
    if json {
        println!("{}", json!({ "ok": true, "message": message }));
    } else {
        println!("{message}");
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn started(manager: &mut Manager, status: &Status, json: bool) -> Result<()> {
    let info = Info::collect(manager, Some(status))?;
    if json {
        return print_json(&info);
    }
    println!(
        "代理已在后台启动 · pid {} · {}",
        status.pid,
        mode_text(status)
    );
    info.print(true);
    println!("停止：proxy-tui stop");
    Ok(())
}

/// Printed once the foreground proxy listens.
pub fn foreground(dir: &Path, config_path: &Path, status: &Status, json: bool) {
    let info = Info::live(dir, config_path, status);
    if json {
        let _ = print_json(&info);
        return;
    }
    println!("以一次性模式在前台运行（Ctrl+C 或 proxy-tui stop 停止，关闭终端也会停止）");
    info.print(true);
}

pub fn stopped(stopped: Option<&Status>, json: bool) {
    let text = match stopped {
        Some(s) => format!("已停止一次性代理（pid {}）", s.pid),
        None => "代理未运行".to_string(),
    };
    if json {
        println!(
            "{}",
            json!({ "ok": true, "stopped": stopped.is_some(), "pid": stopped.map(|s| s.pid), "message": text })
        );
    } else {
        println!("{text}");
    }
}

/// Prints the status; returns whether the proxy runs.
pub fn status(manager: &mut Manager, json: bool, show_password: bool) -> Result<bool> {
    let daemon = manager.running()?;
    let service = manage::service_status();
    let info = Info::collect(manager, daemon.as_ref())?;
    if json {
        let mut value = serde_json::to_value(&info)?;
        value["service"] = serde_json::to_value(&service)?;
        value["daemon"] = serde_json::to_value(&daemon)?;
        print_json(&value)?;
        return Ok(daemon.is_some());
    }
    match &daemon {
        Some(s) => row(
            "状态",
            format!(
                "运行中 · {} · pid {} · 已运行 {}",
                mode_text(s),
                s.pid,
                fmt_duration(s.uptime_secs)
            ),
        ),
        None => row(
            "状态",
            "未运行（启动：proxy-tui run --detach；以下为配置文件中的设置）".into(),
        ),
    }
    row("服务", service_line(&service));
    info.print(show_password);
    if let Some(s) = &daemon {
        row(
            "流量",
            format!(
                "↓ {}/s 共 {} · ↑ {}/s 共 {} · 活动 TCP {} UDP {} · 累计 {}",
                fmt_bytes(s.rate_down),
                fmt_bytes(s.total_down),
                fmt_bytes(s.rate_up),
                fmt_bytes(s.total_up),
                s.active_tcp,
                s.active_udp,
                s.total_conns
            ),
        );
        println!("网络出口");
        for i in &s.ifaces {
            println!(
                "  {:<10} v4 [{}] v6 [{}] v6出口 {} 策略路由 {}",
                i.name,
                i.ipv4.join(" "),
                i.ipv6.join(" "),
                if i.v6_ok { "是" } else { "否" },
                if i.routing.is_empty() {
                    "-"
                } else {
                    &i.routing
                }
            );
        }
    }
    Ok(daemon.is_some())
}

pub fn settings(manager: &mut Manager, json: bool, show_password: bool) -> Result<()> {
    let daemon = manager.running()?;
    let info = Info::collect(manager, daemon.as_ref())?;
    if json {
        return print_json(&info);
    }
    let source = if daemon.is_some() {
        "运行中的代理（实时）"
    } else {
        "配置文件（代理未运行）"
    };
    row("来源", source.into());
    info.print(show_password);
    Ok(())
}

pub fn updated(manager: &mut Manager, applied: Applied, update: &Update, json: bool) -> Result<()> {
    let daemon = manager.running()?;
    let info = Info::collect(manager, daemon.as_ref())?;
    let text = match applied {
        Applied::Live => "设置已生效（运行中的代理已更新并保存）".to_string(),
        Applied::Saved => format!(
            "代理未运行：已写入 {}，下次启动生效",
            manager.config_path().display()
        ),
    };
    if json {
        let mut value = serde_json::to_value(&info)?;
        value["applied"] = json!(if applied == Applied::Live {
            "live"
        } else {
            "saved"
        });
        value["message"] = json!(text);
        return print_json(&value);
    }
    println!("{text}");
    info.print(update.password.is_some());
    Ok(())
}

pub fn conns(manager: &mut Manager, limit: usize, json: bool) -> Result<()> {
    let status = manager.status()?;
    let conns = &status.conns[..status.conns.len().min(limit)];
    if json {
        return print_json(&json!({
            "ok": true,
            "active_tcp": status.active_tcp,
            "active_udp": status.active_udp,
            "total": status.total_conns,
            "conns": conns,
        }));
    }
    println!(
        "活动 TCP {} · UDP {} · 累计 {}",
        status.active_tcp, status.active_udp, status.total_conns
    );
    let pad = crate::pad;
    println!(
        "{} {} {} {} {} {:>10} {:>10} {:>7}",
        pad("协议", 10),
        pad("客户端", 22),
        pad("目标", 32),
        pad("出口", 9),
        pad("出站地址", 22),
        "↑",
        "↓",
        "时长"
    );
    for c in conns {
        println!(
            "{:<10} {:<22} {:<32} {:<9} {:<22} {:>10} {:>10} {:>7}",
            c.proto,
            c.client,
            c.target,
            c.egress,
            c.local,
            fmt_bytes(c.up),
            fmt_bytes(c.down),
            fmt_duration(c.age_secs)
        );
    }
    Ok(())
}

fn service_line(s: &ServiceStatus) -> String {
    if !s.supported {
        return format!("{} 不可用（{}）", s.manager, s.note);
    }
    let mut text = format!("{} {}", s.manager, s.state_label());
    if let Some(pid) = s.pid {
        text.push_str(&format!(" · pid {pid}"));
    }
    if s.installed {
        text.push_str(if s.enabled {
            " · 开机自启"
        } else {
            " · 未设开机自启"
        });
    }
    text
}

pub fn service_status(s: &ServiceStatus, json: bool) {
    if json {
        println!("{}", json!({ "ok": true, "service": s }));
    } else {
        println!("{}", service_line(s));
        if s.installed {
            println!("{}", s.unit_path);
        }
    }
}

/// `--json` prints one object; with `--follow` one `{"line": ...}` object per line.
pub fn logs(manager: &mut Manager, lines: usize, follow: bool, json: bool) -> Result<()> {
    let path = manager.log_path();
    let (tail, mut offset) =
        manage::tail_log(&path, lines).map_err(|e| anyhow!("读取 {} 失败: {e}", path.display()))?;
    let mut out = std::io::stdout().lock();
    let emit = |out: &mut std::io::StdoutLock, line: &str| -> std::io::Result<()> {
        if json {
            writeln!(out, "{}", json!({ "line": line }))
        } else {
            writeln!(out, "{line}")
        }
    };
    if json && !follow {
        writeln!(
            out,
            "{}",
            json!({ "ok": true, "log_path": path, "lines": tail })
        )?;
        return Ok(());
    }
    for line in &tail {
        emit(&mut out, line)?;
    }
    out.flush()?;
    if !follow {
        return Ok(());
    }
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(mut file) = crate::config::safefs::open_shared(&path) else {
            continue;
        };
        let len = file.metadata()?.len();
        if len < offset {
            // Rotated: continue from the start of the new file.
            offset = 0;
        }
        if len == offset {
            continue;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        // Only whole lines; a partial last line is picked up next round.
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
            continue;
        };
        offset += end as u64 + 1;
        for line in String::from_utf8_lossy(&buf[..end]).lines() {
            emit(&mut out, line)?;
        }
        out.flush()?;
    }
}
