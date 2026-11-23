//! Boot-time service management: systemd on Linux, launchd on macOS.
//! Every function returns text instead of printing so the TUI can call it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use serde::Serialize;

use crate::config::{config_file, is_root, set_mode, Config, SafeDir};
use crate::tr;

pub const BIN_PATH: &str = "/usr/local/bin/proxy-tui";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Stopped,
    Failed,
    Busy(String),
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServiceStatus {
    pub manager: &'static str,
    pub supported: bool,
    pub installed: bool,
    pub enabled: bool,
    pub state: RunState,
    pub pid: Option<u32>,
    pub unit_path: &'static str,
    pub note: String,
}

impl ServiceStatus {
    #[allow(dead_code)]
    fn unsupported(note: String) -> Self {
        ServiceStatus {
            manager: platform::MANAGER,
            supported: false,
            installed: false,
            enabled: false,
            state: RunState::Unknown,
            pid: None,
            unit_path: platform::UNIT_PATH,
            note,
        }
    }

    pub fn state_label(&self) -> String {
        if !self.supported {
            return tr!("unavailable", "不可用").into();
        }
        if !self.installed {
            return tr!("not installed", "未安装").into();
        }
        match &self.state {
            RunState::Running => tr!("running", "运行中").into(),
            RunState::Stopped => tr!("stopped", "已停止").into(),
            RunState::Failed => tr!("failed", "失败").into(),
            RunState::Busy(s) => s.clone(),
            RunState::Unknown => tr!("unknown", "未知").into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Install,
    Uninstall,
}

impl Action {
    pub const ALL: [Action; 5] = [
        Action::Start,
        Action::Stop,
        Action::Restart,
        Action::Install,
        Action::Uninstall,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::Start => tr!("start", "启动"),
            Action::Stop => tr!("stop", "停止"),
            Action::Restart => tr!("restart", "重启"),
            Action::Install => tr!("install", "安装"),
            Action::Uninstall => tr!("uninstall", "卸载"),
        }
    }

    pub fn progress(self) -> &'static str {
        match self {
            Action::Start => tr!("Starting the service", "正在启动服务"),
            Action::Stop => tr!("Stopping the service", "正在停止服务"),
            Action::Restart => tr!("Restarting the service", "正在重启服务"),
            Action::Install => tr!("Installing the service", "正在安装服务"),
            Action::Uninstall => tr!("Uninstalling the service", "正在卸载服务"),
        }
    }
}

/// Why an action cannot run right now, if it cannot.
pub fn availability(action: Action, s: &ServiceStatus) -> Result<(), &'static str> {
    if !s.supported {
        return Err(tr!("not supported on this system", "当前系统不支持"));
    }
    if !is_root() {
        return Err(tr!("needs root", "需要 root"));
    }
    match action {
        Action::Start | Action::Stop | Action::Restart | Action::Uninstall if !s.installed => {
            Err(tr!("not installed", "未安装"))
        }
        Action::Start if s.state == RunState::Running => Err(tr!("already running", "已在运行")),
        Action::Stop if s.state == RunState::Stopped => Err(tr!("not running", "未运行")),
        _ => Ok(()),
    }
}

/// Warning shown before actions that drop proxied connections.
pub fn confirm_text(action: Action, s: &ServiceStatus) -> Option<&'static str> {
    match action {
        Action::Stop => Some(tr!(
            "Stopping the service drops every proxied connection",
            "停止服务会断开所有代理连接"
        )),
        Action::Restart => Some(tr!(
            "Restarting the service drops every proxied connection",
            "重启服务会断开所有代理连接"
        )),
        Action::Uninstall => Some(tr!(
            "Uninstalling stops the service and disables start at boot (config and program are kept)",
            "卸载会停止服务并取消开机自启（保留配置和程序）"
        )),
        Action::Install if s.installed => Some(tr!(
            "Reinstalls the service with this program and restarts it",
            "用当前程序覆盖安装并重启服务"
        )),
        _ => None,
    }
}

pub fn status() -> ServiceStatus {
    platform::status()
}

/// Runs a service action for the data directory `dir`; requires root.
pub fn perform(action: Action, dir: &Path) -> Result<String> {
    if !is_root() {
        bail!(tr!(
            "service actions need root: run with sudo",
            "服务操作需要 root 权限：请用 sudo 运行"
        ));
    }
    match action {
        Action::Install => install(dir),
        Action::Uninstall => uninstall(dir),
        Action::Start => platform::start(),
        Action::Stop => platform::stop(),
        Action::Restart => platform::restart(),
    }
}

#[allow(dead_code)]
fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program).args(args).output().with_context(|| {
        tr!(
            format!("cannot run {program}"),
            format!("无法执行 {program}")
        )
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        let cmd = args.join(" ");
        bail!(tr!(
            format!("{program} {cmd} failed: {detail}"),
            format!("{program} {cmd} 失败: {detail}")
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Copies the running executable to BIN_PATH. Returns true when it copied.
fn install_binary() -> Result<bool> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let bin = Path::new(BIN_PATH);
    if bin.exists() && bin.canonicalize()? == exe {
        return Ok(false);
    }
    std::fs::create_dir_all(bin.parent().unwrap())?;
    let tmp = bin.with_extension("new");
    std::fs::copy(&exe, &tmp).with_context(|| {
        tr!(
            format!("copying to {}", tmp.display()),
            format!("复制到 {}", tmp.display())
        )
    })?;
    set_mode(&tmp, 0o755);
    // rename() swaps the directory entry, so a running old binary is untouched.
    std::fs::rename(&tmp, bin)?;
    Ok(true)
}

fn install(dir: &Path) -> Result<String> {
    let current = status();
    if !current.supported {
        bail!(tr!(
            format!("services cannot be managed here: {}", current.note),
            format!("当前系统无法管理服务: {}", current.note)
        ));
    }
    // The service starts in `/`: it needs an absolute path, and one that the
    // unit / plist can carry without ambiguity.
    let dir = &std::path::absolute(dir)?;
    let Some(dir_text) = dir.to_str().filter(|d| !d.contains(['\n', '\r'])) else {
        bail!(tr!(
            format!(
                "the data directory path must be UTF-8 without line breaks: {}",
                dir.display()
            ),
            format!("数据目录路径必须是不含换行的 UTF-8：{}", dir.display())
        ));
    };
    let copied = install_binary()?;
    let config_path = config_file(dir);
    let (cfg, created) = Config::load_or_create(&config_path)?;
    let log_dir = cfg.log_dir(&config_path);
    SafeDir::create(&log_dir).with_context(|| {
        tr!(
            format!("creating {}", log_dir.display()),
            format!("创建 {}", log_dir.display())
        )
    })?;
    platform::install(dir_text)?;
    let (manager, dir) = (platform::MANAGER, dir.display());
    let mut msg = tr!(
        format!("Installed and started ({manager}, starts at boot); data directory {dir}"),
        format!("已安装并启动（{manager}，开机自启）；数据目录 {dir}")
    );
    if copied {
        msg.push_str(&tr!(
            format!("; program {BIN_PATH}"),
            format!("；程序 {BIN_PATH}")
        ));
    }
    if created {
        let (port, user, pass) = (cfg.listen.port, &cfg.auth.username, &cfg.auth.password);
        msg.push_str(&tr!(
            format!("; new config: port {port} user {user} password {pass}"),
            format!("；新配置 端口 {port} 用户 {user} 密码 {pass}")
        ));
    }
    Ok(msg)
}

fn uninstall(dir: &Path) -> Result<String> {
    platform::uninstall()?;
    let dir = dir.display();
    Ok(tr!(
        format!("Service uninstalled; data directory {dir} and program {BIN_PATH} are kept"),
        format!("服务已卸载；数据目录 {dir} 和程序 {BIN_PATH} 保留")
    ))
}

/// Data directory used by the installed service (parsed from its unit / plist).
pub fn installed_data_dir() -> Option<PathBuf> {
    let text = std::fs::read_to_string(platform::UNIT_PATH).ok()?;
    parse_dir_arg(&text).map(PathBuf::from)
}

/// A systemd ExecStart argument: double-quoted, with `\` `"` escaped and `%`
/// (specifiers) and `$` (variables) doubled.
#[allow(dead_code)]
fn systemd_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[allow(dead_code)]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Splits an ExecStart command line into arguments (inverse of `systemd_quote`).
fn systemd_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            return args;
        };
        let mut arg = String::new();
        if first == '"' {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => arg.extend(chars.next()),
                    c => arg.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                arg.push(c);
                chars.next();
            }
        }
        args.push(arg.replace("%%", "%").replace("$$", "$"));
    }
}

/// Finds the value after `--dir` in a systemd ExecStart line or a plist
/// ProgramArguments array.
fn parse_dir_arg(text: &str) -> Option<String> {
    if let Some(line) = text
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("ExecStart="))
    {
        let args = systemd_args(line);
        let pos = args.iter().position(|a| a == "--dir")?;
        return args.get(pos + 1).cloned();
    }
    let strings: Vec<String> = text
        .split("<string>")
        .skip(1)
        .filter_map(|s| s.split("</string>").next())
        .map(xml_unescape)
        .collect();
    let pos = strings.iter().position(|s| s == "--dir")?;
    strings.get(pos + 1).cloned()
}

/// Parses `systemctl show -p LoadState,ActiveState,SubState,MainPID,UnitFileState`.
#[allow(dead_code)]
fn parse_systemctl_show(text: &str) -> (RunState, Option<u32>, bool) {
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .unwrap_or("")
    };
    let state = match get("ActiveState") {
        "active" => RunState::Running,
        "inactive" => RunState::Stopped,
        "failed" => RunState::Failed,
        "" => RunState::Unknown,
        "activating" => RunState::Busy(tr!("starting", "启动中").into()),
        "deactivating" => RunState::Busy(tr!("stopping", "停止中").into()),
        other => RunState::Busy(format!("{other}/{}", get("SubState"))),
    };
    let pid = get("MainPID").parse::<u32>().ok().filter(|p| *p != 0);
    let enabled = matches!(get("UnitFileState"), "enabled" | "enabled-runtime");
    (state, pid, enabled)
}

/// Parses the top-level `state` / `pid` / `last exit code` of `launchctl print`.
#[allow(dead_code)]
fn parse_launchctl_print(text: &str) -> (RunState, Option<u32>) {
    let top = |key: &str| {
        text.lines().find_map(|l| {
            let rest = l.strip_prefix('\t')?;
            if rest.starts_with('\t') {
                return None;
            }
            rest.strip_prefix(key)?.strip_prefix(" = ").map(str::trim)
        })
    };
    let state = match top("state") {
        Some("running") => RunState::Running,
        Some("not running") => match top("last exit code") {
            Some(code) if code != "0" && !code.starts_with("(never") => RunState::Failed,
            _ => RunState::Stopped,
        },
        Some("spawn scheduled") => RunState::Busy(tr!("waiting to start", "等待启动").into()),
        Some(other) => RunState::Busy(other.to_string()),
        None => RunState::Unknown,
    };
    let pid = top("pid").and_then(|p| p.parse::<u32>().ok());
    (state, pid)
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub const MANAGER: &str = "systemd";
    pub const UNIT_PATH: &str = "/etc/systemd/system/proxy-tui.service";
    const UNIT: &str = "proxy-tui.service";

    pub fn status() -> ServiceStatus {
        let out = Command::new("systemctl")
            .args([
                "show",
                UNIT,
                "--property=LoadState,ActiveState,SubState,MainPID,UnitFileState",
            ])
            .output();
        let out = match out {
            Ok(out) if out.status.success() => out,
            Ok(out) => {
                return ServiceStatus::unsupported(
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                )
            }
            Err(e) => return ServiceStatus::unsupported(format!("systemctl: {e}")),
        };
        let (state, pid, enabled) = parse_systemctl_show(&String::from_utf8_lossy(&out.stdout));
        ServiceStatus {
            manager: MANAGER,
            supported: true,
            installed: Path::new(UNIT_PATH).exists(),
            enabled,
            state,
            pid,
            unit_path: UNIT_PATH,
            note: String::new(),
        }
    }

    pub fn install(dir: &str) -> Result<()> {
        let unit = format!(
            "[Unit]\n\
Description=proxy-tui mixed HTTP/HTTPS/SOCKS5 proxy\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
Environment=PROXY_TUI_SERVICE=systemd\n\
ExecStart={BIN_PATH} daemon --dir {}\n\
Restart=always\n\
RestartSec=2\n\
LimitNOFILE=1048576\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
            systemd_quote(dir)
        );
        std::fs::write(UNIT_PATH, unit)
            .with_context(|| tr!(format!("writing {UNIT_PATH}"), format!("写入 {UNIT_PATH}")))?;
        run("systemctl", &["daemon-reload"])?;
        run("systemctl", &["enable", UNIT])?;
        run("systemctl", &["restart", UNIT])?;
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = run("systemctl", &["disable", "--now", UNIT]);
        if Path::new(UNIT_PATH).exists() {
            std::fs::remove_file(UNIT_PATH)?;
        }
        run("systemctl", &["daemon-reload"])?;
        let _ = run("systemctl", &["reset-failed", UNIT]);
        Ok(())
    }

    pub fn start() -> Result<String> {
        run("systemctl", &["start", UNIT])?;
        Ok(tr!("Service started", "服务已启动").into())
    }

    pub fn stop() -> Result<String> {
        run("systemctl", &["stop", UNIT])?;
        Ok(tr!(
            "Service stopped (it still starts at boot)",
            "服务已停止（开机仍会自启）"
        )
        .into())
    }

    pub fn restart() -> Result<String> {
        run("systemctl", &["restart", UNIT])?;
        Ok(tr!("Service restarted", "服务已重启").into())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::time::{Duration, Instant};

    pub const MANAGER: &str = "launchd";
    pub const UNIT_PATH: &str = "/Library/LaunchDaemons/com.proxy-tui.daemon.plist";
    const LABEL: &str = "com.proxy-tui.daemon";

    fn target() -> String {
        format!("system/{LABEL}")
    }

    /// `launchctl print` output when the job is loaded.
    fn loaded() -> Option<String> {
        let out = Command::new("launchctl")
            .args(["print", &target()])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn disabled() -> bool {
        let Ok(out) = Command::new("launchctl")
            .args(["print-disabled", "system"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        text.contains(&format!("\"{LABEL}\" => disabled"))
            || text.contains(&format!("\"{LABEL}\" => true"))
    }

    pub fn status() -> ServiceStatus {
        let installed = Path::new(UNIT_PATH).exists();
        let (state, pid) = match loaded() {
            Some(text) => parse_launchctl_print(&text),
            None => (RunState::Stopped, None),
        };
        ServiceStatus {
            manager: MANAGER,
            supported: true,
            installed,
            enabled: installed && !disabled(),
            state,
            pid,
            unit_path: UNIT_PATH,
            note: String::new(),
        }
    }

    fn bootout() {
        if loaded().is_none() {
            return;
        }
        let _ = run("launchctl", &["bootout", &target()]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while loaded().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn bootstrap() -> Result<()> {
        if !Path::new(UNIT_PATH).exists() {
            bail!(tr!("the service is not installed", "服务未安装"));
        }
        let _ = run("launchctl", &["enable", &target()]);
        let mut last = None;
        for _ in 0..3 {
            match run("launchctl", &["bootstrap", "system", UNIT_PATH]) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
        Err(last.unwrap())
    }

    /// launchd opens these as root, so they stay out of the user-owned data dir.
    pub const LAUNCHD_LOG: &str = "/var/log/proxy-tui.launchd.log";

    pub fn install(dir: &str) -> Result<()> {
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{BIN_PATH}</string>
        <string>daemon</string>
        <string>--dir</string>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PROXY_TUI_SERVICE</key><string>launchd</string>
    </dict>
    <key>StandardOutPath</key><string>{LAUNCHD_LOG}</string>
    <key>StandardErrorPath</key><string>{LAUNCHD_LOG}</string>
</dict>
</plist>
"#,
            xml_escape(dir)
        );
        bootout();
        std::fs::write(UNIT_PATH, plist)
            .with_context(|| tr!(format!("writing {UNIT_PATH}"), format!("写入 {UNIT_PATH}")))?;
        set_mode(Path::new(UNIT_PATH), 0o644);
        bootstrap()
    }

    pub fn uninstall() -> Result<()> {
        bootout();
        if Path::new(UNIT_PATH).exists() {
            std::fs::remove_file(UNIT_PATH)?;
        }
        Ok(())
    }

    pub fn start() -> Result<String> {
        if loaded().is_some() {
            run("launchctl", &["kickstart", &target()])?;
        } else {
            bootstrap()?;
        }
        Ok(tr!("Service started", "服务已启动").into())
    }

    pub fn stop() -> Result<String> {
        // KeepAlive would respawn a killed job, so unload it instead.
        bootout();
        if loaded().is_some() {
            bail!(tr!(
                "launchctl bootout did not stop the service",
                "launchctl bootout 未能停止服务"
            ));
        }
        Ok(tr!(
            "Service stopped (it still starts at boot)",
            "服务已停止（开机仍会自启）"
        )
        .into())
    }

    pub fn restart() -> Result<String> {
        if loaded().is_some() {
            run("launchctl", &["kickstart", "-k", &target()])?;
        } else {
            bootstrap()?;
        }
        Ok(tr!("Service restarted", "服务已重启").into())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;

    pub const MANAGER: &str = "none";
    pub const UNIT_PATH: &str = "";
    fn unsupported() -> &'static str {
        tr!(
            "only Linux (systemd) and macOS (launchd) are supported",
            "仅支持 Linux (systemd) 和 macOS (launchd)"
        )
    }

    pub fn status() -> ServiceStatus {
        ServiceStatus::unsupported(unsupported().into())
    }

    pub fn install(_dir: &str) -> Result<()> {
        bail!(unsupported())
    }

    pub fn uninstall() -> Result<()> {
        bail!(unsupported())
    }

    pub fn start() -> Result<String> {
        bail!(unsupported())
    }

    pub fn stop() -> Result<String> {
        bail!(unsupported())
    }

    pub fn restart() -> Result<String> {
        bail!(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemctl_show_states() {
        let text = "LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=4242\nUnitFileState=enabled\n";
        assert_eq!(
            parse_systemctl_show(text),
            (RunState::Running, Some(4242), true)
        );
        let text =
            "LoadState=not-found\nActiveState=inactive\nSubState=dead\nMainPID=0\nUnitFileState=\n";
        assert_eq!(parse_systemctl_show(text), (RunState::Stopped, None, false));
        let text = "ActiveState=failed\nMainPID=0\nUnitFileState=disabled\n";
        assert_eq!(parse_systemctl_show(text), (RunState::Failed, None, false));
    }

    #[test]
    fn launchctl_print_states() {
        let running = "system/com.proxy-tui.daemon = {\n\tactive count = 1\n\tpath = /Library/LaunchDaemons/x.plist\n\tstate = running\n\n\tprogram = /usr/local/bin/proxy-tui\n\tendpoints = {\n\t\tstate = active\n\t}\n\tpid = 812\n}\n";
        assert_eq!(
            parse_launchctl_print(running),
            (RunState::Running, Some(812))
        );
        let crashed = "x = {\n\tstate = not running\n\tlast exit code = 1\n}\n";
        assert_eq!(parse_launchctl_print(crashed), (RunState::Failed, None));
        let idle = "x = {\n\tstate = not running\n\tlast exit code = (never exited)\n}\n";
        assert_eq!(parse_launchctl_print(idle), (RunState::Stopped, None));
    }

    #[test]
    fn dir_arg_from_unit_and_plist() {
        let unit =
            "[Service]\nExecStart=/usr/local/bin/proxy-tui daemon --dir /home/u/.proxy-tui\n";
        assert_eq!(parse_dir_arg(unit).as_deref(), Some("/home/u/.proxy-tui"));
        let plist = "<array>\n<string>/usr/local/bin/proxy-tui</string>\n<string>daemon</string>\n<string>--dir</string>\n<string>/Users/alice/.proxy-tui</string>\n</array>";
        assert_eq!(parse_dir_arg(plist).as_deref(), Some("/Users/alice/.proxy-tui"));
        assert_eq!(parse_dir_arg("<string>x</string>"), None);

        let odd = "/srv/my data/100%$HOME\\\"x";
        let unit = format!(
            "ExecStart=/usr/local/bin/proxy-tui daemon --dir {}\n",
            systemd_quote(odd)
        );
        assert_eq!(parse_dir_arg(&unit).as_deref(), Some(odd));
        let amp = "/Users/a&b/<dir>";
        let plist = format!(
            "<string>--dir</string>\n<string>{}</string>",
            xml_escape(amp)
        );
        assert_eq!(parse_dir_arg(&plist).as_deref(), Some(amp));
    }
}
