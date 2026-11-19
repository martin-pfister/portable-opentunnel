//! A local fake OpenTunnel API and bridge used by the integration tests.
//!
//! It speaks the pinned protocol over loopback, so the **real SDK** performs
//! provisioning, TLS termination, and forwarding against it. Nothing here
//! contacts the hosted service.
//!
//! Endpoints:
//!
//! ```text
//! POST   /api/tunnel                    -> 201 { tunnel, token }
//! GET    /api/tunnel/:id                -> 200 TunnelInfo
//! POST   /api/tunnel/:id/certificate    -> 202 CertificateInfo
//! GET    /api/tunnel/:id/certificate    -> 200 CertificateInfo
//! DELETE /api/tunnel/:id                -> 204
//! GET    /api/tunnel/:id/connect        -> 101 upgrade to the bridge
//! ```
//!
//! Certificates are signed from the CSR the SDK actually submitted, by a CA
//! created per test, so the tunnel's own TLS termination is real TLS. The
//! visitor side is a `rustls` client whose bytes travel through the bridge data
//! frames, which exercises route lookup, target resolution, and bidirectional
//! copying.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use opentunnel::protocol::api::{
    CertificateInfo, CertificateState, CreateTunnelResponse, TunnelInfo, TunnelState,
};
use opentunnel::protocol::bridge::{self, ClientMessage, ServerMessage};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, IsCa, Issuer, KeyPair, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::{Bytes, Message};

// ---------------------------------------------------------------------------
// Test certificate authority
// ---------------------------------------------------------------------------

/// A throwaway CA that signs the CSR the SDK generates.
pub struct TestCa {
    ca_pem: String,
    ca_der: CertificateDer<'static>,
    issuer: Issuer<'static, KeyPair>,
    ca_path: std::path::PathBuf,
}

impl TestCa {
    pub fn new(directory: &std::path::Path) -> Self {
        let mut ca = Self::in_memory();
        ca.ca_path = directory.join("test-ca.pem");
        std::fs::write(&ca.ca_path, &ca.ca_pem)
            .expect("writing the test CA PEM into the scratch directory");
        ca
    }

    /// The same CA without writing a PEM file.
    ///
    /// The lifecycle harness passes `ca_path()` to the child as `--ca-cert`, so
    /// it needs the file. Fixture-only tests (and environments that forbid
    /// `*.pem` writes) can use this instead: the certificates still come from
    /// the same in-memory authority.
    pub fn in_memory() -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2035, 1, 1);
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "portable-opentunnel test CA");
        params.distinguished_name = name;
        let ca = params.self_signed(&ca_key).unwrap();
        let ca_pem = ca.pem();
        let ca_der = ca.der().clone();

        // `Issuer` borrows the params it was built from; one short-lived leak
        // per test keeps that borrow valid without unsafe code.
        let issuer_params: &'static CertificateParams = Box::leak(Box::new(params));
        let issuer = Issuer::from_params(issuer_params, ca_key);

        Self {
            ca_pem,
            ca_der,
            issuer,
            ca_path: std::path::PathBuf::new(),
        }
    }

    /// Whether this authority was given a directory to write its PEM into.
    pub fn has_file(&self) -> bool {
        !self.ca_path.as_os_str().is_empty()
    }

    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    pub fn ca_path(&self) -> &std::path::Path {
        &self.ca_path
    }

    /// Signs the submitted CSR. Returns `(leaf_pem, chain_pem, expiry)`.
    ///
    /// A malformed CSR is the test's mistake, not something to paper over, so
    /// the error is surfaced with the CSR's shape rather than as a bare panic
    /// from deep inside `rcgen`.
    pub fn sign_csr(
        &self,
        csr_pem: &str,
        hostname: &str,
    ) -> Result<(String, String, String), String> {
        let request = CertificateSigningRequestParams::from_pem(csr_pem).map_err(|error| {
            format!(
                "the SDK submitted an unparseable CSR ({} bytes, starts {:?}): {error}",
                csr_pem.len(),
                csr_pem.chars().take(27).collect::<String>()
            )
        })?;
        let mut params = request.params;
        // The hosted issuer issues for the tunnel hostname and its wildcard,
        // matching what the SDK asks for in the CSR.
        params.subject_alt_names = vec![
            SanType::DnsName(
                hostname
                    .try_into()
                    .map_err(|error| format!("invalid hostname {hostname}: {error}"))?,
            ),
            SanType::DnsName(
                format!("*.{hostname}")
                    .try_into()
                    .map_err(|error| format!("invalid wildcard for {hostname}: {error}"))?,
            ),
        ];
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2035, 1, 1);
        let leaf = params
            .signed_by(&request.public_key, &self.issuer)
            .map_err(|error| format!("signing the CSR: {error}"))?;
        Ok((
            leaf.pem(),
            self.ca_pem.clone(),
            "2035-01-01T00:00:00.000Z".to_owned(),
        ))
    }

    /// A server config for the API itself, when the fixture speaks HTTPS.
    pub fn api_server_config(&self) -> Arc<ServerConfig> {
        let key = KeyPair::generate().unwrap();
        let params =
            CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()]).unwrap();
        let leaf = params.signed_by(&key, &self.issuer).unwrap();
        server_config(vec![leaf.der().clone()], key.serialize_der())
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots.add(self.ca_der.clone()).unwrap();
        Arc::new(
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }
}

fn server_config(certificates: Vec<CertificateDer<'static>>, key: Vec<u8>) -> Arc<ServerConfig> {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certificates, PrivateKeyDer::try_from(key).unwrap())
            .unwrap();
    // The fake API speaks HTTP/1.1 only.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

// ---------------------------------------------------------------------------
// Behaviour and metrics
// ---------------------------------------------------------------------------

/// Knobs a test sets before starting the binary.
#[derive(Debug, Clone, Default)]
pub struct Behaviour {
    /// Answer certificate GETs with `failed` and this reason.
    pub certificate_failure: Option<String>,
    /// Never leave `issuing`, so attachment can never become ready.
    pub certificate_never_ready: bool,
    /// Answer this many certificate GETs with `issuing` first.
    pub issuing_polls: usize,
    /// Delay every certificate GET, to hold a refresh in flight.
    pub certificate_delay: Duration,
    /// Reject `DELETE` with this status.
    pub delete_status: Option<u16>,
    /// Reject the attach with this code and close the bridge.
    pub attach_error: Option<String>,
    /// Close the bridge right after attach, to exercise reconnection.
    ///
    /// Drops *every* attach while set. Prefer [`Self::drop_first_attach_only`]:
    /// flipping this from the test races the SDK's reconnect, because the retry
    /// can attach while the flag is still `true`.
    pub drop_after_attach: bool,
    /// Close the bridge after the **first** attach only, then behave normally.
    ///
    /// The fixture consumes this itself, so no test-side flag flip is needed and
    /// a fast reconnect cannot be dropped by accident. That makes the
    /// "first attach dropped, second attach forwards" sequence deterministic.
    pub drop_first_attach_only: bool,
    /// Hold the `POST /api/tunnel` response for this long.
    ///
    /// The registration is created and counted *before* the delay, so the
    /// server knows a tunnel while the client still has no token. That is the
    /// exact window in which an interrupted create can orphan a registration.
    pub create_delay: Duration,
    /// Hold the bridge `Attached` reply for this long, before any attach error.
    pub attach_delay: Duration,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub creates: AtomicUsize,
    pub binds: AtomicUsize,
    pub polls: AtomicUsize,
    pub deletes: AtomicUsize,
    pub attaches: AtomicUsize,
    pub opens: AtomicUsize,
    pub backend_requests: AtomicUsize,
    /// Certificate GETs currently inside the fixture's delay.
    ///
    /// Unlike `polls`, which only proves a request happened at some point, a
    /// non-zero value here proves one is **in flight right now** — the state a
    /// shutdown must survive.
    pub active_refresh: AtomicUsize,
}

struct Tunnel {
    token: String,
    hostname: String,
    csr: Option<String>,
    certificate: Option<(String, String, String)>,
    polls: usize,
    deleted: bool,
}

struct Shared {
    metrics: Arc<Metrics>,
    behaviour: Arc<Mutex<Behaviour>>,
    tunnels: Arc<Mutex<HashMap<String, Tunnel>>>,
    ca: Arc<TestCa>,
    backend: Option<SocketAddr>,
    counter: Arc<AtomicUsize>,
    /// Route labels the bridge last saw, for diagnostics.
    routes: Arc<Mutex<Vec<String>>>,
    /// The plaintext response the visitor received, once the exchange is done.
    visitor_response: Arc<AsyncMutex<Option<String>>>,
}

/// The running fixture. Dropping it stops the listener and joins the thread.
pub struct FakeApi {
    address: SocketAddr,
    pub metrics: Arc<Metrics>,
    pub behaviour: Arc<Mutex<Behaviour>>,
    shared: Arc<Shared>,
    https: bool,
    /// Set when the visitor exchange completed with a backend response.
    visitor_response: Arc<AsyncMutex<Option<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeApi {
    /// Starts the fixture over plain HTTP. `backend` is the loopback target
    /// the single route forwards to; `None` disables visitor traffic.
    pub fn start(directory: &std::path::Path, backend: Option<SocketAddr>) -> Self {
        Self::build(directory, backend, false, true)
    }

    /// Starts the fixture over HTTPS, for the CA-option test.
    pub fn start_https(directory: &std::path::Path, backend: Option<SocketAddr>) -> Self {
        Self::build(directory, backend, true, true)
    }

    /// Starts the plain-HTTP fixture without writing a CA PEM file.
    ///
    /// `FakeApi::start` writes `test-ca.pem` because the lifecycle harness hands
    /// that path to the child as `--ca-cert`. Fixture-only tests do not need the
    /// file, and some environments forbid `*.pem` writes, so they use this.
    pub fn start_in_memory(backend: Option<SocketAddr>) -> Self {
        Self::build(std::path::Path::new(""), backend, false, false)
    }

    fn build(
        directory: &std::path::Path,
        backend: Option<SocketAddr>,
        https: bool,
        ca_on_disk: bool,
    ) -> Self {
        let ca = if ca_on_disk {
            TestCa::new(directory)
        } else {
            TestCa::in_memory()
        };
        let acceptor = https.then(|| {
            let config = ca.api_server_config();
            tokio_rustls::TlsAcceptor::from(config)
        });

        // A current-thread runtime only advances inside `block_on`, so the
        // server must own a thread that drives it. Binding happens on that
        // thread, and the address comes back over a channel.
        let (address_tx, address_rx) = std::sync::mpsc::channel::<SocketAddr>();
        let metrics = Arc::new(Metrics::default());
        let visitor_response = Arc::new(AsyncMutex::new(None));
        let shared = Arc::new(Shared {
            metrics: Arc::clone(&metrics),
            behaviour: Arc::new(Mutex::new(Behaviour::default())),
            tunnels: Arc::new(Mutex::new(HashMap::new())),
            ca: Arc::new(ca),
            backend,
            counter: Arc::new(AtomicUsize::new(0)),
            routes: Arc::new(Mutex::new(Vec::new())),
            visitor_response: Arc::clone(&visitor_response),
        });

        let server_shared = Arc::clone(&shared);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("fake-api".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("creating the fixture runtime");
                runtime.block_on(async move {
                    let listener = TcpListener::bind(("127.0.0.1", 0))
                        .await
                        .expect("binding the fixture listener");
                    address_tx
                        .send(listener.local_addr().expect("listener address"))
                        .expect("reporting the fixture address");
                    serve(listener, server_shared, acceptor, server_stop).await;
                });
            })
            .expect("spawning the fixture thread");

        let address = address_rx
            .recv()
            .expect("the fixture must report its address");
        Self {
            address,
            metrics,
            behaviour: Arc::clone(&shared.behaviour),
            shared,
            https,
            visitor_response,
            stop,
            thread: Some(thread),
        }
    }

    /// The API base URL the child is pointed at.
    ///
    /// Always a loopback **literal**, never a hostname. A statically linked musl
    /// binary resolves `localhost` through a different path than glibc and can
    /// fail it outright (observed: `EACCES` where glibc returns `::1` then
    /// `127.0.0.1`), which would make every HTTPS test fail for a reason that
    /// has nothing to do with the product. The fixture's server certificate
    /// carries a `127.0.0.1` SAN, so an HTTPS URL with an IP literal validates.
    ///
    /// Target-resolution behaviour is still tested on purpose — through the
    /// route *target*, where the wrapper and SDK must handle names — rather than
    /// by making the control-plane transport depend on the resolver.
    pub fn api_url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}", self.address)
    }

    pub fn ca_path(&self) -> &std::path::Path {
        self.shared.ca.ca_path()
    }

    pub fn ca_pem(&self) -> &str {
        self.shared.ca.ca_pem()
    }

    pub fn set(&self, behaviour: Behaviour) {
        *self.behaviour.lock().unwrap() = behaviour;
    }

    pub fn change(&self, f: impl FnOnce(&mut Behaviour)) {
        f(&mut self.behaviour.lock().unwrap());
    }

    pub fn creates(&self) -> usize {
        self.metrics.creates.load(Ordering::SeqCst)
    }

    pub fn binds(&self) -> usize {
        self.metrics.binds.load(Ordering::SeqCst)
    }

    pub fn polls(&self) -> usize {
        self.metrics.polls.load(Ordering::SeqCst)
    }

    pub fn deletes(&self) -> usize {
        self.metrics.deletes.load(Ordering::SeqCst)
    }

    pub fn attaches(&self) -> usize {
        self.metrics.attaches.load(Ordering::SeqCst)
    }

    pub fn opens(&self) -> usize {
        self.metrics.opens.load(Ordering::SeqCst)
    }

    pub fn backend_requests(&self) -> usize {
        self.metrics.backend_requests.load(Ordering::SeqCst)
    }

    /// Certificate GETs currently held inside the fixture's delay.
    ///
    /// A non-zero value proves a refresh is in flight *now*, which `polls()`
    /// (a monotonic count) cannot show.
    pub fn active_refreshes(&self) -> usize {
        self.metrics.active_refresh.load(Ordering::SeqCst)
    }

    /// Registered hostname of the (single) created tunnel.
    pub fn hostname(&self) -> Option<String> {
        self.shared
            .tunnels
            .lock()
            .unwrap()
            .values()
            .next()
            .map(|tunnel| tunnel.hostname.clone())
    }

    /// True when every registration the fixture created has been deleted.
    pub fn all_deleted(&self) -> bool {
        self.shared
            .tunnels
            .lock()
            .unwrap()
            .values()
            .all(|tunnel| tunnel.deleted)
    }

    /// The plaintext HTTP response the visitor received through the tunnel.
    pub async fn visitor_response(&self) -> Option<String> {
        self.visitor_response.lock().await.clone()
    }

    /// Waits until the visitor exchange produced a response.
    pub async fn wait_visitor_response(&self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(response) = self.visitor_response().await {
                return Some(response);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }
}

impl Drop for FakeApi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn serve(
    listener: TcpListener,
    shared: Arc<Shared>,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let accepted = tokio::select! {
            result = listener.accept() => result,
            _ = tokio::time::sleep(Duration::from_millis(25)) => continue,
        };
        let Ok((stream, _)) = accepted else {
            return;
        };
        let shared = Arc::clone(&shared);
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            match acceptor {
                Some(acceptor) => {
                    if let Ok(tls) = acceptor.accept(stream).await {
                        let _ = serve_conn(tls, shared).await;
                    }
                }
                None => {
                    let _ = serve_conn(stream, shared).await;
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    body: &[u8],
    content_type: &str,
) -> io::Result<()> {
    let reason = match status {
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

async fn respond_json<S: AsyncWrite + Unpin, T: serde::Serialize>(
    stream: &mut S,
    status: u16,
    value: &T,
) -> io::Result<()> {
    let body = serde_json::to_vec(value).unwrap();
    respond(stream, status, &body, "application/json").await
}

async fn respond_error<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    tag: &str,
    message: &str,
) -> io::Result<()> {
    let body = serde_json::to_vec(&serde_json::json!({"_tag": tag, "message": message})).unwrap();
    respond(stream, status, &body, "application/json").await
}

async fn serve_conn<S>(mut stream: S, shared: Arc<Shared>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let request = read_request(&mut stream).await?;
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();

    if path.ends_with("/connect") && request.headers.contains_key("sec-websocket-key") {
        return bridge(stream, request, shared).await;
    }

    if request.method == "POST" && path == "/api/tunnel" {
        return create_tunnel(&mut stream, shared).await;
    }

    let Some(rest) = path.strip_prefix("/api/tunnel/") else {
        return respond_error(&mut stream, 404, "NotFoundError", "unknown path").await;
    };
    let (id, suffix) = match rest.split_once('/') {
        Some((id, suffix)) => (id.to_owned(), format!("/{suffix}")),
        None => (rest.to_owned(), String::new()),
    };

    if request.method == "DELETE" && suffix.is_empty() {
        let behaviour = shared.behaviour.lock().unwrap().clone();
        if let Some(status) = behaviour.delete_status {
            return respond_error(&mut stream, status, "InternalError", "delete failed").await;
        }
        // Scope the guard: it must not live across the response `.await`.
        let deleted = {
            let mut tunnels = shared.tunnels.lock().unwrap();
            match tunnels.get_mut(&id) {
                Some(tunnel) => {
                    tunnel.deleted = true;
                    true
                }
                None => false,
            }
        };
        return if deleted {
            shared.metrics.deletes.fetch_add(1, Ordering::SeqCst);
            respond(&mut stream, 204, b"", "text/plain").await
        } else {
            respond_error(&mut stream, 404, "NotFoundError", "no such tunnel").await
        };
    }

    // Every other endpoint requires the bearer token of a live tunnel.
    let token = request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    let authorized = shared
        .tunnels
        .lock()
        .unwrap()
        .get(&id)
        .is_some_and(|tunnel| !tunnel.deleted && token.as_deref() == Some(tunnel.token.as_str()));
    if !authorized {
        return respond_error(&mut stream, 401, "UnauthorizedError", "bad token").await;
    }

    match (request.method.as_str(), suffix.as_str()) {
        ("GET", "") => {
            let info = {
                let tunnels = shared.tunnels.lock().unwrap();
                let tunnel = &tunnels[&id];
                TunnelInfo {
                    id: id.clone(),
                    hostname: tunnel.hostname.clone(),
                    state: TunnelState::Online,
                    certificate_id: None,
                }
            };
            respond_json(&mut stream, 200, &info).await
        }
        ("POST", "/certificate") => {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let csr = body
                .get("csr")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned();
            shared.metrics.binds.fetch_add(1, Ordering::SeqCst);
            let hostname = {
                let mut tunnels = shared.tunnels.lock().unwrap();
                let tunnel = tunnels.get_mut(&id).unwrap();
                if tunnel.csr.as_deref() != Some(csr.as_str()) {
                    tunnel.csr = Some(csr.clone());
                    tunnel.certificate = None;
                }
                tunnel.hostname.clone()
            };
            // A bind is idempotent: binding the CSR a tunnel already has
            // answers with its current certificate.
            let cached = {
                let tunnels = shared.tunnels.lock().unwrap();
                tunnels
                    .get(&id)
                    .and_then(|tunnel| tunnel.certificate.clone())
            };
            if let Some((certificate, chain, expiry)) = cached {
                let info = CertificateInfo {
                    id: format!("cert-{id}"),
                    state: CertificateState::Ready {
                        certificate,
                        chain,
                        expiry,
                    },
                };
                return respond_json(&mut stream, 202, &info).await;
            }
            let (certificate, chain, expiry) = match shared.ca.sign_csr(&csr, &hostname) {
                Ok(values) => values,
                Err(reason) => {
                    return respond_error(&mut stream, 400, "InvalidCsrError", &reason).await;
                }
            };
            shared
                .tunnels
                .lock()
                .unwrap()
                .get_mut(&id)
                .unwrap()
                .certificate = Some((certificate.clone(), chain.clone(), expiry.clone()));
            let info = CertificateInfo {
                id: format!("cert-{id}"),
                state: CertificateState::Ready {
                    certificate,
                    chain,
                    expiry,
                },
            };
            respond_json(&mut stream, 202, &info).await
        }
        ("GET", "/certificate") => {
            let behaviour = shared.behaviour.lock().unwrap().clone();
            shared.metrics.polls.fetch_add(1, Ordering::SeqCst);
            // Count this request as in flight *while* the delay holds it, so a
            // test can observe the real state rather than infer it from a count
            // that only ever grows. The guard decrements on every exit path,
            // including an early return.
            shared.metrics.active_refresh.fetch_add(1, Ordering::SeqCst);
            let _in_flight = ActiveRefresh(Arc::clone(&shared.metrics));
            if !behaviour.certificate_delay.is_zero() {
                tokio::time::sleep(behaviour.certificate_delay).await;
            }
            if let Some(reason) = behaviour.certificate_failure {
                let info = CertificateInfo {
                    id: format!("cert-{id}"),
                    state: CertificateState::Failed { reason },
                };
                return respond_json(&mut stream, 200, &info).await;
            }
            let polls = {
                let mut tunnels = shared.tunnels.lock().unwrap();
                let tunnel = tunnels.get_mut(&id).unwrap();
                tunnel.polls += 1;
                tunnel.polls
            };
            if behaviour.certificate_never_ready || polls <= behaviour.issuing_polls {
                let info = CertificateInfo {
                    id: format!("cert-{id}"),
                    state: CertificateState::Issuing,
                };
                return respond_json(&mut stream, 200, &info).await;
            }
            // Resolve and cache the certificate without holding the guard
            // across any `.await`.
            let resolved = {
                let mut tunnels = shared.tunnels.lock().unwrap();
                let tunnel = tunnels.get_mut(&id).unwrap();
                let cached = tunnel.certificate.clone();
                let signing = if cached.is_none() {
                    let csr = tunnel.csr.clone().unwrap_or_default();
                    let hostname = tunnel.hostname.clone();
                    Some((csr, hostname))
                } else {
                    None
                };
                (cached, signing)
            };
            let values = match resolved {
                (Some(values), _) => values,
                (None, Some((csr, hostname))) => match shared.ca.sign_csr(&csr, &hostname) {
                    Ok(values) => {
                        shared
                            .tunnels
                            .lock()
                            .unwrap()
                            .get_mut(&id)
                            .unwrap()
                            .certificate = Some(values.clone());
                        values
                    }
                    Err(reason) => {
                        return respond_error(&mut stream, 400, "InvalidCsrError", &reason).await;
                    }
                },
                (None, None) => {
                    return respond_error(
                        &mut stream,
                        400,
                        "InvalidCsrError",
                        "no certificate and no CSR to issue one",
                    )
                    .await;
                }
            };
            let (certificate, chain, expiry) = values;
            let info = CertificateInfo {
                id: format!("cert-{id}"),
                state: CertificateState::Ready {
                    certificate,
                    chain,
                    expiry,
                },
            };
            respond_json(&mut stream, 200, &info).await
        }
        _ => respond_error(&mut stream, 404, "NotFoundError", "unknown path").await,
    }
}

/// Decrements `active_refresh` when the certificate request leaves, on every
/// path including an early `return` or a cancelled future.
struct ActiveRefresh(Arc<Metrics>);

impl Drop for ActiveRefresh {
    fn drop(&mut self) {
        self.0.active_refresh.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn create_tunnel<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    shared: Arc<Shared>,
) -> io::Result<()> {
    let index = shared.counter.fetch_add(1, Ordering::SeqCst) + 1;
    let id = format!("tunnel{index:04}");
    let hostname = format!("{id}.example.test");
    let token = format!("token-{id}");
    shared.metrics.creates.fetch_add(1, Ordering::SeqCst);
    shared.tunnels.lock().unwrap().insert(
        id.clone(),
        Tunnel {
            token: token.clone(),
            hostname: hostname.clone(),
            csr: None,
            certificate: None,
            polls: 0,
            deleted: false,
        },
    );
    let response = CreateTunnelResponse {
        tunnel: TunnelInfo {
            id,
            hostname,
            state: TunnelState::Offline,
            certificate_id: None,
        },
        token,
    };
    // The registration exists and is counted above; only the *response* is
    // held. A client interrupted here has a server-side tunnel whose token it
    // never received.
    let delay = shared.behaviour.lock().unwrap().create_delay;
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    respond_json(stream, 201, &response).await
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

type Bridge = WebSocketStream<Box<dyn Transport>>;

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

async fn bridge<S>(mut stream: S, request: Request, shared: Arc<Shared>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let key = request
        .headers
        .get("sec-websocket-key")
        .cloned()
        .unwrap_or_default();
    let accept = derive_accept_key(key.as_bytes());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\
         sec-websocket-accept: {accept}\r\nsec-websocket-protocol: opentunnel\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    shared.metrics.attaches.fetch_add(1, Ordering::SeqCst);

    let boxed: Box<dyn Transport> = Box::new(stream);
    let mut socket: Bridge = WebSocketStream::from_raw_socket(boxed, Role::Server, None).await;

    let routes = match read_attach(&mut socket).await? {
        Some(routes) => routes,
        None => return Ok(()),
    };
    *shared.routes.lock().unwrap() = routes.clone();
    let behaviour = {
        let mut guard = shared.behaviour.lock().unwrap();
        // Consume the one-shot drop under a single lock so exactly the first
        // attach is dropped, no matter how fast the client reconnects.
        let one_shot = std::mem::take(&mut guard.drop_first_attach_only);
        let mut behaviour = guard.clone();
        if one_shot {
            behaviour.drop_after_attach = true;
        }
        behaviour
    };

    // Hold the Attached reply, so a test can interrupt while the bridge is
    // connected but the tunnel is not yet ready. The delay sits before the
    // attach-error check so an error can still be tested without waiting.
    if !behaviour.attach_delay.is_zero() && behaviour.attach_error.is_none() {
        tokio::time::sleep(behaviour.attach_delay).await;
    }

    if let Some(code) = behaviour.attach_error {
        send(&mut socket, &ServerMessage::AttachError { code }).await?;
        let _ = socket.close(None).await;
        return Ok(());
    }

    send(
        &mut socket,
        &ServerMessage::Attached {
            session: "session-1".to_owned(),
            routes: routes.clone(),
            heartbeat_ms: 1_000,
            idle_timeout_ms: 30_000,
        },
    )
    .await?;

    if behaviour.drop_after_attach {
        let _ = socket.close(None).await;
        return Ok(());
    }

    let Some(backend) = shared.backend else {
        // No visitor traffic: stay attached until the client closes.
        drain(&mut socket).await;
        return Ok(());
    };

    // One bounded visitor exchange per attach.
    let hostname = shared
        .tunnels
        .lock()
        .unwrap()
        .values()
        .next()
        .map(|tunnel| tunnel.hostname.clone())
        .unwrap_or_else(|| "example.test".to_owned());
    let route = routes.first().cloned().unwrap_or_else(|| "@".to_owned());
    let sni = if route == "@" {
        hostname.clone()
    } else {
        format!("{route}.{hostname}")
    };
    let conn = 1u32;
    shared.metrics.opens.fetch_add(1, Ordering::SeqCst);
    send(
        &mut socket,
        &ServerMessage::Open {
            conn,
            peer: "203.0.113.9".to_owned(),
            sni: sni.clone(),
            alpn: "http/1.1".to_owned(),
        },
    )
    .await?;

    let _ = visit(&mut socket, conn, &sni, backend, Arc::clone(&shared)).await;
    // A healthy session must NOT be closed here. Closing it would make the SDK
    // legitimately reconnect, and that reconnect opens a second visitor
    // connection — so any test asserting "one session carried traffic" would be
    // racing the fixture, not observing the product. Stay attached instead, the
    // same way the no-backend path does, until the client shuts the bridge down.
    drain(&mut socket).await;
    Ok(())
}

async fn read_attach(socket: &mut Bridge) -> io::Result<Option<Vec<String>>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let message = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => return Err(io::Error::other(error)),
            Ok(None) => return Ok(None),
            Err(_) => return Ok(None),
        };
        let Message::Text(text) = message else {
            continue;
        };
        match serde_json::from_str::<ClientMessage>(&text) {
            Ok(ClientMessage::Attach { routes, .. }) => return Ok(Some(routes)),
            _ => continue,
        }
    }
}

/// Keeps reading until the bridge closes, answering pings so the session is
/// not considered idle.
async fn drain(socket: &mut Bridge) {
    loop {
        match socket.next().await {
            Some(Ok(Message::Ping(payload))) => {
                let _ = socket.send(Message::Pong(payload)).await;
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
            Some(Ok(_)) => {}
        }
    }
}

async fn send(socket: &mut Bridge, message: &ServerMessage) -> io::Result<()> {
    let text = serde_json::to_string(message).unwrap();
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(io::Error::other)
}

/// Acts as the public visitor: a `rustls` client whose bytes are carried in
/// bridge data frames. The SDK terminates TLS, resolves the route, connects to
/// the backend, and copies both ways.
///
/// Returns the plaintext response the backend produced, and records it on the
/// fixture so a test can wait for it without owning the bridge task.
async fn visit(
    socket: &mut Bridge,
    conn: u32,
    sni: &str,
    backend: SocketAddr,
    shared: Arc<Shared>,
) -> io::Result<String> {
    let config = shared.ca.client_config();
    let server_name = ServerName::try_from(sni.to_owned()).map_err(io::Error::other)?;
    let mut tls = ClientConnection::new(config, server_name).map_err(io::Error::other)?;
    let mut outbound = Vec::new();
    let mut response = Vec::new();
    let mut request_sent = false;
    let mut ended = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    while !ended && tokio::time::Instant::now() < deadline {
        // Push whatever TLS wants to send into the bridge, in frames the SDK
        // can reassemble for its own `rustls` server side.
        outbound.clear();
        while tls.wants_write() {
            tls.write_tls(&mut outbound).map_err(io::Error::other)?;
        }
        if !outbound.is_empty() {
            let frame = bridge::encode_data_frame(conn, &outbound);
            socket
                .send(Message::Binary(Bytes::from(frame)))
                .await
                .map_err(io::Error::other)?;
        }

        // Drain any plaintext the tunnel has already delivered.
        let mut plaintext = [0u8; 4096];
        loop {
            match tls.reader().read(&mut plaintext) {
                Ok(0) => break,
                Ok(read) => response.extend_from_slice(&plaintext[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }

        // Once the handshake is done, make one HTTP request through the tunnel.
        if !tls.is_handshaking() && !request_sent {
            let request = format!("GET / HTTP/1.1\r\nHost: {sni}\r\nConnection: close\r\n\r\n");
            tls.writer()
                .write_all(request.as_bytes())
                .map_err(io::Error::other)?;
            request_sent = true;
            continue;
        }

        // Wait for the next bridge frame: tunneled TLS bytes, or a half-close.
        let message = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                return Err(io::Error::other(error));
            }
            Ok(None) | Err(_) => break,
        };
        match message {
            Message::Binary(frame) => {
                if let Some((id, payload)) = bridge::decode_data_frame(&frame)
                    && id == conn
                {
                    let mut payload = payload;
                    while !payload.is_empty() {
                        let read = tls.read_tls(&mut payload).map_err(io::Error::other)?;
                        if read == 0 {
                            break;
                        }
                        tls.process_new_packets().map_err(io::Error::other)?;
                    }
                }
            }
            Message::Text(text) => {
                if let Ok(ServerMessage::End { conn: ended_conn }) =
                    serde_json::from_str::<ServerMessage>(&text)
                    && ended_conn == conn
                {
                    ended = true;
                }
            }
            Message::Ping(payload) => {
                let _ = socket.send(Message::Pong(payload)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    let _ = backend;
    let text = String::from_utf8_lossy(&response).into_owned();
    *shared.visitor_response.lock().await = Some(text.clone());
    Ok(text)
}

// ---------------------------------------------------------------------------
// Backend HTTP server
// ---------------------------------------------------------------------------

/// Starts a loopback HTTP backend that answers `200` with `body`, counting
/// requests. Returns its address.
pub async fn start_backend(body: &'static str, counter: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    address
}
