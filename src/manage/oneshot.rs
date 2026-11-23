//! 一次性模式：把守护进程嵌在当前进程里运行，进程退出时代理随之停止。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::api::RunMode;
use crate::config::{control_socket_for, Config};
use crate::daemon::{self, logging, Daemon};

struct Embedded {
    runtime: tokio::runtime::Runtime,
    daemon: Daemon,
}

/// Handle to the proxy embedded in this process. Cloning shares the handle.
#[derive(Clone, Default)]
pub struct Oneshot {
    slot: Arc<Mutex<Option<Embedded>>>,
}

impl Oneshot {
    pub fn is_running(&self) -> bool {
        self.slot.lock().map(|e| e.is_some()).unwrap_or(false)
    }

    pub fn start(&self, config_path: &Path) -> Result<()> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Ok(());
        }
        let cfg = Config::load(config_path).unwrap_or_default();
        logging::init(&cfg, config_path, "oneshot", false);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("proxy-oneshot")
            .build()
            .context(crate::tr!("creating the runtime failed", "创建运行时失败"))?;
        let started = runtime.block_on(daemon::start(
            config_path.to_path_buf(),
            control_socket_for(config_path),
            RunMode::Oneshot,
            false,
        ));
        match started {
            Ok(daemon) => {
                *slot = Some(Embedded { runtime, daemon });
                Ok(())
            }
            Err(e) => {
                runtime.shutdown_background();
                Err(e)
            }
        }
    }

    /// Stops the embedded proxy, dropping its connections. Returns true if one ran.
    pub fn stop(&self) -> bool {
        let taken = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(Embedded { runtime, daemon }) = taken else {
            return false;
        };
        runtime.block_on(daemon.shutdown());
        runtime.shutdown_timeout(Duration::from_secs(2));
        logging::flush();
        true
    }

    /// Closing the terminal (SIGHUP), `kill` (SIGTERM) or SIGINT must still stop
    /// the proxy cleanly, even if the UI thread is stuck on a hung-up tty: a
    /// dedicated thread waits for the signal, runs `before_exit`, stops the proxy
    /// and exits. Call before any other thread is spawned so all inherit the mask.
    #[cfg(unix)]
    pub fn stop_on_termination(&self, before_exit: fn()) {
        let set = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGHUP);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            set
        };
        let handle = self.clone();
        std::thread::spawn(move || {
            let mut sig: libc::c_int = 0;
            if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                return;
            }
            before_exit();
            handle.stop();
            std::process::exit(128 + sig);
        });
    }

    /// Closing the console window, logging off or shutting down must still stop
    /// the proxy cleanly: a dedicated thread waits for the event, runs
    /// `before_exit`, stops the proxy and exits (Windows allows a few seconds).
    #[cfg(windows)]
    pub fn stop_on_termination(&self, before_exit: fn()) {
        let handle = self.clone();
        std::thread::spawn(move || {
            use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let ended = runtime.block_on(async {
                let mut close = ctrl_close().ok();
                let mut logoff = ctrl_logoff().ok();
                let mut shutdown = ctrl_shutdown().ok();
                tokio::select! {
                    Some(_) = async { close.as_mut()?.recv().await } => true,
                    Some(_) = async { logoff.as_mut()?.recv().await } => true,
                    Some(_) = async { shutdown.as_mut()?.recv().await } => true,
                    else => false,
                }
            });
            if !ended {
                return;
            }
            before_exit();
            handle.stop();
            std::process::exit(1);
        });
    }
}
