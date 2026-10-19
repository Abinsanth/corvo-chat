//! The core in a real browser, against the test bench (`examples/test_server`),
//! which must be running. `CORVO_TEST_BENCH` names the file it wrote.
#![cfg(all(target_arch = "wasm32", feature = "web"))]

use corvo_chat_core::web::{WebTransport, connect};
use corvo_chat_core::{Strength, TransportError, hello, sign_in};
use corvo_chat_proto::{
    B64, Number,
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
