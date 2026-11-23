//! 日志视图：全屏跟随共享日志文件。

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::theme::{rounded, DIM};
use crate::manage;
use crate::tr;

const MAX_LINES: usize = 5000;
const REFRESH: Duration = Duration::from_millis(500);

pub struct LogView {
    path: PathBuf,
    lines: Vec<String>,
    /// Lines scrolled up from the bottom; 0 means showing the newest.
    scroll: usize,
    follow: bool,
    signature: Option<(u64, SystemTime)>,
    last_read: Option<Instant>,
    error: Option<String>,
}

impl LogView {
    pub fn new(path: PathBuf) -> Self {
        let mut view = LogView {
            path,
            lines: Vec::new(),
            scroll: 0,
            follow: true,
            signature: None,
            last_read: None,
            error: None,
        };
        view.refresh();
        view
    }

    pub fn tick(&mut self) {
        if self.last_read.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        self.last_read = Some(Instant::now());
        let signature = std::fs::metadata(&self.path)
            .ok()
            .map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        if signature.is_some() && signature == self.signature {
            return;
        }
        match manage::tail_log(&self.path, MAX_LINES) {
            Ok((lines, _)) => {
                let added = lines.len().saturating_sub(self.lines.len());
                if !self.follow {
                    // Keep the viewport on the same lines while new ones arrive.
                    self.scroll = (self.scroll + added).min(lines.len().saturating_sub(1));
                }
                self.lines = lines;
                self.error = None;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.lines.clear();
                self.error = Some(
                    tr!(
                        "No log file yet (written once the proxy starts)",
                        "日志文件尚未生成（启动代理后会写入）"
                    )
                    .into(),
                );
            }
            Err(e) => {
                self.error = Some(tr!(
                    format!("cannot read the log: {e}"),
                    format!("无法读取日志：{e}")
                ))
            }
        }
        self.signature = signature;
    }

    fn scroll_by(&mut self, delta: i64) {
        let max = self.lines.len().saturating_sub(1) as i64;
        self.scroll = (self.scroll as i64 + delta).clamp(0, max) as usize;
        self.follow = self.scroll == 0;
    }

    /// Handles a key; returns false when the view should close.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => return false,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(-1),
            KeyCode::PageUp => self.scroll_by(20),
            KeyCode::PageDown => self.scroll_by(-20),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_by(i64::MAX / 2),
            KeyCode::End | KeyCode::Char('G') => self.scroll_by(i64::MIN / 2),
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                if self.follow {
                    self.scroll = 0;
                }
            }
            _ => {}
        }
        true
    }

    pub fn keys() -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓", tr!("scroll", "滚动")),
            ("PgUp/PgDn", tr!("page", "翻页")),
            ("Home/End", tr!("oldest/newest", "最早/最新")),
            ("f", tr!("follow", "跟随")),
            ("l/Esc", tr!("back", "返回")),
        ]
    }

    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let height = area.height.saturating_sub(2) as usize;
        let total = self.lines.len();
        let end = total.saturating_sub(self.scroll);
        let start = end.saturating_sub(height);
        let lines: Vec<Line> = match &self.error {
            Some(err) => vec![Line::from(Span::styled(
                err.clone(),
                Style::new().fg(Color::Yellow),
            ))],
            None => self.lines[start..end]
                .iter()
                .map(|l| Line::from(Span::styled(l.clone(), line_style(l))))
                .collect(),
        };
        let scroll = self.scroll;
        let state = if self.follow {
            tr!("following", "跟随最新").to_string()
        } else {
            tr!(
                format!("paused · {scroll} lines up"),
                format!("已暂停 · 向上 {scroll} 行")
            )
        };
        let path = self.path.display();
        let title = tr!(
            format!(" Log · {path} · {total} lines · {state} "),
            format!(" 日志 · {path} · {total} 行 · {state} ")
        );
        f.render_widget(Paragraph::new(lines).block(rounded(title)), area);
    }
}

fn line_style(line: &str) -> Style {
    if line.contains(" ERROR ") {
        Style::new().fg(Color::Red)
    } else if line.contains(" WARN ") {
        Style::new().fg(Color::Yellow)
    } else if line.contains(" DEBUG ") || line.contains(" TRACE ") {
        Style::new().fg(DIM)
    } else if line.contains(" INFO close ") {
        Style::new().fg(Color::Gray)
    } else {
        Style::new().fg(Color::White)
    }
}
