use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use proxy_tui::api::{RunMode, Update};
use proxy_tui::config::{Config, Protocols};
use proxy_tui::daemon::{self, Daemon};
use proxy_tui::manage::{is_not_running, Applied, Manager};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio_rustls::rustls;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Env {
    daemon: Option<Daemon>,
    v4: SocketAddr,
    v6: Option<SocketAddr>,
    dir: PathBuf,
}

impl Env {
    async fn new() -> Env {
        let dir = std::env::temp_dir().join(format!(
            "pt-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::default();
        cfg.listen.port = 0;
        cfg.listen.ipv4 = "127.0.0.1".into();
        cfg.listen.ipv6 = "::1".into();
        cfg.auth.username = "user".into();
        cfg.auth.password = "pass".into();
        let config_path = dir.join("config.toml");
        cfg.save(&config_path).unwrap();
        let daemon = daemon::start(
            config_path,
            dir.join("control.sock"),
            RunMode::Oneshot,
            false,
        )
        .await
        .unwrap();
        let bound = daemon.listening().await;
        let v4 = *bound.iter().find(|a| a.is_ipv4()).unwrap();
        let v6 = bound.iter().find(|a| a.is_ipv6()).copied();
        Env {
            daemon: Some(daemon),
            v4,
            v6,
            dir,
        }
    }

    async fn stop(mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown().await;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn echo_server(addr: &str) -> SocketAddr {
    let listener = TcpListener::bind(addr).await.unwrap();
    let local = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    local
}

fn basic(user: &str, pass: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
}

async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn assert_echo<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, msg: &[u8]) {
    s.write_all(msg).await.unwrap();
    let mut back = vec![0u8; msg.len()];
    s.read_exact(&mut back).await.unwrap();
    assert_eq!(back, msg);
}

/// Reads one response framed by `Content-Length`; returns (head, body).
async fn read_response<S: AsyncRead + Unpin>(s: &mut S) -> (String, String) {
    let head = read_head(s).await;
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .map_or(0, |v| v.trim().parse().unwrap());
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.unwrap();
    (head, String::from_utf8(body).unwrap())
}

/// How a test origin treats its keep-alive connections.
#[derive(Clone, Copy, PartialEq)]
enum Keep {
    Open,
    CloseAfterResponse,
    /// Answers one request, then closes on reading the next without answering.
    DropSecondRequest,
}

/// An origin answering each request with its path as the body. Records
/// request heads and counts connections.
struct Origin {
    addr: SocketAddr,
    heads: Arc<Mutex<Vec<String>>>,
    conns: Arc<AtomicUsize>,
}

async fn origin(keep: Keep) -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let conns = Arc::new(AtomicUsize::new(0));
    let (seen, count) = (heads.clone(), conns.clone());
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            let seen = seen.clone();
            tokio::spawn(async move {
                for served in 0.. {
                    let head = read_head(&mut s).await;
                    if head.is_empty() || (keep == Keep::DropSecondRequest && served == 1) {
                        break;
                    }
                    let path = head.split(' ').nth(1).unwrap_or("").to_string();
                    seen.lock().unwrap().push(head);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{path}",
                        path.len()
                    );
                    if s.write_all(resp.as_bytes()).await.is_err()
                        || keep == Keep::CloseAfterResponse
                    {
                        break;
                    }
                }
            });
        }
    });
    Origin { addr, heads, conns }
}

fn get(origin: SocketAddr, path: &str, auth: Option<&str>) -> String {
    let auth = auth
        .map(|a| format!("Proxy-Authorization: Basic {a}\r\n"))
        .unwrap_or_default();
    format!(
        "GET http://{origin}{path} HTTP/1.1\r\nHost: {origin}\r\n{auth}Proxy-Connection: keep-alive\r\n\r\n"
    )
}

async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    target: &str,
    auth: Option<&str>,
) -> String {
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Proxy-Authorization: Basic {a}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    read_head(s).await
}

async fn socks5_handshake(s: &mut TcpStream, user: &str, pass: &str) -> bool {
    s.write_all(&[5, 1, 2]).await.unwrap();
    let mut m = [0u8; 2];
    s.read_exact(&mut m).await.unwrap();
    assert_eq!(m, [5, 2]);
    let mut auth = vec![1, user.len() as u8];
    auth.extend_from_slice(user.as_bytes());
    auth.push(pass.len() as u8);
    auth.extend_from_slice(pass.as_bytes());
    s.write_all(&auth).await.unwrap();
    let mut r = [0u8; 2];
    s.read_exact(&mut r).await.unwrap();
    r == [1, 0]
}

async fn socks5_reply(s: &mut TcpStream) -> (u8, SocketAddr) {
    let mut hdr = [0u8; 4];
    s.read_exact(&mut hdr).await.unwrap();
    let ip: std::net::IpAddr = if hdr[3] == 1 {
        let mut b = [0u8; 4];
        s.read_exact(&mut b).await.unwrap();
        b.into()
    } else {
        let mut b = [0u8; 16];
        s.read_exact(&mut b).await.unwrap();
        b.into()
    };
    let port = s.read_u16().await.unwrap();
    (hdr[1], SocketAddr::new(ip, port))
}

#[tokio::test]
async fn http_connect_with_and_without_auth() {
    let env = Env::new().await;
    let echo = echo_server("127.0.0.1:0").await;

    let mut s = TcpStream::connect(env.v4).await.unwrap();
    let head = http_connect(&mut s, &echo.to_string(), None).await;
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");

    let mut s = TcpStream::connect(env.v4).await.unwrap();
    let head = http_connect(&mut s, &echo.to_string(), Some(&basic("user", "wrong"))).await;
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");

    let mut s = TcpStream::connect(env.v4).await.unwrap();
    let head = http_connect(&mut s, &echo.to_string(), Some(&basic("user", "pass"))).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_echo(&mut s, b"hello through CONNECT").await;
    env.stop().await;
}

#[tokio::test]
async fn http_forward_leaves_no_trace_of_the_proxy() {
    let env = Env::new().await;
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_addr = origin.local_addr().unwrap();
    let seen = tokio::spawn(async move {
        let (mut s, _) = origin.accept().await.unwrap();
        let head = read_head(&mut s).await;
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
        head
    });
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    let req = format!(
        "GET http://{origin_addr}/path?q=1 HTTP/1.1\r\nHost: {origin_addr}\r\nProxy-Authorization: Basic {}\r\n\
         Proxy-Connection: keep-alive\r\nVia: 1.1 other-proxy\r\nX-Forwarded-For: 10.1.2.3\r\n\
         Forwarded: for=10.1.2.3\r\nX-Real-IP: 10.1.2.3\r\nAccept: */*\r\n\r\n",
        basic("user", "pass")
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let (head, body) = read_response(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(body, "ok");
    assert_eq!(
        seen.await.unwrap(),
        format!(
            "GET /path?q=1 HTTP/1.1\r\nHost: {origin_addr}\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n"
        )
    );
    env.stop().await;
}

#[tokio::test]
async fn http_keep_alive_reuses_the_origin_connection() {
    let env = Env::new().await;
    let a = origin(Keep::Open).await;
    let b = origin(Keep::Open).await;
    let echo = echo_server("127.0.0.1:0").await;
    let auth = basic("user", "pass");
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    // Later requests on an authenticated connection may omit the credentials.
    for (origin, path, auth) in [
        (a.addr, "/1", Some(auth.as_str())),
        (a.addr, "/2", None),
        (b.addr, "/3", None),
        (a.addr, "/4", Some(auth.as_str())),
    ] {
        s.write_all(get(origin, path, auth).as_bytes())
            .await
            .unwrap();
        let (head, body) = read_response(&mut s).await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert_eq!(body, path);
    }
    // Switching sites parks the old connection in the pool instead of closing it.
    assert_eq!(a.conns.load(Ordering::SeqCst), 1);
    assert_eq!(b.conns.load(Ordering::SeqCst), 1);
    for head in a.heads.lock().unwrap().iter() {
        assert!(head.contains("\r\nConnection: keep-alive\r\n"), "{head}");
        assert!(!head.to_ascii_lowercase().contains("proxy-"), "{head}");
    }
    let head = http_connect(&mut s, &echo.to_string(), None).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_echo(&mut s, b"tunnel on a reused connection").await;

    let mut s = TcpStream::connect(env.v4).await.unwrap();
    s.write_all(get(a.addr, "/5", Some(&auth)).as_bytes())
        .await
        .unwrap();
    assert_eq!(read_response(&mut s).await.1, "/5");
    s.write_all(get(a.addr, "/6", Some(&basic("user", "wrong"))).as_bytes())
        .await
        .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).await.unwrap();
    assert!(resp.starts_with("HTTP/1.1 407"), "{resp}");
    assert_eq!(a.conns.load(Ordering::SeqCst), 1);
    env.stop().await;
}

#[tokio::test]
async fn origin_connections_are_shared_across_clients() {
    let env = Env::new().await;
    let o = origin(Keep::Open).await;
    let auth = basic("user", "pass");
    for path in ["/x", "/y", "/z"] {
        // One request per client connection, which the client asks to close.
        let mut s = TcpStream::connect(env.v4).await.unwrap();
        let req = format!(
            "GET http://{}{path} HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\nProxy-Connection: close\r\n\r\n",
            o.addr
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert!(resp.ends_with(path), "{resp}");
    }
    assert_eq!(o.conns.load(Ordering::SeqCst), 1);
    for head in o.heads.lock().unwrap().iter() {
        assert!(!head.contains("Connection"), "{head}");
    }
    env.stop().await;
}

#[tokio::test]
async fn http_keep_alive_survives_origin_closing() {
    let env = Env::new().await;
    let auth = basic("user", "pass");
    // Closed while idle: noticed before reuse. Closed on receiving the request:
    // the GET goes out again on a new connection.
    for keep in [Keep::CloseAfterResponse, Keep::DropSecondRequest] {
        let o = origin(keep).await;
        let mut s = TcpStream::connect(env.v4).await.unwrap();
        for path in ["/a", "/b", "/c"] {
            s.write_all(get(o.addr, path, Some(&auth)).as_bytes())
                .await
                .unwrap();
            let (head, body) = read_response(&mut s).await;
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
            assert_eq!(body, path);
        }
        assert_eq!(o.conns.load(Ordering::SeqCst), 3);
    }
    env.stop().await;
}

#[tokio::test]
async fn socks5_connect_ip_and_domain() {
    let env = Env::new().await;
    let echo = echo_server("127.0.0.1:0").await;

    let mut s = TcpStream::connect(env.v4).await.unwrap();
    assert!(!socks5_handshake(&mut s, "user", "bad").await);

    let mut s = TcpStream::connect(env.v4).await.unwrap();
    assert!(socks5_handshake(&mut s, "user", "pass").await);
    let mut req = vec![5, 1, 0, 1];
    req.extend_from_slice(&[127, 0, 0, 1]);
    req.extend_from_slice(&echo.port().to_be_bytes());
    s.write_all(&req).await.unwrap();
    assert_eq!(socks5_reply(&mut s).await.0, 0);
    assert_echo(&mut s, b"socks ip").await;

    let proxy = env.v6.unwrap_or(env.v4);
    let mut s = TcpStream::connect(proxy).await.unwrap();
    assert!(socks5_handshake(&mut s, "user", "pass").await);
    let mut req = vec![5, 1, 0, 3, 9];
    req.extend_from_slice(b"localhost");
    req.extend_from_slice(&echo.port().to_be_bytes());
    s.write_all(&req).await.unwrap();
    assert_eq!(socks5_reply(&mut s).await.0, 0);
    assert_echo(&mut s, b"socks domain").await;

    let mut s = TcpStream::connect(env.v4).await.unwrap();
    assert!(socks5_handshake(&mut s, "user", "pass").await);
    // Port 1 is privileged and unused; a freshly released ephemeral port could be
    // reused by parallel tests.
    let req = vec![5, 1, 0, 1, 127, 0, 0, 1, 0, 1];
    s.write_all(&req).await.unwrap();
    assert_eq!(socks5_reply(&mut s).await.0, 5);
    env.stop().await;
}

#[tokio::test]
async fn ipv6_listener_and_target() {
    let env = Env::new().await;
    let Some(v6) = env.v6 else {
        env.stop().await;
        return;
    };
    let echo = echo_server("[::1]:0").await;
    let mut s = TcpStream::connect(v6).await.unwrap();
    let head = http_connect(&mut s, &echo.to_string(), Some(&basic("user", "pass"))).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_echo(&mut s, b"over ipv6").await;
    env.stop().await;
}

#[tokio::test]
async fn socks5_udp_associate() {
    let env = Env::new().await;
    let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = echo.recv_from(&mut buf).await else {
                break;
            };
            let _ = echo.send_to(&buf[..n], from).await;
        }
    });
    let mut ctrl = TcpStream::connect(env.v4).await.unwrap();
    assert!(socks5_handshake(&mut ctrl, "user", "pass").await);
    ctrl.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0])
        .await
        .unwrap();
    let (code, relay) = socks5_reply(&mut ctrl).await;
    assert_eq!(code, 0);
    assert_eq!(relay.ip(), env.v4.ip());

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut pkt = vec![0, 0, 0, 1, 127, 0, 0, 1];
    pkt.extend_from_slice(&echo_addr.port().to_be_bytes());
    pkt.extend_from_slice(b"udp payload");
    client.send_to(&pkt, relay).await.unwrap();
    let mut buf = [0u8; 2048];
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.recv_from(&mut buf),
    )
    .await
    .expect("udp reply")
    .unwrap();
    assert_eq!(&buf[..4], &[0, 0, 0, 1]);
    assert_eq!(&buf[8..10], &echo_addr.port().to_be_bytes());
    assert_eq!(&buf[10..n], b"udp payload");
    env.stop().await;
}

#[tokio::test]
async fn https_proxy_with_self_signed_cert() {
    let env = Env::new().await;
    let echo = echo_server("127.0.0.1:0").await;
    let pem = std::fs::read(env.dir.join("cert.pem")).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(env.v4).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(name, tcp).await.unwrap();
    let head = http_connect(&mut tls, &echo.to_string(), Some(&basic("user", "pass"))).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_echo(&mut tls, b"inside https proxy").await;
    env.stop().await;
}

/// Runs blocking service-layer calls (the TUI / CLI interface) off the runtime.
async fn with_manager<T: Send + 'static>(
    dir: &std::path::Path,
    f: impl FnOnce(&mut Manager) -> T + Send + 'static,
) -> T {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || f(&mut Manager::new(dir)))
        .await
        .unwrap()
}

#[tokio::test]
async fn manager_updates_live_then_file() {
    let mut env = Env::new().await;
    let (applied, bad, status) = with_manager(&env.dir, |m| {
        let update = Update {
            password: Some("newpass".into()),
            ..Default::default()
        };
        let applied = m.update(&update).unwrap();
        let bad = m.update(&Update {
            port: Some(0),
            ..Default::default()
        });
        (applied, bad.is_err(), m.status().unwrap())
    })
    .await;
    assert_eq!(applied, Applied::Live);
    assert!(bad);
    assert_eq!(status.settings.password, "newpass");
    assert_eq!(status.mode, RunMode::Oneshot);
    assert!(
        !status.stoppable,
        "embedded daemons are stopped by their owner"
    );
    assert!(!status.listening.is_empty());

    let echo = echo_server("127.0.0.1:0").await;
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    let head = http_connect(&mut s, &echo.to_string(), Some(&basic("user", "pass"))).await;
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    let head = http_connect(&mut s, &echo.to_string(), Some(&basic("user", "newpass"))).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let saved = Config::load(&env.dir.join("config.toml")).unwrap();
    assert_eq!(saved.auth.password, "newpass");

    let stop_refused = with_manager(&env.dir, |m| m.stop().is_err()).await;
    assert!(stop_refused);

    env.daemon.take().unwrap().shutdown().await;
    let (not_running, applied, settings) = with_manager(&env.dir, |m| {
        let not_running = m.status().map_err(|e| is_not_running(&e)).err();
        let applied = m
            .update(&Update {
                port: Some(18123),
                protocols: Some(Protocols::parse_list("socks5").unwrap()),
                ..Default::default()
            })
            .unwrap();
        (not_running, applied, m.settings().unwrap().0)
    })
    .await;
    assert_eq!(not_running, Some(true));
    assert_eq!(applied, Applied::Saved);
    assert_eq!(settings.port, 18123);
    assert_eq!(settings.protocols.list(), "socks5");
    assert_eq!(settings.password, "newpass");
    env.stop().await;
}

#[tokio::test]
async fn oneshot_daemon_restarts_in_process() {
    let mut env = Env::new().await;
    let config = env.dir.join("config.toml");
    let sock = env.dir.join("control.sock");
    env.daemon.take().unwrap().shutdown().await;
    assert!(!sock.exists());

    let again = daemon::start(config, sock.clone(), RunMode::Oneshot, false)
        .await
        .unwrap();
    assert!(sock.exists());
    let addr = *again
        .listening()
        .await
        .iter()
        .find(|a| a.is_ipv4())
        .unwrap();
    let echo = echo_server("127.0.0.1:0").await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = http_connect(&mut s, &echo.to_string(), Some(&basic("user", "pass"))).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_echo(&mut s, b"after restart").await;
    again.shutdown().await;
    env.stop().await;
}

#[tokio::test]
async fn protocol_switches_are_enforced() {
    let env = Env::new().await;
    let echo = echo_server("127.0.0.1:0").await;
    let dir = env.dir.clone();
    let set = |list: &'static str| {
        let dir = dir.clone();
        async move {
            let update = Update {
                protocols: Some(Protocols::parse_list(list).unwrap()),
                ..Default::default()
            };
            let applied = with_manager(&dir, move |m| m.update(&update).unwrap()).await;
            assert_eq!(applied, Applied::Live);
        }
    };

    set("socks5").await;
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    s.write_all(
        format!(
            "CONNECT {echo} HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\n",
            basic("user", "pass")
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(
        s.read(&mut buf).await.unwrap_or(0),
        0,
        "http must be closed"
    );

    let mut s = TcpStream::connect(env.v4).await.unwrap();
    assert!(socks5_handshake(&mut s, "user", "pass").await);
    s.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
    assert_eq!(socks5_reply(&mut s).await.0, 0x07, "udp must be refused");

    set("http,udp,socks5").await;
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    let head = http_connect(&mut s, &echo.to_string(), Some(&basic("user", "pass"))).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");

    set("https").await;
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    s.write_all(&[5, 1, 2]).await.unwrap();
    assert_eq!(
        s.read(&mut buf).await.unwrap_or(0),
        0,
        "socks5 must be closed"
    );
    env.stop().await;
}

/// An origin that answers the first request, then records everything else the
/// proxy sends it for a moment before closing.
async fn recording_origin(reply: &'static [u8]) -> (SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut seen = read_head(&mut s).await.into_bytes();
        s.write_all(reply).await.unwrap();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(300);
        while let Ok(Ok(n)) = tokio::time::timeout_at(deadline, s.read(&mut buf)).await {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
        }
        seen
    });
    (addr, task)
}

#[tokio::test]
async fn pipelined_requests_reach_only_their_own_origin() {
    let env = Env::new().await;
    let (first, first_seen) =
        recording_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
    let (second, second_seen) =
        recording_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nok2").await;
    let auth = basic("user", "pass");
    let two = format!(
        "GET http://{first}/first HTTP/1.1\r\nHost: evil\r\nProxy-Authorization: Basic {auth}\r\n\r\n\
         GET http://{second}/second HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\nProxy-Connection: close\r\n\r\n"
    );
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    s.write_all(two.as_bytes()).await.unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).await.unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(
        resp.ends_with("\r\n\r\nokHTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nok2"),
        "{resp}"
    );
    let seen = String::from_utf8(first_seen.await.unwrap()).unwrap();
    assert_eq!(
        seen,
        format!("GET /first HTTP/1.1\r\nHost: {first}\r\n\r\n")
    );
    let seen = String::from_utf8(second_seen.await.unwrap()).unwrap();
    assert!(!seen.contains("evil"), "{seen}");
    assert_eq!(
        seen,
        format!("GET /second HTTP/1.1\r\nHost: {second}\r\n\r\n")
    );
    env.stop().await;
}

#[tokio::test]
async fn websocket_upgrade_becomes_a_tunnel() {
    let env = Env::new().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let head = read_head(&mut s).await;
        assert!(head.contains("Upgrade: websocket"), "{head}");
        s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let (mut r, mut w) = s.split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
    });
    let mut s = TcpStream::connect(env.v4).await.unwrap();
    let req = format!(
        "GET http://{origin}/ws HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nProxy-Authorization: Basic {}\r\n\r\n",
        basic("user", "pass")
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let head = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert_echo(&mut s, b"frames after upgrade").await;
    env.stop().await;
}
