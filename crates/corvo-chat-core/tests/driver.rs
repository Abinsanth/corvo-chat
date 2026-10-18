mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{PASSWORD, S, TestClient, TestServer, wait};
use corvo_chat_core::native::{Command, Driver, Reply, first_start};
use corvo_chat_core::{Cache, Client, ConnState, Error, LocalKey, Mark, MemoryCache, Update, sign_up, view};
use corvo_chat_proto::frames::{Device, SignedOutReason};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

/// A TCP proxy in front of the test server that the test can cut (every
/// connection dropped, the port refusing) and open again on the same port.
struct Proxy {
    addr: SocketAddr,
    target: SocketAddr,
    accept: Option<JoinHandle<()>>,
}

impl Proxy {
    async fn start(server: &TestServer) -> Proxy {
        let port = server.url.rsplit(':').next().unwrap().trim_end_matches("/ws").parse::<u16>().unwrap();
        let target = SocketAddr::from(([127, 0, 0, 1], port));
        let addr = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let mut proxy = Proxy { addr, target, accept: None };
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
        let target = self.target;
        self.accept = Some(tokio::spawn(async move {
            // The connections live in this set, so aborting the task drops them all.
            let mut links = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut inbound, _)) = accepted else { return };
                        links.spawn(async move {
                            if let Ok(mut outbound) = TcpStream::connect(target).await {
                                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                            }
                        });
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

/// A new account, activated by the admin, with the key file `first_start`
/// makes for it and a client on its sign-up connection (drop it before a
/// driver resumes, or the resume closes it).
async fn account(server: &TestServer, base: &str) -> (LocalKey, Device, TestClient) {
    let mut t = common::hello_conn(server).await;
    let dev = common::device("laptop");
    let up = sign_up(&mut t, base, base, PASSWORD, &dev, S).await.unwrap();
    let key = first_start(&server.url, &up.signed_in, PASSWORD, &dev, S);
    let client = Client::new(t, up.signed_in, dev.clone(), S, MemoryCache::default());
    common::activate(server, client.account().number);
    (key, dev, client)
}

fn start(
    url: String,
    server: &TestServer,
    key: LocalKey,
    path: &Path,
    dev: Device,
) -> (Driver, UnboundedReceiver<Update>) {
    Driver::start(url, server.roots.clone(), key, PASSWORD, path.to_path_buf(), dev, S).unwrap()
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

#[tokio::test]
async fn waiting_message_is_sent_once_after_a_reconnect() {
    let server = common::start().await;
    let mut proxy = Proxy::start(&server).await;
    let (key, dev, mut a) = account(&server, "alice").await;
    let a_number = a.account().number;
    let mut b = common::pending(&server, "bob").await;
    let b_number = b.account().number;
    let invite = a.invite(b_number).await.unwrap();
    b.activate(&invite.code).await.unwrap();
    drop(a);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let (driver, mut updates) = start(proxy.url(), &server, key, &path, dev);
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

    b.sync().await.unwrap();
    let chat_with_a = |b: &TestClient| b.cache().chat_with(a_number).unwrap().map(|c| c.info.chat_id.0);
    wait(&mut b, |b| chat_with_a(b).is_some_and(|id| !b.cache().messages(id, i64::MAX, 10).unwrap().is_empty())).await;
    // A round trip, so a second copy would have arrived by now.
    b.invites().await.unwrap();
    let chat_id = chat_with_a(&b).unwrap();
    let shown = view::messages(b.cache(), b.secrets(), chat_id, None, 50).unwrap();
    let copies = shown.iter().filter(|m| m.text.as_deref() == Ok(text)).count();
    assert_eq!((shown.len(), copies), (1, 1));
}

#[tokio::test]
async fn backoff_grows_and_resets() {
    let server = common::start().await;
    let mut proxy = Proxy::start(&server).await;
    let (key, dev, a) = account(&server, "alice").await;
    drop(a);
    proxy.cut().await;

    let dir = tempfile::tempdir().unwrap();
    let (_driver, mut updates) = start(proxy.url(), &server, key, &dir.path().join("cache.db"), dev);
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
    let (key, dev, a) = account(&server, "alice").await;
    let username = a.account().username.clone();
    drop(a);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let (driver, mut updates) = start(server.url.clone(), &server, key, &path, dev.clone());
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
    let (key, dev, _a) = account(&server, "alice").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let started = Driver::start(server.url.clone(), server.roots.clone(), key, "not it at all", path.clone(), dev, S);
    assert!(matches!(started, Err(Error::WrongPassword)));
    assert!(!path.exists());
}
