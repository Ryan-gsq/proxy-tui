//! 服务层：TUI（给人用）和 CLI（给 Agent / 脚本用）共用的管理接口。
//!
//! 交互层只调用这里，不直接读写配置文件、控制 socket 或 systemctl / launchctl。
//! 一切都以数据目录定位：服务模式和一次性模式共用同一个目录。
//! 设置修改在代理运行时实时生效，未运行时写入配置文件。

pub mod client;
pub mod oneshot;
pub mod service;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::api::{Request, Response, Status};
use crate::config::{self, Config, Settings, Update};
use crate::daemon::logging;
use crate::tr;

pub use crate::daemon::logging::tail as tail_log;
pub use client::{is_not_running, NotRunning};
pub use oneshot::Oneshot;
pub use service::{Action, RunState, ServiceStatus};

const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a settings change ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Applied by the running daemon (and saved by it).
    Live,
    /// No daemon running: written to the config file for the next start.
    Saved,
}

pub struct Manager {
    dir: PathBuf,
    config_path: PathBuf,
    socket: PathBuf,
    client: Option<client::Client>,
}

impl Manager {
    /// Manages the proxy living in data directory `dir`.
    pub fn new(dir: PathBuf) -> Manager {
        // Absolute, so a detached daemon or a service finds the same directory.
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        let config_path = config::config_file(&dir);
        Manager {
            socket: config::control_socket_for(&config_path),
            config_path,
            dir,
            client: None,
        }
    }

    /// `dir`, or the shared default data directory.
    pub fn open(dir: Option<PathBuf>) -> Manager {
        Manager::new(dir.unwrap_or_else(config::data_dir))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    fn call(&mut self, req: &Request) -> Result<Response> {
        let reused = self.client.is_some();
        let mut result = self.send(req);
        if result.is_err() && reused {
            // The cached connection may be stale (daemon restarted or stopped).
            result = self.send(req);
        }
        match result? {
            Response::Error { message } => bail!(message),
            other => Ok(other),
        }
    }

    fn send(&mut self, req: &Request) -> Result<Response> {
        if self.client.is_none() {
            self.client = Some(client::Client::connect(&self.socket)?);
        }
        let result = self.client.as_mut().expect("connected above").request(req);
        if result.is_err() {
            self.client = None;
        }
        result
    }

    /// Status of the running daemon; fails with [`NotRunning`] when none runs.
    pub fn status(&mut self) -> Result<Status> {
        match self.call(&Request::Status)? {
            Response::Status(status) => Ok(*status),
            other => bail!("unexpected response: {other:?}"),
        }
    }

    /// Status, or `None` when no daemon runs.
    pub fn running(&mut self) -> Result<Option<Status>> {
        match self.status() {
            Ok(status) => Ok(Some(status)),
            Err(e) if is_not_running(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The config file (created with defaults on first use).
    pub fn load_config(&self) -> Result<Config> {
        Ok(Config::load_or_create(&self.config_path)?.0)
    }

    /// Current settings: from the daemon when it runs, else from the config file.
    pub fn settings(&mut self) -> Result<(Settings, Applied)> {
        match self.running()? {
            Some(status) => Ok((status.settings, Applied::Live)),
            None => Ok((self.load_config()?.settings(), Applied::Saved)),
        }
    }

    /// Changes settings: live when the daemon runs, else in the config file.
    pub fn update(&mut self, update: &Update) -> Result<Applied> {
        match self.call(&Request::Update(update.clone())) {
            Ok(_) => Ok(Applied::Live),
            Err(e) if is_not_running(&e) => {
                Config::update_file(&self.config_path, update)?;
                Ok(Applied::Saved)
            }
            Err(e) => Err(e),
        }
    }

    /// Makes the running daemon re-read its config file.
    pub fn reload(&mut self) -> Result<String> {
        match self.call(&Request::Reload)? {
            Response::Ok { message } => Ok(message),
            other => bail!("unexpected response: {other:?}"),
        }
    }

    /// Log file of the running daemon, else the one configured in the file.
    pub fn log_path(&mut self) -> PathBuf {
        match self.status() {
            Ok(status) if !status.log_path.is_empty() => PathBuf::from(status.log_path),
            _ => logging::log_path_for(&self.config_path),
        }
    }

    /// Writes startup settings into the config file. Refuses when a daemon
    /// already runs in this data directory (a second one could not listen).
    pub fn prepare_start(&mut self, update: &Update) -> Result<Config> {
        if let Some(status) = self.running()? {
            let stop = match status.mode {
                crate::api::RunMode::Service => "sudo proxy-tui service stop",
                crate::api::RunMode::Oneshot => "proxy-tui stop",
            };
            let (mode, pid) = (status.mode.label(), status.pid);
            bail!(tr!(
                format!("the proxy is already running ({mode}, pid {pid}). Change settings with proxy-tui config set, stop it with {stop}"),
                format!("代理已在运行（{mode} pid {pid}）。修改设置请用 proxy-tui config set，停止请用 {stop}")
            ));
        }
        Config::update_file(&self.config_path, update)
    }

    /// Runs the daemon in the foreground of this process; returns when stopped
    /// (Ctrl+C, SIGTERM, closing the terminal or, for one-shot, `proxy-tui stop`).
    /// `on_ready` gets the status once the proxy listens.
    pub fn run_foreground(&self, on_ready: impl FnOnce(&Status)) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(crate::daemon::run(self.config_path.clone(), on_ready))
    }

    /// One-shot mode as a background process. Returns once it is listening.
    pub fn start_detached(&mut self) -> Result<Status> {
        let cfg = self.load_config()?;
        let log = cfg.log_path(&self.config_path);
        // Early failures (before file logging starts) land in the same log.
        let stderr = logging::open_append(&log).with_context(|| {
            tr!(
                format!("opening log {}", log.display()),
                format!("打开日志 {}", log.display())
            )
        })?;
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("daemon")
            .arg("--dir")
            .arg(&self.dir)
            .env_remove(crate::daemon::SERVICE_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        // Own session: closing the caller's terminal must not stop it.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().context(tr!(
            "starting the background process failed",
            "启动后台进程失败"
        ))?;
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            // Only our own child counts (another start may race us), and only
            // once it listens: the control socket comes up just before the ports.
            let status = self.running().ok().flatten();
            if let Some(status) = status.filter(|s| s.pid == child.id() && !s.listening.is_empty())
            {
                return Ok(status);
            }
            if let Some(code) = child.try_wait()? {
                let tail = logging::tail(&log, 5).map(|(l, _)| l).unwrap_or_default();
                let (log, tail) = (log.display(), tail.join("\n"));
                bail!(tr!(
                    format!("the proxy failed to start ({code}). Log {log}:\n{tail}"),
                    format!("代理启动失败（{code}）。日志 {log}：\n{tail}")
                ));
            }
            if Instant::now() >= deadline {
                // Do not leave a half-started proxy behind.
                let _ = child.kill();
                let _ = child.wait();
                let log = log.display();
                bail!(tr!(
                    format!("timed out waiting for the proxy; the process was stopped, see {log}"),
                    format!("等待代理就绪超时，已终止该进程，见日志 {log}")
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Stops a headless one-shot daemon and waits until it is gone.
    /// Returns `Ok(None)` if nothing was running.
    pub fn stop(&mut self) -> Result<Option<Status>> {
        let Some(status) = self.running()? else {
            return Ok(None);
        };
        self.call(&Request::Shutdown)?;
        let deadline = Instant::now() + STOP_TIMEOUT;
        while self.running()?.is_some() {
            if Instant::now() >= deadline {
                let pid = status.pid;
                bail!(tr!(
                    format!("timed out waiting for the proxy to stop (pid {pid})"),
                    format!("等待代理停止超时（pid {pid}）")
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(Some(status))
    }

    /// Refuses while a one-shot proxy runs here: the service would need its port.
    fn ensure_no_oneshot(&mut self) -> Result<()> {
        match self.running()? {
            Some(s) if s.mode == crate::api::RunMode::Oneshot => {
                let pid = s.pid;
                bail!(tr!(
                    format!("a one-shot proxy is running (pid {pid}); stop it first: proxy-tui stop, or in its panel"),
                    format!("一次性代理正在运行（pid {pid}），请先停止：proxy-tui stop 或在其管理面板中停止")
                ))
            }
            _ => Ok(()),
        }
    }

    /// Before installing the service: writes `update` into the config file.
    pub fn prepare_install(&mut self, update: &Update) -> Result<()> {
        if !config::is_root() {
            bail!(tr!(
                "service actions need root: run with sudo",
                "服务操作需要 root 权限：请用 sudo 运行"
            ));
        }
        self.ensure_no_oneshot()?;
        Config::update_file(&self.config_path, update)?;
        Ok(())
    }

    /// Runs a service action for this data directory; requires root.
    pub fn service_action(&mut self, action: Action) -> Result<String> {
        if matches!(action, Action::Start | Action::Restart | Action::Install) {
            self.ensure_no_oneshot()?;
        }
        service::perform(action, &self.dir)
    }
}

/// State of the boot-time service (systemd / launchd).
pub fn service_status() -> ServiceStatus {
    service::status()
}
