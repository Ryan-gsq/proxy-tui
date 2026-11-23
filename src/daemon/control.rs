//! 控制 socket 服务端：一行一个 JSON 请求 / 响应。

use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use crate::api::{Request, Response};
use crate::config::{safefs, SafeDir};
use crate::daemon::Ctx;

/// Longest request line accepted; status requests are tiny.
const MAX_REQUEST: u64 = 1 << 20;

pub async fn serve(path: PathBuf, ctx: Arc<Ctx>) -> Result<JoinHandle<()>> {
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

/// Removes the control socket without following a symlink.
pub fn remove_socket(path: &std::path::Path) {
    if let Ok((dir, name)) = safefs::split(path) {
        if let Ok(dir) = SafeDir::open(dir) {
            let _ = dir.remove(name);
        }
    }
}

async fn serve_client(stream: UnixStream, ctx: Arc<Ctx>) {
    let (reader, mut writer) = stream.into_split();
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
        if writer.write_all(&out).await.is_err() {
            break;
        }
    }
}
