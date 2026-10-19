//! What a device keeps between connections: the account's state as the server
//! last sent it, message bodies still encrypted, and the messages not yet
//! sent. `MemoryCache` keeps it in memory; the native build also has a file.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use corvo_chat_proto::{
    Body, Number, PublicKeys,
    chat::{ChatInfo, Message, NewChat, Person},
    frames::{Account, DeviceInfo, ErrorCode},
};

use crate::Error;
use crate::crypto::MsgId;

#[cfg(feature = "native")]
mod sqlite;
#[cfg(feature = "native")]
pub use sqlite::SqliteCache;

#[cfg(any(test, feature = "web"))]
#[doc(hidden)]
pub use behaviour::exercise_cache;

/// A chat as the server last sent it. `complete` means every older message
/// it shows is already cached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedChat {
    pub info: ChatInfo,
    pub complete: bool,
}

/// Someone as the server last sent them. `mismatch` means their keys don't
/// match the pinned ones (or never checked out): nothing is sealed to them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPerson {
    pub person: Person,
    pub mismatch: bool,
}

/// A message of mine that the server hasn't echoed back yet. `body` and
/// `new_chat` are what was (or will be) sent; `None` until it's encrypted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outgoing {
    pub chat_id: [u8; 16],
    pub msg_id: MsgId,
    pub to: Number,
    pub text: String,
    pub body: Option<Body>,
    pub new_chat: Option<NewChat>,
    pub created_ms: u64,
    pub state: OutState,
}

impl Outgoing {
    /// The seq the server gave it, once `sent`.
    pub fn seq(&self) -> Option<i64> {
        match self.state {
            OutState::Sent { seq } => Some(seq),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutState {
    /// Not sent yet, or sent with no answer.
    Waiting,
    /// Its new chat lost to one that already exists; it moves there once
    /// that chat is cached.
    NeedsChat,
    Sent {
        seq: i64,
    },
    Refused {
        code: ErrorCode,
        message: String,
    },
}

/// Where a client keeps its state. Every method can fail, since a file can.
pub trait Cache {
    /// The last event log seq this device has, `None` before its first sync.
    fn cursor(&self) -> Result<Option<i64>, Error>;
    fn set_cursor(&mut self, cursor: i64) -> Result<(), Error>;

    fn account(&self) -> Result<Option<Account>, Error>;
    fn set_account(&mut self, account: Account) -> Result<(), Error>;

    fn person(&self, number: Number) -> Result<Option<CachedPerson>, Error>;
    fn put_person(&mut self, person: CachedPerson) -> Result<(), Error>;
    fn people(&self) -> Result<Vec<CachedPerson>, Error>;

    fn chat(&self, chat_id: [u8; 16]) -> Result<Option<CachedChat>, Error>;
    /// The chat with this person, if there is one (one per pair).
    fn chat_with(&self, number: Number) -> Result<Option<CachedChat>, Error>;
    fn put_chat(&mut self, chat: CachedChat) -> Result<(), Error>;
    fn chats(&self) -> Result<Vec<CachedChat>, Error>;

    /// Stores a message, replacing any with the same chat id and seq.
    fn put_message(&mut self, message: Message) -> Result<(), Error>;
    /// Up to `limit` messages before `before`, the newest of them, oldest first.
    fn messages(&self, chat_id: [u8; 16], before: i64, limit: usize) -> Result<Vec<Message>, Error>;
    /// The seq of the chat's oldest cached message.
    fn oldest(&self, chat_id: [u8; 16]) -> Result<Option<i64>, Error> {
        Ok(self.messages(chat_id, i64::MAX, usize::MAX)?.first().map(|m| m.seq))
    }
    /// Drops the chat's messages up to and including `seq`.
    fn clear_through(&mut self, chat_id: [u8; 16], seq: i64) -> Result<(), Error>;

    /// The keys pinned for a number, opened.
    fn pin(&self, number: Number) -> Result<Option<PublicKeys>, Error>;
    /// Pins keys for a number; `None` removes the pin.
    fn put_pin(&mut self, number: Number, keys: Option<PublicKeys>) -> Result<(), Error>;
    /// Every pin, by number.
    fn pins(&self) -> Result<Vec<(Number, PublicKeys)>, Error>;
    /// Pins made here that the server hasn't echoed back yet, to send again.
    fn unsent_pins(&self) -> Result<Vec<Number>, Error>;
    fn mark_pin_unsent(&mut self, number: Number) -> Result<(), Error>;
    /// The server holds this pin now (or never will): stop sending it.
    fn mark_pin_sent(&mut self, number: Number) -> Result<(), Error>;

    fn devices(&self) -> Result<Vec<DeviceInfo>, Error>;
    fn set_devices(&mut self, devices: Vec<DeviceInfo>) -> Result<(), Error>;

    /// Messages not yet echoed back, oldest first.
    fn outbox(&self) -> Result<Vec<Outgoing>, Error>;
    /// Adds an entry, or replaces the one with the same chat id and message id.
    fn put_outgoing(&mut self, outgoing: Outgoing) -> Result<(), Error>;
    fn remove_outgoing(&mut self, chat_id: [u8; 16], msg_id: MsgId) -> Result<(), Error>;

    /// Starts over from a snapshot: everything goes but the outbox and the
    /// devices, and the cursor is unset until `synced`. No pin is left unsent;
    /// the caller marks again any it keeps.
    fn replace_all(
        &mut self,
        account: Account,
        people: Vec<CachedPerson>,
        chats: Vec<CachedChat>,
        pins: Vec<(Number, PublicKeys)>,
    ) -> Result<(), Error>;
    /// Forgets everything, for a sign-out.
    fn wipe(&mut self) -> Result<(), Error>;
}

/// Maps keyed by a number or a chat id as lists of pairs: JSON keys are text.
mod pairs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<'a, M, K, V, S>(map: &'a M, serializer: S) -> Result<S::Ok, S::Error>
    where
        &'a M: IntoIterator<Item = (&'a K, &'a V)>,
        K: Serialize + 'a,
        V: Serialize + 'a,
        S: Serializer,
    {
        serializer.collect_seq(map)
    }

    pub fn deserialize<'de, M, K, V, D>(deserializer: D) -> Result<M, D::Error>
    where
        M: FromIterator<(K, V)>,
        K: Deserialize<'de>,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Ok(Vec::<(K, V)>::deserialize(deserializer)?.into_iter().collect())
    }
}

/// A cache that lives only as long as the client, on every platform.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryCache {
    cursor: Option<i64>,
    account: Option<Account>,
    #[serde(with = "pairs")]
    people: HashMap<Number, CachedPerson>,
    #[serde(with = "pairs")]
    chats: BTreeMap<[u8; 16], CachedChat>,
    #[serde(with = "pairs")]
    messages: BTreeMap<[u8; 16], BTreeMap<i64, Message>>,
    #[serde(with = "pairs")]
    pins: HashMap<Number, PublicKeys>,
    unsent_pins: HashSet<Number>,
    devices: Vec<DeviceInfo>,
    outbox: Vec<Outgoing>,
}

impl MemoryCache {
    pub fn new() -> MemoryCache {
        MemoryCache::default()
    }
}

impl Cache for MemoryCache {
    fn cursor(&self) -> Result<Option<i64>, Error> {
        Ok(self.cursor)
    }

    fn set_cursor(&mut self, cursor: i64) -> Result<(), Error> {
        self.cursor = Some(cursor);
        Ok(())
    }

    fn account(&self) -> Result<Option<Account>, Error> {
        Ok(self.account.clone())
    }

    fn set_account(&mut self, account: Account) -> Result<(), Error> {
        self.account = Some(account);
        Ok(())
    }

    fn person(&self, number: Number) -> Result<Option<CachedPerson>, Error> {
        Ok(self.people.get(&number).cloned())
    }

    fn put_person(&mut self, person: CachedPerson) -> Result<(), Error> {
        self.people.insert(person.person.number, person);
        Ok(())
    }

    fn people(&self) -> Result<Vec<CachedPerson>, Error> {
        let mut people: Vec<CachedPerson> = self.people.values().cloned().collect();
        people.sort_by_key(|p| p.person.number.digits());
        Ok(people)
    }

    fn chat(&self, chat_id: [u8; 16]) -> Result<Option<CachedChat>, Error> {
        Ok(self.chats.get(&chat_id).cloned())
    }

    fn chat_with(&self, number: Number) -> Result<Option<CachedChat>, Error> {
        Ok(self.chats.values().find(|c| c.info.with == number).cloned())
    }

    fn put_chat(&mut self, chat: CachedChat) -> Result<(), Error> {
        self.chats.insert(chat.info.chat_id.0, chat);
        Ok(())
    }

    fn chats(&self) -> Result<Vec<CachedChat>, Error> {
        Ok(self.chats.values().cloned().collect())
    }

    fn put_message(&mut self, message: Message) -> Result<(), Error> {
        self.messages.entry(message.chat_id.0).or_default().insert(message.seq, message);
        Ok(())
    }

    fn messages(&self, chat_id: [u8; 16], before: i64, limit: usize) -> Result<Vec<Message>, Error> {
        let Some(chat) = self.messages.get(&chat_id) else { return Ok(Vec::new()) };
        let mut page: Vec<Message> = chat.range(..before).rev().take(limit).map(|(_, m)| m.clone()).collect();
        page.reverse();
        Ok(page)
    }

    fn oldest(&self, chat_id: [u8; 16]) -> Result<Option<i64>, Error> {
        Ok(self.messages.get(&chat_id).and_then(|chat| chat.keys().next().copied()))
    }

    fn clear_through(&mut self, chat_id: [u8; 16], seq: i64) -> Result<(), Error> {
        if let Some(chat) = self.messages.get_mut(&chat_id) {
            chat.retain(|s, _| *s > seq);
        }
        Ok(())
    }

    fn pin(&self, number: Number) -> Result<Option<PublicKeys>, Error> {
        Ok(self.pins.get(&number).cloned())
    }

    fn put_pin(&mut self, number: Number, keys: Option<PublicKeys>) -> Result<(), Error> {
        match keys {
            Some(keys) => self.pins.insert(number, keys),
            None => self.pins.remove(&number),
        };
        Ok(())
    }

    fn pins(&self) -> Result<Vec<(Number, PublicKeys)>, Error> {
        let mut pins: Vec<(Number, PublicKeys)> = self.pins.iter().map(|(n, k)| (*n, k.clone())).collect();
        pins.sort_by_key(|(n, _)| n.digits());
        Ok(pins)
    }

    fn unsent_pins(&self) -> Result<Vec<Number>, Error> {
        let mut numbers: Vec<Number> = self.unsent_pins.iter().copied().collect();
        numbers.sort_by_key(Number::digits);
        Ok(numbers)
    }

    fn mark_pin_unsent(&mut self, number: Number) -> Result<(), Error> {
        self.unsent_pins.insert(number);
        Ok(())
    }

    fn mark_pin_sent(&mut self, number: Number) -> Result<(), Error> {
        self.unsent_pins.remove(&number);
        Ok(())
    }

    fn devices(&self) -> Result<Vec<DeviceInfo>, Error> {
        Ok(self.devices.clone())
    }

    fn set_devices(&mut self, devices: Vec<DeviceInfo>) -> Result<(), Error> {
        self.devices = devices;
        Ok(())
    }

    fn outbox(&self) -> Result<Vec<Outgoing>, Error> {
        Ok(self.outbox.clone())
    }

    fn put_outgoing(&mut self, outgoing: Outgoing) -> Result<(), Error> {
        match self.outbox.iter_mut().find(|o| o.chat_id == outgoing.chat_id && o.msg_id == outgoing.msg_id) {
            Some(entry) => *entry = outgoing,
            None => self.outbox.push(outgoing),
        }
        Ok(())
    }

    fn remove_outgoing(&mut self, chat_id: [u8; 16], msg_id: MsgId) -> Result<(), Error> {
        self.outbox.retain(|o| !(o.chat_id == chat_id && o.msg_id == msg_id));
        Ok(())
    }

    fn replace_all(
        &mut self,
        account: Account,
        people: Vec<CachedPerson>,
        chats: Vec<CachedChat>,
        pins: Vec<(Number, PublicKeys)>,
    ) -> Result<(), Error> {
        *self = MemoryCache {
            cursor: None,
            account: Some(account),
            people: people.into_iter().map(|p| (p.person.number, p)).collect(),
            chats: chats.into_iter().map(|c| (c.info.chat_id.0, c)).collect(),
            messages: BTreeMap::new(),
            pins: pins.into_iter().collect(),
            unsent_pins: HashSet::new(),
            devices: std::mem::take(&mut self.devices),
            outbox: std::mem::take(&mut self.outbox),
        };
        Ok(())
    }

    fn wipe(&mut self) -> Result<(), Error> {
        *self = MemoryCache::default();
        Ok(())
    }
}

/// The shared behaviour test, for every cache here and the browser's.
#[cfg(any(test, feature = "web"))]
mod behaviour {
    use corvo_chat_proto::{
        B64, Blob,
        chat::{Marks, Relation},
        frames::DeviceKind,
    };

    use super::*;
    use crate::crypto::Secrets;

    const CHAT: [u8; 16] = [7; 16];
    const OTHER_CHAT: [u8; 16] = [8; 16];

    fn number(body: u32) -> Number {
        Number::from_body(body).unwrap()
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

    fn person(number: Number, name: &str, mismatch: bool) -> CachedPerson {
        CachedPerson {
            person: Person {
                number,
                display_name: Some(name.into()),
                public_keys: Secrets::generate().public_keys(),
                relation: Relation::Contact,
                deleted: false,
            },
            mismatch,
        }
    }

    fn message(chat_id: [u8; 16], seq: i64, fill: u8) -> Message {
        Message {
            chat_id: B64(chat_id),
            seq,
            msg_id: B64([seq as u8; 16]),
            from: number(222_222_222),
            body: Body(vec![fill; 40]),
            sent_at: 1_800_000_000 + seq,
        }
    }

    fn outgoing(chat_id: [u8; 16], id: u8, text: &str) -> Outgoing {
        Outgoing {
            chat_id,
            msg_id: MsgId([id; 16]),
            to: number(222_222_222),
            text: text.into(),
            body: None,
            new_chat: None,
            created_ms: 1_800_000_000_000 + u64::from(id),
            state: OutState::Waiting,
        }
    }

    fn device(id: u8, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: B64([id; 16]),
            kind: DeviceKind::Cli,
            name: name.into(),
            first_seen: 1,
            last_used: 2,
            this_device: id == 1,
        }
    }

    fn seqs(cache: &dyn Cache, before: i64, limit: usize) -> Vec<i64> {
        cache.messages(CHAT, before, limit).unwrap().iter().map(|m| m.seq).collect()
    }

    /// Every behaviour a cache must have, so each one is held to the same.
    /// It starts from an empty cache and panics at the first difference.
    pub fn exercise_cache(cache: &mut dyn Cache) {
        let (a, b) = (number(222_222_222), number(333_333_333));

        // Empty to begin with.
        assert_eq!(cache.cursor().unwrap(), None);
        assert_eq!(cache.account().unwrap(), None);
        assert!(cache.people().unwrap().is_empty());
        assert!(cache.chats().unwrap().is_empty());
        assert!(cache.devices().unwrap().is_empty());
        assert!(cache.outbox().unwrap().is_empty());
        assert_eq!(cache.pin(a).unwrap(), None);
        assert!(cache.messages(CHAT, i64::MAX, 10).unwrap().is_empty());

        cache.set_cursor(5).unwrap();
        cache.set_cursor(9).unwrap();
        assert_eq!(cache.cursor().unwrap(), Some(9));
        cache.set_account(account("Abin")).unwrap();
        cache.set_account(account("Abin S")).unwrap();
        assert_eq!(cache.account().unwrap(), Some(account("Abin S")));

        // People, listed by number.
        let (pa, pb) = (person(a, "Rahul", false), person(b, "Mira", true));
        cache.put_person(pb.clone()).unwrap();
        cache.put_person(pa.clone()).unwrap();
        assert_eq!(cache.person(a).unwrap(), Some(pa.clone()));
        assert_eq!(cache.person(number(444_444_444)).unwrap(), None);
        assert_eq!(cache.people().unwrap(), vec![pa.clone(), pb.clone()]);
        let renamed = person(a, "Rahul K", true);
        cache.put_person(renamed.clone()).unwrap();
        assert_eq!(cache.people().unwrap(), vec![renamed, pb]);

        // Chats, found by id or by the other person.
        cache.put_chat(chat(OTHER_CHAT, b, 4)).unwrap();
        cache.put_chat(chat(CHAT, a, 2)).unwrap();
        cache.put_chat(chat(CHAT, a, 3)).unwrap();
        assert_eq!(cache.chat(CHAT).unwrap(), Some(chat(CHAT, a, 3)));
        assert_eq!(cache.chat([9; 16]).unwrap(), None);
        assert_eq!(cache.chat_with(b).unwrap(), Some(chat(OTHER_CHAT, b, 4)));
        assert_eq!(cache.chat_with(number(444_444_444)).unwrap(), None);
        assert_eq!(cache.chats().unwrap(), vec![chat(CHAT, a, 3), chat(OTHER_CHAT, b, 4)]);

        // Messages: the newest page below `before`, oldest first.
        for seq in [3, 1, 5, 2, 4] {
            cache.put_message(message(CHAT, seq, 0)).unwrap();
        }
        cache.put_message(message(OTHER_CHAT, 1, 0)).unwrap();
        assert_eq!(seqs(cache, i64::MAX, 100), vec![1, 2, 3, 4, 5]);
        assert_eq!(cache.oldest(CHAT).unwrap(), Some(1));
        assert_eq!(cache.oldest([9; 16]).unwrap(), None);
        assert_eq!(seqs(cache, i64::MAX, 2), vec![4, 5]);
        assert_eq!(seqs(cache, 4, 2), vec![2, 3]);
        assert_eq!(seqs(cache, 2, 5), vec![1]);
        assert_eq!(seqs(cache, 1, 5), Vec::<i64>::new());
        assert_eq!(seqs(cache, i64::MAX, 0), Vec::<i64>::new());
        cache.put_message(message(CHAT, 3, 9)).unwrap();
        assert_eq!(cache.messages(CHAT, 4, 1).unwrap(), vec![message(CHAT, 3, 9)]);
        cache.clear_through(CHAT, 2).unwrap();
        assert_eq!(seqs(cache, i64::MAX, 100), vec![3, 4, 5]);
        assert_eq!(cache.oldest(CHAT).unwrap(), Some(3));
        assert_eq!(cache.messages(OTHER_CHAT, i64::MAX, 100).unwrap().len(), 1);
        cache.clear_through([9; 16], 2).unwrap();

        // Pins, and the ones not yet sent.
        let keys = Secrets::generate().public_keys();
        cache.put_pin(a, Some(keys.clone())).unwrap();
        assert_eq!(cache.pin(a).unwrap(), Some(keys.clone()));
        assert_eq!(cache.pin(b).unwrap(), None);
        let other_keys = Secrets::generate().public_keys();
        cache.put_pin(b, Some(other_keys.clone())).unwrap();
        assert_eq!(cache.pins().unwrap(), vec![(a, keys.clone()), (b, other_keys)]);
        cache.put_pin(b, None).unwrap();
        assert_eq!(cache.pins().unwrap(), vec![(a, keys.clone())]);
        cache.mark_pin_unsent(b).unwrap();
        cache.mark_pin_unsent(a).unwrap();
        cache.mark_pin_unsent(a).unwrap();
        assert_eq!(cache.unsent_pins().unwrap(), vec![a, b]);
        cache.put_pin(a, None).unwrap();
        assert_eq!(cache.pin(a).unwrap(), None);
        assert_eq!(cache.unsent_pins().unwrap(), vec![a, b]);
        cache.mark_pin_sent(a).unwrap();
        cache.mark_pin_sent(a).unwrap();
        assert_eq!(cache.unsent_pins().unwrap(), vec![b]);

        // Devices.
        cache.set_devices(vec![device(1, "laptop"), device(2, "phone")]).unwrap();
        assert_eq!(cache.devices().unwrap(), vec![device(1, "laptop"), device(2, "phone")]);
        cache.set_devices(vec![device(1, "laptop")]).unwrap();
        assert_eq!(cache.devices().unwrap(), vec![device(1, "laptop")]);

        // The outbox keeps its order; replacing keeps the place.
        cache.put_outgoing(outgoing(CHAT, 2, "second")).unwrap();
        cache.put_outgoing(outgoing(CHAT, 1, "first")).unwrap();
        cache.put_outgoing(outgoing(OTHER_CHAT, 1, "other")).unwrap();
        let mut sent = outgoing(CHAT, 2, "second");
        sent.state = OutState::Sent { seq: 6 };
        sent.body = Some(Body(vec![1, 2, 3]));
        sent.new_chat = Some(NewChat {
            number: a,
            my_key: Blob(vec![4; 104]),
            their_key: Blob(vec![5; 104]),
            signature: B64([6; 64]),
        });
        cache.put_outgoing(sent.clone()).unwrap();
        let mut refused = outgoing(CHAT, 1, "first");
        refused.state = OutState::Refused { code: ErrorCode::TryAgain, message: "Later.".into() };
        cache.put_outgoing(refused.clone()).unwrap();
        let mut needs = outgoing(OTHER_CHAT, 1, "other");
        needs.state = OutState::NeedsChat;
        cache.put_outgoing(needs.clone()).unwrap();
        assert_eq!(cache.outbox().unwrap(), vec![sent.clone(), refused.clone(), needs.clone()]);
        cache.remove_outgoing(CHAT, MsgId([2; 16])).unwrap();
        cache.remove_outgoing(CHAT, MsgId([2; 16])).unwrap();
        assert_eq!(cache.outbox().unwrap(), vec![refused.clone(), needs.clone()]);

        // A snapshot keeps the outbox and the devices, and drops the rest.
        cache.mark_pin_unsent(b).unwrap();
        cache
            .replace_all(account("New"), vec![pa.clone()], vec![chat(OTHER_CHAT, b, 8)], vec![(b, keys.clone())])
            .unwrap();
        assert_eq!(cache.cursor().unwrap(), None);
        assert_eq!(cache.account().unwrap(), Some(account("New")));
        assert_eq!(cache.people().unwrap(), vec![pa]);
        assert_eq!(cache.chats().unwrap(), vec![chat(OTHER_CHAT, b, 8)]);
        assert_eq!(cache.pin(b).unwrap(), Some(keys.clone()));
        assert_eq!(cache.pins().unwrap(), vec![(b, keys)]);
        assert!(cache.unsent_pins().unwrap().is_empty());
        assert!(cache.messages(CHAT, i64::MAX, 100).unwrap().is_empty());
        assert!(cache.messages(OTHER_CHAT, i64::MAX, 100).unwrap().is_empty());
        assert_eq!(cache.devices().unwrap(), vec![device(1, "laptop")]);
        assert_eq!(cache.outbox().unwrap(), vec![refused, needs]);

        // A wipe forgets it all.
        cache.set_cursor(3).unwrap();
        cache.mark_pin_unsent(a).unwrap();
        cache.wipe().unwrap();
        assert_eq!(cache.cursor().unwrap(), None);
        assert_eq!(cache.account().unwrap(), None);
        assert!(cache.people().unwrap().is_empty());
        assert!(cache.chats().unwrap().is_empty());
        assert_eq!(cache.pin(b).unwrap(), None);
        assert!(cache.pins().unwrap().is_empty());
        assert!(cache.unsent_pins().unwrap().is_empty());
        assert!(cache.devices().unwrap().is_empty());
        assert!(cache.outbox().unwrap().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_cache_behaves() {
        exercise_cache(&mut MemoryCache::new());
    }

    #[cfg(feature = "native")]
    #[test]
    fn sqlite_cache_behaves() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = SqliteCache::open(&dir.path().join("cache.db"), &crate::Secrets::generate()).unwrap();
        exercise_cache(&mut cache);
    }
}
