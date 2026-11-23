//! 交互层 · 命令行（给 Agent / 脚本用）。只通过服务层 `manage` 管理代理。
//!
//! 约定：`--json` 输出机器可读结果（错误也以 JSON 输出到 stdout）；
//! 退出码 0 成功、1 失败、3 代理未运行。

mod output;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{random_password, EgressPolicy, Protocols, Update};
use crate::i18n::Language;
use crate::manage::{self, is_not_running, Action, Manager};

const GUIDE: &str = "\
快速上手（Agent / 脚本）:
  # 后台启动代理，就绪后返回连接信息；不需要 root，不开机自启
  PROXY_TUI_PASSWORD='s3cret' proxy-tui run --detach --json -p 7890 -u agent --protocols http,socks5,udp

  # 查看状态（运行中退出码 0，未运行退出码 3）
  proxy-tui status --json

  # 修改设置：运行中立即生效，未运行时写入配置文件
  proxy-tui config set -p 8080 --random-password --json

  # 停止后台代理
  proxy-tui stop

  # 服务模式：常驻后台、开机自启（需要 root）
  sudo proxy-tui service install -p 7890 -u agent -P 's3cret'

设置参数（run / config set / service install / tui --start 通用）:
  -p, --port <PORT>        mixed 端口，HTTP / HTTPS / SOCKS5 共用一个端口
  -u, --user <USER>        用户名，需与密码同时设置；--no-auth 关闭认证
  -P, --password <PASS>    密码。指定 -u 而不写 -P 时，读取环境变量 PROXY_TUI_PASSWORD，
                           这样密码不出现在进程列表里
      --random-password    生成 16 位随机密码（新密码见输出）
      --protocols <LIST>   启用的协议：http,https,socks5,udp 或 all
                           至少启用 http / https / socks5 之一；udp 依赖 socks5
      --policy <POLICY>    出口策略：same_iface（默认）| strict | prefer_v4 | prefer_v6
  设置写入数据目录的 config.toml，之后一直沿用，直到再次修改。

客户端连接方式（HOST 为本机地址，本机可用 127.0.0.1）:
  http://USER:PASS@HOST:PORT      HTTP 代理（访问 HTTPS 网站走 CONNECT）
  https://USER:PASS@HOST:PORT     到代理的连接用 TLS 加密（自签证书，curl 需 --proxy-insecure）
  socks5h://USER:PASS@HOST:PORT   SOCKS5（h 表示由代理解析域名，保证出口一致）；支持 UDP
  `status --json` / `run --detach --json` 的 proxy_urls 字段给出可直接使用的地址。

运行模式:
  一次性模式  run（前台，Ctrl+C 停止）、run --detach（后台，proxy-tui stop 停止）、
              或在管理面板里启动（退出面板即停止）；不需要 root
  服务模式    service install；systemd / launchd 托管，常驻后台、开机自启；需要 root

数据目录（配置、证书、日志、控制 socket）:
  默认 ~/.proxy-tui（用 sudo 时是调用者本人的目录）；已安装服务时跟随服务的目录；
  --dir 或环境变量 PROXY_TUI_DIR 可指定。TUI 与 CLI、两种运行模式都共用这个目录。

退出码: 0 成功 · 1 失败 · 2 参数错误 · 3 代理未运行";

#[derive(Parser)]
#[command(
    name = "proxy-tui",
    version,
    about = "HTTP / HTTPS / SOCKS5 mixed 端口代理：多出口 IP 一致，命令行给 Agent，TUI 给人",
    after_help = GUIDE
)]
struct Cli {
    /// 数据目录；默认 ~/.proxy-tui，已安装服务时跟随服务（也可用 PROXY_TUI_DIR）
    #[arg(short, long, global = true, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// 输出 JSON（错误也以 JSON 输出）
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Settings flags shared by every command that starts or reconfigures the proxy.
#[derive(Args, Debug, Default, Clone)]
struct SettingsArgs {
    /// mixed 端口，HTTP / HTTPS / SOCKS5 共用（1-65535）
    #[arg(short, long, value_parser = clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    /// 用户名（需与密码同时设置）
    #[arg(short, long)]
    user: Option<String>,
    /// 密码；指定了 -u 时也可用环境变量 PROXY_TUI_PASSWORD 传入
    #[arg(short = 'P', long)]
    password: Option<String>,
    /// 生成 16 位随机密码
    #[arg(long)]
    random_password: bool,
    /// 关闭认证（清空用户名和密码）
    #[arg(long, conflicts_with_all = ["user", "random_password"])]
    no_auth: bool,
    /// 启用的协议，逗号分隔：http,https,socks5,udp 或 all
    #[arg(long, value_name = "LIST", value_parser = parse_protocols)]
    protocols: Option<Protocols>,
    /// 出口策略：same_iface | strict | prefer_v4 | prefer_v6
    #[arg(long, value_name = "POLICY", value_parser = parse_policy)]
    policy: Option<EgressPolicy>,
}

fn parse_protocols(text: &str) -> Result<Protocols, String> {
    Protocols::parse_list(text).map_err(|e| e.to_string())
}

fn parse_policy(text: &str) -> Result<EgressPolicy, String> {
    EgressPolicy::parse(text).map_err(|e| e.to_string())
}

/// Password source for `-u USER` without `-P`, so it stays out of `ps`.
const PASSWORD_ENV: &str = "PROXY_TUI_PASSWORD";

impl SettingsArgs {
    fn to_update(&self) -> Update {
        self.to_update_with(std::env::var(PASSWORD_ENV).ok().filter(|p| !p.is_empty()))
    }

    /// Credentials are set as a pair: `PROXY_TUI_PASSWORD` is only read together
    /// with `-u`, so an exported variable never sneaks into unrelated commands.
    fn to_update_with(&self, env_password: Option<String>) -> Update {
        let (username, password) = if self.no_auth {
            (Some(String::new()), Some(String::new()))
        } else if self.random_password {
            (self.user.clone(), Some(random_password(16)))
        } else {
            let password = self
                .password
                .clone()
                .or_else(|| self.user.as_ref().and(env_password));
            (self.user.clone(), password)
        };
        Update {
            port: self.port,
            username,
            password,
            protocols: self.protocols,
            policy: self.policy,
            language: None,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// 打开管理面板（给人用；不带命令时默认打开）
    Tui {
        /// 打开后立即以一次性模式启动代理（退出面板即停止）
        #[arg(long)]
        start: bool,
        /// 面板语言：auto 跟随系统（默认回退英文）、en、zh；本次有效，面板里按 L 切换并保存
        #[arg(long, value_enum, env = "PROXY_TUI_LANG")]
        lang: Option<Language>,
        #[command(flatten)]
        settings: SettingsArgs,
    },
    /// 启动代理（一次性模式：不开机自启，不需要 root）
    #[command(after_help = "\
示例:
  proxy-tui run --detach --json -p 7890 -u agent -P s3cret
  PROXY_TUI_PASSWORD=s3cret proxy-tui run --detach -u agent --protocols socks5,udp
  proxy-tui run                 # 前台运行，Ctrl+C 停止")]
    Run {
        /// 在后台运行：等代理就绪后立即返回，用 proxy-tui stop 停止
        #[arg(long)]
        detach: bool,
        #[command(flatten)]
        settings: SettingsArgs,
    },
    /// 停止一次性模式的代理（服务模式请用 service stop）
    Stop,
    /// 查看运行状态（运行中退出码 0，未运行退出码 3）
    Status {
        /// 文本输出里显示密码（JSON 总是包含密码）
        #[arg(long)]
        show_password: bool,
    },
    /// 查看或修改设置
    #[command(subcommand)]
    Config(ConfigCmd),
    /// 查看活动连接
    Conns {
        /// 最多显示多少条（按流量排序）
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
    },
    /// 查看日志（服务模式和一次性模式写同一个日志）
    Logs {
        /// 显示最后多少行
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
        /// 持续输出新日志
        #[arg(short, long)]
        follow: bool,
    },
    /// 服务模式：systemd / launchd 托管，常驻后台、开机自启（需要 root）
    #[command(subcommand)]
    Service(ServiceCmd),
    /// 守护进程入口（供 systemd / launchd 和 run --detach 调用）
    #[command(hide = true)]
    Daemon,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 显示当前设置（运行中读实时值，否则读配置文件）
    Show {
        /// 文本输出里显示密码（JSON 总是包含密码）
        #[arg(long)]
        show_password: bool,
    },
    /// 修改设置：运行中立即生效，未运行时写入配置文件
    Set {
        #[command(flatten)]
        settings: SettingsArgs,
    },
    /// 让运行中的代理重新读取配置文件（手动改过 config.toml 后使用）
    Reload,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// 查看服务状态
    Status,
    /// 安装并启动服务（开机自启）；可同时写入设置
    Install {
        #[command(flatten)]
        settings: SettingsArgs,
    },
    /// 停止并移除服务（保留数据目录）
    Uninstall,
    /// 启动服务
    Start,
    /// 停止服务（开机仍会自启）
    Stop,
    /// 重启服务
    Restart,
}

/// Exit code for "the proxy is not running".
const NOT_RUNNING: u8 = 3;
/// Exit code for invalid arguments (same as clap's).
const USAGE: u8 = 2;

/// An invalid combination of arguments (exit code 2).
#[derive(Debug)]
struct Usage(&'static str);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Usage {}

pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let usage_error = !matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            if usage_error && std::env::args().any(|a| a == "--json") {
                let message = e.render().to_string();
                let first = message.lines().next().unwrap_or_default();
                let first = first.trim_start_matches("error: ");
                println!("{}", serde_json::json!({ "ok": false, "error": first }));
                return ExitCode::from(USAGE);
            }
            e.exit();
        }
    };
    let json = cli.json;
    match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            let message = format!("{e:#}");
            if json {
                println!("{}", serde_json::json!({ "ok": false, "error": message }));
            } else {
                eprintln!("错误: {message}");
            }
            if is_not_running(&e) {
                ExitCode::from(NOT_RUNNING)
            } else if e.downcast_ref::<Usage>().is_some() {
                ExitCode::from(USAGE)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn dispatch(cli: Cli) -> Result<ExitCode> {
    let json = cli.json;
    let mut manager = Manager::open(cli.dir);
    let cmd = cli.cmd.unwrap_or(Cmd::Tui {
        start: false,
        lang: std::env::var("PROXY_TUI_LANG")
            .ok()
            .and_then(|v| Language::from_str(&v, true).ok()),
        settings: SettingsArgs::default(),
    });
    match cmd {
        Cmd::Tui {
            start,
            lang,
            settings,
        } => {
            let update = settings.to_update();
            if !start && !update.is_empty() {
                bail!(Usage(
                    "设置参数需要配合 --start 使用；只改设置请用 proxy-tui config set"
                ));
            }
            crate::ui::run(manager.dir().to_path_buf(), start.then_some(update), lang)?;
        }
        Cmd::Run { detach, settings } => {
            let update = settings.to_update();
            manager.prepare_start(&update)?;
            if detach {
                let status = manager.start_detached()?;
                output::started(&mut manager, &status, json)?;
            } else {
                let (dir, config_path) = (
                    manager.dir().to_path_buf(),
                    manager.config_path().to_path_buf(),
                );
                manager.run_foreground(|status| {
                    output::foreground(&dir, &config_path, status, json)
                })?;
            }
        }
        Cmd::Stop => {
            let stopped = manager.stop()?;
            output::stopped(stopped.as_ref(), json);
        }
        Cmd::Status { show_password } => {
            let running = output::status(&mut manager, json, show_password)?;
            if !running {
                return Ok(ExitCode::from(NOT_RUNNING));
            }
        }
        Cmd::Config(ConfigCmd::Show { show_password }) => {
            output::settings(&mut manager, json, show_password)?;
        }
        Cmd::Config(ConfigCmd::Set { settings }) => {
            let update = settings.to_update();
            if update.is_empty() {
                bail!(Usage(
                    "没有要修改的设置，可用参数见 proxy-tui config set --help"
                ));
            }
            let applied = manager.update(&update)?;
            output::updated(&mut manager, applied, &update, json)?;
        }
        Cmd::Config(ConfigCmd::Reload) => {
            let message = manager.reload()?;
            output::message(&message, json);
        }
        Cmd::Conns { limit } => output::conns(&mut manager, limit, json)?,
        Cmd::Logs { lines, follow } => output::logs(&mut manager, lines, follow, json)?,
        Cmd::Service(cmd) => service(&mut manager, cmd, json)?,
        Cmd::Daemon => manager.run_foreground(|_| {})?,
    }
    Ok(ExitCode::SUCCESS)
}

fn service(manager: &mut Manager, cmd: ServiceCmd, json: bool) -> Result<()> {
    let action = match cmd {
        ServiceCmd::Status => {
            output::service_status(&manage::service_status(), json);
            return Ok(());
        }
        ServiceCmd::Install { settings } => {
            manager.prepare_install(&settings.to_update())?;
            Action::Install
        }
        ServiceCmd::Uninstall => Action::Uninstall,
        ServiceCmd::Start => Action::Start,
        ServiceCmd::Stop => Action::Stop,
        ServiceCmd::Restart => Action::Restart,
    };
    let message = manager.service_action(action)?;
    output::message(&message, json);
    if action == Action::Install && !json {
        println!("管理面板：proxy-tui（无需 sudo）· 命令行说明：proxy-tui --help");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn settings_flags_become_an_update() {
        let cli = Cli::try_parse_from([
            "proxy-tui",
            "config",
            "set",
            "-p",
            "8080",
            "-u",
            "a",
            "-P",
            "b",
            "--protocols",
            "http,socks5",
            "--policy",
            "strict",
        ])
        .unwrap();
        let Some(Cmd::Config(ConfigCmd::Set { settings })) = cli.cmd else {
            panic!("parsed wrong command");
        };
        let update = settings.to_update();
        assert_eq!(update.port, Some(8080));
        assert_eq!(update.username.as_deref(), Some("a"));
        assert_eq!(update.password.as_deref(), Some("b"));
        assert_eq!(update.protocols.unwrap().list(), "http,socks5");
        assert_eq!(update.policy, Some(EgressPolicy::Strict));

        let cli = Cli::try_parse_from(["proxy-tui", "config", "set", "-p", "1"]).unwrap();
        let Some(Cmd::Config(ConfigCmd::Set { settings })) = cli.cmd else {
            panic!("parsed wrong command");
        };
        let update = settings.to_update_with(Some("from-env".into()));
        assert_eq!(update.password, None, "env password needs -u");
        let cli = Cli::try_parse_from(["proxy-tui", "run", "-u", "a"]).unwrap();
        let Some(Cmd::Run { settings, .. }) = cli.cmd else {
            panic!("parsed wrong command");
        };
        let update = settings.to_update_with(Some("from-env".into()));
        assert_eq!(update.password.as_deref(), Some("from-env"));

        let cli = Cli::try_parse_from(["proxy-tui", "run", "--no-auth", "-P", "x"]).unwrap();
        let Some(Cmd::Run { settings, .. }) = cli.cmd else {
            panic!("parsed wrong command");
        };
        let update = settings.to_update();
        assert_eq!(update.username.as_deref(), Some(""));
        assert_eq!(update.password.as_deref(), Some(""));

        assert!(Cli::try_parse_from(["proxy-tui", "run", "-p", "0"]).is_err());
        assert!(Cli::try_parse_from(["proxy-tui", "run", "--protocols", "udp"]).is_err());
        assert!(Cli::try_parse_from(["proxy-tui", "run", "--policy", "nope"]).is_err());
        assert!(Cli::try_parse_from(["proxy-tui", "--dir", "/tmp/x", "status", "--json"]).is_ok());
    }
}
