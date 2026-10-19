//! The core in a real browser, against the test bench (`examples/test_server`),
//! which must be running. `CORVO_TEST_BENCH` names the file it wrote.
#![cfg(all(target_arch = "wasm32", feature = "web"))]

use corvo_chat_core::web::{IdbCache, WebTransport, connect};
use corvo_chat_core::{
    Cache, CachedPerson, Error, MsgId, OutState, Outgoing, Secrets, Strength, TransportError, exercise_cache, hello,
    sign_in,
};
use corvo_chat_proto::{
    B64, Body, Number,
    chat::{Message, Person, Relation},
    frames::{Device, DeviceKind},
};
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

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

    match IdbCache::open("t-secrets", &Secrets::generate()).await {
        Err(Error::Cache(_)) => {}
        Ok(other) => assert_empty(&other),
        Err(e) => panic!("{e:?}"),
    }
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
