# proxy-tui

**English** | [简体中文](README.zh-CN.md)

[![CI](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/ci.yml)
[![Build](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/release.yml/badge.svg)](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/release.yml)

A high-throughput proxy for Linux, macOS and Windows, written in Rust. One port serves HTTP, HTTPS and SOCKS5; traffic leaves through the same network interface it came in on; and it comes with a TUI dashboard for people and a CLI for agents and scripts.

![proxy-tui dashboard demo](docs/demo.gif)

## Highlights

- **One port, many protocols**: a single mixed port detects HTTP, HTTPS (TLS between client and proxy) and SOCKS5 (TCP + UDP). Each protocol can be switched on or off.
- **Consistent egress IP**: on a machine with several uplinks, a client that connects in through a given interface and IP goes out through that same interface and IP. DNS lookups use that egress too. IPv6 is only used outbound when it actually works.
- **High anonymity**: requests forwarded to the origin carry no trace of the proxy (no `Via`, `X-Forwarded-For` and the like); the origin sees what a direct client would send.
- **Connection reuse**: plain HTTP forwarding supports keep-alive, and origin connections are pooled and shared across client connections.
- **Two ways to manage it**:
  - **TUI dashboard** for people: settings, traffic, interfaces, connections and logs, plus service management. The interface is available in English and Chinese.
  - **CLI** for agents and scripts: full control over the proxy, JSON output and well-defined exit codes.
  - Both go through the same service layer and share **one data directory**.
- **Two run modes**:
  - **One-shot**: no root needed; stops with the process that started it.
  - **Service**: managed by systemd / launchd / the Windows service manager, runs in the background and starts at boot; needs root (Administrator on Windows).

## Contents

- [Installation](#installation)
- [Quick start](#quick-start)
- [Features](#features)
- [Multiple uplinks and IP consistency](#multiple-uplinks-and-ip-consistency)
- [TUI dashboard](#tui-dashboard)
- [CLI (for agents and scripts)](#cli-for-agents-and-scripts)
- [Run modes](#run-modes)
- [Data directory](#data-directory)
- [Logs](#logs)
- [Configuration](#configuration)
- [Project structure](#project-structure)
- [Development and testing](#development-and-testing)
- [Notes](#notes)
- [License](#license)

## Installation

### Prebuilt binaries

Every version tag publishes binaries on the [Releases](https://github.com/Ryan-gsq/proxy-tui/releases) page, and every push to `main` builds them as artifacts of the [Build workflow](https://github.com/Ryan-gsq/proxy-tui/actions/workflows/release.yml):

| Platform | Archive |
|---|---|
| Linux x86_64 / aarch64 (static, musl) | `proxy-tui-*-x86_64-unknown-linux-musl.tar.gz` / `…-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel / Apple silicon | `proxy-tui-*-x86_64-apple-darwin.tar.gz` / `…-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `proxy-tui-*-x86_64-pc-windows-msvc.zip` |

### From source

Requires a stable Rust toolchain (on Windows, with the MSVC build tools).

```bash
git clone https://github.com/Ryan-gsq/proxy-tui.git
cd proxy-tui
cargo build --release
sudo cp target/release/proxy-tui /usr/local/bin/   # optional; installing the service copies it there too
```

Cross-compiling for Linux on macOS (needs [zig](https://ziglang.org/) and [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)):

```bash
cargo zigbuild --release --target x86_64-unknown-linux-gnu
cargo zigbuild --release --target aarch64-unknown-linux-gnu
```

On Linux, iproute2 (the `ip` command) is needed to read routes and set up policy routing.

## Quick start

```bash
# People: open the dashboard (s: start / manage the service, e: edit settings, l: logs, L: language)
proxy-tui

# Agents / scripts: start in the background; once the proxy is ready, it prints JSON with ready-to-use proxy URLs
PROXY_TUI_PASSWORD='s3cret' proxy-tui run --detach --json -p 7890 -u agent --protocols http,socks5,udp
proxy-tui status --json       # status
proxy-tui stop                # stop

# Run in the background and start at boot (needs root)
sudo proxy-tui service install
```

Connecting clients (HOST is the proxy machine's address, `127.0.0.1` locally):

```bash
curl -x http://USER:PASS@HOST:PORT https://example.com                    # HTTP proxy
curl -x socks5h://USER:PASS@HOST:PORT https://example.com                 # SOCKS5 (UDP supported)
curl --proxy-insecure -x https://USER:PASS@HOST:PORT https://example.com  # TLS to the proxy
# The self-signed certificate is cert.pem in the data directory; trust it or pass it with --proxy-cacert
```

The end of `proxy-tui --help` contains a complete usage guide (options, proxy URL formats, run modes, data directory, exit codes) that agents can read directly.

## Features

- **Mixed port**: the protocol is detected from the first byte of a connection
  - `0x05` → SOCKS5
  - `0x16` → TLS, then HTTP or SOCKS5 inside it
  - a letter → HTTP
- **Protocol switches**: HTTP, HTTPS, SOCKS5 and UDP can be enabled or disabled separately (`--protocols`, `config set`, the `[protocols]` section of the config file, or the TUI form). Connections using a disabled protocol are closed (a SOCKS5 UDP request gets `0x07`).
- **HTTP proxy**: `CONNECT` tunnels and plain forwarding (absolute-URI). For plain forwarding:
  - Keep-alive: a client connection can carry many requests, one after another or pipelined. Request and response bodies are delimited strictly by `Content-Length`, chunked encoding or connection close.
  - Each request goes only to the site named in its own URI. A later request (and the proxy credentials in it) never reaches the origin of an earlier one.
  - Requests with both `Content-Length` and `Transfer-Encoding`, or with conflicting `Content-Length` values, get a 400, which prevents request smuggling.
  - `Host` is rewritten from the request URI; `Proxy-Connection` turns back into `Connection`; hop-by-hop headers are removed, including those listed in `Connection` (which cannot remove `Content-Length`, `Transfer-Encoding` or `Host`).
  - 1xx interim responses such as `Expect: 100-continue` work. Upgrade requests such as WebSocket become a two-way tunnel only after the origin answers 101.
  - The first request on a connection must carry valid credentials; later ones may omit them, but if present they must be valid.
  - A client connection can send `CONNECT` after plain requests.
- **HTTPS proxy**: the client-to-proxy leg is encrypted with TLS. A self-signed certificate (`cert.pem` / `key.pem`) is generated on first start. Inside TLS it behaves exactly like the HTTP proxy; TLS session resumption is supported.
- **SOCKS5**: CONNECT and UDP ASSOCIATE, with username/password authentication (RFC 1929).
- **High anonymity**:
  - Plain forwarding adds no headers at all: no `Via`, `X-Forwarded-For` or `Forwarded`.
  - Proxy and client-address headers that the client itself sends are removed as well: every `Proxy-*`, `X-Forwarded-*` and `X-Proxy*` header, plus `Via`, `Forwarded`, `X-Real-IP`, `Client-IP`, `True-Client-IP`, `X-Client-IP`, `X-Cluster-Client-IP`, `X-Originating-IP`, `X-Remote-IP`, `X-Remote-Addr` and similar.
  - All other headers keep the client's order and spelling.
  - `CONNECT` tunnels and SOCKS5 relay bytes unchanged. The origin sees the egress IP.
- **Connection reuse**: origin connections of plain HTTP forwarding go into a pool and are reused by later requests, including requests from other client connections of the HTTP and HTTPS proxies.
  - The pool is keyed by egress and site, so reuse never changes the egress IP.
  - A pooled connection is checked before use. If a reused connection closes without any response, a GET / HEAD / OPTIONS without a body is retried once on a new connection.
  - Idle connections close after 60 seconds; at most 8 are kept per site and 256 in total.
  - When an HTTP/1.1 client asks for `close`, only the client side closes; the origin connection stays available. HTTP/1.0 requests are forwarded as they are.
  - `CONNECT` and SOCKS5 tunnels carry the client's own encrypted session with the site, so they cannot be shared with other clients. Clients reuse their tunnels internally, and the proxy puts no idle limit on them.
- **IPv4 and IPv6**: separate v4 and v6 listeners (`IPV6_V6ONLY` on the v6 socket), so the local address a client connected to is always known exactly.
- **Single-user authentication**: HTTP Basic and SOCKS5 share one username and password; leaving both empty disables authentication.
- **Start at boot**: in service mode, via systemd on Linux, launchd on macOS and the service manager on Windows.
- **Performance**:
  - tokio multi-threaded runtime
  - plain HTTP forwarding reuses origin connections, saving repeated DNS lookups and TCP handshakes
  - 32 KiB copy buffers per direction by default (configurable)
  - `TCP_NODELAY`
  - `IP_BIND_ADDRESS_NO_PORT` on Linux, so binding a source address before connecting does not exhaust ports
  - raises `RLIMIT_NOFILE` automatically
  - measured on loopback: about 32 Gbit/s on a single connection

## Multiple uplinks and IP consistency

Each connection is mapped, by **the local IP the client connected to**, to the interface that owns it. Outbound connections are then bound like this:

- Bound to that interface: `SO_BINDTODEVICE` on Linux, `IP_BOUND_IF` / `IPV6_BOUND_IF` on macOS.
- Bound to a source IP as well:
  - with the same address family as the entry, **the entry IP itself** is used, so several IPs on one interface still map one to one;
  - when the other family is needed, the address at the same position in that family's list is used, so the pairing stays stable.
- DNS queries leave through the same egress:
  - Servers come from the config or `/etc/resolv.conf`. If that only lists a local stub (such as systemd-resolved's `127.0.0.53`), the upstream servers in `/run/systemd/resolve/resolv.conf` are used.
  - `/etc/hosts` is honoured.
  - Truncated responses are retried over TCP.
  - If DNS through the egress is unavailable, the system resolver is used (DNS then no longer follows the egress).
- **Automatic policy routing on Linux** (needs root, `egress.auto_policy_routing = true`):
  - When two or more interfaces have a default route, each interface's routes in the main table are copied into a table of its own (`table_base + ifindex`).
  - Two rules are added per address:
    - `from <IP> lookup main suppress_prefixlength 0` (priority `rule_priority-1`), so local subnets still use their direct routes;
    - `from <IP> lookup <table>` (priority `rule_priority`), so default traffic uses that interface's own gateway.
  - They are re-synchronised every 60 seconds, when interfaces change, and right after `table_base`, `rule_priority` or `auto_policy_routing` change.
  - **Only its own rules and tables are touched**: synchronisation and cleanup on exit delete only rules this process added and flush only tables it wrote. Rules of other programs at the same priorities are left alone.
  - A table that already holds other routes is never flushed: that interface reports that its routing table is in use; change `table_base` in that case.
- **macOS** already supports per-interface default routes; binding to the interface is enough.
- **Windows** sends with the strong host model by default: a socket bound to a source address leaves through the interface that owns it. The proxy also sets `IP_UNICAST_IF` / `IPV6_UNICAST_IF`. Interfaces, default gateways and DNS servers come from the adapter settings (`GetAdaptersAddresses`); no policy routing is needed.
- Interfaces without a default route (docker0, LAN-only interfaces, loopback) are not used as egress; such connections follow the system routes. `egress.iface_override` can map interfaces by hand, for example `eth0 = "eth1"`.

### When IPv6 counts as an egress

An interface is a usable IPv6 egress only when all of these hold:

- it has a global IPv6 address (not `fe80::/10`; on Linux also not deprecated or tentative);
- it has an IPv6 default route;
- a TCP probe to public DNS servers, bound to that interface, succeeds.

The probe runs every `probe_interval_secs` seconds and immediately when interfaces change.

Clients connecting over IPv6 are subject to the same check: if the interface's IPv6 probe fails, the connection goes out over IPv4 (or fails under the `strict` policy). A client connecting to a ULA address (`fc00::/7`, LAN only) goes out from a global IPv6 address of the same interface.

On macOS only routes with the gateway flag (`G`) count as default routes; `link#N` cloned routes such as those of `bridge100` do not.

### Egress policy (`egress.policy`)

| Policy | Behaviour | When the target lacks an address in that family |
|---|---|---|
| `same_iface` (default) | same interface, same family as the entry first | falls back to the interface's other family |
| `strict` | same family as the entry only | fails (SOCKS5 replies `0x03`/`0x04`, HTTP returns 502) |
| `prefer_v4` / `prefer_v6` | same interface, fixed family preference | falls back to the other family |

Domain targets are resolved for both A and AAAA records, filtered to the families the interface can use, and connected with RFC 8305 Happy Eyeballs: the preferred family first, the other one 250 ms later.

## TUI dashboard

Running `proxy-tui` without arguments opens the dashboard. It uses the same service layer and data directory as the CLI: a proxy started from the CLI shows up in the dashboard, and settings changed from the CLI appear there live.

- A coloured badge in the title bar shows the mode: magenta for one-shot, green for service, grey when nothing runs.
- The "Run mode" box shows which process runs the proxy and how to stop it, plus the service state (installed, running, PID, start at boot).
- The rest shows settings, live and total traffic with a rate chart, per-interface statistics, active connections and events.

| Key | Action |
|---|---|
| `s` | Run mode menu: 1 start one-shot · 2 stop · 3 start service · 4 stop service · 5 restart service · 6 install service · 7 uninstall service. Actions that drop connections ask for `y` |
| `e` | Edit settings: port, username, password, protocol switches, egress policy. Applied immediately while running; written to the config file otherwise |
| `l` | Log view: `↑` `↓` scroll (scrolling up pauses following), `End` jumps to the newest line, `f` toggles following |
| `p` | Show or hide the password |
| `r` | Reload the config file |
| `L` | Switch the interface language (English / 中文) and save it to the config file |
| `q` | Quit; if the proxy runs inside this dashboard, asks first, since quitting stops it |

Starting one-shot mode from the dashboard first shows the settings form. The proxy then runs inside the dashboard process; quitting the dashboard or closing the terminal stops it.

Without root (on Windows: an Administrator terminal), service actions (start, stop, install, ...) are unavailable, but one-shot mode works and the service's settings can still be edited (as long as the data directory is yours). Service actions run on a background thread, so the interface never freezes.

### Interface language

The dashboard comes in English and Chinese. The language is chosen in this order:

1. `proxy-tui tui --lang en|zh|auto` or the `PROXY_TUI_LANG` environment variable: for this session only.
2. `[ui] language` in the config file: pressing `L` in the dashboard saves the choice here; service and one-shot mode share it.
3. `auto` (default) follows the system: `LANGUAGE`, `LC_ALL`, `LC_MESSAGES` and `LANG`, in that order; on macOS, when none is set, the system's preferred language. Chinese locales get Chinese, everything else English.

CLI messages are currently in Chinese regardless of this setting.

## CLI (for agents and scripts)

### Commands

| Command | Purpose |
|---|---|
| `run [--detach] [settings]` | Start in one-shot mode. With `--detach` it runs in the background and returns once the proxy is listening; without it, it runs in the foreground until Ctrl+C |
| `stop` | Stop a one-shot proxy (use `service stop` for the service) |
| `status` | State, settings, proxy URLs, traffic, interfaces; when not running, the settings from the config file |
| `config show` | Current settings: live values while running, the config file otherwise |
| `config set <settings>` | Change settings: applied **immediately** while running, otherwise written to the config file for the next start |
| `config reload` | Make a running proxy re-read a hand-edited `config.toml` |
| `conns [-n 50]` | Active connections, by traffic |
| `logs [-n 100] [-f]` | Show the log; `-f` keeps printing new lines. With `--json` the output is `{"ok", "log_path", "lines"}`, and with `-f` one `{"line": ...}` object per line |
| `service status\|install\|start\|stop\|restart\|uninstall` | Service management (everything but `status` needs root, or an Administrator terminal on Windows); `install` also accepts settings |
| `tui [--start] [--lang auto\|en\|zh] [settings]` | Open the dashboard (the default without a command); `--start` starts one-shot mode right away; `--lang` sets the interface language for this session |

Global options for every command:

- `--dir <DIR>`: the data directory, also settable with `PROXY_TUI_DIR`
- `--json`: JSON output

### Settings options

Shared by `run`, `config set`, `service install` and `tui --start`:

| Option | Meaning |
|---|---|
| `-p, --port <PORT>` | The mixed port shared by HTTP / HTTPS / SOCKS5 (1-65535) |
| `-u, --user <USER>` | Username; set together with the password |
| `-P, --password <PASS>` | Password. With `-u` but without `-P`, it is read from `PROXY_TUI_PASSWORD`, which keeps it out of process lists and shell history. Commands without `-u` never read that variable, so an exported value cannot change the password by accident |
| `--random-password` | Generate a random 16-character password (shown in the output) |
| `--no-auth` | Disable authentication (clears username and password) |
| `--protocols <LIST>` | Enabled protocols: any combination of `http,https,socks5,udp`, or `all`. At least one of http, https, socks5; `udp` requires `socks5` |
| `--policy <POLICY>` | Egress policy: `same_iface` (default), `strict`, `prefer_v4`, `prefer_v6` |

Settings are written to `config.toml` in the data directory and stay in effect until changed again. Service and one-shot mode use the same config.

### JSON output

`run --detach --json`, `status --json` and `config show|set --json` return everything needed to use the proxy:

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

- `proxy_urls` lists only enabled protocols and is ready to use; username and password are URL-encoded. SOCKS5 uses `socks5h`, so the proxy resolves names and DNS follows the same egress.
  - The host part follows the actual listen address: loopback (`127.0.0.1` or `::1`) for wildcard listeners, otherwise the specific address.
  - The HTTPS URL is listed only when the certificate loaded.
- `listen_errors`: present only when some address could not be bound; lists the addresses and reasons (the proxy keeps running on the others).
- `mode`: `oneshot` or `service`.
- `stoppable`: whether `proxy-tui stop` can stop it; `false` for the service and for a proxy running inside a dashboard.
- `status --json` also includes `service` (the service state) and `daemon` (the full runtime status).
- `config set --json` also includes `applied`: `live` when a running proxy applied it, `saved` when only the config file was written.
- Errors, including usage errors, print `{"ok": false, "error": "..."}`.
- A foreground `run --json` prints the status once the proxy is listening; if startup fails, it prints only the error.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Failure, such as the proxy already running, root required, or a refused request |
| 2 | Usage error, including `config set` without any setting and `tui` with settings but without `--start` |
| 3 | The proxy is not running (for commands that need it, such as `status`, `config reload`, `conns`) |

### Typical flow

```bash
export PROXY_TUI_PASSWORD='s3cret'                                       # read only together with -u
proxy-tui run --detach --json -u agent -p 7890 --protocols http,socks5   # start
proxy-tui status --json >/dev/null || echo "not running"                 # check
proxy-tui config set --protocols all --json                              # change settings live
proxy-tui stop                                                           # stop
```

## Run modes

| Mode | Started by | Stopped by | Root |
|---|---|---|---|
| **One-shot · foreground** | `proxy-tui run` | Ctrl+C, closing the terminal, `kill`, or `proxy-tui stop` | No |
| **One-shot · background** | `proxy-tui run --detach` | `proxy-tui stop` (closing the calling terminal does not stop it) | No |
| **One-shot · in the dashboard** | `s` → 1 in the dashboard, or `proxy-tui tui --start` | quitting the dashboard or closing the terminal; `proxy-tui stop` is refused | No |
| **Service** | `sudo proxy-tui service install` (Windows: `proxy-tui service install` in an Administrator terminal), or `s` → 6 in the dashboard | `sudo proxy-tui service stop` (it still starts at the next boot) | Yes (Administrator on Windows) |

- Only one proxy can run per data directory. While one runs:
  - `run` fails;
  - `service start` and `service install` ask you to stop the one-shot proxy first.
- Choosing "Install (keep it running)" in the dashboard stops the dashboard's one-shot proxy first and hands over to the service.
- One-shot mode needs no root, but Linux policy routing is configured only when running as root.
- On Windows the service runs as LocalSystem and is restarted automatically if it crashes; the program is installed to `%ProgramFiles%\proxy-tui\proxy-tui.exe`.

## Data directory

Config, certificate, logs and the control socket all live in **one data directory**, shared by the TUI and the CLI, and by service and one-shot mode:

```
~/.proxy-tui/
├── config.toml      configuration
├── cert.pem         self-signed certificate of the HTTPS proxy
├── key.pem
├── logs/            logs
└── control.sock     control socket (only while running; a named pipe on Windows)
```

On Windows the default is `%USERPROFILE%\.proxy-tui`, and the control channel is the named pipe `\\.\pipe\proxy-tui-<hash of the data directory>`, which accepts local clients only.

The data directory is chosen in this order:

1. `--dir <DIR>` or the `PROXY_TUI_DIR` environment variable
2. when the service is installed, the service's directory (the `daemon --dir` in its unit / plist)
3. `~/.proxy-tui`; under `sudo` this is **the calling user's** home, so it is the same place with or without sudo

So you can try one-shot mode without root first, then run `sudo proxy-tui service install`, and the service keeps using the same config.

The service runs as root, but every file it creates or rewrites in the data directory is handed back to the directory's owner. That user can therefore open the dashboard, change settings and read logs **without sudo**; only starting, stopping and installing the service need sudo. New directories get mode 0700; config, private key and logs 0600.

Because a root process reads and writes a directory owned by a regular user, root access to it is guarded:

- Every file is accessed relative to the opened directory without following symlinks; on the way to the directory only symlinks created by root are followed (such as macOS's `/tmp → /private/tmp`).
- Before an existing file is read or written, it must be a regular file with a single hard link, owned by the directory's owner or root.
- New files are created with their final permissions. The config is written to a randomly named temporary file and then atomically renamed.
- `tls.cert`, `tls.key` and `log.dir` may then only point inside the data directory.

This way the directory's owner cannot use the root service to read or write files outside the directory; if the whole data directory is replaced by a symlink, the service refuses to start and says why.

On Windows, files inherit the data directory's permissions. An elevated process refuses to follow symlinks and junctions in the data directory, and `tls.cert`, `tls.key` and `log.dir` must stay inside it. Managing a running service from the CLI or the dashboard needs an Administrator terminal, because only SYSTEM and Administrators may send commands to the service's control pipe.

## Logs

The log is `<data dir>/logs/proxy-tui.log`, written by service and one-shot mode alike; each line says which mode wrote it (`service` / `oneshot`). Beyond `log.max_size_mb` it is rotated to `proxy-tui.log.1 … .N`, keeping `log.max_files` old files, so the logs never take more than about `(max_files + 1) × max_size_mb`: 60 MB with the defaults. `max_size_mb` must be at least 1, so rotation cannot be switched off.

A background thread writes the log, so proxy threads never wait for the disk. When its queue is full, lines are dropped and a line recording how many were dropped is logged. `[log]` settings take effect when the process starts; changing them requires a restart.

The log contains:

- a one-line summary per closed connection: protocol, client → target, egress interface and address, traffic up and down, duration
- connection failures (`connect failed`) and authentication failures (WARN)
- events such as config changes, listener changes, policy routing synchronisation and IPv6 egress probes

Read it with `l` in the dashboard or `proxy-tui logs -f`. On macOS, the service process's stdout and stderr go to `/var/log/proxy-tui.launchd.log`: launchd opens that file as root, so it cannot live in the user's directory.

## Configuration

The config file is `config.toml` in the data directory. It is created on first start (with username `proxy` and a random password). After editing it by hand, press `r` in the dashboard or run `proxy-tui config reload` to apply it to a running proxy.

```toml
[listen]
port = 7890
ipv4 = "0.0.0.0"          # empty: no IPv4 listener
ipv6 = "::"               # empty: no IPv6 listener

[auth]
username = "proxy"        # both empty: no authentication
password = "..."

[protocols]               # at least one of http / https / socks5; udp requires socks5
http = true               # plain HTTP proxy (CONNECT + forwarding)
https = true              # TLS-encrypted proxy connections (self-signed certificate)
socks5 = true             # SOCKS5 TCP
udp = true                # SOCKS5 UDP ASSOCIATE

[tls]
cert = ""                 # empty: cert.pem / key.pem in the data directory (generated when missing)
key = ""

[egress]
policy = "same_iface"     # same_iface | strict | prefer_v4 | prefer_v6
auto_policy_routing = true
table_base = 1000         # 1000 and up; each interface uses routing table table_base + ifindex
rule_priority = 9000      # 1000–32000; rules use rule_priority-1 and rule_priority
probe_interval_secs = 60
connect_timeout_secs = 10
[egress.iface_override]   # entry interface = egress interface
# eth0 = "eth1"

[dns]
servers = []              # empty: the non-local servers from /etc/resolv.conf
timeout_ms = 1500
cache_ttl_max_secs = 600

[performance]
buffer_size = 32768
handshake_timeout_secs = 30

[ui]
language = "auto"         # dashboard language: auto (follow the system; English unless Chinese), en, zh; L in the dashboard saves here

[log]
level = "info"            # off / error / warn / info / debug / trace (per-connection summaries are info)
dir = ""                  # empty: <data dir>/logs
max_size_mb = 10          # size limit per file before rotation (at least 1)
max_files = 5             # keeps proxy-tui.log.1 … .5; 60 MB in total at most
```

## Project structure

```
proxy-tui/
├── Cargo.toml / Cargo.lock
├── LICENSE            MIT license
├── README.md            English documentation
├── README.zh-CN.md      Chinese documentation
├── docs/demo.gif        dashboard demo
├── .github/workflows/
│   ├── ci.yml           fmt, clippy and tests on Linux, macOS and Windows
│   └── release.yml      builds for all platforms; release on version tags
├── src/                 source code (below)
└── tests/e2e.rs         end-to-end tests: a real proxy on localhost, driven by TCP / UDP clients and mock origins
```

The source is organised in three layers:

- **Interface layer**: `cli` (command line, for agents and scripts) and `ui` (TUI, for people). They only call the service layer and never touch the config file, the control socket, systemctl or launchctl directly.
- **Service layer**: `manage`, the management API both interfaces share. It talks to the daemon over the control socket while the proxy runs, and reads and writes the config file when it does not.
- **Runtime layer**: `daemon` (the daemon), `proxy` (proxy protocols), `net` (egress).
- Foundations: `config` (configuration and data directory), `api` (control protocol), `i18n` (interface language).

```
src/
├── main.rs              entry point, hands over to cli::main
├── lib.rs               module declarations and formatting helpers
├── cli/                 interface layer · command line (agents / scripts)
│   ├── mod.rs           commands and options, the usage guide in --help, exit codes
│   └── output.rs        text and JSON output
├── ui/                  interface layer · TUI (people)
│   ├── mod.rs           entry: picks the language, sets up the terminal
│   ├── app.rs           state, event loop, key handling, actions
│   ├── dashboard.rs     the dashboard's panels
│   ├── menu.rs          run mode menu, quit confirmation
│   ├── form.rs          settings form
│   ├── logview.rs       log view
│   └── theme.rs         styles, popups, key hints
├── manage/              service layer · management API shared by both interfaces
│   ├── mod.rs           Manager: status, settings, start / stop, service actions
│   ├── client.rs        control channel client
│   ├── oneshot.rs       one-shot proxy embedded in the dashboard process
│   └── service.rs       systemd / launchd / Windows service
├── daemon/              runtime layer · daemon
│   ├── mod.rs           runtime state, live updates, control requests
│   ├── clock.rs         local time for log and event timestamps
│   ├── control.rs       control socket server (Unix socket / Windows named pipe)
│   ├── logging.rs       file logging and rotation
│   └── stats.rs         traffic and connection statistics
├── proxy/               runtime layer · proxy protocols
│   ├── mod.rs           listeners, protocol detection
│   ├── http.rs          HTTP / HTTPS proxy: CONNECT, forwarding, keep-alive, anonymising rewrite
│   ├── socks5.rs        SOCKS5: CONNECT, UDP ASSOCIATE, authentication
│   ├── tls.rs           self-signed certificate and TLS setup
│   ├── pool.rs          origin connection pool for plain HTTP forwarding
│   └── relay.rs         two-way relay, traffic counting
├── net/                 runtime layer · egress
│   ├── egress.rs        egress selection, Happy Eyeballs, binding sockets to an interface
│   ├── dns.rs           DNS queries sent through the egress interface
│   ├── netif.rs         interface discovery (getifaddrs / GetAdaptersAddresses), IPv6 probes
│   └── routing.rs       Linux policy routing
├── config/
│   ├── mod.rs           config structure, validation, reading and changing settings
│   ├── paths.rs         choosing the data directory, file ownership
│   └── safefs/          safe file access when root works in a user's directory
│       ├── mod.rs       shared interface
│       ├── unix.rs      openat-based, never follows symlinks
│       └── windows.rs   shared-access files, refuses symlinks and junctions when elevated
├── api.rs               control protocol: requests, responses, status
└── i18n.rs              interface language (English / Chinese)
```

## Development and testing

```bash
cargo build --release            # output: target/release/proxy-tui
cargo test                       # unit and end-to-end tests
cargo clippy --all-targets
```

GitHub Actions runs fmt, clippy and all tests on Linux, macOS and Windows for every push and pull request.

The end-to-end tests cover:

- TCP and UDP over HTTP, HTTPS and SOCKS5; IPv6; authentication; protocol switches
- high anonymity: the origin receives the same headers as from a direct client
- HTTP keep-alive: consecutive and pipelined requests reach only their own origins; origin connections are reused, also across client connections; a new connection replaces one the origin closed while idle
- WebSocket upgrades
- service layer: live settings changes while running, config file changes while not

## Notes

- The control socket (`<data dir>/control.sock`) has mode 0600 and belongs to the data directory's owner; only that user and root can manage the proxy.
- An idle client connection of plain HTTP forwarding closes after 60 seconds without a new request.
- When the port or listen addresses change while running:
  - listeners whose address did not change stay untouched; a new address is switched to only after it is bound and saved to the config;
  - if the new port is taken by another program, the whole change is refused and the existing listeners keep running.
- macOS default routes are read with `netstat`.
- Two proxies on the same machine that run as root with `auto_policy_routing` must use different `table_base` values. Otherwise, when one exits it flushes the shared tables, and the other restores them within 60 seconds.
- The egress IP is the IP the client connected to, so the proxy port also listens on the IP that origins see. If it should not be discoverable from outside, use a non-default port or a firewall that only admits your own clients.

## License

[MIT](LICENSE)
