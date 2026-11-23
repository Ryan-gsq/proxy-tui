//! 控制 socket 服务端：一行一个 JSON 请求 / 响应。
//!
//! Unix 上是数据目录里的 socket 文件（0600，归目录主人）；Windows 上是由数据目录
//! 派生出名字的命名管道（只接受本机连接）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

use crate::api::{Request, Response};
use crate::daemon::Ctx;

/// Longest request line accepted; status requests are tiny.
const MAX_REQUEST: u64 = 1 << 20;

#[cfg(unix)]
pub async fn serve(path: PathBuf, ctx: Arc<Ctx>) -> Result<JoinHandle<()>> {
    use anyhow::{bail, Context};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use tokio::net::UnixListener;

    use crate::config::{safefs, SafeDir};

    let (dir_path, name) = safefs::split(&path)?;
    let dir = SafeDir::create(dir_path).with_context(|| format!("打开 {}", dir_path.display()))?;
    if dir.exists(name) {
        if StdUnixStream::connect(&path).is_ok() {
            bail!("已有代理在运行（控制 socket {}）", path.display());
        }
        let _ = dir.remove(name);
    }
    // Create the socket private from the start instead of chmod-ing it by path.
    let old_mask = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(&path);
    unsafe { libc::umask(old_mask) };
    let listener = bound.with_context(|| format!("绑定 {}", path.display()))?;
    // A root service hands the socket to the data dir's owner, so that user's
    // TUI can manage it without sudo.
    dir.give_entry(name)?;
    Ok(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve_client(stream, ctx.clone()));
                }
                Err(e) => {
                    tracing::warn!("control accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }))
}

#[cfg(windows)]
pub async fn serve(path: PathBuf, ctx: Arc<Ctx>) -> Result<JoinHandle<()>> {
    use anyhow::{anyhow, Context};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    let name = crate::config::control_pipe_name(&path);
    let pipe = name.clone();
    let create = move |first: bool| -> std::io::Result<NamedPipeServer> {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create(&pipe)
    };
    let mut server = match create(true) {
        Ok(server) => server,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(anyhow!("已有代理在运行（控制管道 {name}）"));
        }
        Err(e) => return Err(e).with_context(|| format!("创建 {name}")),
    };
    Ok(tokio::spawn(async move {
        loop {
            let accepted = server.connect().await;
            // The next instance must exist before this one is handed off.
            let next = loop {
                match create(false) {
                    Ok(next) => break next,
                    Err(e) => {
                        tracing::warn!("control pipe: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            };
            let connected = std::mem::replace(&mut server, next);
            match accepted {
                Ok(()) => {
                    tokio::spawn(serve_client(connected, ctx.clone()));
                }
                Err(e) => tracing::warn!("control accept failed: {e}"),
            }
        }
    }))
}

/// Where the control channel lives, for messages.
pub fn describe(path: &Path) -> String {
    #[cfg(unix)]
    return path.display().to_string();
    #[cfg(windows)]
    return crate::config::control_pipe_name(path);
}

/// Removes the control socket without following a symlink.
#[cfg(unix)]
pub fn remove_socket(path: &Path) {
    use crate::config::{safefs, SafeDir};
    if let Ok((dir, name)) = safefs::split(path) {
        if let Ok(dir) = SafeDir::open(dir) {
            let _ = dir.remove(name);
        }
    }
}

/// A named pipe disappears with its last handle.
#[cfg(windows)]
pub fn remove_socket(_path: &Path) {}

async fn serve_client<S>(stream: S, ctx: Arc<Ctx>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match (&mut reader).take(MAX_REQUEST).read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) if !line.ends_with('\n') => break,
            Ok(_) => {}
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => ctx.handle(req).await,
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        let Ok(mut out) = serde_json::to_vec(&resp) else {
            break;
        };
        out.push(b'\n');
        if writer.write_all(&out).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
}
