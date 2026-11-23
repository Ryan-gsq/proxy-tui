use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

use crate::config::{safefs, SafeDir};

const MAX_PEM_BYTES: u64 = 1 << 20;

fn read(path: &Path) -> Result<Vec<u8>> {
    let (dir, name) = safefs::split(path)?;
    SafeDir::open(dir)
        .and_then(|d| d.read(name, MAX_PEM_BYTES))
        .with_context(|| format!("读取 {}", path.display()))
}

fn write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let (dir, name) = safefs::split(path)?;
    SafeDir::create(dir)
        .and_then(|d| d.write_atomic(name, data, mode))
        .with_context(|| format!("写入 {}", path.display()))
}

fn exists(path: &Path) -> bool {
    safefs::split(path)
        .ok()
        .and_then(|(dir, name)| SafeDir::open(dir).ok().map(|d| d.exists(name)))
        .unwrap_or(false)
}

/// Generates a self-signed certificate if none exists. Returns true when generated.
pub fn ensure_cert(cert: &Path, key: &Path, sans: Vec<String>) -> Result<bool> {
    if exists(cert) && exists(key) {
        return Ok(false);
    }
    let generated = rcgen::generate_simple_self_signed(sans).context("生成证书失败")?;
    write(key, generated.key_pair.serialize_pem().as_bytes(), 0o600)?;
    write(cert, generated.cert.pem().as_bytes(), 0o644)?;
    Ok(true)
}

pub fn load_acceptor(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    let cert_pem = read(cert)?;
    let key_pem = read(key)?;
    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .context("解析证书失败")?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .context("解析私钥失败")?
        .context("没有找到私钥")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}
