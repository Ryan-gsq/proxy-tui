//! 面板状态、事件循环、按键分发，以及通过服务层执行的操作。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use ratatui::DefaultTerminal;

use super::form::{FormEvent, Purpose, SettingsForm};
use super::logview::LogView;
use super::menu::RunMenu;
use crate::api::{RunMode, Status};
use crate::config::{is_root, Update};
use crate::i18n;
use crate::manage::{self, is_not_running, Action, Applied, Manager, Oneshot, ServiceStatus};
use crate::tr;

const POLL_EVERY: Duration = Duration::from_secs(1);
const SERVICE_POLL_EVERY: Duration = Duration::from_secs(2);

pub struct Flash {
    pub text: String,
    pub ok: bool,
    at: Instant,
}

impl Flash {
    pub fn visible(&self) -> bool {
        self.at.elapsed() < Duration::from_secs(if self.ok { 5 } else { 10 })
    }
}

enum WorkerCmd {
    Refresh,
    Run(Action),
}

enum WorkerEvent {
    Status(ServiceStatus),
    Done(Action, Result<String, String>),
}

/// Runs systemctl / launchctl off the UI thread so the panel never freezes.
struct Worker {
    tx: Sender<WorkerCmd>,
    rx: Receiver<WorkerEvent>,
    pending: bool,
    last: Option<Instant>,
}

impl Worker {
    fn spawn(dir: PathBuf) -> Worker {
        let (tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
        let (event_tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut manager = Manager::new(dir);
            for cmd in cmd_rx {
                if let WorkerCmd::Run(action) = cmd {
                    let result = manager.service_action(action).map_err(|e| format!("{e:#}"));
                    if event_tx.send(WorkerEvent::Done(action, result)).is_err() {
                        break;
                    }
                }
                if event_tx
                    .send(WorkerEvent::Status(manage::service_status()))
                    .is_err()
                {
                    break;
                }
            }
        });
        Worker {
            tx,
            rx,
            pending: false,
            last: None,
        }
    }

    fn send(&mut self, cmd: WorkerCmd) -> bool {
        self.pending = self.tx.send(cmd).is_ok();
        self.last = Some(Instant::now());
        self.pending
    }
}

pub struct App {
    pub manager: Manager,
    pub oneshot: Oneshot,
    pub root: bool,
    /// Daemon status; `None` when no proxy runs (or it is unreachable).
    pub status: Option<Status>,
    /// Why the daemon could not be queried, other than simply not running.
    pub error: Option<String>,
    pub service: Option<ServiceStatus>,
    pub busy: Option<(Action, Instant)>,
    pub flash: Option<Flash>,
    pub show_password: bool,
    pub conns: TableState,
    pub form: Option<SettingsForm>,
    pub menu: Option<RunMenu>,
    pub log: Option<LogView>,
    pub quit_confirm: bool,
    worker: Worker,
    last_poll: Option<Instant>,
    quit: bool,
}

impl App {
    pub fn new(manager: Manager, oneshot: Oneshot) -> Self {
        App {
            worker: Worker::spawn(manager.dir().to_path_buf()),
            manager,
            oneshot,
            root: is_root(),
            status: None,
            error: None,
            service: None,
            busy: None,
            flash: None,
            show_password: false,
            conns: TableState::default(),
            form: None,
            menu: None,
            log: None,
            quit_confirm: false,
            last_poll: None,
            quit: false,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            self.drain_worker();
            if !self.worker.pending
                && self.busy.is_none()
                && self
                    .worker
                    .last
                    .is_none_or(|t| t.elapsed() >= SERVICE_POLL_EVERY)
            {
                self.worker.send(WorkerCmd::Refresh);
            }
            if self.last_poll.is_none_or(|t| t.elapsed() >= POLL_EVERY) {
                self.poll();
            }
            if let Some(log) = self.log.as_mut() {
                log.tick();
            }
            terminal.draw(|f| self.draw(f))?;
            let wait = if self.busy.is_some() { 100 } else { 200 };
            if event::poll(Duration::from_millis(wait))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key);
                    }
                }
            }
        }
        Ok(())
    }

    fn drain_worker(&mut self) {
        while let Ok(event) = self.worker.rx.try_recv() {
            match event {
                WorkerEvent::Status(status) => {
                    self.service = Some(status);
                    self.worker.pending = false;
                }
                WorkerEvent::Done(action, result) => {
                    self.busy = None;
                    self.last_poll = None;
                    match result {
                        Ok(msg) => self.set_flash(msg, true),
                        Err(e) => {
                            let label = action.label();
                            self.set_flash(
                                tr!(
                                    format!("Service {label} failed: {e}"),
                                    format!("{label}失败: {e}")
                                ),
                                false,
                            )
                        }
                    }
                }
            }
        }
    }

    fn poll(&mut self) {
        self.last_poll = Some(Instant::now());
        match self.manager.status() {
            Ok(status) => {
                self.status = Some(status);
                self.error = None;
            }
            Err(e) => {
                self.status = None;
                self.error = (!is_not_running(&e)).then(|| format!("{e:#}"));
            }
        }
    }

    pub fn set_flash(&mut self, text: impl Into<String>, ok: bool) {
        self.flash = Some(Flash {
            text: text.into(),
            ok,
            at: Instant::now(),
        });
    }

    pub fn mode(&self) -> Option<RunMode> {
        self.status.as_ref().map(|s| s.mode)
    }

    pub fn service_running(&self) -> bool {
        self.service
            .as_ref()
            .is_some_and(|s| s.installed && s.state == manage::RunState::Running)
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.menu.is_some() {
            self.on_menu_key(key);
            return;
        }
        if self.quit_confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.quit = true,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.quit_confirm = false
                }
                _ => {}
            }
            return;
        }
        if let Some(form) = self.form.as_mut() {
            match form.on_key(key) {
                FormEvent::None => {}
                FormEvent::Cancel => self.form = None,
                FormEvent::Submit => self.submit_form(),
            }
            return;
        }
        if let Some(log) = self.log.as_mut() {
            if !log.on_key(key) {
                self.log = None;
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if let Some((action, _)) = self.busy {
                    let progress = action.progress();
                    self.set_flash(
                        tr!(
                            format!("{progress}… wait for it to finish (Ctrl+C forces quit)"),
                            format!("{progress}…，请等待完成（Ctrl+C 强制退出）")
                        ),
                        false,
                    );
                } else if self.oneshot.is_running() {
                    self.quit_confirm = true;
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('s') => self.open_menu(),
            KeyCode::Char('e') | KeyCode::Enter => {
                let purpose = if self.status.is_some() {
                    Purpose::Live
                } else {
                    Purpose::Offline
                };
                self.open_form(purpose);
            }
            KeyCode::Char('l') => self.log = Some(LogView::new(self.manager.log_path())),
            KeyCode::Char('p') => self.show_password = !self.show_password,
            KeyCode::Char('L') => self.switch_language(),
            KeyCode::Char('r') => match self.manager.reload() {
                Ok(_) => {
                    let path = self.manager.config_path().display().to_string();
                    self.set_flash(
                        tr!(format!("Reloaded {path}"), format!("已重新读取 {path}")),
                        true,
                    );
                    self.poll();
                }
                Err(e) => self.set_flash(format!("{e:#}"), false),
            },
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => self.conns.select(None),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: i64) {
        let len = self.status.as_ref().map_or(0, |s| s.conns.len());
        if len == 0 {
            self.conns.select(None);
            return;
        }
        let current = self.conns.selected().map(|i| i as i64).unwrap_or(-1);
        let next = (current + delta).clamp(0, len as i64 - 1);
        self.conns.select(Some(next as usize));
    }

    /// Switches between English and Chinese and saves the choice as
    /// `[ui] language` in the shared config file.
    fn switch_language(&mut self) {
        let lang = i18n::current().other();
        i18n::set(lang);
        let update = Update {
            language: Some(lang),
            ..Default::default()
        };
        match self.manager.update(&update) {
            Ok(_) => self.set_flash(tr!("Language: English", "语言：中文"), true),
            Err(e) => self.set_flash(
                tr!(
                    format!("Switched for this session; saving failed: {e:#}"),
                    format!("已切换，但未能保存：{e:#}")
                ),
                false,
            ),
        }
    }

    /// The footer hint for `L`: the language it switches to.
    pub fn language_hint() -> (&'static str, &'static str) {
        ("L", tr!("中文", "English"))
    }

    pub fn open_form(&mut self, purpose: Purpose) {
        match self.manager.settings() {
            Ok((settings, _)) => self.form = Some(SettingsForm::new(purpose, settings)),
            Err(e) => self.set_flash(
                tr!(
                    format!("cannot read settings: {e:#}"),
                    format!("无法读取设置：{e:#}")
                ),
                false,
            ),
        }
    }

    fn submit_form(&mut self) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let update = match form.update() {
            Ok(update) => update,
            Err(e) => {
                form.error = Some(e);
                return;
            }
        };
        if form.purpose == Purpose::StartOneshot {
            self.form = None;
            self.start_oneshot(&update);
            return;
        }
        match self.manager.update(&update) {
            Ok(applied) => {
                self.form = None;
                let path = self.manager.config_path().display();
                let text = match applied {
                    Applied::Live => {
                        tr!("Settings saved and applied", "设置已保存并生效").to_string()
                    }
                    Applied::Saved => tr!(
                        format!("Saved to {path}; used from the next start"),
                        format!("已保存到 {path}，下次启动生效")
                    ),
                };
                self.set_flash(text, true);
                self.poll();
            }
            Err(e) => form.error = Some(format!("{e:#}")),
        }
    }

    /// Saves `update` and starts the proxy inside this panel (one-shot mode).
    pub fn start_oneshot(&mut self, update: &Update) {
        let result = self
            .manager
            .prepare_start(update)
            .and_then(|_| self.oneshot.start(self.manager.config_path()));
        match result {
            Ok(()) => self.set_flash(
                tr!(
                    "One-shot mode started: the proxy stops when the panel quits",
                    "一次性模式已启动：退出面板代理即停止"
                ),
                true,
            ),
            Err(e) => self.set_flash(
                tr!(
                    format!("one-shot start failed: {e:#}"),
                    format!("一次性启动失败: {e:#}")
                ),
                false,
            ),
        }
        self.poll();
    }

    /// Stops the one-shot proxy: the one in this panel, or a background one.
    pub fn stop_oneshot(&mut self) {
        let result = if self.oneshot.stop() {
            Ok(())
        } else {
            self.manager.stop().map(|_| ())
        };
        match result {
            Ok(()) => self.set_flash(tr!("One-shot proxy stopped", "一次性代理已停止"), true),
            Err(e) => self.set_flash(
                tr!(format!("stop failed: {e:#}"), format!("停止失败：{e:#}")),
                false,
            ),
        }
        self.poll();
    }

    pub fn start_service_action(&mut self, action: Action) {
        if self.busy.is_none() && self.worker.send(WorkerCmd::Run(action)) {
            self.busy = Some((action, Instant::now()));
            self.flash = None;
        }
    }
}
