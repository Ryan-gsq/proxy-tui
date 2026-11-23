use anyhow::{bail, Context, Result};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use crate::i18n::Language;
use crate::tr;

mod paths;
pub mod safefs;
pub use paths::*;
pub use safefs::SafeDir;

/// Largest config file accepted.
const MAX_CONFIG_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EgressPolicy {
    #[default]
    SameIface,
    Strict,
    PreferV4,
    PreferV6,
}

impl EgressPolicy {
    pub const ALL: [EgressPolicy; 4] = [
        EgressPolicy::SameIface,
        EgressPolicy::Strict,
        EgressPolicy::PreferV4,
        EgressPolicy::PreferV6,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EgressPolicy::SameIface => "same_iface",
            EgressPolicy::Strict => "strict",
            EgressPolicy::PreferV4 => "prefer_v4",
            EgressPolicy::PreferV6 => "prefer_v6",
        }
    }

    pub fn parse(text: &str) -> Result<EgressPolicy> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str() == text.trim())
            .with_context(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|p| p.as_str()).collect();
                let names = names.join(", ");
                tr!(
                    format!("unknown egress policy {text:?} (one of {names})"),
                    format!("未知出口策略 {text:?}（可选 {names}）")
                )
            })
    }

    pub fn describe(self) -> &'static str {
        match self {
            EgressPolicy::SameIface => tr!(
                "same NIC, same IP family first, may fall back",
                "同网卡，优先同协议，可回退"
            ),
            EgressPolicy::Strict => tr!(
                "same NIC and IP family, no fallback",
                "同网卡同协议，不回退"
            ),
            EgressPolicy::PreferV4 => tr!("same NIC, IPv4 first", "同网卡，优先 IPv4"),
            EgressPolicy::PreferV6 => tr!("same NIC, IPv6 first", "同网卡，优先 IPv6"),
        }
    }

    pub fn next(self) -> Self {
        let pos = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(pos + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let pos = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(pos + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub listen: ListenConfig,
    pub auth: AuthConfig,
    pub protocols: Protocols,
    pub tls: TlsConfig,
    pub egress: EgressConfig,
    pub dns: DnsConfig,
    pub performance: PerfConfig,
    pub log: LogConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Management panel language: auto (follow the system), en or zh.
    pub language: Language,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ListenConfig {
    pub port: u16,
    pub ipv4: String,
    pub ipv6: String,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            port: 7890,
            ipv4: "0.0.0.0".into(),
            ipv6: "::".into(),
        }
    }
}

impl ListenConfig {
    pub fn addrs(&self) -> Result<Vec<SocketAddr>> {
        let mut out = Vec::new();
        if !self.ipv4.trim().is_empty() {
            let ip: Ipv4Addr = self
                .ipv4
                .trim()
                .parse()
                .with_context(|| format!("invalid listen.ipv4 {:?}", self.ipv4))?;
            out.push(SocketAddr::new(IpAddr::V4(ip), self.port));
        }
        if !self.ipv6.trim().is_empty() {
            let ip: Ipv6Addr = self
                .ipv6
                .trim()
                .parse()
                .with_context(|| format!("invalid listen.ipv6 {:?}", self.ipv6))?;
            out.push(SocketAddr::new(IpAddr::V6(ip), self.port));
        }
        if out.is_empty() {
            bail!(tr!(
                "listen.ipv4 and listen.ipv6 cannot both be empty",
                "listen.ipv4 和 listen.ipv6 不能都为空"
            ));
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AuthConfig {
    pub username: String,
    pub password: String,
}

impl AuthConfig {
    pub fn enabled(&self) -> bool {
        !self.username.is_empty()
    }

    pub fn verify(&self, user: &[u8], pass: &[u8]) -> bool {
        let user_ok = ct_eq(self.username.as_bytes(), user);
        let pass_ok = ct_eq(self.password.as_bytes(), pass);
        user_ok & pass_ok
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Protocols {
    /// Plain HTTP proxy (CONNECT + absolute-URI forwarding).
    pub http: bool,
    /// TLS-encrypted proxy connection (HTTP over TLS; SOCKS5 over TLS if socks5 is on).
    pub https: bool,
    /// SOCKS5 CONNECT.
    pub socks5: bool,
    /// SOCKS5 UDP ASSOCIATE; requires socks5.
    pub udp: bool,
}

impl Default for Protocols {
    fn default() -> Self {
        Self {
            http: true,
            https: true,
            socks5: true,
            udp: true,
        }
    }
}

impl Protocols {
    pub const NAMES: [&'static str; 4] = ["http", "https", "socks5", "udp"];

    /// Parses a comma separated list such as `http,socks5,udp` or `all`.
    pub fn parse_list(text: &str) -> Result<Protocols> {
        let mut out = Protocols {
            http: false,
            https: false,
            socks5: false,
            udp: false,
        };
        for name in text
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match name.to_ascii_lowercase().as_str() {
                "all" => out = Protocols::default(),
                "http" => out.http = true,
                "https" | "tls" => out.https = true,
                "socks5" | "socks" => out.socks5 = true,
                "udp" => out.udp = true,
                other => {
                    bail!(tr!(
                        format!("unknown protocol {other:?} (http, https, socks5, udp or all)"),
                        format!("未知协议 {other:?}（可选 http、https、socks5、udp 或 all）")
                    ))
                }
            }
        }
        out.validate()?;
        Ok(out)
    }

    pub fn validate(&self) -> Result<()> {
        if !(self.http || self.https || self.socks5) {
            bail!(tr!(
                "enable at least one of http, https, socks5",
                "http、https、socks5 至少要启用一个"
            ));
        }
        if self.udp && !self.socks5 {
            bail!(tr!(
                "udp rides on socks5: enable socks5 too",
                "udp 依赖 socks5，需同时启用 socks5"
            ));
        }
        Ok(())
    }

    pub fn get(&self, idx: usize) -> bool {
        [self.http, self.https, self.socks5, self.udp][idx]
    }

    /// Toggles one protocol, keeping udp consistent with socks5.
    pub fn toggle(&mut self, idx: usize) {
        match idx {
            0 => self.http = !self.http,
            1 => self.https = !self.https,
            2 => {
                self.socks5 = !self.socks5;
                if !self.socks5 {
                    self.udp = false;
                }
            }
            _ => {
                self.udp = !self.udp;
                if self.udp {
                    self.socks5 = true;
                }
            }
        }
    }

    pub fn list(&self) -> String {
        let names: Vec<&str> = Self::NAMES
            .iter()
            .enumerate()
            .filter(|(i, _)| self.get(*i))
            .map(|(_, n)| *n)
            .collect();
        names.join(",")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TlsConfig {
    /// Certificate / key paths; empty means cert.pem / key.pem next to the config.
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EgressConfig {
    pub policy: EgressPolicy,
    pub auto_policy_routing: bool,
    pub table_base: u32,
    pub rule_priority: u32,
    pub probe_interval_secs: u64,
    pub connect_timeout_secs: u64,
    pub iface_override: HashMap<String, String>,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            policy: EgressPolicy::SameIface,
            auto_policy_routing: true,
            table_base: 1000,
            rule_priority: 9000,
            probe_interval_secs: 60,
            connect_timeout_secs: 10,
            iface_override: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DnsConfig {
    pub servers: Vec<String>,
    pub timeout_ms: u64,
    pub cache_ttl_max_secs: u64,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            timeout_ms: 1500,
            cache_ttl_max_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PerfConfig {
    pub buffer_size: usize,
    pub handshake_timeout_secs: u64,
}

impl Default for PerfConfig {
    fn default() -> Self {
        Self {
            buffer_size: 32 * 1024,
            handshake_timeout_secs: 30,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let (dir, name) = safefs::split(path)?;
        let bytes = SafeDir::open(dir)
            .and_then(|d| d.read(name, MAX_CONFIG_BYTES))
            .with_context(|| {
                tr!(
                    format!("reading {}", path.display()),
                    format!("读取 {}", path.display())
                )
            })?;
        let text = String::from_utf8(bytes).with_context(|| {
            tr!(
                format!("{} is not UTF-8 text", path.display()),
                format!("{} 不是 UTF-8 文本", path.display())
            )
        })?;
        let cfg: Config = toml::from_str(&text).with_context(|| {
            tr!(
                format!("parsing {}", path.display()),
                format!("解析 {}", path.display())
            )
        })?;
        cfg.validate()?;
        cfg.validate_paths(path)?;
        Ok(cfg)
    }

    pub fn load_or_create(path: &Path) -> Result<(Config, bool)> {
        let (dir, name) = safefs::split(path)?;
        let dir = SafeDir::create(dir).with_context(|| {
            tr!(
                format!("opening {}", dir.display()),
                format!("打开 {}", dir.display())
            )
        })?;
        if dir.exists(name) {
            return Ok((Self::load(path)?, false));
        }
        let mut cfg = Config::default();
        cfg.auth.username = "proxy".into();
        cfg.auth.password = random_password(16);
        cfg.save(path)?;
        Ok((cfg, true))
    }

    pub fn validate(&self) -> Result<()> {
        self.listen.addrs()?;
        if self.auth.username.is_empty() != self.auth.password.is_empty() {
            bail!(tr!(
                "set username and password together (both empty turns authentication off)",
                "用户名和密码需同时设置（都留空表示关闭认证）"
            ));
        }
        if self.auth.username.len() > 255 || self.auth.password.len() > 255 {
            bail!(tr!(
                "username and password are at most 255 bytes (SOCKS5 limit)",
                "用户名和密码最长 255 字节（SOCKS5 限制）"
            ));
        }
        if self.auth.username.contains(':') {
            bail!(tr!("the username cannot contain ':'", "用户名不能包含 ':'"));
        }
        self.protocols.validate()?;
        if !LOG_LEVELS.contains(&self.log.level.as_str()) {
            let levels = LOG_LEVELS.join(", ");
            bail!(tr!(
                format!("log.level must be one of {levels}"),
                format!("log.level 只能是 {levels}")
            ));
        }
        // Rotation is what bounds the log: with 0 it would grow without limit.
        if self.log.max_size_mb == 0 {
            bail!(tr!(
                "log.max_size_mb must be at least 1",
                "log.max_size_mb 至少为 1"
            ));
        }
        for server in &self.dns.servers {
            parse_dns_server(server)?;
        }
        if self.performance.buffer_size < 4096 {
            bail!(tr!(
                "performance.buffer_size must be at least 4096",
                "performance.buffer_size 不能小于 4096"
            ));
        }
        // Keep policy routing clear of the kernel's tables (253-255) and rules
        // (0, 32766, 32767) and of the small numbers other tools tend to use.
        if !(1000..=0x7fff_0000).contains(&self.egress.table_base) {
            bail!(tr!(
                "egress.table_base must be between 1000 and 2147418112",
                "egress.table_base 必须在 1000 到 2147418112 之间"
            ));
        }
        if !(1000..=32000).contains(&self.egress.rule_priority) {
            bail!(tr!(
                "egress.rule_priority must be between 1000 and 32000",
                "egress.rule_priority 必须在 1000 到 32000 之间"
            ));
        }
        Ok(())
    }

    /// A root process serving a data directory owned by someone else must not
    /// let that owner point it at files elsewhere: certificate, key and log paths
    /// then have to stay inside the data directory.
    pub fn validate_paths(&self, config_path: &Path) -> Result<()> {
        let dir = config_path.parent().unwrap_or(Path::new("."));
        #[cfg(unix)]
        let delegated = {
            use std::os::unix::fs::MetadataExt;
            is_root() && std::fs::metadata(dir).is_ok_and(|m| m.uid() != 0)
        };
        // The service runs as LocalSystem on a directory in a user's profile.
        #[cfg(windows)]
        let delegated = is_root();
        if !delegated {
            return Ok(());
        }
        for (key, value) in [
            ("tls.cert", &self.tls.cert),
            ("tls.key", &self.tls.key),
            ("log.dir", &self.log.dir),
        ] {
            let path = Path::new(value);
            let inside = path
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
                || (path.starts_with(dir)
                    && !path
                        .components()
                        .any(|c| c == std::path::Component::ParentDir));
            if !value.is_empty() && !inside {
                let dir = dir.display();
                bail!(tr!(
                    format!("{key} = {value:?} is outside the data directory {dir}: not allowed while the directory belongs to a regular user"),
                    format!("{key} = {value:?} 不在数据目录 {dir} 内：数据目录属于普通用户时不允许指向目录之外")
                ));
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        self.validate_paths(path)?;
        let (dir, name) = safefs::split(path)?;
        let text = toml::to_string_pretty(self)?;
        SafeDir::create(dir)
            .and_then(|d| d.write_atomic(name, text.as_bytes(), 0o600))
            .with_context(|| {
                tr!(
                    format!("writing {}", path.display()),
                    format!("写入 {}", path.display())
                )
            })?;
        Ok(())
    }

    pub fn log_dir(&self, config_path: &Path) -> PathBuf {
        let dir = config_path.parent().unwrap_or(Path::new("."));
        if self.log.dir.is_empty() {
            return dir.join("logs");
        }
        let path = PathBuf::from(&self.log.dir);
        if path.is_absolute() {
            path
        } else {
            dir.join(path)
        }
    }

    pub fn log_path(&self, config_path: &Path) -> PathBuf {
        self.log_dir(config_path).join(LOG_FILE)
    }

    pub fn cert_path(&self, config_path: &Path) -> PathBuf {
        resolve_relative(config_path, &self.tls.cert, "cert.pem")
    }

    pub fn key_path(&self, config_path: &Path) -> PathBuf {
        resolve_relative(config_path, &self.tls.key, "key.pem")
    }
}

pub fn parse_dns_server(text: &str) -> Result<IpAddr> {
    let text = text.trim();
    let text = text.split('%').next().unwrap_or(text);
    text.parse::<IpAddr>()
        .with_context(|| format!("invalid dns server {text:?} (expected an IP address)"))
}

/// The everyday settings managed from the TUI, the CLI and startup flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub port: u16,
    pub username: String,
    pub password: String,
    pub protocols: Protocols,
    pub policy: EgressPolicy,
}

impl Settings {
    pub fn auth_enabled(&self) -> bool {
        !self.username.is_empty()
    }

    /// Ready-to-use proxy URLs for every enabled protocol (UDP rides on SOCKS5).
    /// SOCKS5 uses `socks5h` so DNS is resolved by the proxy on the same egress.
    pub fn proxy_urls(&self, host: &str) -> Vec<(&'static str, String)> {
        let auth = if self.auth_enabled() {
            format!(
                "{}:{}@",
                encode_userinfo(&self.username),
                encode_userinfo(&self.password)
            )
        } else {
            String::new()
        };
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        [
            ("http", self.protocols.http, "http"),
            ("https", self.protocols.https, "https"),
            ("socks5", self.protocols.socks5, "socks5h"),
        ]
        .into_iter()
        .filter(|(_, on, _)| *on)
        .map(|(name, _, scheme)| (name, format!("{scheme}://{auth}{host}:{}", self.port)))
        .collect()
    }
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn encode_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A partial settings change; `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Update {
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub protocols: Option<Protocols>,
    pub policy: Option<EgressPolicy>,
    /// Management panel language; no effect on the proxy itself.
    pub language: Option<Language>,
}

impl Update {
    pub fn is_empty(&self) -> bool {
        *self == Update::default()
    }

    /// Only the fields that differ between `old` and `new`.
    pub fn between(old: &Settings, new: &Settings) -> Update {
        Update {
            port: (new.port != old.port).then_some(new.port),
            username: (new.username != old.username).then(|| new.username.clone()),
            password: (new.password != old.password).then(|| new.password.clone()),
            protocols: (new.protocols != old.protocols).then_some(new.protocols),
            policy: (new.policy != old.policy).then_some(new.policy),
            language: None,
        }
    }

    /// Applies the change and validates the result.
    pub fn apply(&self, cfg: &mut Config) -> Result<()> {
        if let Some(port) = self.port {
            if port == 0 {
                bail!(tr!("the port must be 1-65535", "端口必须是 1-65535"));
            }
            cfg.listen.port = port;
        }
        if let Some(user) = &self.username {
            cfg.auth.username = user.trim().to_string();
        }
        if let Some(pass) = &self.password {
            cfg.auth.password = pass.clone();
        }
        if let Some(protocols) = self.protocols {
            cfg.protocols = protocols;
        }
        if let Some(policy) = self.policy {
            cfg.egress.policy = policy;
        }
        if let Some(language) = self.language {
            cfg.ui.language = language;
        }
        cfg.validate()
    }
}

impl Config {
    pub fn settings(&self) -> Settings {
        Settings {
            port: self.listen.port,
            username: self.auth.username.clone(),
            password: self.auth.password.clone(),
            protocols: self.protocols,
            policy: self.egress.policy,
        }
    }

    /// Loads (or creates) the config at `path`, applies `update` and saves it.
    pub fn update_file(path: &Path, update: &Update) -> Result<Config> {
        let (mut cfg, _) = Config::load_or_create(path)?;
        if update.is_empty() {
            return Ok(cfg);
        }
        update.apply(&mut cfg)?;
        cfg.save(path)?;
        Ok(cfg)
    }
}

fn resolve_relative(config_path: &Path, value: &str, default_name: &str) -> PathBuf {
    let dir = config_path.parent().unwrap_or(Path::new("."));
    if value.is_empty() {
        return dir.join(default_name);
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        dir.join(path)
    }
}

pub fn random_password(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LogConfig {
    /// off / error / warn / info / debug / trace
    pub level: String,
    /// Log directory; empty means `<data dir>/logs`.
    pub dir: String,
    pub max_size_mb: u64,
    pub max_files: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            dir: String::new(),
            max_size_mb: 10,
            max_files: 5,
        }
    }
}

pub const LOG_LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];
pub const LOG_FILE: &str = "proxy-tui.log";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_protocol_lists() {
        let p = Protocols::parse_list("http,socks5").unwrap();
        assert_eq!(
            (p.http, p.https, p.socks5, p.udp),
            (true, false, true, false)
        );
        assert_eq!(Protocols::parse_list("all").unwrap(), Protocols::default());
        assert_eq!(
            Protocols::parse_list("HTTPS, socks5 udp").unwrap().list(),
            "https,socks5,udp"
        );
        assert!(Protocols::parse_list("udp").is_err());
        assert!(Protocols::parse_list("").is_err());
        assert!(Protocols::parse_list("http,ftp").is_err());
    }

    #[test]
    fn toggles_keep_udp_consistent() {
        let mut p = Protocols::default();
        p.toggle(2);
        assert!(!p.socks5 && !p.udp);
        p.toggle(3);
        assert!(p.socks5 && p.udp);
    }

    #[test]
    fn proxy_urls_encode_credentials() {
        let settings = Settings {
            port: 7890,
            username: "a@b".into(),
            password: "p:w/1".into(),
            protocols: Protocols::parse_list("http,socks5,udp").unwrap(),
            policy: EgressPolicy::SameIface,
        };
        let urls = settings.proxy_urls("127.0.0.1");
        assert_eq!(
            urls,
            vec![
                ("http", "http://a%40b:p%3Aw%2F1@127.0.0.1:7890".to_string()),
                (
                    "socks5",
                    "socks5h://a%40b:p%3Aw%2F1@127.0.0.1:7890".to_string()
                ),
            ]
        );
        let open = Settings {
            username: String::new(),
            password: String::new(),
            ..settings
        };
        assert_eq!(open.proxy_urls("::1")[0].1, "http://[::1]:7890");
    }

    #[test]
    fn log_size_stays_bounded() {
        let mut cfg = Config::default();
        assert!(cfg.validate().is_ok());
        cfg.log.max_size_mb = 0;
        assert!(cfg.validate().is_err(), "0 would disable rotation");
    }

    #[test]
    fn update_apply_and_between() {
        let mut cfg = Config::default();
        cfg.auth.username = "old".into();
        cfg.auth.password = "old".into();
        let before = cfg.settings();
        Update {
            port: Some(8080),
            password: Some("new".into()),
            protocols: Some(Protocols::parse_list("http").unwrap()),
            ..Default::default()
        }
        .apply(&mut cfg)
        .unwrap();
        assert_eq!(cfg.listen.port, 8080);
        assert_eq!(
            (cfg.auth.username.as_str(), cfg.auth.password.as_str()),
            ("old", "new")
        );
        assert_eq!(cfg.protocols.list(), "http");
        let diff = Update::between(&before, &cfg.settings());
        assert_eq!(diff.port, Some(8080));
        assert_eq!(diff.username, None);
        assert_eq!(diff.password.as_deref(), Some("new"));
        assert!(Update::between(&before, &before).is_empty());

        let no_auth = Update {
            username: Some(String::new()),
            password: Some(String::new()),
            ..Default::default()
        };
        no_auth.apply(&mut cfg).unwrap();
        assert!(!cfg.auth.enabled());
        let half = Update {
            username: Some("only-user".into()),
            ..Default::default()
        };
        assert!(half.apply(&mut cfg.clone()).is_err());
        let zero = Update {
            port: Some(0),
            ..Default::default()
        };
        assert!(zero.apply(&mut cfg).is_err());
    }
}
