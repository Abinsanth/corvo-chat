//! Shared by the integration tests: the real server running in this process
//! on a free port, with a self-signed certificate the client trusts.
#![allow(dead_code)] // each test binary uses only some of these

use std::sync::Arc;

use corvo_chat_core::native::{NativeTransport, Roots};
use corvo_chat_core::{Client, MemoryCache, Strength, hello, sign_in, sign_up};
use corvo_chat_proto::{
    B64, Number,
    frames::{Device, DeviceKind},
    opaque::Suite,
};
use corvo_chat_server::{
    AppState, Timeouts, Tls, app,
    clock::Clock,
    db::{self, Db},
    hub::Hub,
    limits::Limits,
    serve_with,
};
use opaque_ke::ServerSetup;
use rand_core::{OsRng, RngCore};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tempfile::TempDir;
use tokio::net::TcpListener;

/// A running server. Keeps the data directory alive.
pub struct TestServer {
    pub url: String,
    pub roots: Roots,
    pub state: AppState,
    pub dir: TempDir,
}

fn ring() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn self_signed() -> (Arc<rustls::ServerConfig>, CertificateDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(["localhost".to_string()]).expect("a test certificate");
    let der = cert.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let config = rustls::ServerConfig::builder_with_provider(ring())
        .with_safe_default_protocol_versions()
        .expect("ring supports the defaults")
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .expect("the test certificate is valid");
    (Arc::new(config), der)
}

/// A fresh key and database in a temp dir. The clock is the system's, so
/// `admin::activate` (which uses it too) agrees with the server.
fn state(limits: Limits) -> (AppState, TempDir) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let conn = db::open(&dir.path().join(db::DB_FILE)).expect("a database");
    let state = AppState {
        db: Db::new(conn),
        setup: Arc::new(ServerSetup::<Suite>::new(&mut rand_core::OsRng)),
        clock: Clock::system(),
        limits: Arc::new(limits),
        hub: Hub::default(),
    };
    (state, dir)
}

/// A server with the default limits, except signups per IP: every test client
/// comes from 127.0.0.1.
pub async fn start() -> TestServer {
    start_with(Limits { signups_per_ip_day: 1_000, ..Limits::default() }, Timeouts::default()).await
}

/// Like `start`, with chosen limits and timeouts.
pub async fn start_with(limits: Limits, timeouts: Timeouts) -> TestServer {
    let (state, dir) = state(limits);
    let (config, der) = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let addr = listener.local_addr().expect("a bound address");
    tokio::spawn(serve_with(listener, Tls::Static(config), app(state.clone()), timeouts));
    TestServer { url: format!("wss://localhost:{}/ws", addr.port()), roots: Roots::Only(vec![der]), state, dir }
}

/// Activates a pending account, as the server's admin command does.
pub fn activate(server: &TestServer, number: Number) {
    corvo_chat_server::admin::activate(server.dir.path(), &number.digits()).expect("the account activates");
}

pub async fn connect(server: &TestServer) -> NativeTransport {
    NativeTransport::connect(&server.url, server.roots.clone()).await.expect("the connection opens")
}

pub const S: Strength = Strength::FastInsecure;
pub const PASSWORD: &str = "correct horse battery";

/// A live client with an in-memory cache, as most tests use.
pub type TestClient = Client<NativeTransport, MemoryCache>;

pub fn device(name: &str) -> Device {
    let mut id = [0u8; 16];
    OsRng.fill_bytes(&mut id);
    Device { id: B64(id), kind: DeviceKind::Cli, name: name.into() }
}

/// A connection that has said hello.
pub async fn hello_conn(server: &TestServer) -> NativeTransport {
    let mut t = connect(server).await;
    hello(&mut t, "0.0.0").await.expect("the server is ready");
    t
}

/// A new account, still pending, with an empty cache.
pub async fn pending(server: &TestServer, base: &str) -> TestClient {
    let mut t = hello_conn(server).await;
    let dev = device("laptop");
    let up = sign_up(&mut t, base, base, PASSWORD, &dev, S).await.expect("the signup goes through");
    Client::new(t, up.signed_in, dev, S, MemoryCache::default())
}

/// Two accounts that are contacts: the admin activates the first, which
/// invites the second.
pub async fn contacts(server: &TestServer) -> (TestClient, TestClient) {
    let mut a = pending(server, "alice").await;
    activate(server, a.account().number);
    let mut b = pending(server, "bob").await;
    let invite = a.invite(b.account().number).await.expect("the invite goes out");
    b.activate(&invite.code).await.expect("the code works");
    (a, b)
}

/// The same account signed in on another device, with an empty cache.
pub async fn new_device(server: &TestServer, username: &str) -> TestClient {
    let mut t = hello_conn(server).await;
    let dev = device("phone");
    let signed_in = sign_in(&mut t, username, PASSWORD, &dev, S).await.expect("the sign-in goes through");
    Client::new(t, signed_in, dev, S, MemoryCache::default())
}
