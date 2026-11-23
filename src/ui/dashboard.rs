//! 主界面：标题栏、运行模式、设置、流量、网卡、连接、事件、未运行提示和底栏。

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Sparkline, Table, Wrap};
use ratatui::Frame;

use super::app::App;
use super::form::SettingsForm;
use super::logview::LogView;
use super::menu::RunMenu;
use super::theme::{bold, dim, key_hints, mode_badge, pad, rounded, ACCENT, DIM, SPINNER};
use crate::api::{RunMode, Status};
use crate::config::admin_name;
use crate::manage::RunState;
use crate::{fmt_bytes, fmt_duration, tr};

impl App {
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let status = self.status.clone();
        let body_rows = match &status {
            Some(s) if self.log.is_none() => {
                let iface_rows = s.ifaces.len().clamp(1, 8) as u16;
                vec![
                    Constraint::Length(10),
                    Constraint::Length(iface_rows + 3),
                    Constraint::Min(6),
                    Constraint::Length(7),
                ]
            }
            _ => vec![Constraint::Min(5)],
        };
        let mut rows = vec![Constraint::Length(1), Constraint::Length(4)];
        rows.extend(body_rows);
        rows.push(Constraint::Length(1));
        let areas = Layout::vertical(rows).split(area);
        self.draw_header(f, areas[0]);
        self.draw_runtime(f, areas[1]);
        self.draw_footer(f, areas[areas.len() - 1]);
        match (&self.log, &status) {
            (Some(log), _) => log.draw(f, areas[2]),
            (None, Some(s)) => {
                let [settings, traffic] =
                    Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)])
                        .areas(areas[2]);
                self.draw_settings(f, settings, s);
                draw_traffic(f, traffic, s);
                draw_ifaces(f, areas[3], s);
                self.draw_conns(f, areas[4], s);
                draw_events(f, areas[5], s);
            }
            (None, None) => self.draw_idle(f, areas[2]),
        }
        if let Some(form) = &self.form {
            form.draw(f, area, self.manager.config_path());
        }
        if self.menu.is_some() {
            self.draw_menu(f, area);
        }
        if self.quit_confirm {
            self.draw_quit_confirm(f, area);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled(
                " proxy-tui ",
                Style::new()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            mode_badge(self.mode(), self.oneshot.is_running()),
        ];
        spans.push(Span::raw(match &self.status {
            Some(s) => {
                let (version, pid, up) = (&s.version, s.pid, fmt_duration(s.uptime_secs));
                tr!(
                    format!(" v{version}  pid {pid}  up {up}  "),
                    format!(" v{version}  pid {pid}  运行 {up}  ")
                )
            }
            None => format!(" v{}  ", env!("CARGO_PKG_VERSION")),
        }));
        if let Some((action, started)) = self.busy {
            let frame = (started.elapsed().as_millis() / 100) as usize % SPINNER.len();
            spans.push(Span::styled(
                format!("{} {}…", SPINNER[frame], action.progress()),
                bold(Color::Yellow),
            ));
        } else if let Some(flash) = self.flash.as_ref().filter(|fl| fl.visible()) {
            let color = if flash.ok { Color::Green } else { Color::Red };
            spans.push(Span::styled(flash.text.clone(), Style::new().fg(color)));
        }
        f.render_widget(Line::from(spans), area);
    }

    fn draw_runtime(&self, f: &mut Frame, area: Rect) {
        let manager = self.service.as_ref().map_or("…", |s| s.manager);
        let embedded = self.oneshot.is_running();
        let detail = match &self.status {
            Some(s) if s.mode == RunMode::Service => {
                let pid = s.pid;
                tr!(
                    format!("  pid {pid}  managed by {manager}, runs in the background; quitting the panel leaves it running"),
                    format!("  pid {pid}  由 {manager} 托管，常驻后台；退出面板不影响")
                )
            }
            Some(s) if embedded => {
                let pid = s.pid;
                tr!(
                    format!(
                        "  pid {pid} (this panel)  stops when the panel quits · no start at boot"
                    ),
                    format!("  pid {pid}（本面板进程）  退出面板代理即停止 · 不会开机自启")
                )
            }
            Some(s) if s.stoppable => {
                let pid = s.pid;
                tr!(
                    format!("  pid {pid}  background one-shot process · stop: proxy-tui stop or s → 2 · no start at boot"),
                    format!("  pid {pid}  后台一次性进程 · proxy-tui stop 或 s → 2 停止 · 不会开机自启")
                )
            }
            Some(s) => {
                let pid = s.pid;
                tr!(
                    format!("  pid {pid}  runs in another panel and stops when that panel quits"),
                    format!("  pid {pid}  运行在另一个管理面板中，那个面板退出时代理停止")
                )
            }
            None => match &self.error {
                Some(err) => tr!(
                    format!("  cannot reach the daemon: {err}"),
                    format!("  无法连接守护进程：{err}")
                ),
                None if self.service_running() => {
                    tr!("  the service is starting…", "  服务正在启动…").to_string()
                }
                None => tr!(
                    "  proxy not running · press s: start one-shot or install the service",
                    "  代理未运行 · 按 s：一次性启动 或 安装为服务"
                )
                .to_string(),
            },
        };
        let current = Line::from(vec![
            dim(pad(tr!("Current", "当前"), 9)),
            mode_badge(self.mode(), embedded),
            Span::raw(detail),
        ]);

        let mut spans = vec![dim(pad(tr!("Service", "服务"), 9))];
        match &self.service {
            None => spans.push(dim(tr!("reading service status…", "读取服务状态…"))),
            Some(s) if !s.supported => {
                spans.push(Span::styled(
                    tr!("unavailable ", "不可用 "),
                    Style::new().fg(Color::Red),
                ));
                spans.push(dim(s.note.clone()));
            }
            Some(s) => {
                let (dot, color) = if !s.installed {
                    ("○", DIM)
                } else {
                    match s.state {
                        RunState::Running => ("●", Color::Green),
                        RunState::Stopped => ("○", Color::Yellow),
                        RunState::Failed => ("✗", Color::Red),
                        RunState::Busy(_) => ("◌", Color::Yellow),
                        RunState::Unknown => ("?", DIM),
                    }
                };
                spans.push(Span::styled(
                    format!("{dot} {}", s.state_label()),
                    bold(color),
                ));
                if let Some(pid) = s.pid {
                    spans.push(Span::raw(format!("  pid {pid}")));
                }
                if s.installed {
                    spans.push(dim(tr!("  start at boot ", "  开机自启 ")));
                    spans.push(if s.enabled {
                        Span::styled("✓", Style::new().fg(Color::Green))
                    } else {
                        Span::styled("✗", Style::new().fg(Color::Red))
                    });
                    spans.push(dim(format!("  {}", s.unit_path)));
                } else {
                    spans.push(dim(tr!(
                        "  once installed: runs in the background, starts at boot",
                        "  安装后常驻后台、开机自启"
                    )));
                }
            }
        }
        if !self.root {
            spans.push(dim(if cfg!(windows) {
                tr!(
                    "  · service actions need an Administrator terminal",
                    "  · 服务操作需要以管理员身份运行"
                )
            } else {
                tr!("  · service actions need sudo", "  · 服务操作需要 sudo")
            }));
        }
        f.render_widget(
            Paragraph::new(vec![current, Line::from(spans)]).block(rounded(tr!(
                " Run mode · s to manage ",
                " 运行模式 · s 管理 "
            ))),
            area,
        );
    }

    fn draw_idle(&self, f: &mut Frame, area: Rect) {
        let hint = |text: &str| {
            Line::from(Span::styled(
                text.to_string(),
                Style::new().fg(Color::White),
            ))
        };
        let dir = self.manager.dir().display();
        let mut text = vec![
            Line::from(""),
            Line::from(Span::styled(
                tr!("Proxy not running", "代理未运行"),
                bold(Color::Red),
            )),
            Line::from(""),
            Line::from(dim(tr!(
                format!("Data directory {dir}"),
                format!("数据目录 {dir}")
            ))),
            Line::from(""),
        ];
        if let Some(err) = &self.error {
            text.push(Line::from(Span::styled(
                err.clone(),
                Style::new().fg(Color::Yellow),
            )));
            text.push(Line::from(""));
        }
        match &self.service {
            Some(s) if s.installed && s.state == RunState::Running => text.push(hint(tr!(
                "The service is running; waiting for the daemon to get ready…",
                "服务正在运行，正在等待守护进程就绪…"
            ))),
            service => {
                let admin = admin_name();
                text.push(hint(&tr!(
                    format!("Press s → 1 to start one-shot: runs inside this panel and stops when it quits (no install, no {admin})"),
                    format!("按 s → 1 一次性启动：在本面板内运行，退出面板即停止（无需安装、无需{admin}）"),
                )));
                match service {
                    Some(s) if s.installed => text.push(hint(tr!(
                        "Press s → 3 to start the service: runs in the background (installed, starts at boot)",
                        "按 s → 3 启动服务：常驻后台（已安装、开机自启）"
                    ))),
                    Some(s) if s.supported => text.push(hint(&tr!(
                        format!("Press s → 6 to install the service: runs in the background, starts at boot (needs {admin})"),
                        format!("按 s → 6 安装服务：常驻后台、开机自启（需要{admin}）")
                    ))),
                    _ => {}
                }
            }
        }
        let install = if cfg!(windows) {
            tr!(
                "proxy-tui service install (as Administrator)",
                "proxy-tui service install（以管理员身份）"
            )
        } else {
            "sudo proxy-tui service install"
        };
        text.push(Line::from(dim(tr!(
            format!("CLI: proxy-tui run --detach (background one-shot) · {install} (service) · proxy-tui --help"),
            format!("命令行：proxy-tui run --detach（后台一次性）· {install}（服务）· proxy-tui --help"),
        ))));
        f.render_widget(
            Paragraph::new(text)
                .block(rounded(tr!(
                    " Not running · reconnecting every second ",
                    " 未运行 · 每秒自动重连 "
                )))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn draw_settings(&self, f: &mut Frame, area: Rect, s: &Status) {
        let set = &s.settings;
        let label = |t: &str| dim(pad(t, 10));
        let value = |t: String| Span::styled(t, bold(Color::White));
        let password = if set.password.is_empty() {
            tr!("(not set)", "(未设置)").to_string()
        } else if self.show_password {
            set.password.clone()
        } else {
            "•".repeat(set.password.chars().count().min(16))
        };
        let mut protocols = vec![label(tr!("Protocols", "协议"))];
        for (i, name) in ["HTTP", "HTTPS", "SOCKS5", "UDP"].iter().enumerate() {
            protocols.push(if set.protocols.get(i) {
                Span::styled(format!("✓{name}  "), bold(Color::Green))
            } else {
                dim(format!("✗{name}  "))
            });
        }
        if set.protocols.https && !s.tls_ready {
            protocols.push(Span::styled(
                tr!("HTTPS certificate unavailable", "HTTPS 证书不可用"),
                Style::new().fg(Color::Red),
            ));
        }
        let mut lines = vec![
            Line::from(vec![
                label(tr!("Port", "端口")),
                value(set.port.to_string()),
                dim(tr!(
                    "  mixed port (protocol detected automatically)",
                    "  mixed 端口（自动识别协议）"
                )),
            ]),
            Line::from(vec![
                label(tr!("Listening", "监听")),
                Span::raw(s.listening.join("  ")),
            ]),
            Line::from(vec![
                label(tr!("Username", "用户名")),
                value(if set.username.is_empty() {
                    tr!("(no auth)", "(无认证)").into()
                } else {
                    set.username.clone()
                }),
            ]),
            Line::from(vec![label(tr!("Password", "密码")), value(password)]),
            Line::from(protocols),
            Line::from(vec![
                label(tr!("Policy", "出口策略")),
                value(set.policy.as_str().to_string()),
                dim(format!("  {}", set.policy.describe())),
            ]),
            Line::from(vec![
                label(tr!("Routing", "策略路由")),
                Span::raw(if s.auto_policy_routing {
                    tr!("auto", "自动")
                } else {
                    tr!("off", "关闭")
                }),
            ]),
            Line::from(vec![
                label(tr!("Data dir", "数据目录")),
                Span::raw(self.manager.dir().display().to_string()),
                dim(tr!("  config / certs / logs", "  配置 / 证书 / logs")),
            ]),
        ];
        for err in &s.listen_errors {
            lines.push(Line::from(Span::styled(
                tr!(format!("listen failed {err}"), format!("监听失败 {err}")),
                Style::new().fg(Color::Red),
            )));
        }
        f.render_widget(
            Paragraph::new(lines).block(rounded(tr!(" Settings ", " 设置 "))),
            area,
        );
    }

    fn draw_conns(&mut self, f: &mut Frame, area: Rect, s: &Status) {
        let header = Row::new([
            tr!("Proto", "协议"),
            tr!("Client", "客户端"),
            tr!("Entry", "入口"),
            tr!("Target", "目标"),
            tr!("Egress", "出口"),
            tr!("Source", "出站地址"),
            "↑",
            "↓",
            tr!("Age", "时长"),
        ])
        .style(bold(ACCENT));
        let rows: Vec<Row> = s
            .conns
            .iter()
            .map(|c| {
                Row::new(vec![
                    Cell::from(c.proto.clone()),
                    Cell::from(c.client.clone()),
                    Cell::from(c.entry.clone()),
                    Cell::from(c.target.clone()),
                    Cell::from(c.egress.clone()),
                    Cell::from(c.local.clone()),
                    Cell::from(fmt_bytes(c.up)),
                    Cell::from(fmt_bytes(c.down)),
                    Cell::from(fmt_duration(c.age_secs)),
                ])
            })
            .collect();
        if self.conns.selected().is_some_and(|sel| sel >= rows.len()) {
            self.conns.select(rows.len().checked_sub(1));
        }
        let widths = [
            Constraint::Length(10),
            Constraint::Length(22),
            Constraint::Length(16),
            Constraint::Min(20),
            Constraint::Length(9),
            Constraint::Length(22),
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(7),
        ];
        let (tcp, udp, total) = (s.active_tcp, s.active_udp, s.total_conns);
        let title = tr!(
            format!(" Connections TCP {tcp} · UDP {udp} · total {total} "),
            format!(" 活动连接 TCP {tcp} · UDP {udp} · 累计 {total} ")
        );
        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(Style::new().bg(Color::Rgb(40, 40, 60)))
            .block(rounded(title));
        f.render_stateful_widget(table, area, &mut self.conns);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let keys: Vec<(&str, &str)> = if self.quit_confirm {
            vec![
                ("y", tr!("quit and stop the proxy", "退出并停止代理")),
                ("n/Esc", tr!("cancel", "取消")),
            ]
        } else if let Some(menu) = &self.menu {
            if menu.confirming() {
                RunMenu::confirm_keys()
            } else {
                RunMenu::keys()
            }
        } else if self.form.is_some() {
            SettingsForm::keys()
        } else if self.log.is_some() {
            LogView::keys()
        } else if self.status.is_none() {
            vec![
                ("s", tr!("start / service", "启动 / 服务管理")),
                ("e", tr!("edit config", "编辑配置")),
                ("l", tr!("logs", "日志")),
                App::language_hint(),
                ("q", tr!("quit", "退出")),
            ]
        } else {
            vec![
                ("e", tr!("edit settings", "编辑设置")),
                ("s", tr!("run mode", "运行管理")),
                ("l", tr!("logs", "日志")),
                ("p", tr!("show/hide password", "显示/隐藏密码")),
                ("r", tr!("reload config", "重载配置")),
                ("↑↓", tr!("select", "选择连接")),
                App::language_hint(),
                ("q", tr!("quit", "退出")),
            ]
        };
        f.render_widget(key_hints(&keys), area);
    }
}

fn draw_traffic(f: &mut Frame, area: Rect, s: &Status) {
    let block = rounded(tr!(" Traffic ", " 流量 "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [text, spark_down, spark_up] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(2),
        Constraint::Min(2),
    ])
    .areas(inner);
    let rate = |arrow: &str, color: Color, rate: u64, total: u64| {
        Line::from(vec![
            Span::styled(arrow.to_string(), bold(color)),
            Span::styled(
                format!("{:>12}/s", fmt_bytes(rate)),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            dim(tr!(
                format!("   total {}", fmt_bytes(total)),
                format!("   总计 {}", fmt_bytes(total))
            )),
        ])
    };
    let lines = vec![
        rate(
            tr!("↓ down ", "↓ 下载 "),
            Color::Green,
            s.rate_down,
            s.total_down,
        ),
        rate(
            tr!("↑ up   ", "↑ 上传 "),
            Color::Yellow,
            s.rate_up,
            s.total_up,
        ),
        Line::from(dim({
            let (tcp, udp, total) = (s.active_tcp, s.active_udp, s.total_conns);
            tr!(
                format!("active TCP {tcp}  UDP {udp}  total connections {total}"),
                format!("活动 TCP {tcp}  UDP {udp}  累计连接 {total}")
            )
        })),
    ];
    f.render_widget(Paragraph::new(lines), text);
    for (data, color, rect) in [
        (&s.history_down, Color::Green, spark_down),
        (&s.history_up, Color::Yellow, spark_up),
    ] {
        let skip = data.len().saturating_sub(rect.width as usize);
        f.render_widget(
            Sparkline::default()
                .data(&data[skip..])
                .style(Style::new().fg(color)),
            rect,
        );
    }
}

fn summarize(list: &[String]) -> String {
    match list.len() {
        0 => "-".into(),
        1 => list[0].clone(),
        n => format!("{} +{}", list[0], n - 1),
    }
}

fn draw_ifaces(f: &mut Frame, area: Rect, s: &Status) {
    let header = Row::new([
        tr!("NIC", "网卡"),
        "IPv4",
        "IPv6",
        tr!("Default", "默认路由"),
        tr!("v6 out", "v6出口"),
        tr!("Policy rt", "策略路由"),
        tr!("Conns", "连接"),
        tr!("↓ Rate", "↓速率"),
        tr!("↑ Rate", "↑速率"),
        tr!("↓ Total", "↓总计"),
        tr!("↑ Total", "↑总计"),
    ])
    .style(bold(ACCENT));
    let rows: Vec<Row> = s
        .ifaces
        .iter()
        .map(|i| {
            let defaults = match (i.default_v4, i.default_v6) {
                (true, true) => "v4+v6",
                (true, false) => "v4",
                (false, true) => "v6",
                (false, false) => "-",
            };
            let v6 = if i.v6_ok {
                Span::styled("✓", Style::new().fg(Color::Green))
            } else {
                dim("✗")
            };
            Row::new(vec![
                Cell::from(Span::styled(
                    i.name.clone(),
                    Style::new().add_modifier(Modifier::BOLD),
                )),
                Cell::from(summarize(&i.ipv4)),
                Cell::from(summarize(&i.ipv6)),
                Cell::from(defaults),
                Cell::from(v6),
                Cell::from(i.routing.clone()),
                Cell::from(i.active.to_string()),
                Cell::from(format!("{}/s", fmt_bytes(i.rate_down))),
                Cell::from(format!("{}/s", fmt_bytes(i.rate_up))),
                Cell::from(fmt_bytes(i.down)),
                Cell::from(fmt_bytes(i.up)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Min(24),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(16),
        Constraint::Length(5),
        Constraint::Length(13),
        Constraint::Length(13),
        Constraint::Length(11),
        Constraint::Length(11),
    ];
    let table = Table::new(rows, widths).header(header).block(rounded(tr!(
        " Egress (entry NIC = exit NIC) ",
        " 网络出口（入口网卡 = 出口网卡）"
    )));
    f.render_widget(table, area);
}

fn draw_events(f: &mut Frame, area: Rect, s: &Status) {
    let visible = area.height.saturating_sub(2) as usize;
    let skip = s.events.len().saturating_sub(visible);
    let lines: Vec<Line> = s.events[skip..]
        .iter()
        .map(|e| Line::from(Span::styled(e.clone(), Style::new().fg(Color::Gray))))
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(rounded(tr!(" Events ", " 事件 "))),
        area,
    );
}
