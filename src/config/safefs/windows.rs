//! Windows：按路径访问。所有文件都允许其他句柄同时读、写、删除，日志轮转和配置
//! 替换不会被另一个进程打开的句柄挡住。以管理员运行时拒绝跟随软链接和目录联接。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::config::is_root;

/// FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
const SHARE_ALL: u32 = 0x7;

pub struct SafeDir {
    path: PathBuf,
}

fn refused(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {why}", path.display()),
    )
}

/// Symlinks and junctions both count: a user can make a junction without privileges.
fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.share_mode(SHARE_ALL);
    options
}

impl SafeDir {
    /// Opens an existing directory.
    pub fn open(path: &Path) -> io::Result<SafeDir> {
        let path = std::path::absolute(path)?;
        if is_root() && is_link(&path) {
            return Err(refused(&path, "是软链接或目录联接，以管理员运行时拒绝跟随"));
        }
        if !std::fs::metadata(&path)?.is_dir() {
            return Err(refused(&path, "不是目录"));
        }
        Ok(SafeDir { path })
    }

    /// Opens a directory, creating missing levels.
    pub fn create(path: &Path) -> io::Result<SafeDir> {
        std::fs::create_dir_all(path)?;
        Self::open(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A plain name inside this directory, refusing links when elevated.
    fn entry(&self, name: &str) -> io::Result<PathBuf> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("非法文件名 {name:?}"),
            ));
        }
        let path = self.path.join(name);
        if is_root() && is_link(&path) {
            return Err(refused(&path, "是软链接，以管理员运行时拒绝跟随"));
        }
        Ok(path)
    }

    fn verify(&self, file: &File, path: &Path) -> io::Result<()> {
        if !file.metadata()?.is_file() {
            return Err(refused(path, "不是普通文件"));
        }
        Ok(())
    }

    pub fn exists(&self, name: &str) -> bool {
        self.entry(name)
            .is_ok_and(|path| std::fs::symlink_metadata(path).is_ok())
    }

    /// Reads a plain file of at most `limit` bytes.
    pub fn read(&self, name: &str, limit: u64) -> io::Result<Vec<u8>> {
        let path = self.entry(name)?;
        let file = options().read(true).open(&path)?;
        self.verify(&file, &path)?;
        let mut buf = Vec::new();
        file.take(limit + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} 超过 {limit} 字节", path.display()),
            ));
        }
        Ok(buf)
    }

    /// Atomically replaces `name` with `data` (fresh file, then rename over it).
    pub fn write_atomic(&self, name: &str, data: &[u8], _mode: u32) -> io::Result<()> {
        let target = self.entry(name)?;
        let tmp = self.path.join(format!(
            ".{name}.{}.{:08x}.tmp",
            std::process::id(),
            rand::random::<u32>()
        ));
        let result = (|| {
            let mut file = options().write(true).create_new(true).open(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, &target)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Opens `name` for appending, creating it if needed.
    pub fn open_append(&self, name: &str, _mode: u32) -> io::Result<File> {
        let path = self.entry(name)?;
        let file = options().append(true).create(true).open(&path)?;
        self.verify(&file, &path)?;
        Ok(file)
    }

    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(self.entry(from)?, self.entry(to)?)
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        std::fs::remove_file(self.entry(name)?)
    }

    /// Files inherit the directory's ACL; there is no owner to hand over.
    pub fn give_entry(&self, _name: &str) -> io::Result<()> {
        Ok(())
    }
}

/// Opens a file for reading without blocking its rotation or replacement.
pub fn open_shared(path: &Path) -> io::Result<File> {
    options().read(true).open(path)
}
