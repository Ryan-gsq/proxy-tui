//! 在可能不受信任的数据目录里安全地读写文件。
//!
//! 服务以 root（Windows 上是 LocalSystem）运行，数据目录却归普通用户所有。按路径
//! 读写时，用户可以把文件或目录换成软链接，诱使服务改写或读出目录之外的文件。
//! 平台实现见 `unix`（openat 系列，不跟随软链接，校验链接数和属主）与 `windows`。

use std::ffi::OsStr;
use std::io;
use std::path::Path;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::SafeDir;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::SafeDir;

/// Opens a file for reading without blocking its rotation or replacement.
#[cfg(unix)]
pub fn open_shared(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

#[cfg(windows)]
pub use windows::open_shared;

/// Splits `path` into its directory and file name.
pub fn split(path: &Path) -> io::Result<(&Path, &str)> {
    let name = path.file_name().and_then(OsStr::to_str);
    match (path.parent(), name) {
        (Some(dir), Some(name)) => Ok((
            if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            },
            name,
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("无效的文件路径 {}", path.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pt-safedir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn writes_reads_appends_renames_and_removes() {
        let root = scratch("portable");
        let dir = SafeDir::create(&root.join("a").join("b")).unwrap();
        dir.write_atomic("f", b"one", 0o600).unwrap();
        dir.write_atomic("f", b"two", 0o600).unwrap();
        assert_eq!(dir.read("f", 10).unwrap(), b"two");
        assert!(dir.read("f", 2).is_err(), "size limit");
        let mut log = dir.open_append("log", 0o600).unwrap();
        log.write_all(b"x").unwrap();
        // An open handle must not block rotation.
        dir.rename("log", "log.1").unwrap();
        log.write_all(b"y").unwrap();
        drop(log);
        assert_eq!(dir.read("log.1", 10).unwrap(), b"xy");
        assert!(dir.exists("log.1") && !dir.exists("log"));
        dir.remove("log.1").unwrap();
        assert!(!dir.exists("log.1"));
        assert!(
            dir.read("../f", 10).is_err(),
            "names stay inside the directory"
        );
        let left = std::fs::read_dir(root.join("a").join("b")).unwrap().count();
        assert_eq!(left, 1, "no temp files left behind");
        let _ = std::fs::remove_dir_all(&root);
    }
}
