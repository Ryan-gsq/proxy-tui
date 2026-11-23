//! 交互层 · TUI（给人用）。只通过服务层 `manage` 管理代理。
//!
//! - `app`：状态、事件循环、按键分发、操作
//! - `dashboard`：主界面各面板
//! - `menu`：运行管理菜单与退出确认
//! - `form`：设置表单
//! - `logview`：日志视图
//! - `theme`：样式与弹窗工具

mod app;
mod dashboard;
mod form;
mod logview;
mod menu;
mod theme;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::{Config, Update};
use crate::i18n::{self, Language};
use crate::manage::{Manager, Oneshot};
use crate::tr;

/// Opens the panel on data directory `dir`. With `start`, the proxy is started
/// right away in one-shot mode using those settings. `lang` overrides the
/// saved language for this session.
pub fn run(dir: PathBuf, start: Option<Update>, lang: Option<Language>) -> Result<()> {
    let manager = Manager::new(dir);
    let saved = Config::load(manager.config_path())
        .map(|cfg| cfg.ui.language)
        .unwrap_or_default();
    i18n::set(lang.unwrap_or(saved));
    let oneshot = Oneshot::default();
    // Must precede every other thread (the service worker, tokio).
    oneshot.stop_on_termination(restore_terminal);
    let mut terminal = ratatui::try_init().context(tr!(
        "cannot initialise the terminal: the panel needs an interactive terminal (for scripts, see the commands in proxy-tui --help)",
        "无法初始化终端：管理面板需要在交互式终端中运行（脚本请使用 proxy-tui --help 中的命令）",
    ))?;
    let mut app = app::App::new(manager, oneshot.clone());
    if let Some(update) = start {
        app.start_oneshot(&update);
    }
    let result = app.run(&mut terminal);
    // The tty may already be gone (hung up): never let that skip stopping the proxy.
    restore_terminal();
    if oneshot.stop() {
        let _ = writeln!(
            std::io::stdout(),
            "{}",
            tr!(
                "One-shot mode: the proxy stopped together with the panel",
                "一次性模式：代理已随面板退出而停止"
            )
        );
    }
    result
}

fn restore_terminal() {
    let _ = ratatui::try_restore();
}
