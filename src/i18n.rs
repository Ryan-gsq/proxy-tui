//! 界面语言：英文（默认回退）和中文。
//!
//! 管理面板启动时按 `tui --lang` / `PROXY_TUI_LANG`、配置 `[ui] language`、
//! 系统环境的顺序选定语言，面板里可以随时切换。命令行输出保持中文。

use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// The `[ui] language` setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// Follow the system locale.
    #[default]
    Auto,
    En,
    Zh,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Auto => "auto",
            Language::En => "en",
            Language::Zh => "zh",
        }
    }

    /// `En` or `Zh`: `Auto` resolved against the system locale.
    pub fn resolve(self) -> Language {
        match self {
            Language::Auto => system(),
            lang => lang,
        }
    }

    /// The other concrete language.
    pub fn other(self) -> Language {
        if self.resolve() == Language::Zh {
            Language::En
        } else {
            Language::Zh
        }
    }
}

static ZH: AtomicBool = AtomicBool::new(true);

/// Switches the language of this process's interface text.
pub fn set(lang: Language) {
    ZH.store(lang.resolve() == Language::Zh, Ordering::Relaxed);
}

pub fn is_zh() -> bool {
    ZH.load(Ordering::Relaxed)
}

pub fn current() -> Language {
    if is_zh() {
        Language::Zh
    } else {
        Language::En
    }
}

/// Picks the text for the current language: `tr!(english, chinese)`. Only the
/// chosen argument is evaluated, so both may be `format!` calls.
#[macro_export]
macro_rules! tr {
    ($en:expr, $zh:expr $(,)?) => {
        if $crate::i18n::is_zh() {
            $zh
        } else {
            $en
        }
    };
}

/// The system's interface language; English unless it is Chinese.
pub fn system() -> Language {
    from_env(|name| std::env::var(name).ok())
        .or_else(apple_language)
        .unwrap_or(Language::En)
}

/// The first locale variable that is set decides, in gettext's order.
fn from_env(var: impl Fn(&str) -> Option<String>) -> Option<Language> {
    ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| var(name))
        .map(|value| value.split(':').next().unwrap_or("").trim().to_string())
        .find(|value| !value.is_empty())
        .map(|value| from_tag(&value))
}

fn from_tag(tag: &str) -> Language {
    if tag.to_ascii_lowercase().starts_with("zh") {
        Language::Zh
    } else {
        Language::En
    }
}

/// macOS keeps the language list in user defaults; apps started outside a
/// terminal often have no locale variables.
#[cfg(target_os = "macos")]
fn apple_language() -> Option<Language> {
    let out = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || "\"(),".contains(c)))
        .find(|tag| !tag.is_empty())
        .map(from_tag)
}

#[cfg(not(target_os = "macos"))]
fn apple_language() -> Option<Language> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(vars: &[(&str, &str)]) -> Option<Language> {
        from_env(|name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn locale_variables_in_order() {
        assert_eq!(detect(&[]), None);
        assert_eq!(detect(&[("LANG", "zh_CN.UTF-8")]), Some(Language::Zh));
        assert_eq!(detect(&[("LANG", "en_US.UTF-8")]), Some(Language::En));
        assert_eq!(detect(&[("LANG", "C")]), Some(Language::En));
        assert_eq!(
            detect(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "zh_CN.UTF-8")]),
            Some(Language::En)
        );
        assert_eq!(
            detect(&[("LANGUAGE", "zh_TW:en"), ("LC_ALL", "en_US.UTF-8")]),
            Some(Language::Zh)
        );
        assert_eq!(
            detect(&[("LC_ALL", ""), ("LANG", "zh_HK.UTF-8")]),
            Some(Language::Zh)
        );
    }

    #[test]
    fn other_language() {
        assert_eq!(Language::Zh.other(), Language::En);
        assert_eq!(Language::En.other(), Language::Zh);
    }
}
