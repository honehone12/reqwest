use bytes::{Bytes, Buf};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

use crate::async_impl::body::ResponseBody;
use crate::error::{BoxError, Error, Kind};
use crate::Body;

use futures_util::future;
use h3::client::SendRequest;
use h3_quinn::{Connection, OpenStreams};
use http::uri::{Authority, Scheme};
use http::{Request, Response, Uri};
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};
use log::trace;
use async_stream::try_stream;

pub(super) type Key = (Scheme, Authority);

#[derive(Clone)]
pub struct Pool {
    inner: Arc<Mutex<PoolInner>>,
}

impl Pool {
    pub fn new(timeout: Option<Duration>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                connecting: HashSet::new(),
                idle_conns: HashMap::new(),
                timeout,
            })),
        }
    }

    pub fn connecting(&self, key: Key) -> Result<(), BoxError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.connecting.insert(key.clone()) {
            return Err(format!("HTTP/3 connecting already in progress for {key:?}").into());
        }
        return Ok(());
    }

    pub fn try_pool(&self, key: &Key) -> Option<PoolClient> {
        let mut inner = self.inner.lock().unwrap();
        let timeout = inner.timeout;
        if let Some(conn) = inner.idle_conns.get(&key) {
            // We check first if the connection still valid
            // and if not, we remove it from the pool.
            if conn.is_invalid() {
                trace!("pooled HTTP/3 connection is invalid so removing it...");
                inner.idle_conns.remove(&key);
                return None;
            }

            if let Some(duration) = timeout {
                if Instant::now().saturating_duration_since(conn.idle_timeout) > duration {
                    trace!("pooled connection expired");
                    return None;
                }
            }
        }

        inner
            .idle_conns
            .get_mut(&key)
            .and_then(|conn| Some(conn.pool()))
    }

    pub fn new_connection(
        &mut self,
        key: Key,
        mut driver: h3::client::Connection<Connection, Bytes>,
        tx: SendRequest<OpenStreams, Bytes>,
    ) -> PoolClient {
        let (close_tx, close_rx) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            if let Err(e) = future::poll_fn(|cx| driver.poll_close(cx)).await {
                trace!("poll_close returned error {e:?}");
                close_tx.send(e).ok();
            }
        });

        let mut inner = self.inner.lock().unwrap();

        let client = PoolClient::new(tx);
        let conn = PoolConnection::new(client.clone(), close_rx);
        inner.insert(key.clone(), conn);

        // We clean up "connecting" here so we don't have to acquire the lock again.
        let existed = inner.connecting.remove(&key);
        debug_assert!(existed, "key not in connecting set");

        client
    }
}

struct PoolInner {
    connecting: HashSet<Key>,
    idle_conns: HashMap<Key, PoolConnection>,
    timeout: Option<Duration>,
}

impl PoolInner {
    fn insert(&mut self, key: Key, conn: PoolConnection) {
        if self.idle_conns.contains_key(&key) {
            trace!("connection already exists for key {key:?}");
        }

        self.idle_conns.insert(key, conn);
    }
}

#[derive(Clone)]
pub struct PoolClient {
    inner: SendRequest<OpenStreams, Bytes>,
}

impl PoolClient {
    pub fn new(tx: SendRequest<OpenStreams, Bytes>) -> Self {
        Self { inner: tx }
    }

    pub async fn send_request(
        &mut self,
        req: Request<Body>,
    ) -> Result<Response<ResponseBody>, BoxError> {
        use hyper::body::Body as _;

        let (head, req_body) = req.into_parts();
        let mut req = Request::from_parts(head, ());

        if let Some(n) = req_body.size_hint().exact() {
            if n > 0 {
                req.headers_mut()
                    .insert(http::header::CONTENT_LENGTH, n.into());
            }
        }

        let mut stream = self.inner.send_request(req).await?;

        match req_body.as_bytes() {
            Some(b) if !b.is_empty() => {
                stream.send_data(Bytes::copy_from_slice(b)).await?;
            }
            #[cfg(feature = "multipart")]
            // body is Streaming
            None => {
                use futures_util::StreamExt;
            
                let mut body_as_stream = req_body.into_stream();
                while let Some(frame) = body_as_stream.next().await {
                    stream.send_data(frame?).await?;
                }
            }
            _ => {}
        }

        stream.finish().await?;

        let resp = stream.recv_response().await?;

        let body_stream = try_stream! {
            while let Some(mut buf) = stream.recv_data().await? {
                let len = buf.remaining();
                yield Frame::data(buf.copy_to_bytes(len));
            }
        };
        let resp_body = StreamBody::new(body_stream).boxed();
        
        Ok(resp.map(|_| resp_body))
    }
}

pub struct PoolConnection {
    // This receives errors from polling h3 driver.
    close_rx: Receiver<h3::Error>,
    client: PoolClient,
    idle_timeout: Instant,
}

impl PoolConnection {
    pub fn new(client: PoolClient, close_rx: Receiver<h3::Error>) -> Self {
        Self {
            close_rx,
            client,
            idle_timeout: Instant::now(),
        }
    }

    pub fn pool(&mut self) -> PoolClient {
        self.idle_timeout = Instant::now();
        self.client.clone()
    }

    pub fn is_invalid(&self) -> bool {
        match self.close_rx.try_recv() {
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => true,
            Ok(_) => true,
        }
    }
}

pub(crate) fn extract_domain(uri: &mut Uri) -> Result<Key, Error> {
    let uri_clone = uri.clone();
    match (uri_clone.scheme(), uri_clone.authority()) {
        (Some(scheme), Some(auth)) => Ok((scheme.clone(), auth.clone())),
        _ => Err(Error::new(Kind::Request, None::<Error>)),
    }
}

pub(crate) fn domain_as_uri((scheme, auth): Key) -> Uri {
    http::uri::Builder::new()
        .scheme(scheme)
        .authority(auth)
        .path_and_query("/")
        .build()
        .expect("domain is valid Uri")
}
