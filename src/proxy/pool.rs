//! 普通 HTTP 转发的上游连接池：空闲的源站连接留给后续请求复用，跨客户端连接共享。

use std::collections::HashMap;
use std::sync::Mutex;
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

use tokio::io::ReadBuf;
use tokio::net::TcpStream;

use crate::net::egress::Egress;

/// Idle connections older than this are closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PER_KEY: usize = 8;
const MAX_TOTAL: usize = 256;

/// A connection is shared only between requests for the same authority that
/// leave through the same egress, so reuse never changes the exit address.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub authority: String,
    pub egress: Egress,
}

#[derive(Default)]
pub struct Pool {
    idle: Mutex<HashMap<Key, Vec<(TcpStream, Instant)>>>,
}

impl Pool {
    /// The most recently returned connection for `key` that is still usable.
    pub fn take(&self, key: &Key) -> Option<TcpStream> {
        let mut idle = self.idle.lock().unwrap();
        let list = idle.get_mut(key)?;
        let mut found = None;
        while let Some((stream, since)) = list.pop() {
            if usable(&stream, since) {
                found = Some(stream);
                break;
            }
        }
        if list.is_empty() {
            idle.remove(key);
        }
        found
    }

    /// Keeps a connection that sits between requests for later reuse.
    pub fn put(&self, key: Key, stream: TcpStream) {
        let mut idle = self.idle.lock().unwrap();
        if idle.values().map(Vec::len).sum::<usize>() >= MAX_TOTAL {
            return;
        }
        let list = idle.entry(key).or_default();
        if list.len() >= MAX_PER_KEY {
            list.remove(0);
        }
        list.push((stream, Instant::now()));
    }

    /// Closes connections idle for too long or closed by the origin.
    pub fn sweep(&self) {
        self.idle.lock().unwrap().retain(|_, list| {
            list.retain(|(stream, since)| usable(stream, *since));
            !list.is_empty()
        });
    }

    pub fn len(&self) -> usize {
        self.idle.lock().unwrap().values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Not expired, and the origin has neither closed it nor sent anything unasked.
fn usable(stream: &TcpStream, since: Instant) -> bool {
    if since.elapsed() >= IDLE_TIMEOUT {
        return false;
    }
    let mut cx = Context::from_waker(Waker::noop());
    let mut byte = [0u8; 1];
    stream
        .poll_peek(&mut cx, &mut ReadBuf::new(&mut byte))
        .is_pending()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn key(authority: &str) -> Key {
        Key {
            authority: authority.into(),
            egress: Egress {
                iface: None,
                index: 0,
                src_v4: None,
                src_v6: None,
                order: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn reuses_only_live_connections_for_the_same_key() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pool = Pool::default();

        let live = TcpStream::connect(addr).await.unwrap();
        let (_live_peer, _) = listener.accept().await.unwrap();
        pool.put(key("a"), live);
        assert!(pool.take(&key("b")).is_none());
        assert!(pool.take(&key("a")).is_some());
        assert!(pool.take(&key("a")).is_none());

        let closed = TcpStream::connect(addr).await.unwrap();
        let (closed_peer, _) = listener.accept().await.unwrap();
        let chatty = TcpStream::connect(addr).await.unwrap();
        let (mut chatty_peer, _) = listener.accept().await.unwrap();
        pool.put(key("a"), closed);
        pool.put(key("a"), chatty);
        drop(closed_peer);
        chatty_peer.write_all(b"unasked").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(pool.take(&key("a")).is_none());
        assert!(pool.is_empty());
    }
}
