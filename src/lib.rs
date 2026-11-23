//! proxy-tui：HTTP / HTTPS / SOCKS5 mixed 端口代理。
//!
//! 分层：
//! - 交互层：`cli`（命令行，给 Agent / 脚本用）、`ui`（TUI，给人用）
//! - 服务层：`manage`（两者共用的管理接口）
//! - 运行层：`daemon`（守护进程）、`proxy`（代理协议）、`net`（网络出口）
//! - 基础：`config`（配置与数据目录）、`api`（控制协议）、`i18n`（界面语言）

pub mod api;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod i18n;
pub mod manage;
pub mod net;
pub mod proxy;
pub mod ui;

/// Pads `text` to `width` terminal columns (CJK characters count as two).
pub fn pad(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let fill = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(fill))
}

pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

pub fn fmt_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d{h:02}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}
