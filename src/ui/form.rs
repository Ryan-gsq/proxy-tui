//! 设置表单：端口、用户名、密码、协议开关、出口策略。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::theme::{dim, pad, popup, ACCENT};
use crate::config::{random_password, Protocols, Settings, Update};
use crate::tr;

/// What saving the form does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// The proxy runs: the change applies immediately.
    Live,
    /// Nothing runs: the config file is edited for the next start.
    Offline,
    /// Confirms settings, then starts the one-shot proxy.
    StartOneshot,
}

pub enum FormEvent {
    None,
    Cancel,
    Submit,
}

/// Field order: port, username, password, http, https, socks5, udp, policy.
const FIELD_COUNT: usize = 8;
const FIELD_PROTO_FIRST: usize = 3;
const FIELD_POLICY: usize = 7;
fn proto_info() -> [(&'static str, &'static str); 4] {
    [
        (
            "HTTP",
            tr!(
                "plain HTTP proxy (incl. CONNECT)",
                "明文 HTTP 代理（含 CONNECT）"
            ),
        ),
        (
            "HTTPS",
            tr!(
                "TLS-encrypted proxy (self-signed cert)",
                "TLS 加密的代理连接（自签证书）"
            ),
        ),
        ("SOCKS5", "SOCKS5 TCP"),
        (
            "UDP",
            tr!(
                "SOCKS5 UDP relay (needs SOCKS5)",
                "SOCKS5 UDP 转发（需 SOCKS5）"
            ),
        ),
    ]
}

pub struct SettingsForm {
    pub purpose: Purpose,
    base: Settings,
    port: String,
    username: String,
    password: String,
    protocols: Protocols,
    policy: crate::config::EgressPolicy,
    focus: usize,
    pub error: Option<String>,
}

impl SettingsForm {
    pub fn new(purpose: Purpose, base: Settings) -> Self {
        SettingsForm {
            purpose,
            port: base.port.to_string(),
            username: base.username.clone(),
            password: base.password.clone(),
            protocols: base.protocols,
            policy: base.policy,
            base,
            focus: 0,
            error: None,
        }
    }

    /// The fields changed relative to the settings the form opened with.
    pub fn update(&self) -> Result<Update, String> {
        let port = match self.port.parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => return Err(tr!("the port must be 1-65535", "端口必须是 1-65535").into()),
        };
        if self.username.is_empty() != self.password.is_empty() {
            return Err(tr!(
                "fill in username and password together (both empty means no auth)",
                "用户名和密码需同时填写（都留空表示不认证）"
            )
            .into());
        }
        if let Err(e) = self.protocols.validate() {
            return Err(tr!(
                format!("invalid protocols: {e}"),
                format!("协议设置无效：{e}")
            ));
        }
        let edited = Settings {
            port,
            username: self.username.clone(),
            password: self.password.clone(),
            protocols: self.protocols,
            policy: self.policy,
        };
        Ok(Update::between(&self.base, &edited))
    }

    pub fn on_key(&mut self, key: KeyEvent) -> FormEvent {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Enter => return FormEvent::Submit,
            KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % FIELD_COUNT,
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + FIELD_COUNT - 1) % FIELD_COUNT
            }
            KeyCode::Char('r') if ctrl && self.focus == 2 => self.password = random_password(16),
            KeyCode::Char('u') if ctrl => {
                if let Some(text) = self.text_field() {
                    text.clear();
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focus >= FIELD_PROTO_FIRST =>
            {
                if self.focus < FIELD_POLICY {
                    self.protocols.toggle(self.focus - FIELD_PROTO_FIRST);
                } else if key.code == KeyCode::Left {
                    self.policy = self.policy.prev();
                } else {
                    self.policy = self.policy.next();
                }
            }
            KeyCode::Backspace => {
                if let Some(text) = self.text_field() {
                    text.pop();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                let accept = match self.focus {
                    0 => c.is_ascii_digit() && self.port.len() < 5,
                    1 => !c.is_whitespace() && c != ':',
                    _ => !c.is_control(),
                };
                if accept {
                    if let Some(text) = self.text_field() {
                        text.push(c);
                    }
                }
            }
            _ => {}
        }
        FormEvent::None
    }

    fn text_field(&mut self) -> Option<&mut String> {
        match self.focus {
            0 => Some(&mut self.port),
            1 => Some(&mut self.username),
            2 => Some(&mut self.password),
            _ => None,
        }
    }

    pub fn keys() -> Vec<(&'static str, &'static str)> {
        vec![
            ("Tab/↑↓", tr!("field", "切换字段")),
            (tr!("←→/Space", "←→/空格"), tr!("toggle", "切换选项")),
            ("Ctrl+R", tr!("random password", "随机密码")),
            ("Ctrl+U", tr!("clear", "清空")),
            ("Enter", tr!("save", "保存")),
            ("Esc", tr!("cancel", "取消")),
        ]
    }

    /// `config_path` is shown when saving goes to the file.
    pub fn draw(&self, f: &mut Frame, area: Rect, config_path: &std::path::Path) {
        let field = |idx: usize, name: &str, value: String, hint: &str| {
            let focused = self.focus == idx;
            let style = if focused {
                Style::new().fg(Color::Black).bg(ACCENT)
            } else {
                Style::new().fg(Color::White)
            };
            let cursor = if focused && idx < FIELD_PROTO_FIRST {
                "▏"
            } else {
                ""
            };
            Line::from(vec![
                Span::styled(if focused { "▶ " } else { "  " }, Style::new().fg(ACCENT)),
                dim(pad(name, 10)),
                Span::styled(format!(" {value}{cursor} "), style),
                dim(format!("  {hint}")),
            ])
        };
        let switch = |on: bool| {
            if on {
                tr!("☑ on", "☑ 开启")
            } else {
                tr!("☐ off", "☐ 关闭")
            }
            .to_string()
        };
        let mut lines = vec![
            Line::from(""),
            field(0, tr!("Port", "端口"), self.port.clone(), "1-65535"),
            field(
                1,
                tr!("Username", "用户名"),
                self.username.clone(),
                tr!("empty = no auth", "留空=不认证"),
            ),
            field(
                2,
                tr!("Password", "密码"),
                self.password.clone(),
                tr!("Ctrl+R random", "Ctrl+R 随机"),
            ),
        ];
        for (i, (name, hint)) in proto_info().iter().enumerate() {
            lines.push(field(
                FIELD_PROTO_FIRST + i,
                name,
                switch(self.protocols.get(i)),
                hint,
            ));
        }
        lines.push(field(
            FIELD_POLICY,
            tr!("Policy", "出口策略"),
            self.policy.as_str().into(),
            tr!("←→ change", "←→ 切换"),
        ));
        lines.push(Line::from(""));
        let (title, note, border) = match self.purpose {
            Purpose::Live => (
                tr!(
                    " Edit settings · Enter saves and applies · Esc cancels ",
                    " 编辑设置 · Enter 保存并立即生效 · Esc 取消 "
                ),
                tr!(
                    "Changing the port rebinds the listeners; open connections stay",
                    "修改端口会重建监听；已有连接不受影响"
                )
                .to_string(),
                ACCENT,
            ),
            Purpose::Offline => (
                tr!(
                    " Edit config file · Enter saves · Esc cancels ",
                    " 编辑配置文件 · Enter 保存 · Esc 取消 "
                ),
                {
                    let path = config_path.display();
                    tr!(
                        format!("Not running: saves to {path} for next start"),
                        format!("代理未运行：保存到 {path}，下次启动生效")
                    )
                },
                ACCENT,
            ),
            Purpose::StartOneshot => (
                tr!(
                    " Start one-shot · Enter saves and starts · Esc cancels ",
                    " 一次性启动 · 确认设置 · Enter 保存并启动 · Esc 取消 "
                ),
                tr!(
                    "Stops when the panel quits; settings become the next defaults",
                    "退出面板即停止；设置会保存为下次的默认值"
                )
                .to_string(),
                Color::Magenta,
            ),
        };
        lines.push(Line::from(dim(format!("  {note}"))));
        if let Some(err) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  ✗ {err}"),
                Style::new().fg(Color::Red),
            )));
        }
        popup(f, area, 72, title.to_string(), border, lines);
    }
}
