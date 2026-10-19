//! The server the browser tests talk to: the real one, in this process, on a
//! free port with plain WebSocket (a browser can't be told to trust a
//! self-signed certificate). It makes two active accounts, writes where to
//! find them to `--out <file>`, then serves until killed.
//!
//! ```text
//! cargo run -p corvo-chat-core --example test_server -- --out bench.json
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use corvo_chat_core::{Strength, Transport, TransportError, hello, sign_up};
use corvo_chat_proto::{
    B64,
    frames::{ClientFrame, Device, DeviceKind, ServerFrame},
    opaque::Suite,
};
use corvo_chat_server::{
    AppState, PeerAddr, app,
    clock::Clock,
    db::{self, Db},
    hub::Hub,
    limits::Limits,
};
use futures::{SinkExt, StreamExt};
use opaque_ke::ServerSetup;
use rand_core::{OsRng, RngCore};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const PASSWORD: &str = "correct horse battery";

/// A plain `ws://` connection, for making the accounts. The core's native
/// transport only speaks TLS.
struct Plain(WebSocketStream<TcpStream>);

fn io(error: impl std::fmt::Display) -> TransportError {
    TransportError::Io(error.to_string())
}

impl Plain {
    async fn connect(addr: SocketAddr, url: &str) -> Plain {
        let tcp = TcpStream::connect(addr).await.expect("the server is listening");
        let (ws, _) = tokio_tungstenite::client_async(url, tcp).await.expect("the WebSocket opens");
        Plain(ws)
    }
}

impl Transport for Plain {
    async fn send(&mut self, frame: &ClientFrame) -> Result<(), TransportError> {
        let text = serde_json::to_string(frame).map_err(io)?;
        self.0.send(Message::text(text)).await.map_err(io)
    }

    async fn recv(&mut self) -> Result<ServerFrame, TransportError> {
        loop {
            match self.0.next().await {
                None | Some(Ok(Message::Close(_))) => return Err(TransportError::Closed),
                Some(Err(error)) => return Err(io(error)),
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text).map_err(|e| TransportError::BadFrame(e.to_string()));
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

/// The file to write, from `--out <file>`.
fn out_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("--out"), Some(path)) => PathBuf::from(path),
        _ => {
            eprintln!("usage: test_server --out <file>");
            std::process::exit(2);
        }
    }
}

/// Signs up `base` with the normal password hash (the browser signs in with
/// it), then activates the account as the server's admin command does.
async fn account(addr: SocketAddr, url: &str, dir: &std::path::Path, base: &str) -> serde_json::Value {
    let mut t = Plain::connect(addr, url).await;
    hello(&mut t, env!("CARGO_PKG_VERSION")).await.expect("the server is ready");
    let mut id = [0u8; 16];
    OsRng.fill_bytes(&mut id);
    let device = Device { id: B64(id), kind: DeviceKind::Cli, name: "bench".into() };
    let up = sign_up(&mut t, base, base, PASSWORD, &device, Strength::Normal).await.expect("the signup goes through");
    let account = up.signed_in.account;
    corvo_chat_server::admin::activate(dir, &account.number.digits()).expect("the account activates");
    serde_json::json!({ "username": account.username, "password": PASSWORD, "number": account.number })
}

#[tokio::main]
async fn main() {
    let out = out_path();
    let dir = tempfile::tempdir().expect("a temp dir");
    let conn = db::open(&dir.path().join(db::DB_FILE)).expect("a database");
    let state = AppState {
        db: Db::new(conn),
        setup: Arc::new(ServerSetup::<Suite>::new(&mut OsRng)),
        // The system clock, so `admin::activate` (which uses it too) agrees with the server.
        clock: Clock::system(),
        // Every test client comes from 127.0.0.1.
        limits: Arc::new(Limits { signups_per_ip_day: 1_000, ..Limits::default() }),
        hub: Hub::default(),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let addr = listener.local_addr().expect("a bound address");
    let url = format!("ws://127.0.0.1:{}/ws", addr.port());
    // The server's own `serve` puts each connection's address in the request;
    // here everyone is on this machine.
    let app = app(state).layer(axum::Extension(PeerAddr(SocketAddr::from(([127, 0, 0, 1], 0)))));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let alice = account(addr, &url, dir.path(), "alice").await;
    let bob = account(addr, &url, dir.path(), "bob").await;
    let bench = serde_json::json!({ "url": url, "accounts": [alice, bob] });
    // Written whole and then renamed, so a reader never sees half a file.
    let partial = out.with_extension("partial");
    std::fs::write(&partial, bench.to_string()).expect("the bench file is written");
    std::fs::rename(&partial, &out).expect("the bench file is in place");
    println!("serving {url}; accounts in {}", out.display());

    server.await.expect("the server runs").expect("the server keeps serving");
}
