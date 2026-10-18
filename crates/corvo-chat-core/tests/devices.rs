mod common;

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{TestClient, wait};
use corvo_chat_core::{Cache, Mark, Update, view};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// What a device shows of one chat: each message's seq, side, text and mark,
/// and the chat's unread count.
type Shown = (Vec<(Option<i64>, bool, Result<String, String>, Option<Mark>)>, usize);

fn shown(client: &TestClient, chat_id: [u8; 16]) -> Shown {
    let messages = view::messages(client.cache(), client.secrets(), chat_id, None, 50).unwrap();
    let rows = view::chats(client.cache(), client.secrets(), &HashSet::new()).unwrap();
    let unread = rows.iter().find(|r| r.chat_id == chat_id).map_or(0, |r| r.unread);
    (messages.into_iter().map(|m| (m.seq, m.mine, m.text, m.mark)).collect(), unread)
}

/// Reads updates until one matches, failing after five seconds.
async fn until(client: &mut TestClient, wanted: impl Fn(&Update) -> bool) {
    let run = async {
        loop {
            if client.next().await.unwrap().iter().any(&wanted) {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), run).await.expect("the update arrives in time");
}

fn chat_id(client: &TestClient, with: corvo_chat_proto::Number) -> Option<[u8; 16]> {
    client.cache().chat_with(with).unwrap().map(|c| c.info.chat_id.0)
}

#[tokio::test]
async fn two_devices_stay_in_sync() {
    let server = common::start().await;
    let (mut a1, mut b) = common::contacts(&server).await;
    let (a_number, b_number) = (a1.account().number, b.account().number);
    let username = a1.account().username.clone();
    a1.sync().await.unwrap();
    b.sync().await.unwrap();
    let mut a2 = common::new_device(&server, &username).await;
    a2.sync().await.unwrap();

    // A1 sends: A2 shows it as mine, with the same seq and mark.
    a1.send_text(b_number, "from A1", now_ms()).await.unwrap();
    wait(&mut a1, |c| c.cache().outbox().unwrap().is_empty() && chat_id(c, b_number).is_some()).await;
    wait(&mut a2, |c| chat_id(c, b_number).is_some_and(|id| !shown(c, id).0.is_empty())).await;
    let chat = chat_id(&a1, b_number).unwrap();
    assert_eq!(chat_id(&a2, b_number), Some(chat));

    // B gets it, and both of A's devices end up with the same mark.
    wait(&mut b, |c| chat_id(c, a_number).is_some_and(|id| !shown(c, id).0.is_empty())).await;
    let received = |c: &TestClient| shown(c, chat).0.first().is_some_and(|m| m.3 == Some(Mark::Received));
    wait(&mut a1, received).await;
    wait(&mut a2, received).await;
    let (first, second) = (shown(&a1, chat), shown(&a2, chat));
    assert_eq!(first, second);
    assert_eq!((first.0[0].1, first.0[0].2.clone()), (true, Ok("from A1".into())));

    // B reads it: both devices show it read.
    b.mark_read(chat).await.unwrap();
    let read = |c: &TestClient| shown(c, chat).0.first().is_some_and(|m| m.3 == Some(Mark::Read));
    wait(&mut a1, read).await;
    wait(&mut a2, read).await;

    // B writes: both devices get it, unread on each.
    b.send_text(a_number, "to A", now_ms()).await.unwrap();
    let got = |c: &TestClient| shown(c, chat).0.len() == 2;
    wait(&mut a1, got).await;
    wait(&mut a2, got).await;
    assert_eq!(shown(&a1, chat).1, 1);
    assert_eq!(shown(&a2, chat).1, 1);

    // A1 reads: A2 learns of it and its unread count clears.
    a1.mark_read(chat).await.unwrap();
    assert_eq!(shown(&a1, chat).1, 0);
    until(&mut a2, |u| matches!(u, Update::Read { chat_id } if *chat_id == chat)).await;
    assert_eq!(shown(&a2, chat).1, 0);

    // A2 renames itself: A1 hears of it.
    a2.rename_device("tablet").await.unwrap();
    until(&mut a1, |u| matches!(u, Update::Devices)).await;
    let names: Vec<String> = a1.devices().await.unwrap().into_iter().map(|d| d.name).collect();
    assert!(names.contains(&"tablet".to_string()));

    // A third device, signed in fresh, shows the same as A1.
    let mut a3 = common::new_device(&server, &username).await;
    a3.sync().await.unwrap();
    assert_eq!(chat_id(&a3, b_number), Some(chat));
    let (one, three) = (shown(&a1, chat), shown(&a3, chat));
    assert_eq!(one, three);
    assert_eq!(three.0.len(), 2);
    assert_eq!(three.1, 0);
    assert_eq!(three.0[0].3, Some(Mark::Read));
}
