//! 控制协议：服务层与守护进程之间通过控制 socket 交换的请求、响应和状态。

use serde::{Deserialize, Serialize};

pub use crate::config::{Settings, Update};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Update(Update),
    Reload,
    /// Stops a headless one-shot daemon (`proxy-tui stop`).
    Shutdown,
}

/// How the daemon process was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Lives as long as the process that started it (TUI or `run --no-tui`).
    #[default]
    Oneshot,
    /// Started by systemd / launchd; persistent and starts on boot.
    Service,
}

impl RunMode {
    pub fn label(self) -> &'static str {
        match self {
            RunMode::Service => crate::tr!("service mode", "服务模式"),
            RunMode::Oneshot => crate::tr!("one-shot mode", "一次性模式"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Status(Box<Status>),
    Ok { message: String },
    Error { message: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub version: String,
    pub pid: u32,
    pub mode: RunMode,
    /// Whether `proxy-tui stop` can stop it (headless one-shot).
    pub stoppable: bool,
    pub uptime_secs: u64,
    pub config_path: String,
    pub log_path: String,
    pub settings: Settings,
    pub listening: Vec<String>,
    pub listen_errors: Vec<String>,
    pub tls_ready: bool,
    pub cert_path: String,
    pub auto_policy_routing: bool,
    pub total_up: u64,
    pub total_down: u64,
    pub rate_up: u64,
    pub rate_down: u64,
    pub active_tcp: u64,
    pub active_udp: u64,
    pub total_conns: u64,
    pub history_up: Vec<u64>,
    pub history_down: Vec<u64>,
    pub ifaces: Vec<IfaceStatus>,
    pub conns: Vec<ConnStatus>,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IfaceStatus {
    pub name: String,
    pub index: u32,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub default_v4: bool,
    pub default_v6: bool,
    pub v6_ok: bool,
    pub routing: String,
    pub up: u64,
    pub down: u64,
    pub rate_up: u64,
    pub rate_down: u64,
    pub active: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnStatus {
    pub id: u64,
    pub proto: String,
    pub client: String,
    pub entry: String,
    pub target: String,
    pub egress: String,
    pub local: String,
    pub remote: String,
    pub up: u64,
    pub down: u64,
    pub age_secs: u64,
}
