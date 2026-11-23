//! 数据目录：配置、证书、日志、控制 socket 都在同一个目录里。

use std::path::{Path, PathBuf};

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

/// Windows has no mode bits: files inherit the directory's ACL.
#[cfg(windows)]
pub fn set_mode(_path: &Path, _mode: u32) {}

/// Whether this process may manage the service: root, or on Windows an
/// elevated (Administrator) token.
#[cfg(unix)]
pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub fn is_root() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// The privilege service management needs, for messages.
pub fn admin_name() -> &'static str {
    if cfg!(windows) {
        crate::tr!("Administrator", "管理员")
    } else {
        "root"
    }
}

/// How to get that privilege, for messages.
pub fn elevate_hint() -> &'static str {
    if cfg!(windows) {
        crate::tr!(
            "run the terminal as Administrator",
            "请以管理员身份运行终端"
        )
    } else {
        crate::tr!("run with sudo", "请用 sudo 运行")
    }
}

/// The real user behind `sudo` (SUDO_USER), when running as root through sudo.
#[cfg(unix)]
pub fn sudo_user() -> Option<nix::unistd::User> {
    if !is_root() {
        return None;
    }
    let name = std::env::var("SUDO_USER")
        .ok()
        .filter(|n| !n.is_empty() && n != "root")?;
    nix::unistd::User::from_name(&name).ok().flatten()
}

#[cfg(unix)]
fn home_dir() -> PathBuf {
    if let Some(user) = sudo_user() {
        return user.dir;
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    nix::unistd::User::from_uid(nix::unistd::geteuid())
        .ok()
        .flatten()
        .map(|u| u.dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// An elevated terminal keeps the user's own profile, so this is the same
/// directory with or without elevation.
#[cfg(windows)]
fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Overrides the data directory for every command (same as `--dir`).
pub const DIR_ENV: &str = "PROXY_TUI_DIR";
pub const CONFIG_FILE: &str = "config.toml";

/// Default data directory: `~/.proxy-tui` of the (sudo-invoking) user, so
/// running with or without sudo lands in the same place.
fn default_data_dir() -> PathBuf {
    home_dir().join(".proxy-tui")
}

/// The one directory holding config, certificates, logs and the control socket,
/// shared by the TUI and the CLI and by service and one-shot mode.
/// Order: `PROXY_TUI_DIR`, the installed service's directory, `~/.proxy-tui`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DIR_ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    crate::manage::service::installed_data_dir().unwrap_or_else(default_data_dir)
}

pub fn config_file(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE)
}

pub fn control_socket_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("control.sock")
}

/// Windows has no socket files: the control channel is a named pipe whose name
/// derives from the data directory, so each directory has its own. FNV-1a keeps
/// the name stable across builds (the service and the CLI may differ).
#[cfg(windows)]
pub fn control_pipe_name(socket_path: &Path) -> String {
    let dir = socket_path.parent().unwrap_or(Path::new("."));
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dir.to_string_lossy().to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!(r"\\.\pipe\proxy-tui-{hash:016x}")
}
