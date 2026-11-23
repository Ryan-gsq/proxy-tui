//! Unix：所有访问都从目录 fd 出发（openat 系列），不跟随软链接。

// `mode_t` is u16 on macOS and u32 on Linux: the casts are needed on one of them.
#![allow(clippy::unnecessary_cast)]

use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixListener;
use std::path::{Component, Path, PathBuf};

use crate::config::is_root;

const MAX_SYMLINKS: usize = 32;

/// An open directory; every file operation is relative to its descriptor.
pub struct SafeDir {
    fd: OwnedFd,
    path: PathBuf,
    uid: u32,
    gid: u32,
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn cname(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("非法文件名 {name:?}"),
        ));
    }
    CString::new(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn open_root() -> io::Result<OwnedFd> {
    let fd = cvt(unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat(dir: RawFd, name: &CString, flags: libc::c_int, mode: u32) -> io::Result<OwnedFd> {
    let fd = cvt(unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

fn lstat_at(dir: RawFd, name: &CString) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

fn file_type(st: &libc::stat) -> u32 {
    st.st_mode as u32 & libc::S_IFMT as u32
}

fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    cvt(unsafe { libc::fchmod(fd, mode as libc::mode_t) }).map(|_| ())
}

fn readlink_at(dir: RawFd, name: &CString) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

/// Path components as names to walk; `None` marks the root.
fn steps(path: &Path) -> Vec<Option<OsString>> {
    path.components()
        .filter_map(|c| match c {
            Component::RootDir => Some(None),
            Component::Normal(name) => Some(Some(name.to_os_string())),
            Component::ParentDir => Some(Some(OsString::from(".."))),
            Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

fn symlink_refused(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "拒绝跟随软链接 {}（以 root 运行时只跟随 root 自己创建的软链接）",
            path.display()
        ),
    )
}

impl SafeDir {
    /// Opens an existing directory.
    pub fn open(path: &Path) -> io::Result<SafeDir> {
        Self::walk(path, false)
    }

    /// Opens a directory, creating missing levels (0700, owned like their parent).
    pub fn create(path: &Path) -> io::Result<SafeDir> {
        Self::walk(path, true)
    }

    /// Walks `path` one component at a time without following symlinks. As root,
    /// a symlink is followed only if root owns both it and its directory, so the
    /// owner of an untrusted directory cannot redirect the walk.
    fn walk(path: &Path, create: bool) -> io::Result<SafeDir> {
        let abs = std::path::absolute(path)?;
        let strict = is_root();
        let mut queue: VecDeque<Option<OsString>> = steps(&abs).into();
        let mut fd = open_root()?;
        let mut links = 0;
        while let Some(step) = queue.pop_front() {
            let Some(name) = step else {
                fd = open_root()?;
                continue;
            };
            let c = cname(&name)?;
            let dir_flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
            let err = match openat(fd.as_raw_fd(), &c, dir_flags, 0) {
                Ok(child) => {
                    fd = child;
                    continue;
                }
                Err(e) => e,
            };
            match err.raw_os_error() {
                Some(libc::ENOENT) if create && name != ".." => {
                    let made = unsafe { libc::mkdirat(fd.as_raw_fd(), c.as_ptr(), 0o700) };
                    if made < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                        return Err(io::Error::last_os_error());
                    }
                    let child = openat(fd.as_raw_fd(), &c, dir_flags, 0)?;
                    if made == 0 {
                        fchmod(child.as_raw_fd(), 0o700)?;
                        if strict {
                            let parent = fstat(fd.as_raw_fd())?;
                            cvt(unsafe {
                                libc::fchown(child.as_raw_fd(), parent.st_uid, parent.st_gid)
                            })?;
                        }
                    }
                    fd = child;
                }
                Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
                    let st = lstat_at(fd.as_raw_fd(), &c)?;
                    if file_type(&st) != libc::S_IFLNK as u32 {
                        return Err(err);
                    }
                    if strict && (st.st_uid != 0 || fstat(fd.as_raw_fd())?.st_uid != 0) {
                        return Err(symlink_refused(&abs));
                    }
                    links += 1;
                    if links > MAX_SYMLINKS {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    let target = readlink_at(fd.as_raw_fd(), &c)?;
                    // Relative targets resolve against the directory holding the link.
                    let mut next: VecDeque<_> = steps(&target).into();
                    next.append(&mut queue);
                    queue = next;
                }
                _ => return Err(err),
            }
        }
        let st = fstat(fd.as_raw_fd())?;
        Ok(SafeDir {
            fd,
            path: abs,
            uid: st.st_uid,
            gid: st.st_gid,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Owner uid of the directory.
    pub fn owner(&self) -> u32 {
        self.uid
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Hands a new file to the directory's owner when running as root.
    fn give(&self, fd: &OwnedFd) -> io::Result<()> {
        if is_root() {
            cvt(unsafe { libc::fchown(fd.as_raw_fd(), self.uid, self.gid) })?;
        }
        Ok(())
    }

    /// Refuses anything but a plain file; as root also a hard link or a file
    /// owned by someone other than root or the directory's owner.
    fn verify(&self, fd: &OwnedFd, name: &str) -> io::Result<()> {
        let st = fstat(fd.as_raw_fd())?;
        let bad = |why: &str| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} {why}", self.path.join(name).display()),
            )
        };
        if file_type(&st) != libc::S_IFREG as u32 {
            return Err(bad("不是普通文件"));
        }
        if is_root() && (st.st_nlink != 1 || (st.st_uid != 0 && st.st_uid != self.uid)) {
            return Err(bad("是硬链接或属主不符"));
        }
        Ok(())
    }

    pub fn exists(&self, name: &str) -> bool {
        cname(OsStr::new(name))
            .and_then(|c| lstat_at(self.raw(), &c))
            .is_ok()
    }

    /// Reads a plain file of at most `limit` bytes.
    pub fn read(&self, name: &str, limit: u64) -> io::Result<Vec<u8>> {
        let c = cname(OsStr::new(name))?;
        let fd = openat(
            self.raw(),
            &c,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )?;
        self.verify(&fd, name)?;
        let mut buf = Vec::new();
        File::from(fd).take(limit + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} 超过 {limit} 字节", self.path.join(name).display()),
            ));
        }
        Ok(buf)
    }

    /// Atomically replaces `name` with `data` (fresh file with `mode`, then rename).
    pub fn write_atomic(&self, name: &str, data: &[u8], mode: u32) -> io::Result<()> {
        let target = cname(OsStr::new(name))?;
        let tmp_name = format!(
            ".{name}.{}.{:08x}.tmp",
            std::process::id(),
            rand::random::<u32>()
        );
        let tmp = cname(OsStr::new(&tmp_name))?;
        let fd = openat(
            self.raw(),
            &tmp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            mode,
        )?;
        let result = (|| {
            fchmod(fd.as_raw_fd(), mode)?;
            self.give(&fd)?;
            let mut file = File::from(fd);
            file.write_all(data)?;
            file.sync_all()?;
            cvt(unsafe { libc::renameat(self.raw(), tmp.as_ptr(), self.raw(), target.as_ptr()) })
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(self.raw(), tmp.as_ptr(), 0) };
        }
        result.map(|_| ())
    }

    /// Opens `name` for appending; a new file gets `mode` and the directory's owner.
    pub fn open_append(&self, name: &str, mode: u32) -> io::Result<File> {
        let c = cname(OsStr::new(name))?;
        let base = libc::O_WRONLY | libc::O_APPEND | libc::O_NOFOLLOW;
        match openat(self.raw(), &c, base | libc::O_CREAT | libc::O_EXCL, mode) {
            Ok(fd) => {
                fchmod(fd.as_raw_fd(), mode)?;
                self.give(&fd)?;
                Ok(File::from(fd))
            }
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                let fd = openat(self.raw(), &c, base | libc::O_NONBLOCK, 0)?;
                self.verify(&fd, name)?;
                Ok(File::from(fd))
            }
            Err(e) => Err(e),
        }
    }

    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (a, b) = (cname(OsStr::new(from))?, cname(OsStr::new(to))?);
        cvt(unsafe { libc::renameat(self.raw(), a.as_ptr(), self.raw(), b.as_ptr()) }).map(|_| ())
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        let c = cname(OsStr::new(name))?;
        cvt(unsafe { libc::unlinkat(self.raw(), c.as_ptr(), 0) }).map(|_| ())
    }

    /// Binds a Unix socket as `name` that is private (0600, the directory's
    /// owner) from the moment it appears: it is bound in a fresh 0700 directory,
    /// fixed up there and hard-linked in. Fails if `name` already exists.
    pub fn bind_socket(&self, name: &str) -> io::Result<UnixListener> {
        let target = cname(OsStr::new(name))?;
        let stage_name = format!(".sock-{:08x}", rand::random::<u32>());
        let stage = cname(OsStr::new(&stage_name))?;
        cvt(unsafe { libc::mkdirat(self.raw(), stage.as_ptr(), 0o700) })?;
        let dir_flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        let result = openat(self.raw(), &stage, dir_flags, 0).and_then(|stage_fd| {
            let listener = UnixListener::bind(self.path.join(&stage_name).join(name))?;
            let staged = stage_fd.as_raw_fd();
            let placed = (|| {
                cvt(unsafe { libc::fchmodat(staged, target.as_ptr(), 0o600, 0) })?;
                if is_root() {
                    cvt(unsafe {
                        libc::fchownat(
                            staged,
                            target.as_ptr(),
                            self.uid,
                            self.gid,
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    })?;
                }
                cvt(unsafe {
                    libc::linkat(staged, target.as_ptr(), self.raw(), target.as_ptr(), 0)
                })
                .map_err(|e| match e.raw_os_error() {
                    Some(libc::EEXIST) => io::Error::from_raw_os_error(libc::EADDRINUSE),
                    _ => e,
                })
            })();
            unsafe { libc::unlinkat(staged, target.as_ptr(), 0) };
            placed.map(|_| listener)
        });
        unsafe { libc::unlinkat(self.raw(), stage.as_ptr(), libc::AT_REMOVEDIR) };
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pt-safefs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn creates_writes_appends_and_renames() {
        let root = scratch("basic");
        let dir = SafeDir::create(&root.join("a/b")).unwrap();
        dir.write_atomic("f", b"one", 0o600).unwrap();
        dir.write_atomic("f", b"two", 0o600).unwrap();
        assert_eq!(dir.read("f", 10).unwrap(), b"two");
        assert!(dir.read("f", 2).is_err(), "size limit");
        let mut log = dir.open_append("log", 0o600).unwrap();
        log.write_all(b"x").unwrap();
        drop(log);
        dir.open_append("log", 0o600)
            .unwrap()
            .write_all(b"y")
            .unwrap();
        assert_eq!(dir.read("log", 10).unwrap(), b"xy");
        dir.rename("log", "log.1").unwrap();
        assert!(dir.exists("log.1") && !dir.exists("log"));
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root.join("a/b")), 0o700);
        assert_eq!(mode(&root.join("a/b/f")), 0o600);
        let leftovers = std::fs::read_dir(root.join("a/b")).unwrap().count();
        assert_eq!(leftovers, 2, "no temp files left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn never_follows_a_symlinked_file() {
        let root = scratch("link");
        let dir = SafeDir::create(&root).unwrap();
        std::fs::write(root.join("secret"), b"top secret").unwrap();
        std::os::unix::fs::symlink(root.join("secret"), root.join("config.toml")).unwrap();
        assert!(dir.read("config.toml", 100).is_err());
        assert!(dir.open_append("config.toml", 0o600).is_err());
        dir.write_atomic("config.toml", b"new", 0o600).unwrap();
        assert_eq!(std::fs::read(root.join("secret")).unwrap(), b"top secret");
        assert_eq!(dir.read("config.toml", 100).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn binds_a_private_socket_in_place() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixStream;
        let root = scratch("sock");
        let dir = SafeDir::create(&root).unwrap();
        let listener = dir.bind_socket("s").unwrap();
        let meta = std::fs::symlink_metadata(root.join("s")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        UnixStream::connect(root.join("s")).unwrap();
        listener.accept().unwrap();
        let taken = dir.bind_socket("s").unwrap_err();
        assert_eq!(taken.raw_os_error(), Some(libc::EADDRINUSE));
        let entries = std::fs::read_dir(&root).unwrap().count();
        assert_eq!(entries, 1, "staging directories are removed");
        let _ = std::fs::remove_dir_all(&root);
    }
}
