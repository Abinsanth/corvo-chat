//! What a client draws, read from the cache: the chat list with unread
//! counts, and a chat's messages with the marks on my own.
//! Bodies are opened here, with each chat's key checked against the pins.

use std::cmp::Reverse;
use std::collections::HashSet;

use corvo_chat_proto::{
    Number,
    chat::{ChatInfo, Marks, Message, Relation},
};

use crate::Error;
use crate::cache::{Cache, OutState, Outgoing};
use crate::crypto::{ChatKey, MsgId, Secrets, chat_key, decrypt_message};

/// Where one of my messages has got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `○`: not sent yet.
    Waiting,
    /// `·`: the server has it.
    Sent,
    /// `✓`: one of their devices has it.
    Received,
    /// `✓✓`: they read it.
    Read,
    /// `✗`: the server refused it, with its reason.
    Refused(String),
}

/// One message as shown. `text` is `Err` with a warning when it won't open;
/// `seq` is `None` until the server has it, and `mark` is `None` for theirs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownMessage {
    pub chat_id: [u8; 16],
    pub seq: Option<i64>,
    pub msg_id: MsgId,
    pub mine: bool,
    pub from: Number,
    pub text: Result<String, String>,
    pub sent_at_ms: u64,
    pub mark: Option<Mark>,
}

/// One line of the chat list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatRow {
    pub chat_id: [u8; 16],
    pub with: Number,
    /// Their display name, when they share it.
    pub name: Option<String>,
    pub relation: Relation,
    pub last: Option<ShownMessage>,
    pub unread: usize,
    pub online: bool,
    /// They deleted their account.
    pub deleted: bool,
}

/// One line of the people list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonRow {
    pub number: Number,
    /// Their display name, when they share it.
    pub name: Option<String>,
    pub relation: Relation,
    /// A contact who is online now.
    pub online: bool,
    /// Their keys changed from the ones pinned here.
    pub mismatch: bool,
    /// They deleted their account.
    pub deleted: bool,
}

/// A chat's key, opened with the other person's pinned keys.
pub(crate) enum Opened {
    Key(ChatKey),
    /// Their keys aren't pinned yet, so the chat can't be checked.
    Unpinned,
    /// The signature or my sealed copy doesn't check out.
    Broken,
}

/// Opens a chat's key, only ever with pinned keys: a chat's key is checked
/// against the keys pinned for the other person, never the server's copy.
pub(crate) fn open_chat(cache: &dyn Cache, secrets: &Secrets, me: Number, info: &ChatInfo) -> Result<Opened, Error> {
    let Some(theirs) = cache.pin(info.with)? else { return Ok(Opened::Unpinned) };
    Ok(match chat_key(info, me, &secrets.public_keys(), &theirs, secrets) {
        Ok(key) => Opened::Key(key),
        Err(_) => Opened::Broken,
    })
}

/// The mark of my message `seq`, from the other side's marks. A withheld
/// message stays `·` for good, and with no marks nothing is known.
fn mark_of(marks: Option<&Marks>, seq: i64) -> Mark {
    match marks {
        Some(m) if m.withheld.iter().any(|[from, to]| (*from..=*to).contains(&seq)) => Mark::Sent,
        Some(m) if m.read_through >= seq => Mark::Read,
        Some(m) if m.received_through >= seq => Mark::Received,
        _ => Mark::Sent,
    }
}

fn warning(from: Number) -> String {
    format!("⚠ couldn't decrypt a message from {from}")
}

/// A message from the cache, opened if the chat's key is known.
fn stored(m: &Message, key: Option<&ChatKey>, me: Number, marks: Option<&Marks>) -> ShownMessage {
    let mine = m.from == me;
    ShownMessage {
        chat_id: m.chat_id.0,
        seq: Some(m.seq),
        msg_id: MsgId(m.msg_id.0),
        mine,
        from: m.from,
        text: key.and_then(|k| decrypt_message(k, m).ok()).map(|p| p.text).ok_or_else(|| warning(m.from)),
        sent_at_ms: u64::try_from(m.sent_at).unwrap_or(0).saturating_mul(1000),
        mark: mine.then(|| mark_of(marks, m.seq)),
    }
}

/// Whether an entry will go into whatever chat with its person is cached:
/// one whose new chat lost, or one written before a sync, not sealed yet.
fn waits_for_chat(o: &Outgoing) -> bool {
    o.state == OutState::NeedsChat || (o.state == OutState::Waiting && o.body.is_none())
}

/// A message of mine the server hasn't echoed back yet.
fn outgoing(o: Outgoing, me: Number, marks: Option<&Marks>) -> ShownMessage {
    let (seq, mark) = match o.state {
        OutState::Waiting | OutState::NeedsChat => (None, Mark::Waiting),
        OutState::Sent { seq } => (Some(seq), mark_of(marks, seq)),
        OutState::Refused { message, .. } => (None, Mark::Refused(message)),
    };
    ShownMessage {
        chat_id: o.chat_id,
        seq,
        msg_id: o.msg_id,
        mine: true,
        from: me,
        text: Ok(o.text),
        sent_at_ms: o.created_ms,
        mark: Some(mark),
    }
}

/// Up to `limit` of a chat's messages before seq `before` (the newest with
/// `None`), oldest first. Messages of mine not yet echoed are merged in:
/// those with no seq yet come last, and only on the newest page. A message
/// that moves here from a chat that lost (`chat_exists`), or one not sealed
/// yet, shows here already.
pub fn messages(
    cache: &dyn Cache,
    secrets: &Secrets,
    chat_id: [u8; 16],
    before: Option<i64>,
    limit: usize,
) -> Result<Vec<ShownMessage>, Error> {
    let Some(me) = cache.account()?.map(|a| a.number) else { return Ok(Vec::new()) };
    let chat = cache.chat(chat_id)?;
    let info = chat.as_ref().map(|c| &c.info);
    let key = match info {
        Some(info) => match open_chat(cache, secrets, me, info)? {
            Opened::Key(key) => Some(key),
            Opened::Unpinned | Opened::Broken => None,
        },
        None => None,
    };
    let marks = info.and_then(|i| i.marks.as_ref());
    let mut shown: Vec<ShownMessage> = cache
        .messages(chat_id, before.unwrap_or(i64::MAX), limit)?
        .iter()
        .map(|m| stored(m, key.as_ref(), me, marks))
        .collect();
    for o in cache.outbox()? {
        let here = o.chat_id == chat_id || (waits_for_chat(&o) && info.is_some_and(|i| i.with == o.to));
        // A message cleared before its echo came is gone.
        let cleared = o.seq().is_some_and(|seq| info.is_some_and(|i| seq <= i.cleared_through));
        if !here || cleared || shown.iter().any(|m| m.msg_id == o.msg_id) {
            continue;
        }
        let o = outgoing(o, me, marks);
        let fits = match (before, o.seq) {
            (None, _) => true,
            (Some(before), Some(seq)) => seq < before,
            (Some(_), None) => false,
        };
        if fits {
            shown.push(o);
        }
    }
    // Stable, so messages with no seq keep the outbox's order.
    shown.sort_by_key(|m| m.seq.unwrap_or(i64::MAX));
    if shown.len() > limit {
        shown.drain(..shown.len() - limit);
    }
    Ok(shown)
}

/// Visible messages from them after my `read_through` (protocol §11).
fn unread(cache: &dyn Cache, me: Number, info: &ChatInfo) -> Result<usize, Error> {
    let id = info.chat_id.0;
    let newest = cache.messages(id, i64::MAX, 1)?.last().map_or(0, |m| m.seq);
    // Seqs are distinct, so no more than this many can be newer.
    let span = usize::try_from(newest - info.read_through).unwrap_or(0);
    if span == 0 {
        return Ok(0);
    }
    let newer = cache.messages(id, i64::MAX, span)?;
    Ok(newer.iter().filter(|m| m.from != me && m.seq > info.read_through).count())
}

/// The chat list, newest first, with `online` the contacts online now.
/// Chats with no visible messages and no relation are hidden (protocol
/// §11). A first message whose chat the server hasn't confirmed yet gets a
/// row of its own.
pub fn chats(cache: &dyn Cache, secrets: &Secrets, online: &HashSet<Number>) -> Result<Vec<ChatRow>, Error> {
    let Some(me) = cache.account()?.map(|a| a.number) else { return Ok(Vec::new()) };
    let mut rows = Vec::new();
    for chat in cache.chats()? {
        let unread = unread(cache, me, &chat.info)?;
        rows.push(row(cache, secrets, chat.info.chat_id.0, chat.info.with, unread, online)?);
    }
    let mut pending = HashSet::new();
    for o in cache.outbox()? {
        let moving = waits_for_chat(&o) && cache.chat_with(o.to)?.is_some();
        if !moving && cache.chat(o.chat_id)?.is_none() && pending.insert(o.chat_id) {
            rows.push(row(cache, secrets, o.chat_id, o.to, 0, online)?);
        }
    }
    rows.retain(|r| r.last.is_some() || r.relation != Relation::None);
    rows.sort_by_key(|r| Reverse(r.last.as_ref().map(|m| m.sent_at_ms)));
    Ok(rows)
}

/// Everyone known here but me, by name (case-insensitive, nameless last),
/// then by number. `online` shows only for contacts.
pub fn people(cache: &dyn Cache, online: &HashSet<Number>) -> Result<Vec<PersonRow>, Error> {
    let me = cache.account()?.map(|a| a.number);
    let mut rows: Vec<PersonRow> = cache
        .people()?
        .into_iter()
        .filter(|c| Some(c.person.number) != me)
        .map(|c| PersonRow {
            number: c.person.number,
            name: c.person.display_name,
            relation: c.person.relation,
            online: c.person.relation == Relation::Contact && online.contains(&c.person.number),
            mismatch: c.mismatch,
            deleted: c.person.deleted,
        })
        .collect();
    rows.sort_by_key(|r| (r.name.is_none(), r.name.as_ref().map(|n| n.to_lowercase()), r.number.to_string()));
    Ok(rows)
}

fn row(
    cache: &dyn Cache,
    secrets: &Secrets,
    chat_id: [u8; 16],
    with: Number,
    unread: usize,
    online: &HashSet<Number>,
) -> Result<ChatRow, Error> {
    let person = cache.person(with)?.map(|p| p.person);
    Ok(ChatRow {
        chat_id,
        with,
        name: person.as_ref().and_then(|p| p.display_name.clone()),
        relation: person.as_ref().map_or(Relation::None, |p| p.relation),
        last: messages(cache, secrets, chat_id, None, 1)?.pop(),
        unread,
        online: online.contains(&with),
        deleted: person.is_some_and(|p| p.deleted),
    })
}

#[cfg(test)]
mod tests {
    use corvo_chat_proto::frames::ErrorCode;

    use super::*;

    fn entry(state: OutState) -> Outgoing {
        Outgoing {
            chat_id: [1; 16],
            msg_id: MsgId([2; 16]),
            to: Number::from_body(222_222_222).unwrap(),
            text: "hi".into(),
            body: None,
            new_chat: None,
            created_ms: 7,
            state,
        }
    }

    #[test]
    fn marks_follow_the_rule() {
        let marks = Marks { received_through: 5, read_through: 3, withheld: vec![[2, 2]] };
        assert_eq!(mark_of(None, 1), Mark::Sent);
        assert_eq!(mark_of(Some(&marks), 1), Mark::Read);
        // Withheld stays `·` whatever the cursors say.
        assert_eq!(mark_of(Some(&marks), 2), Mark::Sent);
        assert_eq!(mark_of(Some(&marks), 4), Mark::Received);
        assert_eq!(mark_of(Some(&marks), 6), Mark::Sent);

        let me = Number::from_body(111_111_111).unwrap();
        let mark = |state| outgoing(entry(state), me, Some(&marks)).mark.unwrap();
        assert_eq!(mark(OutState::Waiting), Mark::Waiting);
        assert_eq!(mark(OutState::NeedsChat), Mark::Waiting);
        assert_eq!(mark(OutState::Sent { seq: 4 }), Mark::Received);
        let refused = OutState::Refused { code: ErrorCode::Banned, message: "no".into() };
        assert_eq!(mark(refused), Mark::Refused("no".into()));
    }
}
