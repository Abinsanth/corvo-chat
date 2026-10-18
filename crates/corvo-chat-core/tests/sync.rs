mod common;

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestClient;
use corvo_chat_core::{
    Cache, ChatKey, Client, MemoryCache, MsgId, Secrets, SignedIn, Transport, TransportError, Update, chat_key,
    decrypt_message, encrypt_message, new_chat, seal_pin,
};
use corvo_chat_proto::{
    B64, Number, PublicKeys,
    chat::{Event, EventBody, Person, Pin, Relation},
    frames::{Account, ClientFrame, ServerFrame},
};
use rand_core::{OsRng, RngCore};
use zeroize::Zeroizing;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// A chat one side is sending into: its id and key, and whether the server
/// has it yet.
struct RawChat {
    id: [u8; 16],
    key: ChatKey,
    started: bool,
}

impl RawChat {
    fn new(from: &TestClient, to: Number, their: &PublicKeys) -> (RawChat, corvo_chat_proto::chat::NewChat) {
        let mut id = [0u8; 16];
        OsRng.fill_bytes(&mut id);
        let (key, new) = new_chat(from.secrets(), from.account().number, &id, to, their);
        (RawChat { id, key, started: false }, new)
    }
}

/// Sends one text the raw way (real sending comes later) and gives its seq.
async fn send_raw(from: &mut TestClient, chat: &mut RawChat, new: &corvo_chat_proto::chat::NewChat, text: &str) -> i64 {
    let msg_id = MsgId::new(now_ms());
    let body = encrypt_message(&chat.key, &chat.id, from.account().number, &msg_id, now_ms(), text).unwrap();
    let new_chat = (!chat.started).then(|| new.clone());
    let frame = ClientFrame::Send { chat_id: B64(chat.id), msg_id: B64(msg_id.0), body, new_chat };
    match from.request(frame).await.unwrap() {
        ServerFrame::Sent { seq, .. } => {
            chat.started = true;
            seq
        }
        other => panic!("expected sent, got {other:?}"),
    }
}

/// Reads updates until one matches, failing after five seconds.
async fn until(client: &mut TestClient, wanted: impl Fn(&Update, &TestClient) -> bool) -> Update {
    let wait = async {
        loop {
            for update in client.next().await.unwrap() {
                if wanted(&update, client) {
                    return update;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait).await.expect("the update arrives in time")
}

#[tokio::test]
async fn new_device_gets_a_snapshot_and_history() {
    let server = common::start().await;
    let (mut a, b) = common::contacts(&server).await;
    let (a_number, b_number) = (a.account().number, b.account().number);
    let (a_keys, b_keys) = (a.secrets().public_keys(), b.secrets().public_keys());

    let (mut chat, new) = RawChat::new(&a, b_number, &b_keys);
    for text in ["one", "two", "three"] {
        send_raw(&mut a, &mut chat, &new, text).await;
    }

    let mut fresh = common::new_device(&server, &b.account().username).await;
    // Known from the start, so its own messages are never acked.
    assert_eq!(fresh.cache().account().unwrap().unwrap().number, b_number);
    fresh.sync().await.unwrap();
    let cache = fresh.cache();
    assert_eq!(cache.account().unwrap().unwrap().number, b_number);
    assert!(cache.cursor().unwrap().is_some());

    let alice = cache.person(a_number).unwrap().expect("the contact is known");
    assert_eq!(alice.person.public_keys, a_keys);
    assert!(!alice.mismatch);
    // Seen for the first time, so pinned.
    assert_eq!(cache.pin(a_number).unwrap(), Some(a_keys.clone()));

    let cached = cache.chat(chat.id).unwrap().expect("the chat is known");
    assert!(cached.complete);
    assert_eq!(cache.chat_with(a_number).unwrap().unwrap().info.chat_id.0, chat.id);

    let messages = cache.messages(chat.id, i64::MAX, 50).unwrap();
    assert_eq!(messages.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2, 3]);
    // The cache keeps the ciphertext; it opens with the chat's key.
    let key = chat_key(&cached.info, b_number, &b_keys, &a_keys, fresh.secrets()).unwrap();
    let texts: Vec<String> = messages.iter().map(|m| decrypt_message(&key, m).unwrap().text).collect();
    assert_eq!(texts, ["one", "two", "three"]);

    // History asked for is the reply, not a push.
    let asked = ClientFrame::History { chat_id: B64(chat.id), before: 3, limit: 1 };
    match fresh.request(asked).await.unwrap() {
        ServerFrame::History { messages, complete, .. } => {
            assert_eq!(messages.iter().map(|m| m.seq).collect::<Vec<_>>(), [2]);
            assert!(!complete);
        }
        other => panic!("expected history, got {other:?}"),
    }

    // The pin went to the server after `synced`; its echo confirms it, and
    // the next new device finds it in the snapshot.
    until(&mut fresh, |_, c| c.cache().unsent_pins().unwrap().is_empty()).await;
    let mut third = common::new_device(&server, &b.account().username).await;
    third.sync().await.unwrap();
    assert_eq!(third.cache().pin(a_number).unwrap(), Some(a_keys));
    assert!(third.cache().unsent_pins().unwrap().is_empty());
}

#[tokio::test]
async fn answered_replies_are_kept_too() {
    let server = common::start().await;
    let (mut a, mut b) = common::contacts(&server).await;
    let b_keys = b.secrets().public_keys();
    let (mut chat, new) = RawChat::new(&a, b.account().number, &b_keys);
    for text in ["one", "two", "three"] {
        send_raw(&mut a, &mut chat, &new, text).await;
    }
    // The `sent` replies went through the cache's outbox handling too.
    let sent = a.next().await.unwrap();
    assert_eq!(sent.iter().filter(|u| matches!(u, Update::Sent { .. })).count(), 3);

    // History asked for is stored, not only returned.
    let asked = ClientFrame::History { chat_id: B64(chat.id), before: 4, limit: 2 };
    assert!(matches!(b.request(asked).await.unwrap(), ServerFrame::History { .. }));
    let seqs: Vec<i64> = b.cache().messages(chat.id, i64::MAX, 50).unwrap().iter().map(|m| m.seq).collect();
    assert_eq!(seqs, [2, 3]);
}

#[tokio::test]
async fn a_reconnect_syncs_only_what_is_new() {
    let server = common::start().await;
    let (mut a, b) = common::contacts(&server).await;
    let username = b.account().username.clone();
    let b_keys = b.secrets().public_keys();
    let (mut chat, new) = RawChat::new(&a, b.account().number, &b_keys);
    send_raw(&mut a, &mut chat, &new, "one").await;
    send_raw(&mut a, &mut chat, &new, "two").await;

    let mut first = common::new_device(&server, &username).await;
    first.sync().await.unwrap();
    let cache = first.into_cache();
    let cursor = cache.cursor().unwrap();
    let seq = send_raw(&mut a, &mut chat, &new, "three").await;

    let mut again = common::new_device_with(&server, &username, cache).await;
    again.sync().await.unwrap();
    let updates = again.next().await.unwrap();
    // From the cursor: no snapshot (which would start with the profile),
    // just the new message.
    assert!(!updates.contains(&Update::Profile), "{updates:?}");
    let messages: Vec<&Update> = updates.iter().filter(|u| matches!(u, Update::Message { .. })).collect();
    assert_eq!(messages, [&Update::Message { chat_id: chat.id, seq }]);
    assert!(again.cache().cursor().unwrap() > cursor);
    let seqs: Vec<i64> = again.cache().messages(chat.id, i64::MAX, 50).unwrap().iter().map(|m| m.seq).collect();
    assert_eq!(seqs, [1, 2, 3]);
}

#[tokio::test]
async fn live_message_arrives_as_an_update_and_is_acked() {
    let server = common::start().await;
    let (mut a, mut b) = common::contacts(&server).await;
    a.sync().await.unwrap();
    b.sync().await.unwrap();
    let b_keys = b.secrets().public_keys();

    let (mut chat, new) = RawChat::new(&a, b.account().number, &b_keys);
    let seq = send_raw(&mut a, &mut chat, &new, "hello").await;

    let id = chat.id;
    let got = until(&mut b, |u, _| matches!(u, Update::Message { chat_id, .. } if *chat_id == id)).await;
    assert_eq!(got, Update::Message { chat_id: id, seq });
    assert_eq!(b.cache().messages(id, i64::MAX, 50).unwrap().len(), 1);

    // B's client acked it, so A's marks catch up.
    until(&mut a, |u, a| {
        matches!(u, Update::Marks { chat_id } if *chat_id == id)
            && a.cache().chat(id).unwrap().unwrap().info.marks.is_some_and(|m| m.received_through >= seq)
    })
    .await;
}

/// A transport that plays frames the test lines up, and keeps what's sent.
/// With nothing lined up, `recv` waits for ever, like a quiet server.
#[derive(Clone, Default)]
struct Script {
    inbox: Arc<Mutex<VecDeque<ServerFrame>>>,
    sent: Arc<Mutex<Vec<ClientFrame>>>,
    /// While set, sending `list_devices` never finishes (a stalled socket).
    stall_barrier: Arc<AtomicBool>,
}

impl Script {
    fn push(&self, frame: ServerFrame) {
        self.inbox.lock().unwrap().push_back(frame);
    }

    fn sent(&self) -> Vec<ClientFrame> {
        self.sent.lock().unwrap().clone()
    }

    fn set_pins(&self) -> Vec<Number> {
        self.sent()
            .iter()
            .filter_map(|f| if let ClientFrame::SetPin { pin } = f { Some(pin.number) } else { None })
            .collect()
    }
}

impl Transport for Script {
    async fn send(&mut self, frame: &ClientFrame) -> Result<(), TransportError> {
        if matches!(frame, ClientFrame::ListDevices) && self.stall_barrier.load(Ordering::SeqCst) {
            return std::future::pending().await;
        }
        self.sent.lock().unwrap().push(frame.clone());
        Ok(())
    }

    async fn recv(&mut self) -> Result<ServerFrame, TransportError> {
        let next = self.inbox.lock().unwrap().pop_front();
        match next {
            Some(frame) => Ok(frame),
            None => std::future::pending().await,
        }
    }
}

fn number(body: u32) -> Number {
    Number::from_body(body).unwrap()
}

/// A client on a scripted connection, with an empty cache.
fn scripted() -> (Client<Script, MemoryCache>, Script) {
    let script = Script::default();
    let account = Account {
        username: "abin-7k3q".into(),
        number: number(111_111_111),
        display_name: "Abin".into(),
        pending_until: None,
    };
    let signed_in = SignedIn { token: Zeroizing::new([0; 32]), account, secrets: Secrets::generate() };
    let client = Client::new(script.clone(), signed_in, common::device("laptop"), common::S, MemoryCache::default());
    (client, script)
}

fn person(number: Number, keys: &PublicKeys) -> EventBody {
    EventBody::Person {
        person: Person {
            number,
            display_name: None,
            public_keys: keys.clone(),
            relation: Relation::Contact,
            deleted: false,
        },
    }
}

fn events(seq: i64, event: EventBody) -> ServerFrame {
    ServerFrame::Events { events: vec![Event { seq, event }], cursor: seq }
}

#[tokio::test]
async fn a_pin_in_a_later_frame_of_the_replay_still_wins() {
    let (mut client, script) = scripted();
    let x = number(222_222_222);
    let (old, new) = (Secrets::generate().public_keys(), Secrets::generate().public_keys());
    let pin = seal_pin(client.secrets(), x, &old);
    script.push(events(1, person(x, &new)));
    script.push(events(2, EventBody::Pin { pin: Pin { number: x, pin: Some(pin) } }));
    script.push(ServerFrame::Synced { cursor: 2 });
    client.sync().await.unwrap();

    assert!(script.set_pins().is_empty());
    assert_eq!(client.cache().pin(x).unwrap(), Some(old));
    assert!(client.cache().person(x).unwrap().unwrap().mismatch);
}

#[tokio::test]
async fn a_dropped_next_leaves_no_stray_reply() {
    let (mut client, script) = scripted();
    script.push(ServerFrame::Synced { cursor: 0 });
    client.sync().await.unwrap();
    assert_eq!(client.next().await.unwrap(), [Update::Synced]);
    let x = number(222_222_222);
    script.push(events(1, person(x, &Secrets::generate().public_keys())));
    client.next().await.unwrap();

    // The next call sends the new pin behind a barrier, then waits; it's
    // dropped there, as a `select!` would.
    {
        let mut next = pin!(client.next());
        poll_fn(|cx| {
            let _ = next.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;
    }
    let sent = script.sent();
    assert!(matches!(&sent[sent.len() - 2..], [ClientFrame::SetPin { .. }, ClientFrame::ListDevices]));

    script.push(ServerFrame::Devices { devices: vec![] });
    script.push(ServerFrame::Invites { invites: vec![] });
    assert_eq!(client.invites().await.unwrap(), vec![]);
}

#[tokio::test]
async fn a_pin_removed_and_made_again_is_sent_again() {
    let (mut client, script) = scripted();
    script.push(ServerFrame::Synced { cursor: 0 });
    client.sync().await.unwrap();
    assert_eq!(client.next().await.unwrap(), [Update::Synced]);
    let x = number(222_222_222);
    let keys = Secrets::generate().public_keys();
    script.push(events(1, person(x, &keys)));
    client.next().await.unwrap();
    script.push(ServerFrame::Devices { devices: vec![] });
    let echo = seal_pin(client.secrets(), x, &keys);
    script.push(events(2, EventBody::Pin { pin: Pin { number: x, pin: Some(echo) } }));
    client.next().await.unwrap();
    assert_eq!(script.set_pins(), [x]);
    assert!(client.cache().unsent_pins().unwrap().is_empty());

    // Removed on another device, then seen again: pinned and sent anew.
    script.push(events(3, EventBody::Pin { pin: Pin { number: x, pin: None } }));
    client.next().await.unwrap();
    script.push(events(4, person(x, &keys)));
    client.next().await.unwrap();
    script.push(ServerFrame::Devices { devices: vec![] });
    script.push(ServerFrame::Presence { number: x, online: true });
    client.next().await.unwrap();
    assert_eq!(script.set_pins(), [x, x]);
}

#[tokio::test]
async fn a_next_dropped_while_sending_its_barrier_sends_another() {
    let (mut client, script) = scripted();
    script.push(ServerFrame::Synced { cursor: 0 });
    client.sync().await.unwrap();
    assert_eq!(client.next().await.unwrap(), [Update::Synced]);
    script.push(events(1, person(number(222_222_222), &Secrets::generate().public_keys())));
    client.next().await.unwrap();

    // Dropped while the barrier is being written: it may or may not go out.
    script.stall_barrier.store(true, Ordering::SeqCst);
    {
        let mut next = pin!(client.next());
        poll_fn(|cx| {
            let _ = next.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;
    }
    script.stall_barrier.store(false, Ordering::SeqCst);

    // The next request sends a barrier again and waits for its answer first.
    script.push(ServerFrame::Devices { devices: vec![] });
    script.push(ServerFrame::Invites { invites: vec![] });
    assert_eq!(client.invites().await.unwrap(), vec![]);
    assert!(matches!(script.sent()[..], [.., ClientFrame::ListDevices, ClientFrame::ListInvites]));

    // Had the first barrier gone out after all, its answer is skipped.
    script.push(ServerFrame::Devices { devices: vec![] });
    script.push(ServerFrame::Invites { invites: vec![] });
    assert_eq!(client.invites().await.unwrap(), vec![]);
}

#[tokio::test]
async fn add_before_synced_looks_up_and_pins_nothing() {
    let (mut client, script) = scripted();
    let them = number(222_222_222);
    assert!(matches!(client.add(them).await, Err(corvo_chat_core::Error::NotSynced)));
    assert!(script.sent().is_empty());
    assert!(client.cache().pin(them).unwrap().is_none());
}
