//! 控制 socket 客户端（阻塞）：服务层通过它与守护进程通信。

use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::api::{Request, Response};
use crate::tr;

/// No daemon is listening on the control socket.
#[derive(Debug)]
pub struct NotRunning(pub PathBuf);

impl fmt::Display for NotRunning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.0.display();
        match crate::i18n::is_zh() {
            true => write!(f, "代理未运行（{path} 无响应）"),
            false => write!(f, "the proxy is not running ({path} does not answer)"),
        }
    }
}

impl std::error::Error for NotRunning {}

pub fn is_not_running(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotRunning>().is_some()
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Client> {
        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                anyhow!(NotRunning(path.to_path_buf()))
            }
            io::ErrorKind::PermissionDenied => anyhow!(tr!(
                format!(
                    "no access to {}: the daemon belongs to another user, use sudo",
                    path.display()
                ),
                format!(
                    "无权访问 {}：守护进程属于其他用户，请用 sudo",
                    path.display()
                )
            )),
            _ => anyhow!(tr!(
                format!("connecting to {} failed: {e}", path.display()),
                format!("连接 {} 失败: {e}", path.display())
            )),
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Client {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        })
    }

    pub fn request(&mut self, req: &Request) -> Result<Response> {
        let mut out = serde_json::to_vec(req)?;
        out.push(b'\n');
        self.writer.write_all(&out)?;
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!(tr!(
                "the daemon closed the connection",
                "守护进程关闭了连接"
            ));
        }
        Ok(serde_json::from_str(&line)?)
    }
}
