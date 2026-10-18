//! Applying what the server sends to the local cache (protocol §11), and the
//! updates that tell a client what changed.

use std::collections::{BTreeMap, HashSet};

use corvo_chat_proto::{
    Number, PublicKeys,
    chat::{ChatInfo, EventBody, Message, Pin, Snapshot},
    frames::{ErrorCode, ServerFrame, SignedOutReason},
};

use crate::Error;
use crate::cache::{Cache, CachedChat, CachedPerson, OutState};
use crate::crypto::{MsgId, Secrets, open_pin};

/// What changed, for a client to redraw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// Caught up with the server; live events follow.
    Synced,
    Message {
        chat_id: [u8; 16],
        seq: i64,
    },
    /// A chat appeared or changed: new state, older messages, or cleared.
    Chat {
        chat_id: [u8; 16],
    },
    Person {
        number: Number,
    },
    /// The other side's marks moved.
    Marks {
        chat_id: [u8; 16],
    },
    /// I read further on another device.
    Read {
        chat_id: [u8; 16],
    },
    Profile,
    Devices,
    Presence {
        number: Number,
        online: bool,
    },
    Sent {
        chat_id: [u8; 16],
        msg_id: MsgId,
    },
    Refused {
        chat_id: [u8; 16],
        msg_id: MsgId,
        message: String,
    },
    /// Their keys aren't the pinned ones: nothing is sealed to them until checked.
    KeyMismatch {
        number: Number,
    },
    SignedOut(SignedOutReason),
}

/// What a frame leaves for the client to do.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Applied {
    /// Per chat, the newest seq now held from the other side, to send as
    /// `received`.
    pub acks: Vec<([u8; 16], i64)>,
    /// Keys seen with no pin yet. They're pinned with [`pin_new`] only once
    /// the client is synced: a replay renders people as they are now, so a
    /// person's new keys can come before the pin that predates them.
    pub unpinned: Vec<(Number, PublicKeys)>,
}

/// Applies one server frame to the cache, adding what changed to `out`.
pub(crate) fn apply(
    cache: &mut dyn Cache,
    secrets: &Secrets,
    frame: ServerFrame,
    out: &mut Vec<Update>,
) -> Result<Applied, Error> {
    let me = cache.account()?.map(|a| a.number);
    let mut a = Applying { cache, secrets, me, out, acks: BTreeMap::new(), unpinned: Vec::new() };
    match frame {
        ServerFrame::Snapshot { snapshot } => a.snapshot(snapshot)?,
        ServerFrame::Events { events, cursor } => {
            for event in events {
                a.event(event.event)?;
            }
            // Only now: a frame cut off halfway comes again whole.
            a.cache.set_cursor(cursor)?;
        }
        ServerFrame::History { chat_id, messages, complete } => a.history(chat_id.0, messages, complete)?,
        ServerFrame::Synced { cursor } => {
            a.cache.set_cursor(cursor)?;
            a.out.push(Update::Synced);
        }
        ServerFrame::Presence { number, online } => a.out.push(Update::Presence { number, online }),
        ServerFrame::Sent { chat_id, msg_id, seq, .. } => {
            let msg_id = MsgId(msg_id.0);
            a.set_state(chat_id.0, msg_id, OutState::Sent { seq })?;
            a.out.push(Update::Sent { chat_id: chat_id.0, msg_id });
        }
        // The chat lost to one that exists, which arrives as an event.
        ServerFrame::NotSent { chat_id, msg_id, code: ErrorCode::ChatExists, .. } => {
            a.set_state(chat_id.0, MsgId(msg_id.0), OutState::NeedsChat)?;
        }
        ServerFrame::NotSent { chat_id, msg_id, code, message } => {
            let msg_id = MsgId(msg_id.0);
            a.set_state(chat_id.0, msg_id, OutState::Refused { code, message: message.clone() })?;
            a.out.push(Update::Refused { chat_id: chat_id.0, msg_id, message });
        }
        ServerFrame::Found { number, public_keys } => a.found(number, &public_keys)?,
        ServerFrame::SignedOut { reason } => {
            a.cache.wipe()?;
            a.out.push(Update::SignedOut(reason));
        }
        // Replies to requests: whoever asked handles them.
        _ => {}
    }
    Ok(Applied { acks: a.acks.into_iter().collect(), unpinned: a.unpinned })
}

/// Pins keys that were seen with no pin (Decision 13), checking again first:
/// a pin or a mismatch may have arrived since. A person's newest keys win
/// over the ones first seen. The pins wait in the cache until the server
/// echoes them back. Gives whether any were pinned.
pub(crate) fn pin_new(cache: &mut dyn Cache, seen: Vec<(Number, PublicKeys)>) -> Result<bool, Error> {
    let me = cache.account()?.map(|a| a.number);
    let mut pinned = false;
    for (number, keys) in seen {
        if Some(number) == me || cache.pin(number)?.is_some() {
            continue;
        }
        let keys = match cache.person(number)? {
            Some(cached) if cached.mismatch => continue,
            Some(cached) => cached.person.public_keys,
            None => keys,
        };
        if !keys.verify() {
            continue;
        }
        cache.put_pin(number, Some(keys))?;
        cache.mark_pin_unsent(number)?;
        pinned = true;
    }
    Ok(pinned)
}

/// One frame being applied, with the newest seq from the other side in each
/// chat (to ack) and the keys still to pin.
struct Applying<'a> {
    cache: &'a mut dyn Cache,
    secrets: &'a Secrets,
    me: Option<Number>,
    out: &'a mut Vec<Update>,
    acks: BTreeMap<[u8; 16], i64>,
    unpinned: Vec<(Number, PublicKeys)>,
}

impl Applying<'_> {
    /// A new device's starting point. Pins come first, so each person is
    /// checked against them. Pins of mine the server never confirmed stay.
    fn snapshot(&mut self, s: Snapshot) -> Result<(), Error> {
        self.me = Some(s.account.number);
        let mut pins = Vec::new();
        let mut bad = HashSet::new();
        for Pin { number, pin } in &s.pins {
            let Some(blob) = pin else { continue };
            match open_pin(self.secrets, *number, blob) {
                Some(keys) => pins.push((*number, keys)),
                None => {
                    bad.insert(*number);
                    self.out.push(Update::KeyMismatch { number: *number });
                }
            }
        }
        let mut unsent = Vec::new();
        for number in self.cache.unsent_pins()? {
            if s.pins.iter().all(|p| p.number != number)
                && let Some(keys) = self.cache.pin(number)?
            {
                unsent.push(number);
                pins.push((number, keys));
            }
        }
        let ids: Vec<[u8; 16]> = s.chats.iter().map(|c| c.chat_id.0).collect();
        // Each chat's latest messages follow in `history` frames.
        let chats = s.chats.into_iter().map(|info| CachedChat { info, complete: false }).collect();
        self.cache.replace_all(s.account, Vec::new(), chats, pins)?;
        for number in unsent {
            self.cache.mark_pin_unsent(number)?;
        }
        self.out.push(Update::Profile);
        for person in s.people {
            let number = person.number;
            let mismatch = self.check_keys(number, &person.public_keys, bad.contains(&number))?;
            self.cache.put_person(CachedPerson { person, mismatch })?;
            self.out.push(Update::Person { number });
        }
        self.out.extend(ids.into_iter().map(|chat_id| Update::Chat { chat_id }));
        Ok(())
    }

    fn event(&mut self, event: EventBody) -> Result<(), Error> {
        match event {
            EventBody::Message { message } => {
                let (chat_id, seq) = (message.chat_id.0, message.seq);
                if self.message(message)? {
                    self.out.push(Update::Message { chat_id, seq });
                }
            }
            EventBody::Chat { chat } => {
                let chat_id = chat.chat_id.0;
                // A chat new here may have older messages to page in.
                let complete = self.cache.chat(chat_id)?.is_some_and(|c| c.complete);
                self.cache.put_chat(CachedChat { info: chat, complete })?;
                self.out.push(Update::Chat { chat_id });
            }
            EventBody::Person { person } => {
                let number = person.number;
                let flagged = self.cache.person(number)?.is_some_and(|p| p.mismatch);
                let mismatch = self.check_keys(number, &person.public_keys, flagged)?;
                self.cache.put_person(CachedPerson { person, mismatch })?;
                self.out.push(Update::Person { number });
            }
            EventBody::Marks { chat_id, marks } => {
                self.change_chat(chat_id.0, |info| info.marks = Some(marks))?;
                self.out.push(Update::Marks { chat_id: chat_id.0 });
            }
            EventBody::Read { chat_id, read_through } => {
                self.change_chat(chat_id.0, |info| info.read_through = info.read_through.max(read_through))?;
                self.out.push(Update::Read { chat_id: chat_id.0 });
            }
            EventBody::Cleared { chat_id, cleared_through } => {
                self.change_chat(chat_id.0, |info| info.cleared_through = info.cleared_through.max(cleared_through))?;
                self.cache.clear_through(chat_id.0, cleared_through)?;
                self.out.push(Update::Chat { chat_id: chat_id.0 });
            }
            EventBody::Profile { account } => {
                self.me = Some(account.number);
                self.cache.set_account(account)?;
                self.out.push(Update::Profile);
            }
            EventBody::Devices { devices } => {
                self.cache.set_devices(devices)?;
                self.out.push(Update::Devices);
            }
            EventBody::Pin { pin } => self.pin_event(pin)?,
        }
        Ok(())
    }

    /// Changes a cached chat; one not cached is left alone.
    fn change_chat(&mut self, chat_id: [u8; 16], change: impl FnOnce(&mut ChatInfo)) -> Result<(), Error> {
        if let Some(mut chat) = self.cache.chat(chat_id)? {
            change(&mut chat.info);
            self.cache.put_chat(chat)?;
        }
        Ok(())
    }

    /// Stores a message, unless its chat was cleared past it. Mine ends its
    /// outbox entry; theirs is acked. Gives whether it was kept.
    fn message(&mut self, message: Message) -> Result<bool, Error> {
        let (chat_id, seq) = (message.chat_id.0, message.seq);
        let chat = self.cache.chat(chat_id)?;
        if chat.as_ref().is_some_and(|c| seq <= c.info.cleared_through) {
            return Ok(false);
        }
        if Some(message.from) == self.me {
            self.cache.remove_outgoing(chat_id, MsgId(message.msg_id.0))?;
        } else {
            let ack = self.acks.entry(chat_id).or_insert(seq);
            *ack = (*ack).max(seq);
        }
        if let Some(mut chat) = chat.filter(|c| seq > c.info.last_seq) {
            chat.info.last_seq = seq;
            self.cache.put_chat(chat)?;
        }
        self.cache.put_message(message)?;
        Ok(true)
    }

    /// A page of a chat's messages, oldest first.
    fn history(&mut self, chat_id: [u8; 16], messages: Vec<Message>, complete: bool) -> Result<(), Error> {
        for message in messages.into_iter().filter(|m| m.chat_id.0 == chat_id) {
            self.message(message)?;
        }
        if let Some(mut chat) = self.cache.chat(chat_id)? {
            chat.complete = complete;
            self.cache.put_chat(chat)?;
        }
        self.out.push(Update::Chat { chat_id });
        Ok(())
    }

    /// Checks keys seen for `number` against its pin (Decision 13): different
    /// or bad ones are a mismatch, and valid ones with no pin are noted for
    /// [`pin_new`]. One already `flagged` stays so until a pin that matches.
    /// Gives the mismatch.
    fn check_keys(&mut self, number: Number, keys: &PublicKeys, flagged: bool) -> Result<bool, Error> {
        if Some(number) == self.me {
            return Ok(false);
        }
        let mismatch = match self.cache.pin(number)? {
            Some(pinned) => flagged || pinned != *keys || !keys.verify(),
            None if flagged || !keys.verify() => true,
            None => {
                self.unpinned.push((number, keys.clone()));
                false
            }
        };
        if mismatch && !flagged {
            self.out.push(Update::KeyMismatch { number });
        }
        Ok(mismatch)
    }

    /// A pin changed on one of my devices (or so the server says). One that
    /// opens is what the server holds now, which settles any pin of mine
    /// waiting to be confirmed.
    fn pin_event(&mut self, Pin { number, pin }: Pin) -> Result<(), Error> {
        let cached = self.cache.person(number)?;
        match pin.map(|blob| open_pin(self.secrets, number, &blob)) {
            None => {
                self.cache.put_pin(number, None)?;
                self.cache.mark_pin_sent(number)?;
            }
            Some(Some(keys)) => {
                self.cache.mark_pin_sent(number)?;
                self.cache.put_pin(number, Some(keys.clone()))?;
                if let Some(mut cached) = cached {
                    // Only a pin of their current keys clears a mismatch.
                    let matches = cached.person.public_keys == keys && keys.verify();
                    if !matches && !cached.mismatch {
                        self.out.push(Update::KeyMismatch { number });
                    }
                    cached.mismatch = !matches;
                    self.cache.put_person(cached)?;
                }
            }
            // Not mine, or changed: what's pinned here stays.
            Some(None) => match cached {
                Some(cached) if cached.mismatch => {}
                Some(mut cached) => {
                    cached.mismatch = true;
                    self.cache.put_person(cached)?;
                    self.out.push(Update::KeyMismatch { number });
                }
                None => self.out.push(Update::KeyMismatch { number }),
            },
        }
        if self.cache.person(number)?.is_some() {
            self.out.push(Update::Person { number });
        }
        Ok(())
    }

    /// A lookup's answer: its keys are checked like a person's.
    fn found(&mut self, number: Number, keys: &PublicKeys) -> Result<(), Error> {
        let cached = self.cache.person(number)?;
        let flagged = cached.as_ref().is_some_and(|p| p.mismatch);
        let mismatch = self.check_keys(number, keys, flagged)?;
        if let Some(mut cached) = cached.filter(|_| mismatch != flagged) {
            cached.mismatch = mismatch;
            self.cache.put_person(cached)?;
            self.out.push(Update::Person { number });
        }
        Ok(())
    }

    /// Moves an outbox entry on, if it's still there.
    fn set_state(&mut self, chat_id: [u8; 16], msg_id: MsgId, state: OutState) -> Result<(), Error> {
        let entry = self.cache.outbox()?.into_iter().find(|o| o.chat_id == chat_id && o.msg_id == msg_id);
        if let Some(mut entry) = entry {
            entry.state = state;
            self.cache.put_outgoing(entry)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use corvo_chat_proto::{
        B64, Blob, Body, Number, PublicKeys,
        chat::{ChatInfo, Event, EventBody, Marks, Message, Person, Pin, Relation, Snapshot},
        frames::{Account, DeviceInfo, ErrorCode, ServerFrame},
    };

    use super::*;
    use crate::cache::{Cache, CachedChat, CachedPerson, MemoryCache, OutState, Outgoing};
    use crate::crypto::{MsgId, Secrets, seal_pin};

    const CHAT: [u8; 16] = [7; 16];

    fn number(body: u32) -> Number {
        Number::from_body(body).unwrap()
    }

    fn me() -> Number {
        number(111_111_111)
    }

    fn them() -> Number {
        number(222_222_222)
    }

    fn account(name: &str) -> Account {
        Account { username: "abin-7k3q".into(), number: me(), display_name: name.into(), pending_until: None }
    }

    fn chat_info(id: [u8; 16], with: Number) -> ChatInfo {
        ChatInfo {
            chat_id: B64(id),
            with,
            keys: vec![],
            signature: B64([0; 64]),
            last_seq: 0,
            read_through: 0,
            cleared_through: 0,
            marks: None,
        }
    }

    fn message(seq: i64, from: Number) -> Message {
        Message {
            chat_id: B64(CHAT),
            seq,
            msg_id: B64([seq as u8; 16]),
            from,
            body: Body(vec![seq as u8; 40]),
            sent_at: 1_800_000_000 + seq,
        }
    }

    fn person(number: Number, keys: &PublicKeys) -> Person {
        Person {
            number,
            display_name: Some("Rahul".into()),
            public_keys: keys.clone(),
            relation: Relation::Contact,
            deleted: false,
        }
    }

    fn events(first: i64, bodies: Vec<EventBody>, cursor: i64) -> ServerFrame {
        let events = bodies.into_iter().zip(first..).map(|(event, seq)| Event { seq, event }).collect();
        ServerFrame::Events { events, cursor }
    }

    /// A cache that knows whose it is, as `Client::sync` leaves it.
    fn cache() -> MemoryCache {
        let mut cache = MemoryCache::default();
        cache.set_account(account("Abin")).unwrap();
        cache
    }

    /// The cache with the chat with `them()` in it.
    fn cache_with_chat() -> MemoryCache {
        let mut cache = cache();
        cache.put_chat(CachedChat { info: chat_info(CHAT, them()), complete: true }).unwrap();
        cache
    }

    /// Applies a frame as a synced client does, pinning new keys at the end.
    /// Gives the acks and the updates.
    fn run(cache: &mut dyn Cache, secrets: &Secrets, frame: ServerFrame) -> (Vec<([u8; 16], i64)>, Vec<Update>) {
        let mut out = Vec::new();
        let applied = apply(cache, secrets, frame, &mut out).unwrap();
        pin_new(cache, applied.unpinned).unwrap();
        (applied.acks, out)
    }

    fn seqs(cache: &dyn Cache) -> Vec<i64> {
        cache.messages(CHAT, i64::MAX, 100).unwrap().iter().map(|m| m.seq).collect()
    }

    fn unsent(cache: &dyn Cache) -> Vec<Number> {
        cache.unsent_pins().unwrap()
    }

    fn pin_of(secrets: &Secrets, number: Number, keys: &PublicKeys) -> EventBody {
        EventBody::Pin { pin: Pin { number, pin: Some(seal_pin(secrets, number, keys)) } }
    }

    #[test]
    fn events_apply_twice_without_harm() {
        let secrets = Secrets::generate();
        let their = Secrets::generate().public_keys();
        let frame = events(
            1,
            vec![
                EventBody::Person { person: person(them(), &their) },
                EventBody::Chat { chat: chat_info(CHAT, them()) },
                EventBody::Message { message: message(1, them()) },
                EventBody::Message { message: message(2, me()) },
                EventBody::Marks {
                    chat_id: B64(CHAT),
                    marks: Marks { received_through: 2, read_through: 2, withheld: vec![] },
                },
                EventBody::Read { chat_id: B64(CHAT), read_through: 1 },
            ],
            6,
        );
        let mut cache = cache();
        run(&mut cache, &secrets, frame.clone());
        let once = cache.clone();
        run(&mut cache, &secrets, frame);

        assert_eq!(cache, once);
        assert_eq!(seqs(&cache), [1, 2]);
        assert_eq!(cache.chat(CHAT).unwrap().unwrap().info.last_seq, 2);
        assert_eq!(cache.pin(them()).unwrap(), Some(their));
        assert_eq!(unsent(&cache), [them()]);
    }

    #[test]
    fn cursor_moves_only_after_a_whole_frame() {
        let secrets = Secrets::generate();
        let mut failing = FailAt { inner: cache_with_chat(), seq: 3 };
        let frame = events(10, (1..=3).map(|seq| EventBody::Message { message: message(seq, them()) }).collect(), 12);
        assert!(apply(&mut failing, &secrets, frame, &mut Vec::new()).is_err());
        assert_eq!(failing.cursor().unwrap(), None);
        assert_eq!(seqs(&failing), [1, 2]);

        // The frame's cursor, even past events it left out.
        let mut cache = cache_with_chat();
        let frame = events(10, (1..=2).map(|seq| EventBody::Message { message: message(seq, them()) }).collect(), 15);
        run(&mut cache, &secrets, frame);
        assert_eq!(cache.cursor().unwrap(), Some(15));
    }

    #[test]
    fn snapshot_replaces_the_cache() {
        let secrets = Secrets::generate();
        let mut cache = cache_with_chat();
        let old = number(333_333_333);
        let old_keys = Secrets::generate().public_keys();
        let old_frame = events(
            1,
            vec![
                EventBody::Person { person: person(old, &old_keys) },
                EventBody::Message { message: message(1, them()) },
            ],
            2,
        );
        run(&mut cache, &secrets, old_frame);
        // Confirmed, so the snapshot's pins replace it.
        let old_frame = events(3, vec![pin_of(&secrets, old, &old_keys)], 3);
        run(&mut cache, &secrets, old_frame);
        let outgoing = Outgoing {
            chat_id: CHAT,
            msg_id: MsgId([9; 16]),
            to: them(),
            text: "still waiting".into(),
            body: None,
            new_chat: None,
            created_ms: 1,
            state: OutState::Waiting,
        };
        cache.put_outgoing(outgoing.clone()).unwrap();

        let new_chat = [8; 16];
        let their = Secrets::generate().public_keys();
        let snapshot = Snapshot {
            cursor: 41,
            account: account("Abin B"),
            people: vec![person(them(), &their)],
            chats: vec![chat_info(new_chat, them())],
            pins: vec![Pin { number: them(), pin: Some(seal_pin(&secrets, them(), &their)) }],
        };
        let (_, out) = run(&mut cache, &secrets, ServerFrame::Snapshot { snapshot });

        assert!(cache.person(old).unwrap().is_none());
        assert!(cache.pin(old).unwrap().is_none());
        assert!(cache.chat(CHAT).unwrap().is_none());
        assert!(seqs(&cache).is_empty());
        assert_eq!(cache.account().unwrap().unwrap().display_name, "Abin B");
        assert_eq!(
            cache.person(them()).unwrap(),
            Some(CachedPerson { person: person(them(), &their), mismatch: false })
        );
        assert_eq!(cache.pin(them()).unwrap(), Some(their));
        assert!(!cache.chat(new_chat).unwrap().unwrap().complete);
        // Already pinned, so nothing to send; the outbox is kept.
        assert!(unsent(&cache).is_empty());
        assert_eq!(cache.outbox().unwrap(), [outgoing]);
        assert!(!out.contains(&Update::Synced));
        // The cursor waits for `synced`, so a cut-off snapshot starts over.
        assert_eq!(cache.cursor().unwrap(), None);

        let mut latest = message(5, them());
        latest.chat_id = B64(new_chat);
        let history = ServerFrame::History { chat_id: B64(new_chat), messages: vec![latest], complete: true };
        let (acks, _) = run(&mut cache, &secrets, history);
        assert!(cache.chat(new_chat).unwrap().unwrap().complete);
        assert_eq!(cache.messages(new_chat, i64::MAX, 50).unwrap().len(), 1);
        assert_eq!(acks, [(new_chat, 5)]);

        let (_, out) = run(&mut cache, &secrets, ServerFrame::Synced { cursor: 41 });
        assert_eq!(out, [Update::Synced]);
        assert_eq!(cache.cursor().unwrap(), Some(41));
    }

    #[test]
    fn cleared_event_hides_messages_up_to_it() {
        let secrets = Secrets::generate();
        let mut cache = cache_with_chat();
        let all = (1..=5).map(|seq| EventBody::Message { message: message(seq, them()) }).collect();
        run(&mut cache, &secrets, events(1, all, 5));

        let (_, out) = run(
            &mut cache,
            &secrets,
            events(6, vec![EventBody::Cleared { chat_id: B64(CHAT), cleared_through: 3 }], 6),
        );
        assert_eq!(seqs(&cache), [4, 5]);
        assert_eq!(cache.chat(CHAT).unwrap().unwrap().info.cleared_through, 3);
        assert_eq!(out, [Update::Chat { chat_id: CHAT }]);

        // A cleared message that turns up again stays hidden.
        run(&mut cache, &secrets, events(2, vec![EventBody::Message { message: message(2, them()) }], 2));
        assert_eq!(seqs(&cache), [4, 5]);
    }

    #[test]
    fn read_event_moves_my_read_through() {
        let secrets = Secrets::generate();
        let mut cache = cache_with_chat();
        let read = |through| events(1, vec![EventBody::Read { chat_id: B64(CHAT), read_through: through }], 1);

        let (_, out) = run(&mut cache, &secrets, read(4));
        assert_eq!(cache.chat(CHAT).unwrap().unwrap().info.read_through, 4);
        assert_eq!(out, [Update::Read { chat_id: CHAT }]);
        // Never backwards.
        run(&mut cache, &secrets, read(2));
        assert_eq!(cache.chat(CHAT).unwrap().unwrap().info.read_through, 4);
    }

    #[test]
    fn a_new_persons_keys_are_pinned_once() {
        let secrets = Secrets::generate();
        let their = Secrets::generate().public_keys();
        let mut cache = cache();
        let frame = events(1, vec![EventBody::Person { person: person(them(), &their) }], 1);

        let (_, out) = run(&mut cache, &secrets, frame.clone());
        assert_eq!(cache.pin(them()).unwrap(), Some(their.clone()));
        assert_eq!(unsent(&cache), [them()]);
        assert_eq!(out, [Update::Person { number: them() }]);

        // The echoed pin event confirms it, and changes nothing else.
        let (_, out) = run(&mut cache, &secrets, events(2, vec![pin_of(&secrets, them(), &their)], 2));
        assert!(unsent(&cache).is_empty());
        assert_eq!(out, [Update::Person { number: them() }]);
        run(&mut cache, &secrets, frame);
        assert!(unsent(&cache).is_empty());
        assert_eq!(cache.pin(them()).unwrap(), Some(their));
    }

    #[test]
    fn changed_or_bad_keys_raise_a_mismatch() {
        let secrets = Secrets::generate();
        let (first, second) = (Secrets::generate().public_keys(), Secrets::generate().public_keys());
        let mut cache = cache();
        run(&mut cache, &secrets, events(1, vec![EventBody::Person { person: person(them(), &first) }], 1));

        let (_, out) =
            run(&mut cache, &secrets, events(2, vec![EventBody::Person { person: person(them(), &second) }], 2));
        assert!(out.contains(&Update::KeyMismatch { number: them() }));
        assert!(cache.person(them()).unwrap().unwrap().mismatch);
        assert_eq!(cache.pin(them()).unwrap(), Some(first.clone()));

        // Keys whose signature fails are never pinned.
        let other = number(444_444_444);
        let mut forged = second.clone();
        forged.signature.0[0] ^= 1;
        let (_, out) =
            run(&mut cache, &secrets, events(3, vec![EventBody::Person { person: person(other, &forged) }], 3));
        assert!(out.contains(&Update::KeyMismatch { number: other }));
        assert!(cache.person(other).unwrap().unwrap().mismatch);
        assert!(cache.pin(other).unwrap().is_none());
        assert_eq!(unsent(&cache), [them()]);
    }

    #[test]
    fn a_pin_that_wont_open_is_a_mismatch() {
        let secrets = Secrets::generate();
        let their = Secrets::generate().public_keys();
        let snapshot = Snapshot {
            cursor: 1,
            account: account("Abin"),
            people: vec![person(them(), &their)],
            chats: vec![],
            pins: vec![Pin { number: them(), pin: Some(Blob(vec![1; 80])) }],
        };
        let mut cache = MemoryCache::default();
        let (_, out) = run(&mut cache, &secrets, ServerFrame::Snapshot { snapshot });
        assert!(cache.pin(them()).unwrap().is_none());
        assert_eq!(out.iter().filter(|u| **u == Update::KeyMismatch { number: them() }).count(), 1);
        assert!(cache.person(them()).unwrap().unwrap().mismatch);

        // A later person event doesn't pin over it.
        run(&mut cache, &secrets, events(2, vec![EventBody::Person { person: person(them(), &their) }], 2));
        assert!(cache.pin(them()).unwrap().is_none());
        assert!(unsent(&cache).is_empty());
        assert!(cache.person(them()).unwrap().unwrap().mismatch);
    }

    #[test]
    fn found_keys_are_checked_like_a_persons() {
        let secrets = Secrets::generate();
        let (first, second) = (Secrets::generate().public_keys(), Secrets::generate().public_keys());
        let mut cache = cache();
        run(&mut cache, &secrets, ServerFrame::Found { number: them(), public_keys: first.clone() });
        assert_eq!(cache.pin(them()).unwrap(), Some(first.clone()));
        assert_eq!(unsent(&cache), [them()]);

        let (_, out) = run(&mut cache, &secrets, ServerFrame::Found { number: them(), public_keys: second });
        assert_eq!(out, [Update::KeyMismatch { number: them() }]);
        assert_eq!(cache.pin(them()).unwrap(), Some(first));
    }

    #[test]
    fn my_own_number_is_never_pinned() {
        let secrets = Secrets::generate();
        let mine = person(me(), &secrets.public_keys());
        let check = |cache: &MemoryCache, out: &[Update]| {
            assert!(cache.pin(me()).unwrap().is_none());
            assert!(unsent(cache).is_empty());
            assert!(!cache.person(me()).unwrap().unwrap().mismatch);
            assert!(!out.iter().any(|u| matches!(u, Update::KeyMismatch { .. })));
        };
        let mut cache = cache();
        let (_, out) = run(&mut cache, &secrets, events(1, vec![EventBody::Person { person: mine.clone() }], 1));
        check(&cache, &out);

        let snapshot =
            Snapshot { cursor: 1, account: account("Abin"), people: vec![mine], chats: vec![], pins: vec![] };
        let mut cache = MemoryCache::default();
        let (_, out) = run(&mut cache, &secrets, ServerFrame::Snapshot { snapshot });
        check(&cache, &out);
    }

    #[test]
    fn a_pin_later_in_a_replay_wins_over_new_keys() {
        let secrets = Secrets::generate();
        let (old, new) = (Secrets::generate().public_keys(), Secrets::generate().public_keys());
        let mut cache = cache();
        // Replayed from today's state: the new keys first, then the pin
        // another device made of the old ones.
        let frame =
            events(1, vec![EventBody::Person { person: person(them(), &new) }, pin_of(&secrets, them(), &old)], 2);
        let (_, out) = run(&mut cache, &secrets, frame);
        assert!(unsent(&cache).is_empty());
        assert_eq!(cache.pin(them()).unwrap(), Some(old));
        assert!(cache.person(them()).unwrap().unwrap().mismatch);
        assert_eq!(out.iter().filter(|u| **u == Update::KeyMismatch { number: them() }).count(), 1);
    }

    #[test]
    fn a_new_pin_clears_a_mismatch_only_if_it_matches() {
        let secrets = Secrets::generate();
        let (old, new, other) =
            (Secrets::generate().public_keys(), Secrets::generate().public_keys(), Secrets::generate().public_keys());
        let mut cache = cache();
        run(&mut cache, &secrets, events(1, vec![pin_of(&secrets, them(), &old)], 1));
        run(&mut cache, &secrets, events(2, vec![EventBody::Person { person: person(them(), &new) }], 2));
        assert!(cache.person(them()).unwrap().unwrap().mismatch);

        // A pin of yet other keys: still flagged, and no second warning.
        let (_, out) = run(&mut cache, &secrets, events(3, vec![pin_of(&secrets, them(), &other)], 3));
        assert!(cache.person(them()).unwrap().unwrap().mismatch);
        assert!(!out.iter().any(|u| matches!(u, Update::KeyMismatch { .. })));

        // A pin of their current keys (made after checking them) clears it.
        run(&mut cache, &secrets, events(4, vec![pin_of(&secrets, them(), &new)], 4));
        assert!(!cache.person(them()).unwrap().unwrap().mismatch);
    }

    #[test]
    fn unconfirmed_pins_survive_a_snapshot() {
        let secrets = Secrets::generate();
        let their = Secrets::generate().public_keys();
        let mut cache = cache();
        run(&mut cache, &secrets, events(1, vec![EventBody::Person { person: person(them(), &their) }], 1));
        assert_eq!(unsent(&cache), [them()]);

        let snapshot = Snapshot {
            cursor: 1,
            account: account("Abin"),
            people: vec![person(them(), &their)],
            chats: vec![],
            pins: vec![],
        };
        run(&mut cache, &secrets, ServerFrame::Snapshot { snapshot });
        assert_eq!(cache.pin(them()).unwrap(), Some(their));
        assert_eq!(unsent(&cache), [them()]);
        assert!(!cache.person(them()).unwrap().unwrap().mismatch);
    }

    #[test]
    fn acks_name_the_highest_message_from_the_other_side() {
        let secrets = Secrets::generate();
        let mut cache = cache_with_chat();
        let frame = events(
            1,
            vec![
                EventBody::Message { message: message(1, them()) },
                EventBody::Message { message: message(2, them()) },
                EventBody::Message { message: message(3, me()) },
            ],
            3,
        );
        let (acks, _) = run(&mut cache, &secrets, frame);
        assert_eq!(acks, [(CHAT, 2)]);

        // Only my own: nothing to ack.
        let (acks, _) = run(&mut cache, &secrets, events(4, vec![EventBody::Message { message: message(4, me()) }], 4));
        assert!(acks.is_empty());
    }

    #[test]
    fn sent_refused_and_echoed_messages_move_the_outbox() {
        let secrets = Secrets::generate();
        let mut cache = cache_with_chat();
        let outgoing = |id: u8| Outgoing {
            chat_id: CHAT,
            msg_id: MsgId([id; 16]),
            to: them(),
            text: "hi".into(),
            body: Some(Body(vec![1; 40])),
            new_chat: None,
            created_ms: 1,
            state: OutState::Waiting,
        };
        for id in [4, 5, 6] {
            cache.put_outgoing(outgoing(id)).unwrap();
        }
        let state = |cache: &MemoryCache, id: u8| {
            cache.outbox().unwrap().into_iter().find(|o| o.msg_id == MsgId([id; 16])).map(|o| o.state)
        };

        let sent = ServerFrame::Sent { chat_id: B64(CHAT), msg_id: B64([4; 16]), seq: 4, sent_at: 1 };
        let (_, out) = run(&mut cache, &secrets, sent);
        assert_eq!(out, [Update::Sent { chat_id: CHAT, msg_id: MsgId([4; 16]) }]);
        assert_eq!(state(&cache, 4), Some(OutState::Sent { seq: 4 }));

        let refused = ServerFrame::NotSent {
            chat_id: B64(CHAT),
            msg_id: B64([5; 16]),
            code: ErrorCode::Banned,
            message: "This account has been banned.".into(),
        };
        let (_, out) = run(&mut cache, &secrets, refused);
        let why = "This account has been banned.".to_string();
        assert_eq!(out, [Update::Refused { chat_id: CHAT, msg_id: MsgId([5; 16]), message: why.clone() }]);
        assert_eq!(state(&cache, 5), Some(OutState::Refused { code: ErrorCode::Banned, message: why }));

        // The chat already exists: the entry waits for it instead.
        let exists = ServerFrame::NotSent {
            chat_id: B64(CHAT),
            msg_id: B64([6; 16]),
            code: ErrorCode::ChatExists,
            message: "You already have a chat with them.".into(),
        };
        let (_, out) = run(&mut cache, &secrets, exists);
        assert!(out.is_empty());
        assert_eq!(state(&cache, 6), Some(OutState::NeedsChat));

        // The server echoes my own message: its entry is done.
        run(&mut cache, &secrets, events(1, vec![EventBody::Message { message: message(4, me()) }], 1));
        assert_eq!(state(&cache, 4), None);
    }

    #[test]
    fn devices_and_profile_events_are_kept() {
        let secrets = Secrets::generate();
        let mut cache = cache();
        let device = DeviceInfo {
            id: B64([3; 16]),
            kind: corvo_chat_proto::frames::DeviceKind::Cli,
            name: "laptop".into(),
            first_seen: 1,
            last_used: 2,
            this_device: true,
        };
        let frame = events(
            1,
            vec![
                EventBody::Profile { account: account("Abin B") },
                EventBody::Devices { devices: vec![device.clone()] },
            ],
            2,
        );
        let (_, out) = run(&mut cache, &secrets, frame);
        assert_eq!(out, [Update::Profile, Update::Devices]);
        assert_eq!(cache.account().unwrap().unwrap().display_name, "Abin B");
        assert_eq!(cache.devices().unwrap(), [device]);
    }

    /// A cache whose `put_message` fails at one seq, to stop a frame halfway.
    struct FailAt {
        inner: MemoryCache,
        seq: i64,
    }

    impl Cache for FailAt {
        fn cursor(&self) -> Result<Option<i64>, Error> {
            self.inner.cursor()
        }
        fn set_cursor(&mut self, cursor: i64) -> Result<(), Error> {
            self.inner.set_cursor(cursor)
        }
        fn account(&self) -> Result<Option<Account>, Error> {
            self.inner.account()
        }
        fn set_account(&mut self, account: Account) -> Result<(), Error> {
            self.inner.set_account(account)
        }
        fn person(&self, number: Number) -> Result<Option<CachedPerson>, Error> {
            self.inner.person(number)
        }
        fn put_person(&mut self, person: CachedPerson) -> Result<(), Error> {
            self.inner.put_person(person)
        }
        fn people(&self) -> Result<Vec<CachedPerson>, Error> {
            self.inner.people()
        }
        fn chat(&self, chat_id: [u8; 16]) -> Result<Option<CachedChat>, Error> {
            self.inner.chat(chat_id)
        }
        fn chat_with(&self, number: Number) -> Result<Option<CachedChat>, Error> {
            self.inner.chat_with(number)
        }
        fn put_chat(&mut self, chat: CachedChat) -> Result<(), Error> {
            self.inner.put_chat(chat)
        }
        fn chats(&self) -> Result<Vec<CachedChat>, Error> {
            self.inner.chats()
        }
        fn put_message(&mut self, message: Message) -> Result<(), Error> {
            if message.seq == self.seq {
                return Err(Error::BadServerMessage);
            }
            self.inner.put_message(message)
        }
        fn messages(&self, chat_id: [u8; 16], before: i64, limit: usize) -> Result<Vec<Message>, Error> {
            self.inner.messages(chat_id, before, limit)
        }
        fn clear_through(&mut self, chat_id: [u8; 16], seq: i64) -> Result<(), Error> {
            self.inner.clear_through(chat_id, seq)
        }
        fn pin(&self, number: Number) -> Result<Option<PublicKeys>, Error> {
            self.inner.pin(number)
        }
        fn put_pin(&mut self, number: Number, keys: Option<PublicKeys>) -> Result<(), Error> {
            self.inner.put_pin(number, keys)
        }
        fn unsent_pins(&self) -> Result<Vec<Number>, Error> {
            self.inner.unsent_pins()
        }
        fn mark_pin_unsent(&mut self, number: Number) -> Result<(), Error> {
            self.inner.mark_pin_unsent(number)
        }
        fn mark_pin_sent(&mut self, number: Number) -> Result<(), Error> {
            self.inner.mark_pin_sent(number)
        }
        fn devices(&self) -> Result<Vec<DeviceInfo>, Error> {
            self.inner.devices()
        }
        fn set_devices(&mut self, devices: Vec<DeviceInfo>) -> Result<(), Error> {
            self.inner.set_devices(devices)
        }
        fn outbox(&self) -> Result<Vec<Outgoing>, Error> {
            self.inner.outbox()
        }
        fn put_outgoing(&mut self, outgoing: Outgoing) -> Result<(), Error> {
            self.inner.put_outgoing(outgoing)
        }
        fn remove_outgoing(&mut self, chat_id: [u8; 16], msg_id: MsgId) -> Result<(), Error> {
            self.inner.remove_outgoing(chat_id, msg_id)
        }
        fn replace_all(
            &mut self,
            account: Account,
            people: Vec<CachedPerson>,
            chats: Vec<CachedChat>,
            pins: Vec<(Number, PublicKeys)>,
        ) -> Result<(), Error> {
            self.inner.replace_all(account, people, chats, pins)
        }
        fn wipe(&mut self) -> Result<(), Error> {
            self.inner.wipe()
        }
    }
}
