mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use common::{PASSWORD, S, TestClient, TestServer, wait};
use corvo_chat_core::native::{Command, Driver, Reply, first_start};
use corvo_chat_core::native::{NativeTransport, Roots};
use corvo_chat_core::{
    Cache, Client, ConnState, Error, LocalKey, Mark, MemoryCache, Secrets, Transport, Update, sign_up, view,
};
use corvo_chat_proto::{
    Number,
    frames::{Device, SignedOutReason},
};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

/// A TCP proxy in front of the test server that the test can cut (every
/// connection dropped, the port refusing) and open again on the same port.
/// While `hold` is set, what the client sends is counted in `held` and goes
/// no further.
struct Proxy {
    addr: SocketAddr,
    target: SocketAddr,
    accept: Option<JoinHandle<()>>,
    hold: Arc<AtomicBool>,
    held: Arc<AtomicUsize>,
}

impl Proxy {
    async fn start(server: &TestServer) -> Proxy {
        let port = server.url.rsplit(':').next().unwrap().trim_end_matches("/ws").parse::<u16>().unwrap();
        let target = SocketAddr::from(([127, 0, 0, 1], port));
        let addr = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let (hold, held) = (Arc::default(), Arc::default());
        let mut proxy = Proxy { addr, target, accept: None, hold, held };
        proxy.open();
        proxy
    }

    fn url(&self) -> String {
        format!("wss://localhost:{}/ws", self.addr.port())
    }

    fn open(&mut self) {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(true).unwrap();
        socket.bind(self.addr).unwrap();
        let listener = socket.listen(64).unwrap();
        let (target, hold, held) = (self.target, self.hold.clone(), self.held.clone());
        self.accept = Some(tokio::spawn(async move {
            // The connections live in this set, so aborting the task drops them all.
            let mut links = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((inbound, _)) = accepted else { return };
                        links.spawn(link(inbound, target, hold.clone(), held.clone()));
                    }
                    Some(_) = links.join_next() => {}
                }
            }
        }));
    }

    async fn cut(&mut self) {
        if let Some(task) = self.accept.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

/// One proxied connection.
async fn link(inbound: TcpStream, target: SocketAddr, hold: Arc<AtomicBool>, held: Arc<AtomicUsize>) {
    let Ok(outbound) = TcpStream::connect(target).await else { return };
    let ((mut client_in, mut client_out), (mut server_in, mut server_out)) =
        (inbound.into_split(), outbound.into_split());
    let up = async {
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = client_in.read(&mut buf).await {
            if hold.load(Ordering::SeqCst) {
                held.fetch_add(n, Ordering::SeqCst);
            } else if server_out.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        _ = up => {}
        _ = tokio::io::copy(&mut server_in, &mut client_out) => {}
    }
}

/// A new account, activated by the admin, with the key file `first_start`
/// makes for it, naming `url` as the server, and a client on its sign-up
/// connection (drop it before a driver resumes, or the resume closes it).
async fn account(server: &TestServer, url: &str, base: &str) -> (LocalKey, Device, TestClient) {
    let mut t = common::hello_conn(server).await;
    let dev = common::device("laptop");
    let up = sign_up(&mut t, base, base, PASSWORD, &dev, S).await.unwrap();
    let key = first_start(url, &up.signed_in, PASSWORD, &dev, S);
    let client = Client::new(t, up.signed_in, dev.clone(), S, MemoryCache::default());
    common::activate(server, client.account().number);
    (key, dev, client)
}

/// Starts a driver on the server the key file names.
fn start(server: &TestServer, key: LocalKey, path: &Path, dev: Device) -> (Driver, UnboundedReceiver<Update>) {
    Driver::start(server.roots.clone(), key, PASSWORD, path.to_path_buf(), dev, S, "0.0.0").unwrap()
}

/// The next connection update, skipping the rest. Long enough to outlast
/// any back-off on a paused clock.
async fn next_conn(updates: &mut UnboundedReceiver<Update>) -> ConnState {
    let run = async {
        loop {
            if let Update::Connection(state) = updates.recv().await.expect("the driver is running") {
                return state;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(100), run).await.expect("a connection update")
}

/// Reads updates until one matches, failing after five seconds.
async fn until(updates: &mut UnboundedReceiver<Update>, wanted: impl Fn(&Update) -> bool) {
    let run = async {
        loop {
            let update = updates.recv().await.expect("the driver is running");
            if wanted(&update) {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), run).await.expect("it happens in time");
}

fn files(path: &Path) -> Vec<PathBuf> {
    let name = path.file_name().unwrap().to_str().unwrap();
    ["", "-wal", "-shm"].iter().map(|end| path.with_file_name(format!("{name}{end}"))).collect()
}

/// Alice (with a key file naming `url` and a device) and Bob, who are contacts.
async fn contacts(server: &TestServer, url: &str) -> (LocalKey, Device, Number, TestClient) {
    let (key, dev, mut a) = account(server, url, "alice").await;
    let mut b = common::pending(server, "bob").await;
    let invite = a.invite(b.account().number).await.unwrap();
    b.activate(&invite.code).await.unwrap();
    (key, dev, a.account().number, b)
}

/// Bob's messages from Alice, once one has arrived and a round trip later
/// (so a second copy would have come too).
async fn from_alice(b: &mut TestClient, a_number: Number) -> Vec<Result<String, String>> {
    b.sync().await.unwrap();
    let chat_with_a = |b: &TestClient| b.cache().chat_with(a_number).unwrap().map(|c| c.info.chat_id.0);
    wait(b, |b| chat_with_a(b).is_some_and(|id| !b.cache().messages(id, i64::MAX, 10).unwrap().is_empty())).await;
    b.invites().await.unwrap();
    let chat_id = chat_with_a(b).unwrap();
    view::messages(b.cache(), b.secrets(), chat_id, None, 50).unwrap().into_iter().map(|m| m.text).collect()
}

#[tokio::test]
async fn waiting_message_is_sent_once_after_a_reconnect() {
    let server = common::start().await;
    let mut proxy = Proxy::start(&server).await;
    let (key, dev, a_number, mut b) = contacts(&server, &proxy.url()).await;
    let b_number = b.account().number;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let (driver, mut updates) = start(&server, key, &path, dev);
    assert_eq!(next_conn(&mut updates).await, ConnState::Online);

    proxy.cut().await;
    assert!(matches!(next_conn(&mut updates).await, ConnState::Reconnecting { .. }));
    let text = "written while away";
    let Ok(Reply::Sent(msg_id)) = driver.call(Command::SendText { to: b_number, text: text.into() }).await else {
        panic!("the message should queue");
    };
    let Ok(Reply::Chats(rows)) = driver.call(Command::Chats).await else { panic!("the chats should show") };
    let last = rows.iter().find(|r| r.with == b_number).and_then(|r| r.last.clone()).expect("a row for bob");
    assert_eq!((last.msg_id, last.mark), (msg_id, Some(Mark::Waiting)));
    // Only sending works offline.
    assert!(matches!(driver.call(Command::Invites).await, Err(Error::Offline)));

    proxy.open();
    until(&mut updates, |u| matches!(u, Update::Sent { msg_id: m, .. } if *m == msg_id)).await;

    assert_eq!(from_alice(&mut b, a_number).await, [Ok(text.to_string())]);
}

#[tokio::test]
async fn a_send_cut_short_still_answers_with_its_id() {
    let server = common::start().await;
    let mut proxy = Proxy::start(&server).await;
    let (key, dev, a_number, mut b) = contacts(&server, &proxy.url()).await;
    let b_number = b.account().number;
    let dir = tempfile::tempdir().unwrap();
    let (driver, mut updates) = start(&server, key, &dir.path().join("cache.db"), dev);
    let driver = Arc::new(driver);
    assert_eq!(next_conn(&mut updates).await, ConnState::Online);

    // The message is in the outbox and on its way when the connection goes.
    proxy.hold.store(true, Ordering::SeqCst);
    let text = "cut short";
    let sending = tokio::spawn({
        let driver = driver.clone();
        async move { driver.call(Command::SendText { to: b_number, text: text.into() }).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while proxy.held.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the send reaches the proxy");
    proxy.cut().await;
    let Ok(Ok(Reply::Sent(msg_id))) = sending.await else { panic!("a queued message answers with its id") };

    // It goes after the reconnect, once.
    proxy.hold.store(false, Ordering::SeqCst);
    proxy.open();
    until(&mut updates, |u| matches!(u, Update::Sent { msg_id: m, .. } if *m == msg_id)).await;
    assert_eq!(from_alice(&mut b, a_number).await, [Ok(text.to_string())]);
}

/// A server too new for this app: it answers `hello` with `update_required`.
/// Counts the connections it gets.
async fn too_new_server() -> (String, Roots, Arc<AtomicUsize>) {
    let (config, der) = common::self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("wss://localhost:{}/ws", listener.local_addr().unwrap().port());
    let tries = Arc::new(AtomicUsize::new(0));
    let counted = tries.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let acceptor = tokio_rustls::TlsAcceptor::from(config.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else { return };
                let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else { return };
                if ws.next().await.is_some() {
                    let refusal = r#"{"type":"error","code":"update_required","message":"run /update"}"#;
                    let _ = ws.send(tokio_tungstenite::tungstenite::Message::text(refusal)).await;
                    let _ = ws.close(None).await;
                }
            });
        }
    });
    (url, Roots::Only(vec![der]), tries)
}

#[tokio::test]
async fn an_app_too_old_stops_trying_and_says_so() {
    let (url, roots, tries) = too_new_server().await;
    let secrets = Secrets::generate();
    let key = LocalKey::lock(&url, "abin-7k3q", [1; 16], PASSWORD, &secrets, &[2; 32], S);
    let dir = tempfile::tempdir().unwrap();
    let dev = common::device("laptop");
    let started = Driver::start(roots, key, PASSWORD, dir.path().join("cache.db"), dev, S, "0.0.0");
    let (driver, mut updates) = started.unwrap();
    assert_eq!(next_conn(&mut updates).await, ConnState::UpdateRequired);

    // No more tries, but what's saved can still be read.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(tries.load(Ordering::SeqCst), 1);
    assert!(matches!(driver.call(Command::Chats).await, Ok(Reply::Chats(rows)) if rows.is_empty()));
    assert!(matches!(driver.call(Command::Invites).await, Err(Error::Offline)));
}

#[tokio::test]
async fn backoff_grows_and_resets() {
    let server = common::start().await;
    let mut proxy = Proxy::start(&server).await;
    let (key, dev, a) = account(&server, &proxy.url(), "alice").await;
    drop(a);
    proxy.cut().await;

    let dir = tempfile::tempdir().unwrap();
    let (_driver, mut updates) = start(&server, key, &dir.path().join("cache.db"), dev);
    assert_eq!(next_conn(&mut updates).await, ConnState::Reconnecting { in_secs: 1 });
    assert_eq!(next_conn(&mut updates).await, ConnState::Reconnecting { in_secs: 2 });

    // Reaching synced resets it.
    proxy.open();
    assert_eq!(next_conn(&mut updates).await, ConnState::Online);
    proxy.cut().await;
    assert_eq!(next_conn(&mut updates).await, ConnState::Reconnecting { in_secs: 1 });

    // Refusing at TCP needs no timers, so the paused clock only skips the waits.
    tokio::time::pause();
    for in_secs in [2, 4, 8, 16, 30, 30] {
        assert_eq!(next_conn(&mut updates).await, ConnState::Reconnecting { in_secs });
    }
}

#[tokio::test]
async fn signed_out_elsewhere_wipes_the_cache() {
    let server = common::start().await;
    let (key, dev, a) = account(&server, &server.url, "alice").await;
    let username = a.account().username.clone();
    drop(a);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let (driver, mut updates) = start(&server, key, &path, dev.clone());
    assert_eq!(next_conn(&mut updates).await, ConnState::Online);
    assert!(path.exists());

    let mut other = common::new_device(&server, &username).await;
    other.sign_out_device(dev.id.0).await.unwrap();
    until(&mut updates, |u| *u == Update::SignedOut(SignedOutReason::ByAnotherDevice)).await;
    for file in files(&path) {
        assert!(!file.exists(), "{} is still there", file.display());
    }
    // It stopped for good.
    assert!(matches!(driver.call(Command::Chats).await, Err(Error::Stopped)));
    assert!(updates.recv().await.is_none());
}

#[tokio::test]
async fn a_wrong_password_fails_at_start() {
    let server = common::start().await;
    let (key, dev, _a) = account(&server, &server.url, "alice").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let started = Driver::start(server.roots.clone(), key, "not it at all", path.clone(), dev, S, "0.0.0");
    assert!(matches!(started, Err(Error::WrongPassword)));
    assert!(!path.exists());
}

#[tokio::test]
async fn devices_come_from_the_cache_when_offline() {
    let server = common::start().await;
    let mut proxy = Proxy::start(&server).await;
    let (key, dev, a) = account(&server, &proxy.url(), "alice").await;
    drop(a);
    let dir = tempfile::tempdir().unwrap();
    let (driver, mut updates) = start(&server, key, &dir.path().join("cache.db"), dev);
    assert_eq!(next_conn(&mut updates).await, ConnState::Online);
    let Ok(Reply::Devices(online)) = driver.call(Command::Devices).await else { panic!("the devices list") };
    assert!(!online.is_empty());

    proxy.cut().await;
    assert!(matches!(next_conn(&mut updates).await, ConnState::Reconnecting { .. }));
    let Ok(Reply::Devices(offline)) = driver.call(Command::Devices).await else { panic!("the cached list") };
    assert_eq!(offline, online);
    assert!(matches!(driver.call(Command::People).await, Ok(Reply::People(_))));
}

/// A server that lets `hello` and `resume` through to the real one, then
/// reads whatever else comes and never answers. Counts the frames it swallows.
async fn stalling_server(server: &TestServer) -> (String, Roots, Arc<AtomicUsize>) {
    let (config, der) = common::self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("wss://localhost:{}/ws", listener.local_addr().unwrap().port());
    let swallowed = Arc::new(AtomicUsize::new(0));
    let (counted, real_url, real_roots) = (swallowed.clone(), server.url.clone(), server.roots.clone());
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = tokio_rustls::TlsAcceptor::from(config.clone());
            let (counted, real_url, real_roots) = (counted.clone(), real_url.clone(), real_roots.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else { return };
                let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else { return };
                let Ok(mut upstream) = NativeTransport::connect(&real_url, real_roots).await else { return };
                let mut passed = 0;
                while let Some(Ok(message)) = ws.next().await {
                    let Ok(text) = message.into_text() else { continue };
                    if passed < 2 {
                        let frame = serde_json::from_str(text.as_str()).unwrap();
                        upstream.send(&frame).await.unwrap();
                        let reply = serde_json::to_string(&upstream.recv().await.unwrap()).unwrap();
                        ws.send(tokio_tungstenite::tungstenite::Message::text(reply)).await.unwrap();
                        passed += 1;
                    } else {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
        }
    });
    (url, Roots::Only(vec![der]), swallowed)
}

#[tokio::test]
async fn a_first_sync_that_never_ends_reconnects() {
    let server = common::start().await;
    let (url, roots, swallowed) = stalling_server(&server).await;
    let (key, dev, a) = account(&server, &url, "alice").await;
    drop(a);
    let dir = tempfile::tempdir().unwrap();
    let (_driver, mut updates) =
        Driver::start(roots, key, PASSWORD, dir.path().join("cache.db"), dev, S, "0.0.0").unwrap();

    // The sync request is out and unanswered; only the limit is left to run.
    tokio::time::timeout(Duration::from_secs(5), async {
        while swallowed.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sync request arrives");
    tokio::time::pause();
    assert_eq!(next_conn(&mut updates).await, ConnState::Reconnecting { in_secs: 1 });
}
