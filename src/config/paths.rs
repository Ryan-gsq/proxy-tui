//! 数据目录：配置、证书、日志、控制 socket 都在同一个目录里。

use std::path::{Path, PathBuf};

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// The real user behind `sudo` (SUDO_USER), when running as root through sudo.
pub fn sudo_user() -> Option<nix::unistd::User> {
    if !is_root() {
        return None;
    }
    let name = std::env::var("SUDO_USER")
        .ok()
        .filter(|n| !n.is_empty() && n != "root")?;
    nix::unistd::User::from_name(&name).ok().flatten()
}

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
