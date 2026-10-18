mod common;

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{TestClient, TestServer};
use corvo_chat_core::{Cache, Error, Mark, MsgId, OutState, Outgoing, ShownMessage, chat_key, encrypt_message, view};
use corvo_chat_proto::{
    Number,
    chat::Relation,
    frames::{ClientFrame, ErrorCode},
};
use corvo_chat_server::{Timeouts, accounts, limits::Limits};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// An account the admin activated, with no contacts.
async fn stranger(server: &TestServer, base: &str) -> TestClient {
    let client = common::pending(server, base).await;
    common::activate(server, client.account().number);
    client
}

/// Reads frames until `done` holds, failing after five seconds.
async fn wait(client: &mut TestClient, done: impl Fn(&TestClient) -> bool) {
    let run = async {
        while !done(client) {
            client.next().await.unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(5), run).await.expect("it happens in time");
}

fn chat_with(client: &TestClient, number: Number) -> Option<[u8; 16]> {
    client.cache().chat_with(number).unwrap().map(|c| c.info.chat_id.0)
}

fn shown(client: &TestClient, chat_id: [u8; 16]) -> Vec<ShownMessage> {
    view::messages(client.cache(), client.secrets(), chat_id, None, 50).unwrap()
}

/// The mark of my message `msg_id` in the chat with `number`.
fn mark(client: &TestClient, number: Number, msg_id: MsgId) -> Option<Mark> {
    let chat_id = chat_with(client, number).expect("the chat is cached");
    shown(client, chat_id).into_iter().find(|m| m.msg_id == msg_id).expect("the message shows").mark
}

fn texts(client: &TestClient, chat_id: [u8; 16]) -> Vec<Result<String, String>> {
    shown(client, chat_id).into_iter().map(|m| m.text).collect()
}

#[tokio::test]
async fn first_message_makes_a_request_and_marks_follow_accept() {
    let server = common::start().await;
    let (mut a, mut b) = (stranger(&server, "alice").await, stranger(&server, "bob").await);
    let (a_number, b_number) = (a.account().number, b.account().number);
    a.sync().await.unwrap();
    b.sync().await.unwrap();

    let msg_id = a.send_text(b_number, "hello", now_ms()).await.unwrap();
    wait(&mut a, |a| a.cache().outbox().unwrap().is_empty() && chat_with(a, b_number).is_some()).await;
    assert_eq!(mark(&a, b_number, msg_id), Some(Mark::Sent));

    // B sees a request with one unread message.
    wait(&mut b, |b| chat_with(b, a_number).is_some_and(|id| !shown(b, id).is_empty())).await;
    let rows = view::chats(b.cache(), b.secrets(), &HashSet::new()).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!((row.with, row.relation, row.unread), (a_number, Relation::Request, 1));
    let last = row.last.as_ref().expect("the message shows");
    assert_eq!((last.text.clone(), last.mine, last.mark.clone()), (Ok("hello".into()), false, None));

    // B reads it: the count clears, but A sees nothing until B accepts.
    let chat_id = row.chat_id;
    b.mark_read(chat_id).await.unwrap();
    let rows = view::chats(b.cache(), b.secrets(), &HashSet::new()).unwrap();
    assert_eq!(rows[0].unread, 0);
    // A round trip on A's side, so anything the read caused has arrived.
    a.invites().await.unwrap();
    assert_eq!(mark(&a, b_number, msg_id), Some(Mark::Sent));

    b.request(ClientFrame::Accept { number: a_number }).await.unwrap();
    wait(&mut a, |a| mark(a, b_number, msg_id) == Some(Mark::Read)).await;
    let rows = view::chats(a.cache(), a.secrets(), &HashSet::new()).unwrap();
    assert_eq!((rows[0].relation, rows[0].unread), (Relation::Contact, 0));
}

#[tokio::test]
async fn refused_message_shows_the_servers_reason() {
    let server = common::start().await;
    let (mut a, b) = common::contacts(&server).await;
    let b_number = b.account().number;
    a.sync().await.unwrap();
    let now = server.state.clock.now();
    server
        .state
        .db
        .call(move |c| {
            let id = accounts::by_number(c, b_number, now).unwrap().unwrap().id;
            accounts::ban(c, id, now).unwrap();
        })
        .await;

    let msg_id = a.send_text(b_number, "hello", now_ms()).await.unwrap();
    let entry = a.cache().outbox().unwrap().into_iter().find(|o| o.msg_id == msg_id).expect("it stays in the outbox");
    let message = shown(&a, entry.chat_id).into_iter().find(|m| m.msg_id == msg_id).unwrap();
    assert_eq!(message.mark, Some(Mark::Refused("This account has been banned.".into())));
    assert_eq!(message.text, Ok("hello".into()));
    // The chat never started, but it still shows in the list.
    let rows = view::chats(a.cache(), a.secrets(), &HashSet::new()).unwrap();
    assert_eq!(rows.iter().map(|r| r.with).collect::<Vec<_>>(), [b_number]);
}

#[tokio::test]
async fn tampered_message_shows_a_warning_not_a_crash() {
    let server = common::start().await;
    let (mut a, b) = common::contacts(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);
    a.sync().await.unwrap();
    a.send_text(b_number, "one", now_ms()).await.unwrap();
    wait(&mut a, |a| chat_with(a, b_number).is_some()).await;
    a.send_text(b_number, "two", now_ms()).await.unwrap();
    wait(&mut a, |a| a.cache().outbox().unwrap().is_empty()).await;

    server
        .state
        .db
        .call(|c| {
            let mut body: Vec<u8> = c.query_row("SELECT body FROM messages WHERE seq = 1", [], |r| r.get(0)).unwrap();
            body[30] ^= 1;
            c.execute("UPDATE messages SET body = ?1 WHERE seq = 1", [body]).unwrap();
        })
        .await;

    let mut fresh = common::new_device(&server, &b.account().username).await;
    fresh.sync().await.unwrap();
    let chat_id = chat_with(&fresh, a_number).unwrap();
    let warning = format!("⚠ couldn't decrypt a message from {a_number}");
    assert_eq!(texts(&fresh, chat_id), [Err(warning), Ok("two".into())]);
}

#[tokio::test]
async fn both_start_a_chat_at_once_and_one_message_moves_over() {
    let server = common::start().await;
    let (mut a, mut b) = common::contacts(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);
    a.sync().await.unwrap();
    b.sync().await.unwrap();

    // B sends before reading anything, so it doesn't know of A's chat yet.
    a.send_text(b_number, "from a", now_ms()).await.unwrap();
    assert!(chat_with(&b, a_number).is_none());
    let moved = b.send_text(a_number, "from b", now_ms()).await.unwrap();
    let outbox = b.cache().outbox().unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!((outbox[0].msg_id, &outbox[0].state), (moved, &OutState::NeedsChat));
    let lost = outbox[0].chat_id;

    wait(&mut b, |b| b.cache().outbox().unwrap().is_empty()).await;
    let chat_id = chat_with(&b, a_number).unwrap();
    assert_ne!(chat_id, lost);
    assert_eq!(texts(&b, chat_id), [Ok("from a".into()), Ok("from b".into())]);
    assert_eq!(shown(&b, chat_id)[1].msg_id, moved);

    wait(&mut a, |a| chat_with(a, b_number).is_some_and(|id| shown(a, id).len() == 2)).await;
    assert_eq!(chat_with(&a, b_number), Some(chat_id));
    assert_eq!(texts(&a, chat_id), [Ok("from a".into()), Ok("from b".into())]);
}

#[tokio::test]
async fn strangers_starting_at_once_deliver_after_accept() {
    let server = common::start().await;
    let (mut a, mut b) = (stranger(&server, "alice").await, stranger(&server, "bob").await);
    let (a_number, b_number) = (a.account().number, b.account().number);
    a.sync().await.unwrap();
    b.sync().await.unwrap();

    // B has A's keys already, so its send asks nothing first, and doesn't
    // read A's chat before it goes.
    b.request(ClientFrame::Lookup { number: a_number }).await.unwrap();
    assert!(b.cache().pin(a_number).unwrap().is_some());
    a.send_text(b_number, "from a", now_ms()).await.unwrap();
    let moved = b.send_text(a_number, "from b", now_ms()).await.unwrap();
    assert_eq!(b.cache().outbox().unwrap()[0].state, OutState::NeedsChat);

    // A's message made B a request: the move waits for B's accept, rather
    // than being refused.
    wait(&mut b, |b| {
        chat_with(b, a_number).is_some()
            && b.cache().person(a_number).unwrap().is_some_and(|p| p.person.relation == Relation::Request)
    })
    .await;
    b.invites().await.unwrap();
    b.next().await.unwrap();
    assert_eq!(b.cache().outbox().unwrap()[0].state, OutState::NeedsChat);

    b.request(ClientFrame::Accept { number: a_number }).await.unwrap();
    wait(&mut b, |b| b.cache().outbox().unwrap().is_empty()).await;
    let chat_id = chat_with(&b, a_number).unwrap();
    assert_eq!(texts(&b, chat_id), [Ok("from a".into()), Ok("from b".into())]);
    assert_eq!(shown(&b, chat_id)[1].msg_id, moved);
    wait(&mut a, |a| chat_with(a, b_number).is_some_and(|id| shown(a, id).len() == 2)).await;
    assert_eq!(texts(&a, chat_id), [Ok("from a".into()), Ok("from b".into())]);
}

/// Rows of a server table, counted.
async fn count(server: &TestServer, sql: &'static str) -> i64 {
    server.state.db.call(move |c| c.query_row(sql, [], |r| r.get(0)).unwrap()).await
}

#[tokio::test]
async fn a_message_written_before_sync_waits_for_it() {
    let server = common::start().await;
    let (mut a, mut b) = (stranger(&server, "alice").await, stranger(&server, "bob").await);
    let (a_number, b_number) = (a.account().number, b.account().number);

    let msg_id = a.send_text(b_number, "early", now_ms()).await.unwrap();
    let outbox = a.cache().outbox().unwrap();
    assert_eq!((outbox[0].msg_id, &outbox[0].state, &outbox[0].body), (msg_id, &OutState::Waiting, &None));
    // Nothing went out: no lookup, no chat, no message.
    assert_eq!(count(&server, "SELECT COUNT(*) FROM attempts WHERE kind = 'lookup'").await, 0);
    assert_eq!(count(&server, "SELECT COUNT(*) FROM messages").await, 0);
    assert!(a.cache().pin(b_number).unwrap().is_none());
    let rows = view::chats(a.cache(), a.secrets(), &HashSet::new()).unwrap();
    assert_eq!(rows[0].last.as_ref().unwrap().mark, Some(Mark::Waiting));

    // Synced: looked up, pinned, sealed and sent.
    a.sync().await.unwrap();
    wait(&mut a, |a| a.cache().outbox().unwrap().is_empty()).await;
    assert!(a.cache().pin(b_number).unwrap().is_some());
    b.sync().await.unwrap();
    let chat_id = chat_with(&b, a_number).unwrap();
    assert_eq!(texts(&b, chat_id), [Ok("early".into())]);
}

#[tokio::test]
async fn a_cleared_message_leaves_the_outbox() {
    let server = common::start().await;
    let (mut a, b) = common::contacts(&server).await;
    let b_number = b.account().number;
    let username = a.account().username.clone();
    a.sync().await.unwrap();
    a.send_text(b_number, "one", now_ms()).await.unwrap();
    // Gone before its echo came back.
    assert!(matches!(a.cache().outbox().unwrap()[0].state, OutState::Sent { .. }));
    let cache = a.into_cache();

    let mut other = common::new_device(&server, &username).await;
    other.request(ClientFrame::Remove { number: b_number, clear_history: true }).await.unwrap();

    let mut a = common::new_device_with(&server, &username, cache).await;
    a.sync().await.unwrap();
    a.next().await.unwrap();
    assert!(a.cache().outbox().unwrap().is_empty());
    let chat_id = chat_with(&a, b_number).unwrap();
    assert!(shown(&a, chat_id).is_empty());
}

#[tokio::test]
async fn a_sent_message_the_server_lost_is_sent_again() {
    let server = common::start().await;
    let (mut a, mut b) = common::contacts(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);
    let username = a.account().username.clone();
    a.sync().await.unwrap();
    a.send_text(b_number, "one", now_ms()).await.unwrap();
    wait(&mut a, |a| chat_with(a, b_number).is_some() && a.cache().outbox().unwrap().is_empty()).await;

    // As after a server restore: the outbox says sent, the server never had it.
    let chat = a.cache().chat_with(b_number).unwrap().unwrap();
    let pin = a.cache().pin(b_number).unwrap().unwrap();
    let key = chat_key(&chat.info, a_number, &a.secrets().public_keys(), &pin, a.secrets()).unwrap();
    let (chat_id, msg_id) = (chat.info.chat_id.0, MsgId::new(now_ms()));
    let body = encrypt_message(&key, &chat_id, a_number, &msg_id, now_ms(), "lost").unwrap();
    let mut cache = a.into_cache();
    cache
        .put_outgoing(Outgoing {
            chat_id,
            msg_id,
            to: b_number,
            text: "lost".into(),
            body: Some(body),
            new_chat: None,
            created_ms: now_ms(),
            state: OutState::Sent { seq: 99 },
        })
        .unwrap();

    let mut a = common::new_device_with(&server, &username, cache).await;
    a.sync().await.unwrap();
    wait(&mut a, |a| a.cache().outbox().unwrap().is_empty()).await;
    let delivered = shown(&a, chat_id).into_iter().find(|m| m.msg_id == msg_id).unwrap();
    assert_eq!(delivered.seq, Some(2));
    b.sync().await.unwrap();
    assert_eq!(texts(&b, chat_id), [Ok("one".into()), Ok("lost".into())]);
}

#[tokio::test]
async fn a_refused_ack_is_an_error() {
    let server = common::start().await;
    let (mut a, _b) = common::contacts(&server).await;
    a.sync().await.unwrap();
    let unknown = ClientFrame::Received { chat_id: corvo_chat_proto::B64([7; 16]), through: 1 };
    let refused = a.request(unknown).await;
    assert!(matches!(refused, Err(Error::Server { code: ErrorCode::ChatUnknown, .. })), "{refused:?}");
}

#[tokio::test]
async fn older_pages_until_complete() {
    let limits = Limits { signups_per_ip_day: 1_000, messages_per_minute: 1_000, ..Limits::default() };
    let server = common::start_with(limits, Timeouts::default()).await;
    let (mut a, b) = common::contacts(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);
    a.sync().await.unwrap();
    a.send_text(b_number, "0", now_ms()).await.unwrap();
    wait(&mut a, |a| chat_with(a, b_number).is_some()).await;
    for i in 1..120 {
        a.send_text(b_number, &i.to_string(), now_ms()).await.unwrap();
    }

    let mut fresh = common::new_device(&server, &b.account().username).await;
    fresh.sync().await.unwrap();
    let chat_id = chat_with(&fresh, a_number).unwrap();
    let count = |c: &TestClient| c.cache().messages(chat_id, i64::MAX, 1_000).unwrap().len();
    assert_eq!(count(&fresh), 50);
    assert!(!fresh.older(chat_id).await.unwrap());
    assert_eq!(count(&fresh), 100);
    assert!(fresh.older(chat_id).await.unwrap());
    assert_eq!(count(&fresh), 120);
    // Nothing older: no more asking.
    assert!(fresh.older(chat_id).await.unwrap());
    let oldest = view::messages(fresh.cache(), fresh.secrets(), chat_id, Some(3), 50).unwrap();
    assert_eq!(oldest.iter().map(|m| m.text.clone()).collect::<Vec<_>>(), [Ok("0".into()), Ok("1".into())]);
}

#[tokio::test]
async fn a_mismatched_person_gets_nothing() {
    let server = common::start().await;
    let (mut a, b) = common::contacts(&server).await;
    let b_number = b.account().number;
    a.sync().await.unwrap();
    let username = a.account().username.clone();
    // As if B's keys changed since they were pinned.
    let mut cache = a.into_cache();
    let mut person = cache.person(b_number).unwrap().unwrap();
    person.mismatch = true;
    cache.put_person(person).unwrap();
    // And a message written before they changed, not sealed yet.
    let waiting = Outgoing {
        chat_id: [7; 16],
        msg_id: MsgId::new(now_ms()),
        to: b_number,
        text: "earlier".into(),
        body: None,
        new_chat: None,
        created_ms: now_ms(),
        state: OutState::Waiting,
    };
    cache.put_outgoing(waiting.clone()).unwrap();
    let mut a = common::new_device_with(&server, &username, cache).await;
    a.sync().await.unwrap();

    let why = "Their keys changed. Check with /verify.";
    let refused = OutState::Refused { code: ErrorCode::NotAllowed, message: why.into() };
    assert_eq!(a.cache().outbox().unwrap(), [Outgoing { state: refused, ..waiting }]);
    let refused = a.send_text(b_number, "hello", now_ms()).await;
    assert!(matches!(refused, Err(Error::KeyMismatch)), "{refused:?}");
    assert_eq!(a.cache().outbox().unwrap().len(), 1);
    assert_eq!(Error::KeyMismatch.to_string(), why);
    assert_eq!(count(&server, "SELECT COUNT(*) FROM messages").await, 0);
}
