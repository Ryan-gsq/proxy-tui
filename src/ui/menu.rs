//! 运行管理菜单：一次性模式启动 / 停止，服务安装 / 启停 / 卸载；以及退出确认。

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::app::App;
use super::form::Purpose;
use super::theme::{bold, dim, pad, popup, ACCENT, DIM};
use crate::api::RunMode;
use crate::config::admin_name;
use crate::manage::{service, Action};
use crate::tr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    OnceStart,
    OnceStop,
    Service(Action),
}

const ITEMS: [MenuItem; 7] = [
    MenuItem::OnceStart,
    MenuItem::OnceStop,
    MenuItem::Service(Action::Start),
    MenuItem::Service(Action::Stop),
    MenuItem::Service(Action::Restart),
    MenuItem::Service(Action::Install),
    MenuItem::Service(Action::Uninstall),
];

pub struct RunMenu {
    selected: usize,
    confirm: Option<MenuItem>,
}

impl RunMenu {
    pub fn confirming(&self) -> bool {
        self.confirm.is_some()
    }

    pub fn keys() -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓/1-7", tr!("select", "选择")),
            ("Enter", tr!("run", "执行")),
            ("Esc", tr!("close", "关闭")),
        ]
    }

    pub fn confirm_keys() -> Vec<(&'static str, &'static str)> {
        vec![
            ("y", tr!("confirm", "确认")),
            ("n/Esc", tr!("cancel", "取消")),
        ]
    }
}

impl App {
    /// A one-shot proxy that runs in some other process.
    fn other_oneshot(&self) -> bool {
        !self.oneshot.is_running() && self.mode() == Some(RunMode::Oneshot)
    }

    pub fn open_menu(&mut self) {
        if self.service.is_none() {
            self.set_flash(tr!("Reading service status…", "正在读取服务状态…"), true);
            return;
        }
        let item = if self.oneshot.is_running() || self.other_oneshot() {
            MenuItem::OnceStop
        } else if self.mode() == Some(RunMode::Service) {
            MenuItem::Service(Action::Restart)
        } else if self.service.as_ref().is_some_and(|s| s.installed) && !self.service_running() {
            MenuItem::Service(Action::Start)
        } else {
            MenuItem::OnceStart
        };
        self.menu = Some(RunMenu {
            selected: ITEMS.iter().position(|i| *i == item).unwrap_or(0),
            confirm: None,
        });
    }

    fn item_label(&self, item: MenuItem) -> &'static str {
        let installed = self.service.as_ref().is_some_and(|s| s.installed);
        match item {
            MenuItem::OnceStart => tr!("Start", "启动"),
            MenuItem::OnceStop => tr!("Stop", "停止"),
            MenuItem::Service(Action::Start) => tr!("Start service", "启动服务"),
            MenuItem::Service(Action::Stop) => tr!("Stop service", "停止服务"),
            MenuItem::Service(Action::Restart) => tr!("Restart service", "重启服务"),
            MenuItem::Service(Action::Install) if installed => {
                tr!("Reinstall (update program)", "重新安装服务（更新程序）")
            }
            MenuItem::Service(Action::Install) if self.oneshot.is_running() => {
                tr!("Install (keep it running)", "安装服务（转为常驻）")
            }
            MenuItem::Service(Action::Install) => {
                tr!("Install (background, boot)", "安装服务（常驻 + 开机自启）")
            }
            MenuItem::Service(Action::Uninstall) => tr!("Uninstall service", "卸载服务"),
        }
    }

    fn item_availability(&self, item: MenuItem) -> Result<(), &'static str> {
        let embedded = self.oneshot.is_running();
        let stoppable = self.status.as_ref().is_some_and(|s| s.stoppable);
        match item {
            MenuItem::OnceStart if embedded => Err(tr!("already running", "已在运行")),
            MenuItem::OnceStart
                if self.mode() == Some(RunMode::Service) || self.service_running() =>
            {
                Err(tr!("the service is running", "服务正在运行"))
            }
            MenuItem::OnceStart if self.other_oneshot() => {
                Err(tr!("running in another process", "已在另一个进程中运行"))
            }
            MenuItem::OnceStart => Ok(()),
            MenuItem::OnceStop if embedded || (self.other_oneshot() && stoppable) => Ok(()),
            MenuItem::OnceStop if self.other_oneshot() => {
                Err(tr!("runs in another panel", "运行在另一个面板中"))
            }
            MenuItem::OnceStop => Err(tr!("not running", "未运行")),
            MenuItem::Service(action) => {
                let svc = self.service.as_ref().ok_or(tr!("loading", "读取中"))?;
                service::availability(action, svc)?;
                if self.other_oneshot()
                    && matches!(action, Action::Start | Action::Restart | Action::Install)
                {
                    return Err(tr!("stop the one-shot proxy first", "请先停止一次性代理"));
                }
                Ok(())
            }
        }
    }

    /// Handing over from the panel's one-shot proxy to the service stops it first.
    fn replaces_oneshot(&self, item: MenuItem) -> bool {
        self.oneshot.is_running()
            && matches!(
                item,
                MenuItem::Service(Action::Start | Action::Restart | Action::Install)
            )
    }

    fn item_confirm(&self, item: MenuItem) -> Option<&'static str> {
        if self.replaces_oneshot(item) {
            return Some(tr!(
                "Stops the one-shot proxy first (dropping connections), then hands over to the service",
                "会先停止一次性代理（断开连接），再交给服务常驻运行"
            ));
        }
        match item {
            MenuItem::OnceStart => None,
            MenuItem::OnceStop => Some(tr!(
                "Stopping the one-shot proxy drops every proxied connection",
                "停止一次性代理会断开所有代理连接"
            )),
            MenuItem::Service(action) => service::confirm_text(action, self.service.as_ref()?),
        }
    }

    pub fn on_menu_key(&mut self, key: KeyEvent) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if let Some(item) = menu.confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.menu = None;
                    self.execute(item);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => menu.confirm = None,
                _ => {}
            }
            return;
        }
        let count = ITEMS.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => self.menu = None,
            KeyCode::Up | KeyCode::Char('k') => menu.selected = (menu.selected + count - 1) % count,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                menu.selected = (menu.selected + 1) % count
            }
            KeyCode::Char(c @ '1'..='7') => {
                let selected = c as usize - '1' as usize;
                menu.selected = selected;
                self.activate(ITEMS[selected]);
            }
            KeyCode::Enter => {
                let item = ITEMS[menu.selected];
                self.activate(item);
            }
            _ => {}
        }
    }

    fn activate(&mut self, item: MenuItem) {
        if self.busy.is_some() {
            self.set_flash(
                tr!("A service action is already running", "已有服务操作在执行"),
                false,
            );
            return;
        }
        if let Err(reason) = self.item_availability(item) {
            let label = self.item_label(item);
            self.set_flash(
                tr!(
                    format!("Cannot {}: {reason}", label.to_lowercase()),
                    format!("无法{label}：{reason}")
                ),
                false,
            );
            return;
        }
        if self.item_confirm(item).is_some() {
            if let Some(menu) = self.menu.as_mut() {
                menu.confirm = Some(item);
            }
        } else {
            self.menu = None;
            self.execute(item);
        }
    }

    fn execute(&mut self, item: MenuItem) {
        match item {
            MenuItem::OnceStart => self.open_form(Purpose::StartOneshot),
            MenuItem::OnceStop => self.stop_oneshot(),
            MenuItem::Service(action) => {
                if self.replaces_oneshot(item) {
                    self.oneshot.stop();
                }
                self.start_service_action(action);
            }
        }
    }

    pub fn draw_menu(&self, f: &mut Frame, area: Rect) {
        let Some(menu) = &self.menu else {
            return;
        };
        let manager = self.service.as_ref().map_or("…", |s| s.manager);
        let item_line = |idx: usize, item: MenuItem| {
            let selected = idx == menu.selected;
            let available = self.item_availability(item);
            let style = match (selected, available.is_ok()) {
                (true, true) => Style::new()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
                (true, false) => Style::new().fg(Color::Black).bg(DIM),
                (false, true) => Style::new().fg(Color::White),
                (false, false) => Style::new().fg(DIM),
            };
            let mut spans = vec![
                Span::styled(
                    if selected { " ▶ " } else { "   " },
                    Style::new().fg(ACCENT),
                ),
                dim(format!("{} ", idx + 1)),
                Span::styled(format!(" {} ", pad(self.item_label(item), 28)), style),
            ];
            if let Err(reason) = available {
                spans.push(dim(format!("  {reason}")));
            }
            Line::from(spans)
        };
        let mut lines = vec![Line::from(Span::styled(
            {
                let admin = admin_name();
                tr!(
                    format!(" One-shot mode  exits with its process · no install, no {admin}"),
                    format!(" 一次性模式  随启动它的进程退出 · 无需安装和{admin}")
                )
            },
            bold(Color::Magenta),
        ))];
        for (idx, item) in ITEMS.iter().enumerate() {
            if idx == 2 {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    {
                        let admin = admin_name();
                        tr!(
                            format!(" Service mode  {manager} · background · starts at boot · needs {admin}"),
                            format!(" 服务模式  {manager} 托管，常驻后台、开机自启 · 需要{admin}")
                        )
                    },
                    bold(Color::Green),
                )));
            }
            lines.push(item_line(idx, *item));
        }
        lines.push(Line::from(""));
        match menu.confirm {
            Some(item) => {
                let warn = self.item_confirm(item).unwrap_or("");
                lines.push(Line::from(Span::styled(
                    format!("  ⚠ {warn}"),
                    Style::new().fg(Color::Yellow),
                )));
                let label = self.item_label(item);
                lines.push(Line::from(vec![
                    Span::styled(
                        tr!(format!("  {label}? "), format!("  确认「{label}」？ ")),
                        bold(Color::Red),
                    ),
                    Span::raw(tr!("y confirm / n cancel", "y 确认 / n 取消")),
                ]));
            }
            None => lines.push(Line::from(dim(tr!(
                "  ↑↓/number select · Enter run · Esc close",
                "  ↑↓/数字 选择 · Enter 执行 · Esc 关闭"
            )))),
        }
        popup(
            f,
            area,
            76,
            tr!(" Run mode ", " 运行管理 ").into(),
            ACCENT,
            lines,
        );
    }

    pub fn draw_quit_confirm(&self, f: &mut Frame, area: Rect) {
        let active = self
            .status
            .as_ref()
            .map_or(0, |s| s.active_tcp + s.active_udp);
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                tr!(
                    "  One-shot mode: quitting the panel also stops the proxy",
                    "  一次性模式：退出面板会同时停止代理"
                ),
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from(tr!(
                format!("  All {active} active connections will be dropped"),
                format!("  当前 {active} 个活动连接将全部断开")
            )),
            Line::from(""),
            Line::from(dim(tr!(
                "  Keep it running? Press n, then s → 6 to install the service",
                "  想让代理常驻后台？按 n 返回，再按 s → 6 安装服务"
            ))),
            Line::from(""),
            Line::from(vec![
                Span::styled("  y ", bold(Color::Red)),
                Span::raw(tr!("quit and stop the proxy    ", "退出并停止代理    ")),
                Span::styled("n ", Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(tr!("cancel", "取消")),
            ]),
        ];
        popup(
            f,
            area,
            66,
            tr!(" Quit ", " 退出确认 ").into(),
            Color::Magenta,
            lines,
        );
    }
}
