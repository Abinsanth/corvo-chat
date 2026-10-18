mod common;

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{TestClient, stranger, wait};
use corvo_chat_core::{Cache, Error, Secrets, Update, parse_number, safety_code, view};
use corvo_chat_proto::{Number, chat::Relation, frames::ErrorCode};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// Reads updates until one matches, failing after five seconds.
async fn until(client: &mut TestClient, wanted: impl Fn(&Update) -> bool) -> Update {
    let wait = async {
        loop {
            for update in client.next().await.unwrap() {
                if wanted(&update) {
                    return update;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait).await.expect("the update arrives in time")
}

fn relation(client: &TestClient, number: Number) -> Option<Relation> {
    client.cache().person(number).unwrap().map(|p| p.person.relation)
}

/// `a` asks `b` with a first message, and `b` has the request.
async fn ask(a: &mut TestClient, b: &mut TestClient) {
    let (a_number, b_number) = (a.account().number, b.account().number);
    a.send_text(b_number, "hello", now_ms()).await.unwrap();
    wait(b, |b| relation(b, a_number) == Some(Relation::Request)).await;
    wait(a, |a| a.cache().outbox().unwrap().is_empty()).await;
}

/// Two synced strangers.
async fn pair(server: &common::TestServer) -> (TestClient, TestClient) {
    let (mut a, mut b) = (stranger(server, "alice").await, stranger(server, "bob").await);
    a.sync().await.unwrap();
    b.sync().await.unwrap();
    (a, b)
}

#[tokio::test]
async fn add_pins_keys_and_accept_makes_contacts() {
    let server = common::start().await;
    let (mut a, mut b) = pair(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);

    a.add(b_number).await.unwrap();
    let keys = a.cache().pin(b_number).unwrap().expect("their keys are pinned");
    assert_eq!(keys, b.secrets().public_keys());
    assert_eq!(relation(&a, b_number), Some(Relation::None));

    ask(&mut a, &mut b).await;
    b.accept(a_number).await.unwrap();
    until(&mut b, |u| *u == Update::Presence { number: a_number, online: true }).await;
    until(&mut a, |u| *u == Update::Presence { number: b_number, online: true }).await;
    wait(&mut a, |a| relation(a, b_number) == Some(Relation::Contact)).await;
    assert_eq!(relation(&b, a_number), Some(Relation::Contact));
    assert!(a.online().contains(&b_number) && b.online().contains(&a_number));
}

#[test]
fn a_mistyped_number_is_a_typo() {
    // Of the ten last digits, exactly one makes a number the check accepts.
    let prefix = "482910557";
    let good: Vec<u8> = (0..10).filter(|d| parse_number(&format!("{prefix}{d}")).is_ok()).collect();
    assert_eq!(good.len(), 1);
    let wrong = (good[0] + 1) % 10;
    assert!(matches!(parse_number(&format!("{prefix}{wrong}")), Err(Error::NumberTypo)));
    // Spaces and dashes are fine, as in the proto.
    assert!(parse_number(&format!("482 910-557{}", good[0])).is_ok());
    assert!(matches!(parse_number("12"), Err(Error::BadNumber)));
}

#[tokio::test]
async fn reject_and_block_hide_the_request() {
    let server = common::start().await;
    let (mut a, mut b) = pair(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);

    ask(&mut a, &mut b).await;
    b.reject(a_number).await.unwrap();
    let rows = view::chats(b.cache(), b.secrets(), &HashSet::new()).unwrap();
    assert!(rows.is_empty(), "a rejected request is gone");
    assert_eq!(relation(&b, a_number), Some(Relation::None));

    // Their next message is a new request, which a block hides.
    a.send_text(b_number, "again", now_ms()).await.unwrap();
    wait(&mut b, |b| relation(b, a_number) == Some(Relation::Request)).await;
    b.block(a_number).await.unwrap();
    assert_eq!(relation(&b, a_number), Some(Relation::Blocked));
    b.unblock(a_number).await.unwrap();
    assert_eq!(relation(&b, a_number), Some(Relation::None));
}

#[tokio::test]
async fn remove_ends_the_contact() {
    let server = common::start().await;
    let (mut a, mut b) = common::contacts(&server).await;
    let b_number = b.account().number;
    a.sync().await.unwrap();
    b.sync().await.unwrap();
    a.remove(b_number, true).await.unwrap();
    assert_eq!(relation(&a, b_number), Some(Relation::None));
}

#[tokio::test]
async fn swapped_keys_raise_a_mismatch_and_block_sending() {
    let server = common::start().await;
    let (mut a, b) = common::contacts(&server).await;
    let b_number = b.account().number;
    a.sync().await.unwrap();
    a.add(b_number).await.unwrap();
    // The pin has to reach the server for a new device to see it.
    wait(&mut a, |a| a.cache().unsent_pins().unwrap().is_empty()).await;

    let fresh = Secrets::generate().public_keys();
    server
        .state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE accounts SET signing_key = ?, exchange_key = ?, key_signature = ? WHERE number = ?",
                (fresh.signing.0.to_vec(), fresh.exchange.0.to_vec(), fresh.signature.0.to_vec(), b_number.digits()),
            )
            .unwrap();
        })
        .await;
    let count = |server: &common::TestServer| {
        let db = server.state.db.clone();
        async move { db.call(|c| c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0)).unwrap()).await }
    };
    let before = count(&server).await;

    let mut a2 = common::new_device(&server, &a.account().username).await;
    a2.sync().await.unwrap();
    let update = until(&mut a2, |u| matches!(u, Update::KeyMismatch { .. })).await;
    assert_eq!(update, Update::KeyMismatch { number: b_number });
    assert!(a2.cache().person(b_number).unwrap().unwrap().mismatch);
    assert!(matches!(a2.send_text(b_number, "hi", now_ms()).await, Err(Error::KeyMismatch)));
    assert!(matches!(a2.add(b_number).await, Err(Error::KeyMismatch)));
    assert_eq!(count(&server).await, before);
}

#[tokio::test]
async fn verify_codes_match_on_both_sides() {
    let server = common::start().await;
    let (mut a, mut b) = pair(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);
    // No pin yet: nothing to check against.
    assert!(matches!(a.verify(b_number), Err(Error::NotPinned)));
    a.add(b_number).await.unwrap();
    b.add(a_number).await.unwrap();
    let (code_a, code_b) = (a.verify(b_number).unwrap(), b.verify(a_number).unwrap());
    assert_eq!(code_a, code_b);
    assert_eq!(code_a, safety_code(&a.secrets().public_keys(), &b.secrets().public_keys()));
    assert_eq!(code_a.split(' ').count(), 12);
}

#[tokio::test]
async fn report_with_block_blocks_them() {
    let server = common::start().await;
    let (mut a, mut b) = pair(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);
    ask(&mut a, &mut b).await;
    b.report(a_number, Some("spam"), true).await.unwrap();
    wait(&mut b, |b| relation(b, a_number) == Some(Relation::Blocked)).await;

    // A report with a reason that's too long is the server's refusal.
    let long = "x".repeat(201);
    match a.report(b_number, Some(&long), false).await {
        Err(Error::Server { code: ErrorCode::BadFrame, .. }) => {}
        other => panic!("expected bad_frame, got {other:?}"),
    }
}
