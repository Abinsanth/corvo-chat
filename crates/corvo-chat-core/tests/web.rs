//! The core in a real browser, against the test bench (`examples/test_server`),
//! which must be running. `CORVO_TEST_BENCH` names the file it wrote.
#![cfg(all(target_arch = "wasm32", feature = "web"))]

use std::cell::RefCell;
use std::future::Future;

use corvo_chat_core::web::{IdbCache, WebDriver, WebTransport, connect, now_ms};
use corvo_chat_core::{
    Cache, CachedChat, CachedPerson, Command, ConnState, Error, Mark, MsgId, OutState, Outgoing, Reply, Secrets,
    Strength, TransportError, Update, exercise_cache, hello, sign_in,
};
use corvo_chat_proto::{
    B64, Body, Number, PublicKeys,
    chat::{ChatInfo, Marks, Message, Person, Relation},
    frames::{Account, Device, DeviceInfo, DeviceKind, SignedOutReason},
};
use futures::channel::mpsc::UnboundedReceiver;
use futures::{FutureExt, StreamExt};
use gloo_timers::future::TimeoutFuture;
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
use zeroize::Zeroizing;

wasm_bindgen_test_configure!(run_in_browser);

const BENCH: &str = include_str!(env!("CORVO_TEST_BENCH"));

#[derive(Deserialize)]
struct Bench {
    url: String,
    accounts: Vec<BenchAccount>,
}

#[derive(Deserialize)]
struct BenchAccount {
    username: String,
    password: String,
    number: Number,
}

fn bench() -> Bench {
    serde_json::from_str(BENCH).expect("the bench file is readable")
}

fn device() -> Device {
    let mut id = [0u8; 16];
    OsRng.fill_bytes(&mut id);
    Device { id: B64(id), kind: DeviceKind::Web, name: "browser".into() }
}

async fn open() -> WebTransport {
    connect(&bench().url).await.expect("the connection opens")
}

#[wasm_bindgen_test]
async fn says_hello() {
    let mut t = open().await;
    let version = hello(&mut t, "0.1.1").await.expect("the server is ready");
    assert!(version.starts_with("0."), "{version}");
}

#[wasm_bindgen_test]
async fn signs_in() {
    let bench = bench();
    let account = &bench.accounts[0];
    let mut t = open().await;
    hello(&mut t, "0.1.1").await.expect("the server is ready");
    let signed_in = sign_in(&mut t, &account.username, &account.password, &device(), Strength::Normal)
        .await
        .expect("the sign-in goes through");
    assert_eq!(signed_in.account.number, account.number);
}

#[wasm_bindgen_test]
async fn a_closed_server_gives_closed_or_io() {
    let started = js_sys::Date::now();
    let result = connect("ws://127.0.0.1:9/ws").await;
    assert!(matches!(result, Err(TransportError::Io(_))), "{:?}", result.err());
    assert!(js_sys::Date::now() - started < 10_000.0, "it gave up in time");
}

fn number(body: u32) -> Number {
    Number::from_body(body).unwrap()
}

fn person() -> CachedPerson {
    CachedPerson {
        person: Person {
            number: number(222_222_222),
            display_name: Some("Rahul".into()),
            public_keys: Secrets::generate().public_keys(),
            relation: Relation::Contact,
            deleted: false,
        },
        mismatch: false,
    }
}

fn message() -> Message {
    Message {
        chat_id: B64([7; 16]),
        seq: 3,
        msg_id: B64([3; 16]),
        from: number(222_222_222),
        body: Body(vec![9; 40]),
        sent_at: 1_800_000_003,
    }
}

fn outgoing() -> Outgoing {
    Outgoing {
        chat_id: [7; 16],
        msg_id: MsgId([1; 16]),
        to: number(222_222_222),
        text: "hello".into(),
        body: None,
        new_chat: None,
        created_ms: 1_800_000_000_000,
        state: OutState::Waiting,
    }
}

/// Fills a cache with one of everything the reload test checks, and stores it.
async fn fill(cache: &mut IdbCache) {
    cache.put_person(person()).unwrap();
    cache.put_message(message()).unwrap();
    cache.put_outgoing(outgoing()).unwrap();
    cache.set_cursor(41).unwrap();
    cache.flush().await.expect("the writes are stored");
}

fn assert_empty(cache: &IdbCache) {
    assert_eq!(cache.cursor().unwrap(), None);
    assert!(cache.people().unwrap().is_empty());
    assert!(cache.messages([7; 16], i64::MAX, 10).unwrap().is_empty());
    assert!(cache.outbox().unwrap().is_empty());
}

#[wasm_bindgen_test]
async fn idb_cache_behaves_like_every_cache() {
    IdbCache::delete("t-exercise").await.unwrap();
    let mut cache = IdbCache::open("t-exercise", &Secrets::generate()).await.unwrap();
    exercise_cache(&mut cache);
    cache.flush().await.expect("every write is stored");
    drop(cache);
    IdbCache::delete("t-exercise").await.unwrap();
}

#[wasm_bindgen_test]
async fn idb_cache_survives_a_reload() {
    IdbCache::delete("t-reload").await.unwrap();
    let secrets = Secrets::generate();
    let mut cache = IdbCache::open("t-reload", &secrets).await.unwrap();
    assert_eq!(cache.name(), "t-reload");
    let person = person();
    cache.put_person(person.clone()).unwrap();
    cache.put_message(message()).unwrap();
    cache.put_outgoing(outgoing()).unwrap();
    cache.set_cursor(41).unwrap();
    cache.flush().await.expect("the writes are stored");
    drop(cache);

    let cache = IdbCache::open("t-reload", &secrets).await.unwrap();
    assert_eq!(cache.person(person.person.number).unwrap(), Some(person));
    assert_eq!(cache.messages([7; 16], i64::MAX, 10).unwrap(), vec![message()]);
    assert_eq!(cache.outbox().unwrap(), vec![outgoing()]);
    assert_eq!(cache.cursor().unwrap(), Some(41));
    drop(cache);
    IdbCache::delete("t-reload").await.unwrap();
}

#[wasm_bindgen_test]
async fn idb_cache_needs_the_right_secrets() {
    IdbCache::delete("t-secrets").await.unwrap();
    let mut cache = IdbCache::open("t-secrets", &Secrets::generate()).await.unwrap();
    fill(&mut cache).await;
    drop(cache);

    let result = IdbCache::open("t-secrets", &Secrets::generate()).await;
    assert!(matches!(result, Err(Error::Cache(_))), "{result:?}");
    IdbCache::delete("t-secrets").await.unwrap();
}

#[wasm_bindgen_test]
async fn wipe_and_delete_leave_nothing() {
    IdbCache::delete("t-wipe").await.unwrap();
    let secrets = Secrets::generate();
    let mut cache = IdbCache::open("t-wipe", &secrets).await.unwrap();
    fill(&mut cache).await;
    cache.wipe().unwrap();
    cache.flush().await.expect("the wipe is stored");
    drop(cache);
    let mut cache = IdbCache::open("t-wipe", &secrets).await.unwrap();
    assert_empty(&cache);

    fill(&mut cache).await;
    drop(cache);
    IdbCache::delete("t-wipe").await.unwrap();
    let cache = IdbCache::open("t-wipe", &secrets).await.unwrap();
    assert_empty(&cache);
    drop(cache);
    IdbCache::delete("t-wipe").await.unwrap();
}

const CHAT: [u8; 16] = [7; 16];
const OTHER_CHAT: [u8; 16] = [8; 16];

fn named(number: Number, name: &str) -> CachedPerson {
    let mut p = person();
    p.person.number = number;
    p.person.display_name = Some(name.into());
    p
}

fn account(name: &str) -> Account {
    Account {
        username: "abin-7k3q".into(),
        number: number(111_111_111),
        display_name: name.into(),
        pending_until: None,
    }
}

fn chat(id: [u8; 16], with: Number, last_seq: i64) -> CachedChat {
    CachedChat {
        info: ChatInfo {
            chat_id: B64(id),
            with,
            keys: vec![],
            signature: B64([1; 64]),
            last_seq,
            read_through: 0,
            cleared_through: 0,
            marks: Some(Marks { received_through: 1, read_through: 1, withheld: vec![[2, 3]] }),
        },
        complete: true,
    }
}

fn message_in(chat_id: [u8; 16], seq: i64) -> Message {
    Message { chat_id: B64(chat_id), seq, msg_id: B64([seq as u8; 16]), ..message() }
}

fn outgoing_in(chat_id: [u8; 16], id: u8, text: &str) -> Outgoing {
    Outgoing { chat_id, msg_id: MsgId([id; 16]), text: text.into(), ..outgoing() }
}

fn device_info(id: u8, name: &str) -> DeviceInfo {
    DeviceInfo {
        id: B64([id; 16]),
        kind: DeviceKind::Web,
        name: name.into(),
        first_seen: 1,
        last_used: 2,
        this_device: id == 1,
    }
}

/// Everything a cache answers, for comparing two of them.
#[derive(Debug, PartialEq)]
struct Answers {
    cursor: Option<i64>,
    account: Option<Account>,
    people: Vec<CachedPerson>,
    chats: Vec<CachedChat>,
    messages: Vec<Vec<Message>>,
    pins: Vec<(Number, PublicKeys)>,
    unsent_pins: Vec<Number>,
    devices: Vec<DeviceInfo>,
    outbox: Vec<Outgoing>,
}

fn answers(cache: &IdbCache) -> Answers {
    Answers {
        cursor: cache.cursor().unwrap(),
        account: cache.account().unwrap(),
        people: cache.people().unwrap(),
        chats: cache.chats().unwrap(),
        messages: [CHAT, OTHER_CHAT].map(|c| cache.messages(c, i64::MAX, usize::MAX).unwrap()).into(),
        pins: cache.pins().unwrap(),
        unsent_pins: cache.unsent_pins().unwrap(),
        devices: cache.devices().unwrap(),
        outbox: cache.outbox().unwrap(),
    }
}

fn seqs(messages: &[Message]) -> Vec<i64> {
    messages.iter().map(|m| m.seq).collect()
}

#[wasm_bindgen_test]
async fn idb_cache_reloads_what_it_stored() {
    IdbCache::delete("t-everything").await.unwrap();
    let secrets = Secrets::generate();
    let mut cache = IdbCache::open("t-everything", &secrets).await.unwrap();
    let (a, b) = (number(222_222_222), number(333_333_333));
    let (keys, other_keys) = (Secrets::generate().public_keys(), Secrets::generate().public_keys());

    cache.set_account(account("Abin")).unwrap();
    cache.set_devices(vec![device_info(1, "browser"), device_info(2, "laptop")]).unwrap();
    cache.put_person(named(a, "Rahul")).unwrap();
    cache.put_person(named(b, "Mira")).unwrap();
    cache.put_chat(chat(CHAT, a, 5)).unwrap();
    cache.put_chat(chat(OTHER_CHAT, b, 2)).unwrap();
    for seq in 1..=5 {
        cache.put_message(message_in(CHAT, seq)).unwrap();
    }
    cache.put_message(message_in(OTHER_CHAT, 1)).unwrap();
    cache.put_pin(a, Some(keys.clone())).unwrap();
    cache.put_pin(b, Some(other_keys.clone())).unwrap();
    cache.mark_pin_unsent(a).unwrap();
    cache.mark_pin_unsent(b).unwrap();
    cache.put_outgoing(outgoing_in(CHAT, 1, "first")).unwrap();
    cache.put_outgoing(outgoing_in(CHAT, 2, "second")).unwrap();
    cache.put_outgoing(outgoing_in(OTHER_CHAT, 3, "third")).unwrap();
    cache.set_cursor(9).unwrap();
    cache.flush().await.expect("the first writes are stored");

    // A snapshot, then changes on top of it.
    cache
        .replace_all(account("Abin S"), vec![named(b, "Mira K")], vec![chat(OTHER_CHAT, b, 6)], vec![(b, other_keys)])
        .unwrap();
    for seq in 3..=6 {
        cache.put_message(message_in(OTHER_CHAT, seq)).unwrap();
    }
    cache.clear_through(OTHER_CHAT, 4).unwrap();
    cache.put_message(message_in(CHAT, 7)).unwrap();
    cache.put_message(message_in(CHAT, 8)).unwrap();
    cache.clear_through(CHAT, 7).unwrap();
    cache.put_pin(a, Some(keys.clone())).unwrap();
    cache.put_pin(b, None).unwrap();
    cache.mark_pin_unsent(a).unwrap();
    cache.mark_pin_unsent(b).unwrap();
    cache.mark_pin_sent(b).unwrap();
    let mut sent = outgoing_in(CHAT, 1, "first");
    sent.state = OutState::Sent { seq: 9 };
    cache.put_outgoing(sent.clone()).unwrap();
    cache.remove_outgoing(CHAT, MsgId([2; 16])).unwrap();
    cache.put_outgoing(outgoing_in(CHAT, 2, "second")).unwrap();
    cache.set_cursor(12).unwrap();
    cache.flush().await.expect("the rest is stored");

    let before = answers(&cache);
    assert_eq!(seqs(&before.messages[0]), vec![8]);
    assert_eq!(seqs(&before.messages[1]), vec![5, 6]);
    assert_eq!(before.pins, vec![(a, keys)]);
    assert_eq!(before.unsent_pins, vec![a]);
    assert_eq!(before.outbox, vec![sent, outgoing_in(OTHER_CHAT, 3, "third"), outgoing_in(CHAT, 2, "second")]);
    assert_eq!(before.cursor, Some(12));
    drop(cache);

    let cache = IdbCache::open("t-everything", &secrets).await.unwrap();
    assert_eq!(answers(&cache), before);
    drop(cache);
    IdbCache::delete("t-everything").await.unwrap();
}

#[wasm_bindgen_test]
async fn a_failed_write_stops_the_writes_and_drops_the_cursor() {
    IdbCache::delete("t-failed").await.unwrap();
    let secrets = Secrets::generate();
    let mut cache = IdbCache::open("t-failed", &secrets).await.unwrap();
    cache.put_message(message_in(CHAT, 3)).unwrap();
    cache.set_cursor(10).unwrap();
    cache.flush().await.expect("the first writes are stored");

    cache.put_message(message_in(CHAT, 5)).unwrap();
    cache.break_the_next_write().unwrap();
    assert!(matches!(cache.flush().await, Err(Error::Cache(_))));
    cache.put_message(message_in(CHAT, 6)).unwrap();
    cache.set_cursor(42).unwrap();
    assert!(matches!(cache.flush().await, Err(Error::Cache(_))), "it keeps saying so");
    // Memory still has it all.
    assert_eq!(seqs(&cache.messages(CHAT, i64::MAX, 10).unwrap()), vec![3, 5, 6]);
    assert_eq!(cache.cursor().unwrap(), Some(42));
    drop(cache);

    let cache = IdbCache::open("t-failed", &secrets).await.unwrap();
    assert_eq!(seqs(&cache.messages(CHAT, i64::MAX, 10).unwrap()), vec![3], "nothing after the hole");
    assert_eq!(cache.cursor().unwrap(), None, "the next sync is a full one");
    drop(cache);
    IdbCache::delete("t-failed").await.unwrap();
}

/// A signed-in device on one of the bench's accounts.
#[derive(Clone)]
struct Session {
    username: String,
    secrets: Vec<u8>,
    token: [u8; 32],
    device: Device,
}

impl Session {
    fn secrets(&self) -> Secrets {
        Secrets::from_bytes(&self.secrets).expect("secrets read back")
    }
}

thread_local! {
    /// One session per account, shared by the driver tests: each sign-in
    /// counts toward the server's daily limit (5) and leaves a device, of
    /// which an account may have three. Its sign-up device and `signs_in`'s
    /// take two of alice's, so a bench serves one run of these tests.
    static SESSIONS: RefCell<[Option<Session>; 2]> = const { RefCell::new([None, None]) };
}

/// Signs a new device in to the bench's account `i`.
async fn new_session(i: usize) -> Session {
    let bench = bench();
    let account = &bench.accounts[i];
    let device = device();
    let mut t = open().await;
    hello(&mut t, "0.1.1").await.expect("the server is ready");
    let signed_in = sign_in(&mut t, &account.username, &account.password, &device, Strength::Normal)
        .await
        .expect("the sign-in goes through");
    Session {
        username: account.username.clone(),
        secrets: signed_in.secrets.to_bytes().to_vec(),
        token: *signed_in.token,
        device,
    }
}

/// The shared session on account `i`, signed in the first time it's asked for.
async fn session(i: usize) -> Session {
    if let Some(session) = SESSIONS.with_borrow(|all| all[i].clone()) {
        return session;
    }
    let session = new_session(i).await;
    SESSIONS.with_borrow_mut(|all| all[i] = Some(session.clone()));
    session
}

/// A driver for `session` on `url`, with a fresh database `db`.
async fn start(session: &Session, url: &str, db: &str) -> (WebDriver, UnboundedReceiver<Update>) {
    IdbCache::delete(db).await.unwrap();
    let secrets = session.secrets();
    let cache = IdbCache::open(db, &secrets).await.unwrap();
    let (username, device) = (session.username.clone(), session.device.clone());
    WebDriver::start(url.into(), secrets, Zeroizing::new(session.token), username, device, cache, "0.1.1".into())
}

/// `f`'s output, or `None` after `ms` milliseconds.
async fn within<F: Future>(ms: u32, f: F) -> Option<F::Output> {
    futures::select! {
        out = f.fuse() => Some(out),
        () = TimeoutFuture::new(ms).fuse() => None,
    }
}

/// Reads updates until one matches, failing after ten seconds.
async fn until(updates: &mut UnboundedReceiver<Update>, wanted: impl Fn(&Update) -> bool) -> Update {
    let run = async {
        loop {
            let update = updates.next().await.expect("the driver is running");
            if wanted(&update) {
                return update;
            }
        }
    };
    within(10_000, run).await.expect("it happens in time")
}

/// The next connection update, skipping the rest.
async fn next_conn(updates: &mut UnboundedReceiver<Update>) -> ConnState {
    let Update::Connection(state) = until(updates, |u| matches!(u, Update::Connection(_))).await else {
        unreachable!("only a connection update matches")
    };
    state
}

/// The names of this page's databases.
async fn databases() -> Vec<String> {
    let get = |target: &JsValue, key: &str| js_sys::Reflect::get(target, &key.into()).unwrap();
    let factory = get(&js_sys::global(), "indexedDB");
    let listing = js_sys::Function::from(get(&factory, "databases")).call0(&factory).unwrap();
    let all = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(listing)).await.unwrap();
    js_sys::Array::from(&all).iter().filter_map(|db| get(&db, "name").as_string()).collect()
}

const NOWHERE: &str = "ws://127.0.0.1:9/ws";

/// A driver on a server that isn't there, with an account in its cache.
async fn offline_driver(db: &str, secrets: &Secrets) -> (WebDriver, UnboundedReceiver<Update>) {
    IdbCache::delete(db).await.unwrap();
    let mut cache = IdbCache::open(db, secrets).await.unwrap();
    let account = Account {
        username: "abin".into(),
        number: number(111_111_111),
        display_name: "Abin".into(),
        pending_until: None,
    };
    cache.set_account(account).unwrap();
    cache.flush().await.unwrap();
    let secrets = Secrets::from_bytes(&secrets.to_bytes()[..]).unwrap();
    WebDriver::start(NOWHERE.into(), secrets, Zeroizing::new([5; 32]), "abin".into(), device(), cache, "0.1.1".into())
}

#[wasm_bindgen_test]
async fn a_message_arrives() {
    let bench = bench();
    let (a_number, b_number) = (bench.accounts[0].number, bench.accounts[1].number);
    let (a, mut a_updates) = start(&session(0).await, &bench.url, "t-driver-a").await;
    let (b, mut b_updates) = start(&session(1).await, &bench.url, "t-driver-b").await;
    assert_eq!(next_conn(&mut a_updates).await, ConnState::Online);
    assert_eq!(next_conn(&mut b_updates).await, ConnState::Online);

    let added = a.call(Command::Add { number: b_number }).await;
    assert!(matches!(added, Ok(Reply::Done)), "{added:?}");
    // A's first message asks B, who accepts.
    let asked = a.call(Command::SendText { to: b_number, text: "hello".into() }).await;
    assert!(matches!(asked, Ok(Reply::Sent(_))), "{asked:?}");
    until(&mut b_updates, |u| matches!(u, Update::Message { .. })).await;
    let accepted = b.call(Command::Accept { number: a_number }).await;
    assert!(matches!(accepted, Ok(Reply::Done)), "{accepted:?}");

    let text = "hi from the browser";
    let sent = a.call(Command::SendText { to: b_number, text: text.into() }).await;
    assert!(matches!(sent, Ok(Reply::Sent(_))), "{sent:?}");
    let Update::Message { chat_id, .. } = until(&mut b_updates, |u| matches!(u, Update::Message { .. })).await else {
        unreachable!("only a message matches")
    };
    let Ok(Reply::Messages(messages)) = b.call(Command::Messages { chat_id, before: None, limit: 10 }).await else {
        panic!("B's messages show");
    };
    assert!(messages.iter().any(|m| m.text == Ok(text.to_string())), "B has it");

    drop((a, b));
    IdbCache::delete("t-driver-a").await.unwrap();
    IdbCache::delete("t-driver-b").await.unwrap();
}

#[wasm_bindgen_test]
async fn commands_answer_offline_from_the_cache() {
    let secrets = Secrets::generate();
    let (driver, mut updates) = offline_driver("t-driver-offline", &secrets).await;
    assert!(matches!(next_conn(&mut updates).await, ConnState::Reconnecting { .. }));

    let to = number(222_222_222);
    let Ok(Reply::Sent(msg_id)) = driver.call(Command::SendText { to, text: "written offline".into() }).await else {
        panic!("the message should queue");
    };
    let Ok(Reply::Chats(rows)) = driver.call(Command::Chats).await else { panic!("the chats should show") };
    let last = rows.iter().find(|r| r.with == to).and_then(|r| r.last.clone()).expect("a row for them");
    assert_eq!((last.msg_id, last.mark), (msg_id, Some(Mark::Waiting)));
    let invited = driver.call(Command::Invite { number: to }).await;
    assert!(matches!(invited, Err(Error::Offline)), "{invited:?}");

    // The queued message was stored before its reply came.
    let cache = IdbCache::open("t-driver-offline", &secrets).await.unwrap();
    assert!(cache.outbox().unwrap().iter().any(|o| o.msg_id == msg_id));
    drop((driver, cache));
    IdbCache::delete("t-driver-offline").await.unwrap();
}

#[wasm_bindgen_test]
async fn wake_connects_without_waiting() {
    let (driver, mut updates) = offline_driver("t-driver-wake", &Secrets::generate()).await;
    // After 1 and 2 seconds of waiting.
    while next_conn(&mut updates).await != (ConnState::Reconnecting { in_secs: 4 }) {}
    let woken = now_ms();
    driver.wake();
    assert_eq!(next_conn(&mut updates).await, ConnState::Reconnecting { in_secs: 8 });
    let took = now_ms() - woken;
    assert!(took < 1_000, "it tried again {took} ms after the wake");
    drop(driver);
    IdbCache::delete("t-driver-wake").await.unwrap();
}

#[wasm_bindgen_test]
async fn signed_out_wipes_the_cache() {
    let bench = bench();
    // A device of bob's made for this test alone, signed out by the shared one.
    let first = new_session(1).await;
    let (driver, mut updates) = start(&first, &bench.url, "t-driver-first").await;
    let (other, mut other_updates) = start(&session(1).await, &bench.url, "t-driver-other").await;
    assert_eq!(next_conn(&mut updates).await, ConnState::Online);
    assert_eq!(next_conn(&mut other_updates).await, ConnState::Online);
    assert!(matches!(driver.call(Command::Account).await, Ok(Reply::Account(_))), "the cache has the account");
    assert!(databases().await.contains(&"t-driver-first".to_string()));

    let out = other.call(Command::SignOutDevice { id: first.device.id.0 }).await;
    assert!(matches!(out, Ok(Reply::Devices(_))), "{out:?}");
    until(&mut updates, |u| *u == Update::SignedOut(SignedOutReason::ByAnotherDevice)).await;
    // It stopped for good.
    assert!(matches!(driver.call(Command::Chats).await, Err(Error::Stopped)));
    assert!(updates.next().await.is_none());
    assert!(!databases().await.contains(&"t-driver-first".to_string()), "its database is gone");

    let cache = IdbCache::open("t-driver-first", &first.secrets()).await.unwrap();
    assert_empty(&cache);
    assert_eq!(cache.account().unwrap(), None);
    drop((other, cache));
    IdbCache::delete("t-driver-first").await.unwrap();
    IdbCache::delete("t-driver-other").await.unwrap();
}
