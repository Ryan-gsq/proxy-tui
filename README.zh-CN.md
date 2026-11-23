# proxy-tui

[English](README.md) | **简体中文**

[![CI](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/ci.yml)
[![Build](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/release.yml/badge.svg)](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/release.yml)

用 Rust 编写的高吞吐代理，支持 Linux、macOS 和 Windows：一个端口同时提供 HTTP、HTTPS、SOCKS5 代理，从哪块网卡进来就从哪块网卡出去，并自带 TUI 管理面板和给 Agent 用的命令行。

![proxy-tui 管理面板演示](docs/demo.gif)

## 亮点

- **一个端口，多种协议**：同一个端口（mixed port）自动识别 HTTP、HTTPS（客户端到代理用 TLS 加密）、SOCKS5（TCP + UDP），每种协议都能单独开关。
- **出口 IP 一致**：机器有多个出口时，客户端从哪块网卡、哪个 IP 连进来，代理就从同一个出口出去，DNS 查询也走同一个出口。IPv6 只在确实可用时才用于出站。
- **高匿名**：转发给源站的请求不带任何代理痕迹（没有 `Via`、`X-Forwarded-For` 等头），源站看到的和客户端直连一样。
- **连接复用**：普通 HTTP 转发支持 keep-alive，源站连接放进连接池，跨客户端连接复用。
- **两种管理方式**：
  - **TUI 面板**：给人用，显示设置、流量、网卡、连接、日志，可以管理服务，界面支持中英文切换。
  - **命令行**：给 Agent 和脚本调用，可以完全管理代理，支持 JSON 输出和约定好的退出码。
  - 两者都通过同一个服务层管理代理，并共用**同一个数据目录**。
- **两种运行模式**：
  - **一次性模式**：不需要 root，随启动它的进程退出。
  - **服务模式**：由 systemd / launchd / Windows 服务管理器托管，常驻后台、开机自启，需要 root（Windows 上需要管理员）。

## 目录

- [安装](#安装)
- [快速开始](#快速开始)
- [功能](#功能)
- [多出口与 IP 一致性](#多出口与-ip-一致性)
- [管理面板（TUI）](#管理面板tui)
- [命令行（给 Agent / 脚本）](#命令行给-agent--脚本)
- [运行模式](#运行模式)
- [数据目录](#数据目录)
- [日志](#日志)
- [配置](#配置)
- [项目结构](#项目结构)
- [开发与测试](#开发与测试)
- [说明](#说明)
- [许可证](#许可证)

## 安装

### 预编译版本

每个版本标签都会在 [Releases](https://github.com/Ryan-gsq/proxy-tui/releases) 页面发布编译好的程序；每次推送到 `main`，[Build 工作流](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/release.yml)也会构建一份，可以在运行记录的 artifacts 里下载：

| 平台 | 文件 |
|---|---|
| Linux x86_64 / aarch64（静态链接，musl） | `proxy-tui-*-x86_64-unknown-linux-musl.tar.gz` / `…-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel / Apple 芯片 | `proxy-tui-*-x86_64-apple-darwin.tar.gz` / `…-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `proxy-tui-*-x86_64-pc-windows-msvc.zip` |

### 从源码编译

需要 Rust 工具链（stable；Windows 上还需要 MSVC 构建工具）。

```bash
git clone https://github.com/Ryan-gsq/proxy-tui.git
cd proxy-tui
cargo build --release
sudo cp target/release/proxy-tui /usr/local/bin/   # 可选；安装服务时会自动复制到这里
```

在 macOS 上交叉编译 Linux 版本（需要 [zig](https://ziglang.org/) 和 [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)）：

```bash
cargo zigbuild --release --target x86_64-unknown-linux-gnu
cargo zigbuild --release --target aarch64-unknown-linux-gnu
```

Linux 上需要 iproute2（`ip` 命令），用于读取路由和配置策略路由。

## 快速开始

```bash
# 人：打开管理面板（按 s 启动或管理服务，e 改设置，l 看日志，L 切换语言）
proxy-tui

# Agent / 脚本：在后台启动，代理就绪后返回 JSON（包含可直接使用的代理地址）
PROXY_TUI_PASSWORD='s3cret' proxy-tui run --detach --json -p 7890 -u agent --protocols http,socks5,udp
proxy-tui status --json       # 查看状态
proxy-tui stop                # 停止

# 常驻后台、开机自启（需要 root）
sudo proxy-tui service install
```

客户端连接方式（HOST 为代理所在机器的地址，本机用 `127.0.0.1`）：

```bash
curl -x http://USER:PASS@HOST:PORT https://example.com                    # HTTP 代理
curl -x socks5h://USER:PASS@HOST:PORT https://example.com                 # SOCKS5（支持 UDP）
curl --proxy-insecure -x https://USER:PASS@HOST:PORT https://example.com  # 到代理的连接用 TLS
# 自签证书在数据目录的 cert.pem，可以导入信任，或用 --proxy-cacert 指定
```

`proxy-tui --help` 末尾附有完整的调用说明（参数、连接地址格式、运行模式、数据目录、退出码），Agent 可以直接读取。

## 功能

- **Mixed 端口**：根据连接的第一个字节识别协议
  - `0x05` → SOCKS5
  - `0x16` → TLS，解开后再按 HTTP 或 SOCKS5 处理
  - 字母开头 → HTTP
- **协议开关**：HTTP、HTTPS、SOCKS5、UDP 可以分别启用或关闭（`--protocols`、`config set`、配置文件 `[protocols]`、TUI 编辑表单）；关闭的协议会被直接断开连接（SOCKS5 UDP 请求返回 `0x07`）
- **HTTP 代理**：支持 `CONNECT` 隧道和普通转发（absolute-URI）。普通转发时：
  - 支持 keep-alive，同一个客户端连接上可以连续或管线化地发多个请求。请求和响应的消息体都严格按 `Content-Length`、chunked 或关闭连接来定界。
  - 每个请求只发给它自己 URI 里的站点。后一个请求（以及其中的代理凭据）不会发给前一个请求的源站。
  - `Content-Length` 与 `Transfer-Encoding` 同时出现、或有多个不一致的 `Content-Length` 时，直接返回 400，防止请求走私。
  - `Host` 按请求 URI 重写；`Proxy-Connection` 换回 `Connection`；去掉逐跳头，包括 `Connection` 中列出的头（但 `Content-Length`、`Transfer-Encoding`、`Host` 不会因此被去掉）。
  - 支持 `Expect: 100-continue` 等 1xx 中间响应；WebSocket 等 Upgrade 请求，要等上游返回 101 后才转为双向隧道。
  - 连接上的第一个请求必须带正确的凭据；之后的请求可以不带，带了就必须正确。
  - 同一个客户端连接上，普通请求之后也可以再发 `CONNECT`。
- **HTTPS 代理**：客户端到代理这一段用 TLS 加密，首次启动时自动生成自签证书（`cert.pem` / `key.pem`）。解开 TLS 后与 HTTP 代理完全相同，支持 TLS 会话恢复。
- **SOCKS5**：支持 CONNECT 和 UDP ASSOCIATE，以及用户名/密码认证（RFC 1929）
- **高匿名**：
  - 普通转发不添加任何请求头，没有 `Via`、`X-Forwarded-For`、`Forwarded`。
  - 客户端自己带的代理和来源地址类请求头也会去掉：所有 `Proxy-*`、`X-Forwarded-*`、`X-Proxy*`，以及 `Via`、`Forwarded`、`X-Real-IP`、`Client-IP`、`True-Client-IP`、`X-Client-IP`、`X-Cluster-Client-IP`、`X-Originating-IP`、`X-Remote-IP`、`X-Remote-Addr` 等。
  - 其余请求头保持客户端原来的顺序和大小写。
  - `CONNECT` 隧道和 SOCKS5 只转发字节，不改内容。源站看到的地址是出口 IP。
- **连接复用**：普通 HTTP 转发的源站连接放进连接池，后续请求优先复用，HTTP 和 HTTPS 代理的不同客户端连接也能共用。
  - 连接池按"出口 + 站点"区分，复用不会改变出口 IP。
  - 取出前会检查连接是否已被源站关闭。如果复用的连接发出请求后没有任何响应就断开，不带请求体的 GET / HEAD / OPTIONS 会在新连接上重发一次。
  - 空闲连接 60 秒后关闭；每个站点最多保留 8 个，总共最多 256 个。
  - HTTP/1.1 客户端要求 `close` 时，只关闭客户端这一侧，源站连接仍然留给后续请求。HTTP/1.0 请求按原样转发。
  - `CONNECT` 和 SOCKS5 隧道里是客户端自己与站点的加密会话，无法给其他客户端复用；隧道由客户端在内部复用，代理不设空闲超时。
- **IPv4 和 IPv6**：v4、v6 分别监听（v6 套接字设置 `IPV6_V6ONLY`），能准确拿到客户端连入的本机地址
- **单用户认证**：HTTP Basic 与 SOCKS5 共用一组用户名和密码；两者都留空表示不认证
- **开机自启动**：服务模式下，Linux 用 systemd，macOS 用 launchd
- **性能**：
  - tokio 多线程运行时
  - 普通 HTTP 转发复用源站连接，省去重复的 DNS 查询和 TCP 握手
  - 默认 32 KiB 双向拷贝缓冲（可配置）
  - 开启 `TCP_NODELAY`
  - Linux 上使用 `IP_BIND_ADDRESS_NO_PORT`，避免先绑定源地址再连接时耗尽端口
  - 自动提高 `RLIMIT_NOFILE`
  - 本机回环实测：单连接约 32 Gbit/s

## 多出口与 IP 一致性

每个连接根据**客户端连入的本机 IP** 找到它所在的网卡，出站连接按以下方式绑定：

- 绑定到这块网卡：Linux 用 `SO_BINDTODEVICE`，macOS 用 `IP_BOUND_IF` / `IPV6_BOUND_IF`
- 同时绑定源 IP：
  - 协议族与入口相同时，直接用**入口 IP 本身**出站，因此一块网卡上有多个 IP 时也是一一对应
  - 需要使用另一种协议族时，按入口 IP 在网卡地址列表中的序号，选取另一族中对应序号的地址，保证配对稳定
- DNS 查询也绑定在同一出口发出：
  - 服务器取自配置或 `/etc/resolv.conf`。如果其中只有本地 stub（例如 systemd-resolved 的 `127.0.0.53`），改用 `/run/systemd/resolve/resolv.conf` 中的上游服务器。
  - 支持 `/etc/hosts`。
  - 响应被截断时改用 TCP 重新查询。
  - 出口 DNS 不可用时，回退到系统解析器（这时 DNS 不保证走同一出口）。
- **Linux 自动配置策略路由**（需要 root，`egress.auto_policy_routing = true`）：
  - 当两块以上网卡有默认路由时，把每块网卡在 main 表中的路由复制到独立的表（`table_base + ifindex`）
  - 为每个地址添加两条规则：
    - `from <IP> lookup main suppress_prefixlength 0`（优先级 `rule_priority-1`），保证本地网段仍走直连路由
    - `from <IP> lookup <表>`（优先级 `rule_priority`），默认流量走该网卡自己的网关
  - 每 60 秒、网卡变化时，以及修改 `table_base`、`rule_priority`、`auto_policy_routing` 后，都会立即重新同步。
  - **只改动自己创建的规则和路由表**：同步和退出清理时，只删除本进程添加的规则、只清空本进程写入的表。其他程序在相同优先级上的规则不受影响。
  - 如果某张路由表里已经有别的路由，不会清空它：这块网卡会报"路由表已有其他路由"，请调整 `table_base`。
- **macOS**：系统本身支持按网卡区分的默认路由，绑定网卡即可，不需要额外配置
- **Windows**：默认使用强主机模型发送，绑定了源地址的连接会从拥有该地址的网卡出去；代理还会设置 `IP_UNICAST_IF` / `IPV6_UNICAST_IF`。网卡、默认网关和 DNS 服务器取自网卡设置（`GetAdaptersAddresses`），不需要策略路由
- 没有默认路由的网卡（例如 docker0、仅内网网卡、loopback）不作为出口，这类连接交给系统路由。也可以用 `egress.iface_override` 手动指定映射，例如 `eth0 = "eth1"`

### IPv6 出口判断

一块网卡同时满足以下条件，才把它当作可用的 IPv6 出口：

- 有全局 IPv6 地址（排除 `fe80::/10`；Linux 下还排除 deprecated、tentative 状态的地址）
- 有 IPv6 默认路由
- 绑定该网卡向公共 DNS 发起的 TCP 探测成功

探测每 `probe_interval_secs` 秒执行一次，网卡变化时也会立即执行。

客户端经 IPv6 连入时也遵守这个判断：如果这块网卡的 IPv6 探测不通，就改用 IPv4 出站（`strict` 策略下直接失败）。客户端经 ULA 地址（`fc00::/7`，仅局域网可达）连入时，出站使用同一网卡上的全局 IPv6 地址。

macOS 判断默认路由时，只认带网关标志（`G`）的路由，`bridge100` 这类 `link#N` 克隆路由不算出口。

### 出口策略（`egress.policy`）

| 策略 | 行为 | 目标缺少对应协议的地址时 |
|---|---|---|
| `same_iface`（默认） | 同网卡出站，优先使用与入口相同的协议 | 回退到该网卡的另一种协议 |
| `strict` | 只用与入口相同的协议 | 直接失败（SOCKS5 返回 `0x03`/`0x04`，HTTP 返回 502） |
| `prefer_v4` / `prefer_v6` | 同网卡，按固定偏好选择协议 | 回退到另一种协议 |

域名目标会同时查询 A 和 AAAA 记录，只保留这块网卡可用的协议族，再按 RFC 8305（Happy Eyeballs）交替尝试连接：优先族先连，250ms 后再尝试另一族。

## 管理面板（TUI）

不带参数运行 `proxy-tui` 就会打开面板。它和命令行使用同一个服务层、同一个数据目录：命令行启动的代理会出现在面板里，命令行改的设置也会实时反映到面板上。

- 标题栏用颜色标签显示当前模式：紫色是一次性模式，绿色是服务模式，灰色是未运行。
- "运行模式"区域显示代理在哪个进程里运行、怎么停止，以及服务的状态（是否安装、是否运行、PID、是否开机自启）。
- 还会显示设置、实时和累计流量、速率曲线、各网卡的统计、活动连接和事件。

| 按键 | 功能 |
|---|---|
| `s` | 运行管理：1 一次性启动 · 2 停止 · 3 启动服务 · 4 停止服务 · 5 重启服务 · 6 安装服务 · 7 卸载服务。会断开连接的操作需要按 `y` 确认 |
| `e` | 编辑设置：端口、用户名、密码、协议开关、出口策略。运行中保存后立即生效，未运行时写入配置文件 |
| `l` | 日志视图：`↑` `↓` 滚动（向上滚动时暂停跟随），`End` 回到最新，`f` 开关跟随 |
| `p` | 显示或隐藏密码 |
| `r` | 重新读取配置文件 |
| `L` | 切换界面语言（English / 中文），并保存到配置文件 |
| `q` | 退出；如果代理运行在本面板里，会先确认，退出即停止代理 |

在面板里一次性启动时，会先弹出设置表单，确认后再启动。代理运行在面板进程内，退出面板、关闭终端都会让它停止。

不用 root（Windows 上是不以管理员身份）打开面板时，服务相关的操作（启动、停止、安装等）不可用，但可以使用一次性模式，也可以修改服务的设置（只要数据目录属于你）。服务操作在后台线程中执行，不会卡住界面。

### 界面语言

面板有英文和中文两种界面，按下面的顺序确定：

1. `proxy-tui tui --lang en|zh|auto` 或环境变量 `PROXY_TUI_LANG`：只影响这一次。
2. 配置文件中的 `[ui] language`：面板里按 `L` 切换时写入这里，服务模式和一次性模式共用。
3. `auto`（默认）跟随系统：依次看 `LANGUAGE`、`LC_ALL`、`LC_MESSAGES`、`LANG`；macOS 上这些变量都没设置时读取系统首选语言。中文环境用中文，其他情况用英文。

命令行的输出目前是中文，不受这个设置影响。

## 命令行（给 Agent / 脚本）

### 命令

| 命令 | 作用 |
|---|---|
| `run [--detach] [设置参数]` | 以一次性模式启动。加 `--detach` 时在后台运行，等代理开始监听后才返回；不加则在前台运行，Ctrl+C 停止 |
| `stop` | 停止一次性模式的代理（服务模式请用 `service stop`） |
| `status` | 运行状态、设置、代理地址、流量、网卡；未运行时显示配置文件中的设置 |
| `config show` | 查看当前设置：运行中读实时值，未运行时读配置文件 |
| `config set <设置参数>` | 修改设置：运行中**立即生效**，未运行时写入配置文件，下次启动生效 |
| `config reload` | 手动改过 `config.toml` 后，让运行中的代理重新读取 |
| `conns [-n 50]` | 活动连接（按流量排序） |
| `logs [-n 100] [-f]` | 查看日志，`-f` 持续输出新内容。加 `--json` 时输出 `{"ok", "log_path", "lines"}`；同时加 `-f` 时每行输出一个 `{"line": ...}` |
| `service status\|install\|start\|stop\|restart\|uninstall` | 服务模式管理（除 `status` 外都需要 root，Windows 上需要以管理员身份运行终端）；`install` 也可以带设置参数 |
| `tui [--start] [--lang auto\|en\|zh] [设置参数]` | 打开管理面板（不带命令时默认打开）；`--start` 表示打开后立即以一次性模式启动；`--lang` 指定本次的界面语言 |

所有命令都支持全局参数：

- `--dir <DIR>`：指定数据目录，也可以用环境变量 `PROXY_TUI_DIR`
- `--json`：输出 JSON

### 设置参数

`run`、`config set`、`service install`、`tui --start` 通用：

| 参数 | 说明 |
|---|---|
| `-p, --port <PORT>` | mixed 端口，HTTP / HTTPS / SOCKS5 共用（1-65535） |
| `-u, --user <USER>` | 用户名，需要和密码同时设置 |
| `-P, --password <PASS>` | 密码。给了 `-u` 但没给 `-P` 时，读取环境变量 `PROXY_TUI_PASSWORD`，这样密码不会出现在进程列表和 shell 历史里。不带 `-u` 的命令不读取这个变量，导出后也不会悄悄改掉密码 |
| `--random-password` | 生成 16 位随机密码（新密码见输出） |
| `--no-auth` | 关闭认证（清空用户名和密码） |
| `--protocols <LIST>` | 启用的协议：`http,https,socks5,udp` 的任意组合，或 `all`。至少启用 http、https、socks5 中的一个；`udp` 依赖 `socks5` |
| `--policy <POLICY>` | 出口策略：`same_iface`（默认）、`strict`、`prefer_v4`、`prefer_v6` |

设置会写入数据目录的 `config.toml`，之后一直沿用，直到再次修改。服务模式和一次性模式用的是同一份配置。

### JSON 输出

`run --detach --json`、`status --json`、`config show|set --json` 都会返回连接代理所需的全部信息：

```json
{
  "ok": true,
  "running": true,
  "mode": "oneshot",
  "pid": 25297,
  "stoppable": true,
  "settings": {
    "port": 7890,
    "username": "agent",
    "password": "s3cret",
    "protocols": { "http": true, "https": false, "socks5": true, "udp": true },
    "policy": "same_iface"
  },
  "proxy_urls": {
    "http": "http://agent:s3cret@127.0.0.1:7890",
    "socks5": "socks5h://agent:s3cret@127.0.0.1:7890"
  },
  "listening": ["0.0.0.0:7890", "[::]:7890"],
  "data_dir": "/home/agent/.proxy-tui",
  "config_path": "/home/agent/.proxy-tui/config.toml",
  "log_path": "/home/agent/.proxy-tui/logs/proxy-tui.log"
}
```

- `proxy_urls` 只列出已启用的协议，可以直接使用。用户名和密码已做 URL 编码。SOCKS5 地址用 `socks5h`，由代理解析域名，保证 DNS 也走同一个出口。
  - 地址的主机部分按实际监听的地址取：监听通配地址时用本机回环（`127.0.0.1` 或 `::1`），只监听某个具体地址时就用那个地址。
  - HTTPS 地址只在证书加载成功时才列出。
- `listen_errors`：有地址没能监听时才出现，列出失败的地址和原因（代理仍在其余地址上运行）。
- `mode`：`oneshot` 表示一次性模式，`service` 表示服务模式。
- `stoppable`：能否用 `proxy-tui stop` 停止。服务模式，以及运行在管理面板里的代理，都是 `false`。
- `status --json` 还会附带 `service`（服务状态）和 `daemon`（完整运行状态）。
- `config set --json` 会附带 `applied`：`live` 表示已在运行中的代理上生效，`saved` 表示只写入了配置文件。
- 出错时输出 `{"ok": false, "error": "..."}`，参数错误也是这个格式。
- 前台运行的 `run --json` 在代理开始监听后输出一次状态；启动失败则只输出错误。

### 退出码

| 退出码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 失败，比如代理已在运行、需要 root、请求被拒绝 |
| 2 | 参数错误，包括 `config set` 没给任何设置、`tui` 带了设置参数却没加 `--start` |
| 3 | 代理未运行（`status`、`config reload`、`conns` 等需要运行中代理的命令） |

### 典型流程

```bash
export PROXY_TUI_PASSWORD='s3cret'                                       # 只在同时给了 -u 时读取
proxy-tui run --detach --json -u agent -p 7890 --protocols http,socks5   # 启动
proxy-tui status --json >/dev/null || echo "未运行"                        # 检查
proxy-tui config set --protocols all --json                              # 运行中改设置，立即生效
proxy-tui stop                                                           # 停止
```

## 运行模式

| 模式 | 启动方式 | 怎么停止 | 需要 root |
|---|---|---|---|
| **一次性 · 前台** | `proxy-tui run` | Ctrl+C、关闭终端、`kill`，或 `proxy-tui stop` | 否 |
| **一次性 · 后台** | `proxy-tui run --detach` | `proxy-tui stop`（关闭调用它的终端不会让它停止） | 否 |
| **一次性 · 面板内** | 面板里按 `s` → 1，或 `proxy-tui tui --start` | 退出面板或关闭终端；`proxy-tui stop` 会被拒绝 | 否 |
| **服务** | `sudo proxy-tui service install`（Windows 上在管理员终端里运行 `proxy-tui service install`），或面板里按 `s` → 6 | `sudo proxy-tui service stop`（下次开机仍会自启） | 是（Windows 上需要管理员） |

- 同一个数据目录里同时只能运行一个代理。已经在运行时：
  - `run` 会报错。
  - `service start` 和 `service install` 会先要求停掉一次性代理。
- 在面板里选择"安装服务（转为常驻）"时，会先停掉面板里的一次性代理，再交给服务运行。
- 一次性模式不需要 root，但 Linux 的策略路由只有以 root 运行时才会自动配置。
- Windows 上服务以 LocalSystem 运行，崩溃后会自动重启；程序安装到 `%ProgramFiles%\proxy-tui\proxy-tui.exe`。

## 数据目录

配置、证书、日志、控制 socket 都放在**同一个数据目录**里。TUI 和 CLI、服务模式和一次性模式都使用它：

```
~/.proxy-tui/
├── config.toml      配置
├── cert.pem         HTTPS 代理的自签证书
├── key.pem
├── logs/            日志
└── control.sock     控制 socket（仅在运行时存在；Windows 上是命名管道）
```

Windows 上默认目录是 `%USERPROFILE%\.proxy-tui`，控制通道是命名管道 `\\.\pipe\proxy-tui-<数据目录的哈希>`，只接受本机连接。

数据目录按以下顺序确定：

1. `--dir <DIR>` 或环境变量 `PROXY_TUI_DIR`
2. 已安装服务时，使用服务所在的目录（从 unit / plist 中的 `daemon --dir` 读出）
3. `~/.proxy-tui`：用 `sudo` 运行时是**调用者本人**的家目录，所以加不加 sudo 都是同一个位置

所以可以先不用 root 试用一次性模式，再执行 `sudo proxy-tui service install`，服务会直接沿用同一份配置。

服务以 root 运行，但它在数据目录里新建或改写的文件，所有者都会交还给目录的主人。因此这个用户**不用 sudo** 也能打开面板、修改设置、查看日志；只有服务的启动、停止、安装等操作需要 sudo。新建的目录权限为 0700，配置、私钥和日志为 0600。

数据目录属于普通用户、读写它的却是 root，所以 root 进程访问这个目录时有专门的防护：

- 所有文件都相对于已打开的目录访问，不跟随软链接；路径途中只跟随 root 自己创建的软链接（例如 macOS 的 `/tmp → /private/tmp`）。
- 读写已有文件前，会确认它是普通文件、只有一个硬链接、属主是目录主人或 root。
- 新文件创建时即带最终权限。配置用随机名的临时文件写好后再原子替换。
- 此时配置里的 `tls.cert`、`tls.key`、`log.dir` 只能指向数据目录内部。

这样，目录主人无法借助 root 服务读写目录之外的文件；如果整个数据目录被换成软链接，服务会拒绝启动并说明原因。

Windows 上文件继承数据目录的权限。以管理员身份运行的进程不跟随数据目录里的软链接和目录联接，`tls.cert`、`tls.key`、`log.dir` 也只能指向数据目录内部。服务运行时，用命令行或面板管理它需要以管理员身份运行终端：只有 SYSTEM 和管理员能向服务的控制管道发送命令。

## 日志

日志在 `<数据目录>/logs/proxy-tui.log`，服务模式和一次性模式都写这个文件。每行都标注是哪种模式写入的（`service` / `oneshot`）。文件超过 `log.max_size_mb` 后轮转为 `proxy-tui.log.1 … .N`，保留 `log.max_files` 个旧文件，所以日志总共最多占用约 `(max_files + 1) × max_size_mb`，默认 60 MB。`max_size_mb` 至少为 1，不能关闭轮转。

日志由后台线程写盘，代理线程不会等待磁盘。写入队列满时会丢弃日志行，并在日志里记一行丢弃了多少。`[log]` 的设置在进程启动时生效，修改后需要重启代理。

日志内容包括：

- 每条连接关闭时的一行摘要：协议、客户端 → 目标、出口网卡和出口地址、上下行流量、耗时
- 连接失败（`connect failed`）、认证失败（WARN）
- 配置变更、监听变化、策略路由同步、IPv6 出口探测等事件

可以用面板的 `l` 或 `proxy-tui logs -f` 查看。macOS 上服务进程的标准输出和标准错误写在 `/var/log/proxy-tui.launchd.log`：launchd 以 root 打开这个文件，所以不能放在用户的目录里。

## 配置

配置文件是数据目录下的 `config.toml`。首次启动时自动生成（包括随机密码）；手动修改后用面板的 `r` 或 `proxy-tui config reload` 让运行中的代理重新读取。

```toml
[listen]
port = 7890
ipv4 = "0.0.0.0"          # 留空表示不监听 IPv4
ipv6 = "::"               # 留空表示不监听 IPv6

[auth]
username = "proxy"        # 用户名和密码都留空表示不认证
password = "..."

[protocols]               # 至少启用 http / https / socks5 之一；udp 依赖 socks5
http = true               # 明文 HTTP 代理（CONNECT + 普通转发）
https = true              # TLS 加密的代理连接（自签证书）
socks5 = true             # SOCKS5 TCP
udp = true                # SOCKS5 UDP ASSOCIATE

[tls]
cert = ""                 # 留空表示使用数据目录下的 cert.pem / key.pem（不存在时自动生成）
key = ""

[egress]
policy = "same_iface"     # same_iface | strict | prefer_v4 | prefer_v6
auto_policy_routing = true
table_base = 1000         # 1000 起；每块网卡用 table_base + ifindex 号路由表
rule_priority = 9000      # 1000–32000；规则使用 rule_priority-1 和 rule_priority
probe_interval_secs = 60
connect_timeout_secs = 10
[egress.iface_override]   # 入口网卡 = 出口网卡
# eth0 = "eth1"

[dns]
servers = []              # 留空表示使用 /etc/resolv.conf 中的非本地服务器
timeout_ms = 1500
cache_ttl_max_secs = 600

[performance]
buffer_size = 32768
handshake_timeout_secs = 30

[ui]
language = "auto"         # 管理面板语言：auto 跟随系统（非中文环境用英文）、en、zh；面板里按 L 切换并保存在这里

[log]
level = "info"            # off / error / warn / info / debug / trace（每条连接的摘要是 info 级别）
dir = ""                  # 留空表示 <数据目录>/logs
max_size_mb = 10          # 单个文件上限，超过后轮转（至少为 1）
max_files = 5             # 保留 proxy-tui.log.1 … .5；总共最多 60 MB
```

## 项目结构

```
proxy-tui/
├── Cargo.toml / Cargo.lock
├── LICENSE            MIT 许可证
├── README.md            英文说明
├── README.zh-CN.md      中文说明
├── docs/demo.gif        管理面板演示
├── .github/workflows/
│   ├── ci.yml           在 Linux、macOS、Windows 上运行 fmt、clippy 和全部测试
│   └── release.yml      构建全部平台；打版本标签时发布 Release
├── src/                 源码（见下）
└── tests/e2e.rs         端到端测试：在本机启动真实代理，用 TCP / UDP 客户端和模拟源站验证
```

源码分为三层：

- **交互层**：`cli`（命令行，给 Agent / 脚本）和 `ui`（TUI，给人）。只调用服务层，从不直接读写配置文件、控制 socket，或调用 systemctl / launchctl。
- **服务层**：`manage`，两个交互层共用的管理接口。代理运行时通过控制 socket 与守护进程通信，未运行时直接读写配置文件。
- **运行层**：`daemon`（守护进程）、`proxy`（代理协议）、`net`（网络出口）。
- 基础模块：`config`（配置与数据目录）、`api`（控制协议）、`i18n`（界面语言）。

```
src/
├── main.rs              入口，直接转到 cli::main
├── lib.rs               模块声明和格式化工具
├── cli/                 交互层 · 命令行（给 Agent / 脚本）
│   ├── mod.rs           命令与参数定义、--help 中的调用说明、退出码
│   └── output.rs        文本和 JSON 输出
├── ui/                  交互层 · TUI（给人）
│   ├── mod.rs           面板入口：选定语言、初始化终端
│   ├── app.rs           状态、事件循环、按键分发、操作
│   ├── dashboard.rs     主界面各个面板
│   ├── menu.rs          运行管理菜单、退出确认
│   ├── form.rs          设置表单
│   ├── logview.rs       日志视图
│   └── theme.rs         样式、弹窗、按键提示
├── manage/              服务层 · 两个交互层共用的管理接口
│   ├── mod.rs           Manager：状态、设置读写、启动 / 停止、服务操作
│   ├── client.rs        控制通道客户端
│   ├── oneshot.rs       内嵌在面板进程里的一次性代理
│   └── service.rs       systemd / launchd / Windows 服务
├── daemon/              运行层 · 守护进程
│   ├── mod.rs           运行时状态、热更新、控制请求处理
│   ├── clock.rs         本地时间（日志和事件的时间戳）
│   ├── control.rs       控制 socket 服务端（Unix socket / Windows 命名管道）
│   ├── logging.rs       文件日志与轮转
│   └── stats.rs         流量和连接统计
├── proxy/               运行层 · 代理协议
│   ├── mod.rs           监听、协议识别
│   ├── http.rs          HTTP / HTTPS 代理：CONNECT、普通转发、keep-alive、高匿名改写
│   ├── socks5.rs        SOCKS5：CONNECT、UDP ASSOCIATE、认证
│   ├── tls.rs           自签证书与 TLS 配置
│   ├── pool.rs          普通 HTTP 转发的源站连接池
│   └── relay.rs         双向转发、流量计数
├── net/                 运行层 · 网络出口
│   ├── egress.rs        出口选择、Happy Eyeballs、把套接字绑定到网卡
│   ├── dns.rs           经出口网卡发送的 DNS 查询
│   ├── netif.rs         网卡发现（getifaddrs / GetAdaptersAddresses）、IPv6 探测
│   └── routing.rs       Linux 策略路由
├── config/
│   ├── mod.rs           配置结构、校验、设置的读取与修改
│   ├── paths.rs         数据目录的确定、文件所有者
│   └── safefs/          root 访问用户目录时的安全读写
│       ├── mod.rs       公共接口
│       ├── unix.rs      基于 openat，不跟随软链接
│       └── windows.rs   允许共享访问的文件；以管理员运行时拒绝软链接和目录联接
├── api.rs               控制协议：请求、响应、状态
└── i18n.rs              界面语言（英文 / 中文）
```

## 开发与测试

```bash
cargo build --release            # 产物：target/release/proxy-tui
cargo test                       # 单元测试 + 端到端测试
cargo clippy --all-targets
```

每次推送和 Pull Request，GitHub Actions 都会在 Linux、macOS、Windows 上运行 fmt、clippy 和全部测试。

端到端测试覆盖：

- HTTP、HTTPS、SOCKS5 的 TCP 和 UDP、IPv6、认证、协议开关
- 高匿名：源站收到的请求头和直连时一致
- HTTP keep-alive：连续和管线化的请求只发给各自的源站；源站连接复用（包括跨客户端连接）；源站关闭空闲连接后自动换新连接
- WebSocket 升级
- 服务层：运行中实时修改设置、未运行时写入配置文件

## 说明

- 控制 socket（`<数据目录>/control.sock`）的权限为 0600，所有者是数据目录的主人。只有该用户和 root 能管理代理。
- 普通 HTTP 转发的客户端连接空闲 60 秒没有新请求就会关闭。
- 运行中修改端口或监听地址时：
  - 地址没变的监听保持不动；新地址先绑定成功，并写入配置后，才切换过去。
  - 新端口被其他程序占用时，整个修改会被拒绝，原来的监听不受影响。
- macOS 用 `netstat` 读取默认路由。
- 同一台机器上，两个以 root 运行并开启 `auto_policy_routing` 的代理，应使用不同的 `table_base`。如果相同，一个退出时会清空共用的路由表，另一个最多 60 秒后才会补回。
- 出口 IP 就是客户端连入的 IP，所以代理端口也在源站看到的那个 IP 上监听。如果不希望被外部探测到，请换用非默认端口，或用防火墙只允许自己的客户端访问。

## 许可证

[MIT](LICENSE)
