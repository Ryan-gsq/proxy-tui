use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use base64::Engine;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf,
    WriteHalf,
};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use super::pool::{Key, Pool};
use super::Session;
use crate::config::AuthConfig;
use crate::daemon::stats::Registered;
use crate::daemon::Ctx;
use crate::net::egress::TargetAddr;
use crate::proxy::relay::{Counted, Io};

const MAX_HEAD: usize = 64 * 1024;
const MAX_CHUNK_LINE: u64 = 4096;
const MAX_TRAILERS: usize = 64;
/// How long an idle keep-alive client connection may wait for its next request.
const KEEP_ALIVE_IDLE: Duration = Duration::from_secs(60);
const RESP_407: &[u8] = b"HTTP/1.1 407 Proxy Authentication Required\r\n\
Proxy-Authenticate: Basic realm=\"proxy-tui\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_CONNECTED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

struct Request {
    method: String,
    path: String,
    version: u8,
    headers: Vec<(String, Vec<u8>)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    fn tokens(&self, name: &str) -> Vec<String> {
        tokens(
            self.headers.iter().map(|(n, v)| (n.as_str(), v.as_slice())),
            name,
        )
    }

    fn is_connect(&self) -> bool {
        self.method.eq_ignore_ascii_case("CONNECT")
    }
}

/// The client connection, split so a request body can stream up while the
/// response streams down. Bytes read past one request stay buffered for the next.
struct Client<S> {
    rd: BufReader<ReadHalf<S>>,
    wr: WriteHalf<S>,
}

/// An origin connection, kept between requests to the same authority and
/// returned to the shared pool when this client is done with it.
struct Upstream {
    key: Key,
    /// It carried a request before, so a failure may just mean the origin
    /// closed it while idle.
    reused: bool,
    rd: BufReader<Counted<OwnedReadHalf>>,
    wr: Counted<OwnedWriteHalf>,
    _reg: Registered,
}

impl Upstream {
    /// An idle connection from the pool when `pooled` and one is there,
    /// otherwise a new one.
    async fn open(
        sess: &Session,
        ctx: &Ctx,
        target: &TargetAddr,
        key: Key,
        pooled: bool,
    ) -> io::Result<Upstream> {
        let idle = if pooled { ctx.pool.take(&key) } else { None };
        let reused = idle.is_some();
        let remote = match idle {
            Some(stream) => stream,
            None => sess.open_upstream(ctx, target).await?,
        };
        let reg = sess.register(ctx, proto(sess), target, &remote);
        let (rd, wr) = remote.into_split();
        Ok(Upstream {
            key,
            reused,
            rd: BufReader::with_capacity(
                sess.cfg.performance.buffer_size,
                Counted::upstream(rd, reg.conn.clone(), reg.iface.clone()),
            ),
            wr: Counted::upstream(wr, reg.conn.clone(), reg.iface.clone()),
            _reg: reg,
        })
    }

    /// An idle connection can be reused only while the origin has neither
    /// closed it nor sent anything unasked.
    async fn reusable(&mut self) -> bool {
        std::future::poll_fn(|cx| {
            Poll::Ready(Pin::new(&mut self.rd).poll_fill_buf(cx).is_pending())
        })
        .await
    }

    /// Hands the connection to the pool. Only for a connection between
    /// requests: the last response was relayed completely.
    fn release(self, pool: &Pool) {
        if !self.rd.buffer().is_empty() {
            return;
        }
        let rd = self.rd.into_inner().into_inner();
        if let Ok(stream) = rd.reunite(self.wr.into_inner()) {
            pool.put(self.key, stream);
        }
    }
}

fn proto(sess: &Session) -> &'static str {
    if sess.tls {
        "https"
    } else {
        "http"
    }
}

pub async fn handle<S: Io>(s: S, sess: Session, ctx: Arc<Ctx>) -> io::Result<()> {
    let (rd, wr) = tokio::io::split(s);
    let client = Client {
        rd: BufReader::with_capacity(sess.cfg.performance.buffer_size, rd),
        wr,
    };
    serve(client, &sess, &ctx).await
}

/// Serves requests one at a time until either side closes. Each plain request
/// reaches only the origin its own URI names.
async fn serve<S: Io>(mut client: Client<S>, sess: &Session, ctx: &Ctx) -> io::Result<()> {
    let mut upstream: Option<Upstream> = None;
    let mut first = true;
    loop {
        let wait = if first {
            sess.handshake_timeout()
        } else {
            KEEP_ALIVE_IDLE
        };
        let head = match tokio::time::timeout(wait, read_head(&mut client.rd)).await {
            Ok(Ok(Some(head))) => head,
            Ok(Err(e)) if first => return Err(e),
            _ => break,
        };
        let Some(req) = parse_request(&head) else {
            respond(&mut client.wr, "400 Bad Request", "malformed request").await?;
            break;
        };
        if !admitted(sess, &req, first) {
            client.wr.write_all(RESP_407).await?;
            break;
        }
        first = false;
        if req.is_connect() {
            if let Some(up) = upstream {
                up.release(&ctx.pool);
            }
            return connect(client, &req, sess, ctx).await;
        }
        if !exchange(&mut client, &req, &mut upstream, sess, ctx).await? {
            break;
        }
    }
    // Errors return early above: only a connection between requests gets here.
    if let Some(up) = upstream {
        up.release(&ctx.pool);
    }
    let _ = client.wr.shutdown().await;
    Ok(())
}

/// The first request on a connection must carry valid credentials; later ones
/// may repeat them, but not present wrong ones.
fn admitted(sess: &Session, req: &Request, first: bool) -> bool {
    let presented = req.header("proxy-authorization").is_some();
    if (first || presented) && !authorized(&sess.cfg.auth, req) {
        if presented {
            tracing::warn!("auth failed (http) from {}", sess.client);
        }
        return false;
    }
    true
}

/// CONNECT: once the target is reached the connection becomes a raw tunnel,
/// carrying along whatever the client already sent past the request.
async fn connect<S: Io>(
    client: Client<S>,
    req: &Request,
    sess: &Session,
    ctx: &Ctx,
) -> io::Result<()> {
    let Client { rd, mut wr } = client;
    let Some(target) = TargetAddr::parse_authority(&req.path, 443) else {
        return respond(&mut wr, "400 Bad Request", "invalid CONNECT target").await;
    };
    let mut remote = match sess.open_upstream(ctx, &target).await {
        Ok(r) => r,
        Err(e) => return respond_err(&mut wr, &e).await,
    };
    wr.write_all(RESP_CONNECTED).await?;
    wr.flush().await?;
    let reg = sess.register(ctx, proto(sess), &target, &remote);
    let early = rd.buffer().to_vec();
    if !early.is_empty() {
        remote.write_all(&early).await?;
        reg.conn.add_up(early.len() as u64);
        reg.iface.traffic.add_up(early.len() as u64);
    }
    sess.splice(rd.into_inner().unsplit(wr), remote, reg).await;
    Ok(())
}

/// Forwards one plain request and its response. Returns whether the client
/// connection stays open for another request.
async fn exchange<S: Io>(
    client: &mut Client<S>,
    req: &Request,
    upstream: &mut Option<Upstream>,
    sess: &Session,
    ctx: &Ctx,
) -> io::Result<bool> {
    let Some((authority, path)) = split_absolute(&req.path) else {
        respond(
            &mut client.wr,
            "400 Bad Request",
            "absolute http:// URI required",
        )
        .await?;
        return Ok(false);
    };
    let Some(target) = TargetAddr::parse_authority(&authority, 80) else {
        respond(&mut client.wr, "400 Bad Request", "invalid host").await?;
        return Ok(false);
    };
    let body = match body_kind(req) {
        Ok(body) => body,
        Err(why) => {
            respond(&mut client.wr, "400 Bad Request", why).await?;
            return Ok(false);
        }
    };
    let upgrade = wants_upgrade(req);
    let head = rewrite(req, &authority, &path, upgrade);
    let key = Key {
        authority: authority.to_ascii_lowercase(),
        egress: sess.egress.clone(),
    };
    // A request that never got an answer may go out once more, on a new
    // connection, when a reused one turns out to be closed (RFC 9112 §9.3.1).
    let mut retry = body == Body::None && matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS");
    let mut pooled = true;
    loop {
        let current = match upstream.as_mut() {
            Some(up) if up.key == key => up.reusable().await,
            _ => false,
        };
        if !current {
            if let Some(old) = upstream.take() {
                if old.key != key {
                    old.release(&ctx.pool);
                }
            }
            match Upstream::open(sess, ctx, &target, key.clone(), pooled).await {
                Ok(up) => *upstream = Some(up),
                Err(e) => {
                    respond_err(&mut client.wr, &e).await?;
                    return Ok(false);
                }
            }
        }
        let Some(up) = upstream.as_mut() else {
            return Ok(false);
        };
        match transfer(client, up, &head, body, req.method == "HEAD", upgrade).await? {
            Done::Reusable => {
                up.reused = true;
                return Ok(keep_alive(req));
            }
            Done::Close => {}
            Done::Tunnel => tunnel(client, up).await,
            Done::NoResponse(e) => {
                let stale = up.reused;
                *upstream = None;
                if stale && retry {
                    retry = false;
                    pooled = false;
                    continue;
                }
                respond_err(&mut client.wr, &e).await?;
            }
        }
        *upstream = None;
        return Ok(false);
    }
}

/// What one request/response exchange left behind.
#[derive(Debug)]
enum Done {
    /// Both connections are at a message boundary and may carry another request.
    Reusable,
    /// The response was relayed, but the connection cannot carry another one.
    Close,
    /// The origin switched protocols: raw bytes both ways from now on.
    Tunnel,
    /// Nothing reached the client: the origin failed before responding.
    NoResponse(io::Error),
}

/// Sends the request head and exactly its body upstream while relaying the
/// response down, so `Expect: 100-continue` and early responses work.
async fn transfer<S: Io>(
    client: &mut Client<S>,
    up: &mut Upstream,
    head: &[u8],
    body: Body,
    head_request: bool,
    upgrade: bool,
) -> io::Result<Done> {
    let Client {
        rd: client_rd,
        wr: client_wr,
    } = client;
    let Upstream {
        rd: up_rd,
        wr: up_wr,
        ..
    } = up;
    let upload = async {
        up_wr.write_all(head).await?;
        copy_body(client_rd, up_wr, body).await?;
        up_wr.flush().await
    };
    let download = async {
        let mut sent = false;
        match relay_response(up_rd, client_wr, head_request, upgrade, &mut sent).await {
            Err(e) if !sent => Ok(Done::NoResponse(e)),
            done => done,
        }
    };
    tokio::pin!(upload, download);
    let mut uploaded = false;
    let done = loop {
        tokio::select! {
            biased;
            res = &mut upload, if !uploaded => match res {
                Ok(()) => uploaded = true,
                // Without a body only the origin side can have failed.
                Err(e) if body == Body::None => break Done::NoResponse(e),
                Err(e) => return Err(e),
            },
            res = &mut download => break res?,
        }
    };
    Ok(match done {
        // The origin answered before the request body was through: the rest of
        // it is still on its way, so neither side is at a request boundary.
        Done::Reusable if !uploaded => Done::Close,
        Done::Tunnel if !uploaded => {
            upload.await?;
            Done::Tunnel
        }
        done => done,
    })
}

/// Relays interim responses, then the final response with exactly its body.
/// `sent` turns true once anything has been written to the client.
async fn relay_response<R, W>(
    from: &mut R,
    to: &mut W,
    head_request: bool,
    upgrade: bool,
    sent: &mut bool,
) -> io::Result<Done>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let head = read_head(from).await?.ok_or(io::ErrorKind::UnexpectedEof)?;
        let resp = parse_response(&head).ok_or_else(|| invalid("malformed response"))?;
        *sent = true;
        to.write_all(&head).await?;
        match resp.status {
            101 => {
                to.flush().await?;
                return Ok(if upgrade { Done::Tunnel } else { Done::Close });
            }
            100..=199 => {
                to.flush().await?;
                continue;
            }
            _ => {}
        }
        let body = if head_request || matches!(resp.status, 204 | 304) {
            Body::None
        } else {
            resp.body
        };
        copy_body(from, to, body).await?;
        to.flush().await?;
        return Ok(if resp.persistent && body != Body::UntilClose {
            Done::Reusable
        } else {
            Done::Close
        });
    }
}

/// Relays raw bytes both ways, starting with whatever either side already
/// buffered, until both directions end or either fails.
async fn tunnel<S: Io>(client: &mut Client<S>, up: &mut Upstream) {
    let Client {
        rd: client_rd,
        wr: client_wr,
    } = client;
    let Upstream {
        rd: up_rd,
        wr: up_wr,
        ..
    } = up;
    let upload = async {
        tokio::io::copy_buf(client_rd, up_wr).await?;
        up_wr.shutdown().await
    };
    let download = async {
        tokio::io::copy_buf(up_rd, client_wr).await?;
        client_wr.shutdown().await
    };
    let _ = tokio::try_join!(upload, download);
}

/// How a message body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    None,
    Length(u64),
    Chunked,
    /// Responses only: the body runs until the origin closes.
    UntilClose,
}

/// Comma-separated values of every `name` header, trimmed and lowercased.
fn tokens<'a>(headers: impl IntoIterator<Item = (&'a str, &'a [u8])>, name: &str) -> Vec<String> {
    headers
        .into_iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .flat_map(|(_, v)| {
            String::from_utf8_lossy(v)
                .split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Ambiguous framing is refused: it is how request smuggling works.
fn body_kind(req: &Request) -> Result<Body, &'static str> {
    let te = req.tokens("transfer-encoding");
    let cl = req.tokens("content-length");
    if !te.is_empty() {
        if !cl.is_empty() {
            return Err("Content-Length 与 Transfer-Encoding 不能同时出现");
        }
        return match te.last().map(String::as_str) {
            Some("chunked") => Ok(Body::Chunked),
            _ => Err("不支持的 Transfer-Encoding"),
        };
    }
    let Some(first) = cl.first() else {
        return Ok(Body::None);
    };
    if cl.iter().any(|v| v != first) {
        return Err("Content-Length 不一致");
    }
    first
        .parse::<u64>()
        .map(Body::Length)
        .map_err(|_| "Content-Length 无效")
}

/// What the proxy needs to know about a response head.
struct Response {
    status: u16,
    /// Framing by the headers alone; HEAD, 204 and 304 have no body regardless.
    body: Body,
    persistent: bool,
}

fn parse_response(head: &[u8]) -> Option<Response> {
    let mut headers = [httparse::EMPTY_HEADER; 256];
    let mut resp = httparse::Response::new(&mut headers);
    if !matches!(resp.parse(head), Ok(httparse::Status::Complete(_))) {
        return None;
    }
    let all = || resp.headers.iter().map(|h| (h.name, h.value));
    let te = tokens(all(), "transfer-encoding");
    let cl = tokens(all(), "content-length");
    let connection = tokens(all(), "connection");
    let body = match te.last() {
        Some(last) if last == "chunked" => Body::Chunked,
        Some(_) => Body::UntilClose,
        None => match cl.first() {
            None => Body::UntilClose,
            Some(first) if cl.iter().all(|v| v == first) => Body::Length(first.parse().ok()?),
            Some(_) => return None,
        },
    };
    let persistent = if resp.version? == 0 {
        connection.iter().any(|t| t == "keep-alive")
    } else {
        !connection.iter().any(|t| t == "close")
    };
    Some(Response {
        status: resp.code?,
        body,
        // With both framings present the length cannot be trusted afterwards.
        persistent: persistent && (te.is_empty() || cl.is_empty()),
    })
}

/// Whether the client means to send another request on this connection.
/// Clients talking to a proxy say so in `Proxy-Connection`.
fn keep_alive(req: &Request) -> bool {
    let options = [req.tokens("connection"), req.tokens("proxy-connection")].concat();
    if options.iter().any(|t| t == "close") {
        return false;
    }
    req.version >= 1 || options.iter().any(|t| t == "keep-alive")
}

fn wants_upgrade(req: &Request) -> bool {
    req.tokens("connection").iter().any(|t| t == "upgrade") && req.header("upgrade").is_some()
}

/// Copies exactly one message body, nothing after it.
async fn copy_body<R, W>(from: &mut R, to: &mut W, body: Body) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match body {
        Body::None => Ok(()),
        Body::Length(n) => {
            let copied = tokio::io::copy_buf(&mut from.take(n), to).await?;
            if copied < n {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            Ok(())
        }
        Body::Chunked => copy_chunked(from, to).await,
        Body::UntilClose => tokio::io::copy_buf(from, to).await.map(drop),
    }
}

async fn copy_chunked<R, W>(from: &mut R, to: &mut W) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line = Vec::new();
    loop {
        read_line(from, &mut line).await?;
        let text = String::from_utf8_lossy(&line);
        let size_text = text.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_text, 16).map_err(|_| invalid("chunk size"))?;
        to.write_all(&line).await?;
        if size == 0 {
            break;
        }
        let with_crlf = size.checked_add(2).ok_or_else(|| invalid("chunk size"))?;
        let copied = tokio::io::copy_buf(&mut (&mut *from).take(with_crlf), to).await?;
        if copied < with_crlf {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    // Trailer section, ended by an empty line.
    for _ in 0..MAX_TRAILERS {
        read_line(from, &mut line).await?;
        to.write_all(&line).await?;
        if line == b"\r\n" || line == b"\n" {
            return Ok(());
        }
    }
    Err(invalid("too many trailers"))
}

async fn read_line<R: AsyncBufRead + Unpin>(from: &mut R, line: &mut Vec<u8>) -> io::Result<()> {
    line.clear();
    (&mut *from)
        .take(MAX_CHUNK_LINE)
        .read_until(b'\n', line)
        .await?;
    if line.last() != Some(&b'\n') {
        return Err(invalid("chunk line"));
    }
    Ok(())
}

/// Reads one message head through its blank line, leaving what follows
/// buffered. `None` when the peer closes before sending anything.
async fn read_head<R: AsyncBufRead + Unpin>(from: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    loop {
        let buf = from.fill_buf().await?;
        if buf.is_empty() {
            if head.is_empty() {
                return Ok(None);
            }
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let read = buf.len();
        let before = head.len();
        let start = before.saturating_sub(3);
        head.extend_from_slice(buf);
        if let Some(pos) = head[start..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = start + pos + 4;
            head.truncate(end);
            from.consume(end - before);
            return Ok(Some(head));
        }
        from.consume(read);
        if head.len() > MAX_HEAD {
            return Err(invalid("message head too large"));
        }
    }
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

fn parse_request(head: &[u8]) -> Option<Request> {
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return None,
    }
    Some(Request {
        method: req.method?.to_string(),
        path: req.path?.to_string(),
        version: req.version.unwrap_or(1),
        headers: req
            .headers
            .iter()
            .map(|h| (h.name.to_string(), h.value.to_vec()))
            .collect(),
    })
}

fn authorized(auth: &AuthConfig, req: &Request) -> bool {
    if !auth.enabled() {
        return true;
    }
    let Some(value) = req.header("proxy-authorization") else {
        return false;
    };
    let Ok(value) = std::str::from_utf8(value) else {
        return false;
    };
    let value = value.trim();
    let Some((scheme, encoded)) = value.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return false;
    }
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Some(colon) = decoded.iter().position(|b| *b == b':') else {
        return false;
    };
    auth.verify(&decoded[..colon], &decoded[colon + 1..])
}

/// Splits `http://host[:port]/path?q` into (authority, origin-form path).
fn split_absolute(uri: &str) -> Option<(String, String)> {
    let lower = uri.get(..7)?.to_ascii_lowercase();
    if lower != "http://" {
        return None;
    }
    let rest = &uri[7..];
    let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..split];
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    let path = &rest[split..];
    let path = match path.chars().next() {
        None | Some('#') => "/".to_string(),
        Some('?') => format!("/{path}"),
        _ => path.to_string(),
    };
    Some((authority.to_string(), path))
}

/// The connection options the client would send the origin directly: those
/// of its `Connection` header, or else of the `Proxy-Connection` it sent us
/// instead, without any header names. An HTTP/1.1 `close` is meant for the
/// client's connection only: the origin connection stays open for reuse.
fn connection_options(req: &Request, upgrade: bool) -> Option<String> {
    let source = if req.header("connection").is_some() {
        "connection"
    } else {
        "proxy-connection"
    };
    let kept: Vec<String> = req
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(source))
        .flat_map(|(_, v)| {
            String::from_utf8_lossy(v)
                .split(',')
                .map(|t| t.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|t| {
            t.eq_ignore_ascii_case("keep-alive")
                || (req.version == 0 && t.eq_ignore_ascii_case("close"))
                || (upgrade && t.eq_ignore_ascii_case("upgrade"))
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join(", "))
}

/// Headers for this hop only. `Upgrade` passes on an upgrade, and `Connection`
/// cannot strip the message framing or `Host`.
fn hop_by_hop(name: &str, listed: &[String], upgrade: bool) -> bool {
    match name {
        "keep-alive" | "te" => true,
        "upgrade" => !upgrade,
        "content-length" | "transfer-encoding" | "host" => false,
        other => listed.iter().any(|t| t == other),
    }
}

/// Headers that would tell the origin a proxy is involved or who the client
/// is. A high-anonymity proxy forwards none of them, whoever added them.
fn reveals_proxy(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "via",
        "forwarded",
        "forwarded-for",
        "forwarded-for-ip",
        "client-ip",
        "true-client-ip",
        "x-real-ip",
        "x-client-ip",
        "x-cluster-client-ip",
        "x-originating-ip",
        "x-remote-ip",
        "x-remote-addr",
        "x-bluecoat-via",
        "x-coming-from",
        "coming-from",
    ];
    name.starts_with("proxy-")
        || name.starts_with("x-forwarded")
        || name.starts_with("x-proxy")
        || NAMES.contains(&name)
}

fn push_header(out: &mut Vec<u8>, name: &str, value: &[u8]) {
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
}

/// The origin-form request as the client would have sent it without a proxy:
/// `Host` from the request URI (RFC 9112 §3.2.2) in place of the client's,
/// `Proxy-Connection` turned back into `Connection`, hop-by-hop headers and
/// anything revealing a proxy dropped, the rest in the client's order and
/// spelling. Nothing is added.
fn rewrite(req: &Request, authority: &str, path: &str, upgrade: bool) -> Vec<u8> {
    let listed = [req.tokens("connection"), req.tokens("proxy-connection")].concat();
    let mut options = Some(connection_options(req, upgrade));
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(format!("{} {} HTTP/1.{}\r\n", req.method, path, req.version).as_bytes());
    let mut host_pending = true;
    if req.header("host").is_none() {
        push_header(&mut out, "Host", authority.as_bytes());
        host_pending = false;
    }
    for (name, value) in &req.headers {
        let lname = name.to_ascii_lowercase();
        match lname.as_str() {
            "host" => {
                if host_pending {
                    push_header(&mut out, name, authority.as_bytes());
                    host_pending = false;
                }
            }
            "connection" | "proxy-connection" => {
                if let Some(Some(value)) = options.take() {
                    let name = if lname == "connection" {
                        name
                    } else {
                        "Connection"
                    };
                    push_header(&mut out, name, value.as_bytes());
                }
            }
            n if hop_by_hop(n, &listed, upgrade) || reveals_proxy(n) => {}
            _ => push_header(&mut out, name, value),
        }
    }
    out.extend_from_slice(b"\r\n");
    out
}

async fn respond<W: AsyncWrite + Unpin>(w: &mut W, status: &str, body: &str) -> io::Result<()> {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    w.write_all(msg.as_bytes()).await?;
    w.flush().await
}

async fn respond_err<W: AsyncWrite + Unpin>(w: &mut W, err: &io::Error) -> io::Result<()> {
    let status = if err.kind() == io::ErrorKind::TimedOut {
        "504 Gateway Timeout"
    } else {
        "502 Bad Gateway"
    };
    respond(w, status, &err.to_string()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(head: &str) -> String {
        let req = parse_request(head.as_bytes()).unwrap();
        let (authority, path) = split_absolute(&req.path).unwrap();
        String::from_utf8(rewrite(&req, &authority, &path, wants_upgrade(&req))).unwrap()
    }

    #[test]
    fn split_absolute_uris() {
        assert_eq!(
            split_absolute("http://example.com:8080/a/b?c=1"),
            Some(("example.com:8080".into(), "/a/b?c=1".into()))
        );
        assert_eq!(
            split_absolute("HTTP://user:pw@[::1]?x"),
            Some(("[::1]".into(), "/?x".into()))
        );
        assert_eq!(split_absolute("http://h"), Some(("h".into(), "/".into())));
        assert_eq!(split_absolute("https://h/"), None);
        assert_eq!(split_absolute("/relative"), None);
    }

    #[test]
    fn rewrite_leaves_no_trace_of_the_proxy() {
        assert_eq!(
            rewritten(
                "GET http://h:8080/x HTTP/1.1\r\nHost: evil.example\r\nProxy-Connection: Keep-Alive\r\n\
                 Proxy-Authorization: Basic eA==\r\nVia: 1.1 squid\r\nX-Forwarded-For: 10.0.0.1\r\n\
                 X-Forwarded-Proto: http\r\nForwarded: for=10.0.0.1\r\nX-Real-IP: 10.0.0.1\r\n\
                 Client-IP: 10.0.0.1\r\nKeep-Alive: timeout=5\r\nuser-agent: ua\r\nAccept: */*\r\n\r\n"
            ),
            "GET /x HTTP/1.1\r\nHost: h:8080\r\nConnection: Keep-Alive\r\nuser-agent: ua\r\nAccept: */*\r\n\r\n"
        );
        // Host goes first when missing; `Connection` keeps only connection
        // options (`close` only for HTTP/1.0) and cannot remove the framing.
        assert_eq!(
            rewritten(
                "POST http://h/x HTTP/1.1\r\nConnection: keep-alive, close, X-Hop, Content-Length\r\n\
                 Proxy-Connection: keep-alive\r\nX-Hop: 1\r\nContent-Length: 3\r\n\r\n"
            ),
            "POST /x HTTP/1.1\r\nHost: h\r\nConnection: keep-alive\r\nContent-Length: 3\r\n\r\n"
        );
        assert_eq!(
            rewritten("GET http://h/ HTTP/1.1\r\nProxy-Connection: close\r\n\r\n"),
            "GET / HTTP/1.1\r\nHost: h\r\n\r\n"
        );
        assert_eq!(
            rewritten("GET http://h/ HTTP/1.0\r\nProxy-Connection: close\r\n\r\n"),
            "GET / HTTP/1.0\r\nHost: h\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(
            rewritten("GET http://h/ HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\n\r\n"),
            "GET / HTTP/1.1\r\nHost: h\r\n\r\n"
        );
        assert_eq!(
            rewritten(
                "GET http://h/ws HTTP/1.1\r\nHost: h\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\n\r\n"
            ),
            "GET /ws HTTP/1.1\r\nHost: h\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\n\r\n"
        );
    }

    #[test]
    fn client_keep_alive() {
        let parse = |head: &str| parse_request(head.as_bytes()).unwrap();
        assert!(keep_alive(&parse("GET http://h/ HTTP/1.1\r\n\r\n")));
        assert!(!keep_alive(&parse(
            "GET http://h/ HTTP/1.1\r\nProxy-Connection: close\r\n\r\n"
        )));
        assert!(!keep_alive(&parse("GET http://h/ HTTP/1.0\r\n\r\n")));
        assert!(keep_alive(&parse(
            "GET http://h/ HTTP/1.0\r\nProxy-Connection: Keep-Alive\r\n\r\n"
        )));
    }

    #[test]
    fn response_framing() {
        let parse = |head: &str| parse_response(head.as_bytes()).unwrap();
        let r = parse("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n");
        assert_eq!(
            (r.status, r.body, r.persistent),
            (200, Body::Length(5), true)
        );
        let r = parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        assert_eq!((r.body, r.persistent), (Body::Chunked, false));
        assert_eq!(parse("HTTP/1.1 200 OK\r\n\r\n").body, Body::UntilClose);
        assert_eq!(
            parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n").body,
            Body::UntilClose
        );
        assert!(!parse("HTTP/1.0 200 OK\r\nContent-Length: 1\r\n\r\n").persistent);
        assert!(
            parse("HTTP/1.0 200 OK\r\nContent-Length: 1\r\nConnection: keep-alive\r\n\r\n")
                .persistent
        );
        assert!(
            !parse("HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n")
                .persistent
        );
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n"
        )
        .is_none());
    }

    #[tokio::test]
    async fn response_relayed_up_to_its_end() {
        let input = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokHTTP/1.1 200 OK\r\n\r\n";
        let mut from = &input[..];
        let mut out = Vec::new();
        let mut sent = false;
        let done = relay_response(&mut from, &mut out, false, false, &mut sent)
            .await
            .unwrap();
        assert!(matches!(done, Done::Reusable) && sent);
        assert_eq!(
            out,
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
        );
        assert_eq!(from, b"HTTP/1.1 200 OK\r\n\r\n");

        let mut from = &b"HTTP/1.1 204 No Content\r\n\r\nrest"[..];
        let done = relay_response(&mut from, &mut Vec::new(), false, false, &mut false)
            .await
            .unwrap();
        assert!(matches!(done, Done::Reusable));
        assert_eq!(from, b"rest");

        let mut from = &b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nrest"[..];
        let done = relay_response(&mut from, &mut Vec::new(), true, false, &mut false)
            .await
            .unwrap();
        assert!(matches!(done, Done::Reusable));
        assert_eq!(from, b"rest");

        let mut from = &b"HTTP/1.1 200 OK\r\n\r\nuntil close"[..];
        let mut out = Vec::new();
        let done = relay_response(&mut from, &mut out, false, false, &mut false)
            .await
            .unwrap();
        assert!(matches!(done, Done::Close));
        assert!(out.ends_with(b"until close"));

        let mut sent = false;
        assert!(
            relay_response(&mut &b""[..], &mut Vec::new(), false, false, &mut sent)
                .await
                .is_err()
        );
        assert!(!sent);
    }

    #[tokio::test]
    async fn heads_are_read_one_at_a_time() {
        let mut from = &b"GET http://a/ HTTP/1.1\r\n\r\nGET http://b/ HTTP/1.1\r\n\r\n"[..];
        let first = read_head(&mut from).await.unwrap().unwrap();
        assert_eq!(first, b"GET http://a/ HTTP/1.1\r\n\r\n");
        let second = read_head(&mut from).await.unwrap().unwrap();
        assert_eq!(second, b"GET http://b/ HTTP/1.1\r\n\r\n");
        assert!(read_head(&mut from).await.unwrap().is_none());
        assert!(read_head(&mut &b"GET / HT"[..]).await.is_err());
    }

    #[test]
    fn body_framing() {
        let parse = |head: &str| parse_request(head.as_bytes()).unwrap();
        assert_eq!(
            body_kind(&parse("GET http://h/ HTTP/1.1\r\n\r\n")),
            Ok(Body::None)
        );
        assert_eq!(
            body_kind(&parse(
                "POST http://h/ HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n"
            )),
            Ok(Body::Length(5))
        );
        assert!(body_kind(&parse(
            "POST http://h/ HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n"
        ))
        .is_err());
        assert!(body_kind(&parse(
            "POST http://h/ HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n"
        ))
        .is_err());
        assert_eq!(
            body_kind(&parse(
                "POST http://h/ HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"
            )),
            Ok(Body::Chunked)
        );
        assert!(body_kind(&parse(
            "POST http://h/ HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n"
        ))
        .is_err());
    }

    #[tokio::test]
    async fn chunked_body_stops_at_its_end() {
        let input = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-T: 1\r\n\r\nGET http://other/ HTTP/1.1\r\n\r\n";
        let mut from = &input[..];
        let mut out = Vec::new();
        copy_body(&mut from, &mut out, Body::Chunked).await.unwrap();
        assert_eq!(
            out,
            b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-T: 1\r\n\r\n".to_vec()
        );
        assert_eq!(from, b"GET http://other/ HTTP/1.1\r\n\r\n");
        let mut out = Vec::new();
        copy_body(&mut &b"helloEXTRA"[..], &mut out, Body::Length(5))
            .await
            .unwrap();
        assert_eq!(out, b"hello");
        assert!(
            copy_body(&mut &b"hel"[..], &mut Vec::new(), Body::Length(5))
                .await
                .is_err()
        );
        assert!(copy_body(
            &mut &b"ffffffffffffffff\r\n"[..],
            &mut Vec::new(),
            Body::Chunked
        )
        .await
        .is_err());
    }

    #[test]
    fn basic_auth() {
        let auth = AuthConfig {
            username: "u".into(),
            password: "p:w".into(),
        };
        let ok =
            parse_request(b"CONNECT h:443 HTTP/1.1\r\nProxy-Authorization: Basic dTpwOnc=\r\n\r\n")
                .unwrap();
        assert!(authorized(&auth, &ok));
        let bad =
            parse_request(b"CONNECT h:443 HTTP/1.1\r\nProxy-Authorization: Basic dTp4\r\n\r\n")
                .unwrap();
        assert!(!authorized(&auth, &bad));
        let none = parse_request(b"CONNECT h:443 HTTP/1.1\r\n\r\n").unwrap();
        assert!(!authorized(&auth, &none));
    }
}
