//! What a device keeps between connections: the account's state as the server
//! last sent it, message bodies still encrypted, and the messages not yet
//! sent. `MemoryCache` keeps it in memory; the native build also has a file.

use std::collections::{BTreeMap, HashMap, HashSet};

use corvo_chat_proto::{
    Body, Number, PublicKeys,
    chat::{ChatInfo, Message, NewChat, Person},
    frames::{Account, DeviceInfo, ErrorCode},
};

use crate::Error;
use crate::crypto::MsgId;

/// A chat as the server last sent it. `complete` means every older message
/// it shows is already cached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedChat {
    pub info: ChatInfo,
    pub complete: bool,
}

/// Someone as the server last sent them. `mismatch` means their keys don't
/// match the pinned ones (or never checked out): nothing is sealed to them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedPerson {
    pub person: Person,
    pub mismatch: bool,
}

/// A message of mine that the server hasn't echoed back yet. `body` and
/// `new_chat` are what was (or will be) sent; `None` until it's encrypted.
#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Drops the chat's messages up to and including `seq`.
    fn clear_through(&mut self, chat_id: [u8; 16], seq: i64) -> Result<(), Error>;

    /// The keys pinned for a number, opened.
    fn pin(&self, number: Number) -> Result<Option<PublicKeys>, Error>;
    /// Pins keys for a number; `None` removes the pin.
    fn put_pin(&mut self, number: Number, keys: Option<PublicKeys>) -> Result<(), Error>;
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

/// A cache that lives only as long as the client, on every platform.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoryCache {
    cursor: Option<i64>,
    account: Option<Account>,
    people: HashMap<Number, CachedPerson>,
    chats: BTreeMap<[u8; 16], CachedChat>,
    messages: BTreeMap<[u8; 16], BTreeMap<i64, Message>>,
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
