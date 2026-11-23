//! 颜色、文字对齐、弹窗和按键提示等公共绘制工具。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Frame;

use crate::api::RunMode;
use crate::tr;

pub const ACCENT: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn dim(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(DIM))
}

pub fn bold(color: Color) -> Style {
    Style::new().fg(color).add_modifier(Modifier::BOLD)
}

pub fn rounded(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.into())
}

pub use crate::pad;

/// Draws `lines` in a bordered popup centred on `area`.
pub fn popup(
    f: &mut Frame,
    area: Rect,
    width: u16,
    title: String,
    border: Color,
    lines: Vec<Line>,
) {
    let width = width.min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    let block = rounded(title).border_style(Style::new().fg(border));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Footer line of `key → description` hints.
pub fn key_hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, desc) in keys {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(Color::Black).bg(DIM),
        ));
        spans.push(Span::raw(format!(" {desc}  ")));
    }
    Line::from(spans)
}

/// Coloured badge naming the run mode; `embedded` means this panel runs it.
pub fn mode_badge(mode: Option<RunMode>, embedded: bool) -> Span<'static> {
    let (text, fg, bg) = match mode {
        Some(RunMode::Service) => (
            tr!(" SERVICE · background ", " 服务模式 · 常驻后台 "),
            Color::Black,
            Color::Green,
        ),
        Some(RunMode::Oneshot) if embedded => (
            tr!(" ONE-SHOT · exits with panel ", " 一次性模式 · 随面板退出 "),
            Color::Black,
            Color::Magenta,
        ),
        Some(RunMode::Oneshot) => (
            tr!(
                " ONE-SHOT · exits with process ",
                " 一次性模式 · 随进程退出 "
            ),
            Color::Black,
            Color::Magenta,
        ),
        None => (
            tr!(" NOT RUNNING ", " 未运行 "),
            Color::White,
            Color::DarkGray,
        ),
    };
    Span::styled(
        text,
        Style::new().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}
