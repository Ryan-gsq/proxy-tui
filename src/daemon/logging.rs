//! File logging shared by every run mode: `<log dir>/proxy-tui.log`, rotated by
//! size into `proxy-tui.log.1 .. .N`. Each line carries the writer's run mode.
//!
//! Lines are handed to a background thread so proxy threads never wait on the
//! disk; when the queue is full lines are dropped and counted instead.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::Duration;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::config::{safefs, Config, SafeDir, LOG_FILE};

const TAIL_BYTES: u64 = 1024 * 1024;
const QUEUE: usize = 8192;
const LOG_MODE: u32 = 0o600;

/// Log file the process is writing to, once logging is initialised.
static ACTIVE: OnceLock<PathBuf> = OnceLock::new();
static SENDER: OnceLock<SyncSender<Msg>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

enum Msg {
    Line(Vec<u8>),
    Flush(SyncSender<()>),
}

struct RotatingFile {
    dir_path: PathBuf,
    dir: Option<SafeDir>,
    file: Option<File>,
    size: u64,
    max_size: u64,
    max_files: usize,
}

impl RotatingFile {
    fn new(dir_path: PathBuf, max_size: u64, max_files: usize) -> Self {
        RotatingFile {
            dir_path,
            dir: None,
            file: None,
            size: 0,
            max_size,
            max_files,
        }
    }

    fn dir(&mut self) -> io::Result<&SafeDir> {
        if self.dir.is_none() {
            self.dir = Some(SafeDir::create(&self.dir_path)?);
        }
        Ok(self.dir.as_ref().expect("opened above"))
    }

    fn file(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            let file = self.dir()?.open_append(LOG_FILE, LOG_MODE)?;
            self.size = file.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("opened above"))
    }

    fn rotate(&mut self) {
        self.file = None;
        let max_files = self.max_files;
        let Ok(dir) = self.dir() else { return };
        for i in (1..max_files).rev() {
            let _ = dir.rename(&numbered(i), &numbered(i + 1));
        }
        let _ = if max_files > 0 {
            dir.rename(LOG_FILE, &numbered(1))
        } else {
            dir.remove(LOG_FILE)
        };
        self.size = 0;
    }

    fn append(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.max_size > 0 && self.size > 0 && self.size + buf.len() as u64 > self.max_size {
            self.rotate();
        }
        self.file()?.write_all(buf)?;
        self.size += buf.len() as u64;
        Ok(())
    }
}

fn numbered(i: usize) -> String {
    format!("{LOG_FILE}.{i}")
}

fn writer_thread(mut file: RotatingFile, rx: Receiver<Msg>) {
    for msg in rx {
        match msg {
            Msg::Line(line) => {
                let dropped = DROPPED.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    let note = format!("[log queue full: {dropped} lines dropped]\n");
                    let _ = file.append(note.as_bytes());
                }
                // Logging must never take the proxy down.
                let _ = file.append(&line);
            }
            Msg::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Opens a log file for appending (private, owned like its directory); used for
/// the stderr of a detached daemon.
pub fn open_append(path: &Path) -> io::Result<File> {
    let (dir, name) = safefs::split(path)?;
    SafeDir::create(dir)?.open_append(name, LOG_MODE)
}

#[derive(Clone)]
struct LogSink {
    stderr: bool,
}

struct SinkWriter<'a>(&'a LogSink);

impl<'a> MakeWriter<'a> for LogSink {
    type Writer = SinkWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SinkWriter(self)
    }
}

impl Write for SinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.0.stderr {
            let _ = io::stderr().write_all(buf);
        }
        if let Some(tx) = SENDER.get() {
            if let Err(TrySendError::Full(_)) = tx.try_send(Msg::Line(buf.to_vec())) {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct LocalTime {
    tag: &'static str,
}

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{} {}", timestamp(), self.tag)
    }
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        now.subsec_millis()
    )
}

/// Installs the process-wide logger writing to the config's log file. `tag`
/// marks the run mode on each line; `stderr` also echoes lines to stderr.
/// Only the first call takes effect: log settings apply from process start.
pub fn init(cfg: &Config, config_path: &Path, tag: &'static str, stderr: bool) {
    if ACTIVE.get().is_some() {
        return;
    }
    let directives = if cfg.log.level == "off" {
        "off".to_string()
    } else {
        format!("{},rustls=warn", cfg.log.level)
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directives));
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(LogSink { stderr })
        .with_timer(LocalTime { tag })
        .with_target(false)
        .with_ansi(false);
    if tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init()
        .is_err()
    {
        return;
    }
    let file = RotatingFile::new(
        cfg.log_dir(config_path),
        cfg.log.max_size_mb.saturating_mul(1024 * 1024),
        cfg.log.max_files,
    );
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let _ = SENDER.set(tx);
    let _ = ACTIVE.set(cfg.log_path(config_path));
    std::thread::Builder::new()
        .name("log-writer".into())
        .spawn(move || writer_thread(file, rx))
        .ok();
}

/// The log file this process writes to, if logging was initialised.
pub fn active_path() -> Option<&'static Path> {
    ACTIVE.get().map(PathBuf::as_path)
}

/// Waits (briefly) until queued lines are on disk; call before exiting.
pub fn flush() {
    let Some(tx) = SENDER.get() else { return };
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    if tx.send(Msg::Flush(done_tx)).is_ok() {
        let _ = done_rx.recv_timeout(Duration::from_secs(2));
    }
}

/// Log file of the config at `config_path` (defaults if the file is unreadable).
pub fn log_path_for(config_path: &Path) -> PathBuf {
    Config::load(config_path)
        .unwrap_or_default()
        .log_path(config_path)
}

/// Reads up to `max_lines` trailing lines. Returns the lines and the file length.
pub fn tail(path: &Path, max_lines: usize) -> io::Result<(Vec<String>, u64)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > max_lines {
        lines.drain(..lines.len() - max_lines);
    }
    Ok((lines, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_by_size_and_keeps_n_files() {
        let dir = std::env::temp_dir().join(format!("pt-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut file = RotatingFile::new(dir.clone(), 100, 2);
        for i in 0..10 {
            file.append(format!("line {i:02} {}\n", "x".repeat(40)).as_bytes())
                .unwrap();
        }
        let path = dir.join(LOG_FILE);
        assert!(path.exists());
        assert!(dir.join(numbered(1)).exists());
        assert!(dir.join(numbered(2)).exists());
        assert!(!dir.join(numbered(3)).exists());
        let (lines, _) = tail(&path, 10).unwrap();
        assert_eq!(lines.last().unwrap().split(' ').nth(1), Some("09"));
        let (lines, _) = tail(&path, 1).unwrap();
        assert_eq!(lines.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
