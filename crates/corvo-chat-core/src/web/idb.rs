//! The browser's cache. It all lives in a `MemoryCache`, so reads never wait;
//! each write changes that copy at once and is queued, in order, to one
//! background task that stores it in IndexedDB, where it outlives the page.
//!
//! Every value is the row's JSON sealed under a key made from the account's
//! data key, bound to its store and row key, so a copied browser profile holds
//! nothing readable. Rows about a person are keyed by a keyed hash of their
//! number; chat ids and seqs are the only plain parts.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};

use ::idb::builder::{DatabaseBuilder, ObjectStoreBuilder};
use ::idb::{Database, Factory, KeyRange, TransactionMode, TransactionResult};
use corvo_chat_proto::{
    Number, PublicKeys,
    chat::Message,
    frames::{Account, DeviceInfo},
};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use js_sys::Uint8Array;
use serde::{Serialize, de::DeserializeOwned};
use wasm_bindgen::{JsCast, JsValue};
use zeroize::Zeroizing;

use crate::Error;
use crate::cache::{Cache, CachedChat, CachedPerson, MemoryCache, Outgoing};
use crate::crypto::{MsgId, Secrets, open, seal, subkey, tag};

const VERSION: u32 = 1;

/// `cursor`, `account` and `devices`.
const META: &str = "meta";
const PEOPLE: &str = "people";
/// Keyed by chat id, in hex.
const CHATS: &str = "chats";
/// Keyed by `<chat id hex>:<seq zero-padded to 19>`, so a chat's rows sort by seq.
const MESSAGES: &str = "messages";
const PINS: &str = "pins";
const UNSENT_PINS: &str = "unsent_pins";
/// Keyed by `<chat id hex>:<msg id hex>`; each value keeps its place in the list.
const OUTBOX: &str = "outbox";
const STORES: [&str; 7] = [META, PEOPLE, CHATS, MESSAGES, PINS, UNSENT_PINS, OUTBOX];

const CACHE_KEY_LABEL: &[u8] = b"corvo-chat web cache v1";

impl From<::idb::Error> for Error {
    fn from(e: ::idb::Error) -> Error {
        Error::Cache(e.to_string())
    }
}

/// One change to the database.
enum Op {
    Put {
        store: &'static str,
        key: String,
        value: Vec<u8>,
    },
    Delete {
        store: &'static str,
        key: String,
    },
    /// Deletes the keys from `from` to `through`, both included.
    DeleteRange {
        store: &'static str,
        from: String,
        through: String,
    },
    Clear {
        store: &'static str,
    },
}

impl Op {
    fn store(&self) -> &'static str {
        match self {
            Op::Put { store, .. } | Op::Delete { store, .. } | Op::DeleteRange { store, .. } | Op::Clear { store } => {
                store
            }
        }
    }
}

enum Job {
    /// The changes one cache call made.
    Write(Vec<Op>),
    /// Answers once every earlier write is stored.
    Flush(oneshot::Sender<Result<(), String>>),
}

/// A cache kept in memory and copied, sealed, to an IndexedDB database.
pub struct IdbCache {
    name: String,
    key: Zeroizing<[u8; 32]>,
    memory: MemoryCache,
    /// Each outbox entry's place in the list, which its stored value keeps:
    /// IndexedDB lists rows by key, not in the order they came.
    places: HashMap<([u8; 16], MsgId), u64>,
    next_place: u64,
    writes: mpsc::UnboundedSender<Job>,
}

impl std::fmt::Debug for IdbCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IdbCache({:?})", self.name)
    }
}

impl IdbCache {
    /// Opens (creating if needed) the database `db_name` and reads all of it
    /// into memory, opening it with a key made from `secrets`. A database
    /// holding another account's rows gives `Error::Cache`, and nothing is
    /// read or written: the caller deletes it to start over.
    pub async fn open(db_name: &str, secrets: &Secrets) -> Result<IdbCache, Error> {
        let builder = STORES.into_iter().fold(DatabaseBuilder::new(db_name).version(VERSION), |b, store| {
            b.add_object_store(ObjectStoreBuilder::new(store))
        });
        // The builder also closes the database when it is to be deleted or upgraded.
        let db = builder.build().await?;
        let (writes, jobs) = mpsc::unbounded();
        let mut cache = IdbCache {
            name: db_name.to_owned(),
            key: subkey(secrets.data_key(), CACHE_KEY_LABEL),
            memory: MemoryCache::new(),
            places: HashMap::new(),
            next_place: 0,
            writes,
        };
        if let Err(e) = cache.load(&db).await {
            db.close();
            return Err(e);
        }
        wasm_bindgen_futures::spawn_local(write_all(db, jobs));
        Ok(cache)
    }

    /// Drops the whole database `db_name`. An open cache of it lets go first.
    pub async fn delete(db_name: &str) -> Result<(), Error> {
        Factory::new()?.delete(db_name)?.await?;
        Ok(())
    }

    /// The database's name, to delete it on a sign-out.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resolves once every write made so far is stored. After a write fails,
    /// the database is behind: this and every later flush give that first
    /// error, and the next `open` reads what was stored.
    pub fn flush(&self) -> impl Future<Output = Result<(), Error>> + use<> {
        let (done, stored) = oneshot::channel();
        let queued = self.writes.unbounded_send(Job::Flush(done));
        async move {
            queued.map_err(|_| stopped())?;
            stored.await.map_err(|_| stopped())?.map_err(Error::Cache)
        }
    }

    async fn load(&mut self, db: &Database) -> Result<(), Error> {
        let key: &[u8; 32] = &self.key;
        let mut outbox = Vec::new();
        for (store, rows) in STORES.into_iter().zip(read_all(db).await?) {
            for (row, sealed) in rows {
                let value = (store, row.as_str(), sealed.as_slice());
                match store {
                    META => match row.as_str() {
                        "cursor" => self.memory.set_cursor(open_json(key, value)?)?,
                        "account" => self.memory.set_account(open_json(key, value)?)?,
                        "devices" => self.memory.set_devices(open_json(key, value)?)?,
                        _ => {}
                    },
                    PEOPLE => self.memory.put_person(open_json(key, value)?)?,
                    CHATS => self.memory.put_chat(open_json(key, value)?)?,
                    MESSAGES => self.memory.put_message(open_json(key, value)?)?,
                    PINS => {
                        let (number, keys): (Number, PublicKeys) = open_json(key, value)?;
                        self.memory.put_pin(number, Some(keys))?;
                    }
                    UNSENT_PINS => self.memory.mark_pin_unsent(open_json(key, value)?)?,
                    _ => outbox.push(open_json::<(u64, Outgoing)>(key, value)?),
                }
            }
        }
        outbox.sort_by_key(|(place, _)| *place);
        for (place, outgoing) in outbox {
            self.places.insert((outgoing.chat_id, outgoing.msg_id), place);
            self.next_place = place + 1;
            self.memory.put_outgoing(outgoing)?;
        }
        Ok(())
    }

    fn put<T: Serialize + ?Sized>(&self, store: &'static str, key: String, value: &T) -> Result<Op, Error> {
        let json = Zeroizing::new(serde_json::to_vec(value).map_err(|e| Error::Cache(e.to_string()))?);
        let value = seal(&self.key, &aad(store, &key), &json);
        Ok(Op::Put { store, key, value })
    }

    fn person_row(&self, number: Number) -> String {
        hex(&tag(&self.key, b"number", number.digits().as_bytes()))
    }

    fn queue(&self, ops: Vec<Op>) -> Result<(), Error> {
        self.writes.unbounded_send(Job::Write(ops)).map_err(|_| stopped())
    }
}

fn stopped() -> Error {
    Error::Cache("the browser's database closed".into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn message_row(chat_id: [u8; 16], seq: i64) -> String {
    format!("{}:{seq:019}", hex(&chat_id))
}

fn outbox_row(chat_id: [u8; 16], msg_id: MsgId) -> String {
    format!("{}:{}", hex(&chat_id), hex(&msg_id.0))
}

/// What a sealed value is bound to: its store, then its row key.
fn aad(store: &str, row: &str) -> Vec<u8> {
    format!("{store}:{row}").into_bytes()
}

fn open_json<T: DeserializeOwned>(key: &[u8; 32], (store, row, sealed): (&str, &str, &[u8])) -> Result<T, Error> {
    let json = open(key, &aad(store, row), sealed)
        .ok_or_else(|| Error::Cache(format!("a value in {store} won't open: wrong account or a damaged database")))?;
    serde_json::from_slice(&json).map_err(|e| Error::Cache(e.to_string()))
}

/// Every row of every store, as (row key, sealed value), in `STORES` order.
async fn read_all(db: &Database) -> Result<Vec<Vec<(String, Vec<u8>)>>, Error> {
    let tx = db.transaction(&STORES, TransactionMode::ReadOnly)?;
    // Every request goes out, listened to, before any is awaited: an answer
    // can come while an earlier one is awaited.
    let mut pending = Vec::new();
    for store in STORES {
        let store = tx.object_store(store)?;
        pending.push((store.get_all_keys(None, None)?.into_future(), store.get_all(None, None)?.into_future()));
    }
    let mut stores = Vec::new();
    for (keys, values) in pending {
        let (keys, values) = (keys.await?, values.await?);
        let bad = || Error::Cache("a stored row isn't the cache's".into());
        let rows = keys
            .into_iter()
            .zip(values)
            .map(|(k, v)| Ok((k.as_string().ok_or_else(bad)?, v.dyn_into::<Uint8Array>().map_err(|_| bad())?.to_vec())))
            .collect::<Result<_, Error>>()?;
        stores.push(rows);
    }
    Ok(stores)
}

/// Stores the queued writes in order until the cache is dropped. Writes
/// queued while one is stored go together in the next transaction.
async fn write_all(db: Database, mut jobs: mpsc::UnboundedReceiver<Job>) {
    let mut failed: Option<String> = None;
    let mut next = jobs.next().await;
    while let Some(job) = next.take() {
        match job {
            Job::Flush(done) => {
                let _ = done.send(failed.clone().map_or(Ok(()), Err));
            }
            Job::Write(mut ops) => {
                while let Ok(job) = jobs.try_recv() {
                    match job {
                        Job::Write(more) => ops.extend(more),
                        flush => {
                            next = Some(flush);
                            break;
                        }
                    }
                }
                if let Err(e) = store(&db, ops).await {
                    failed.get_or_insert(e.to_string());
                }
            }
        }
        if next.is_none() {
            next = jobs.next().await;
        }
    }
    db.close();
}

/// Applies the ops in one transaction.
async fn store(db: &Database, ops: Vec<Op>) -> Result<(), Error> {
    let mut names: Vec<&str> = ops.iter().map(Op::store).collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return Ok(());
    }
    let tx = db.transaction(&names, TransactionMode::ReadWrite)?;
    // A request that fails aborts the transaction, which the commit shows.
    for op in ops {
        let store = tx.object_store(op.store())?;
        match op {
            Op::Put { key, value, .. } => {
                store.put(&Uint8Array::from(value.as_slice()).into(), Some(&JsValue::from(key)))?;
            }
            Op::Delete { key, .. } => {
                store.delete(JsValue::from(key))?;
            }
            Op::DeleteRange { from, through, .. } => {
                store.delete(KeyRange::bound(&from.into(), &through.into(), None, None)?)?;
            }
            Op::Clear { .. } => {
                store.clear()?;
            }
        }
    }
    match tx.commit()?.await? {
        TransactionResult::Committed => Ok(()),
        TransactionResult::Aborted => Err(Error::Cache("the browser didn't store a change".into())),
    }
}

impl Cache for IdbCache {
    fn cursor(&self) -> Result<Option<i64>, Error> {
        self.memory.cursor()
    }

    fn set_cursor(&mut self, cursor: i64) -> Result<(), Error> {
        let op = self.put(META, "cursor".into(), &cursor)?;
        self.memory.set_cursor(cursor)?;
        self.queue(vec![op])
    }

    fn account(&self) -> Result<Option<Account>, Error> {
        self.memory.account()
    }

    fn set_account(&mut self, account: Account) -> Result<(), Error> {
        let op = self.put(META, "account".into(), &account)?;
        self.memory.set_account(account)?;
        self.queue(vec![op])
    }

    fn person(&self, number: Number) -> Result<Option<CachedPerson>, Error> {
        self.memory.person(number)
    }

    fn put_person(&mut self, person: CachedPerson) -> Result<(), Error> {
        let op = self.put(PEOPLE, self.person_row(person.person.number), &person)?;
        self.memory.put_person(person)?;
        self.queue(vec![op])
    }

    fn people(&self) -> Result<Vec<CachedPerson>, Error> {
        self.memory.people()
    }

    fn chat(&self, chat_id: [u8; 16]) -> Result<Option<CachedChat>, Error> {
        self.memory.chat(chat_id)
    }

    fn chat_with(&self, number: Number) -> Result<Option<CachedChat>, Error> {
        self.memory.chat_with(number)
    }

    fn put_chat(&mut self, chat: CachedChat) -> Result<(), Error> {
        let op = self.put(CHATS, hex(&chat.info.chat_id.0), &chat)?;
        self.memory.put_chat(chat)?;
        self.queue(vec![op])
    }

    fn chats(&self) -> Result<Vec<CachedChat>, Error> {
        self.memory.chats()
    }

    fn put_message(&mut self, message: Message) -> Result<(), Error> {
        let op = self.put(MESSAGES, message_row(message.chat_id.0, message.seq), &message)?;
        self.memory.put_message(message)?;
        self.queue(vec![op])
    }

    fn messages(&self, chat_id: [u8; 16], before: i64, limit: usize) -> Result<Vec<Message>, Error> {
        self.memory.messages(chat_id, before, limit)
    }

    fn oldest(&self, chat_id: [u8; 16]) -> Result<Option<i64>, Error> {
        self.memory.oldest(chat_id)
    }

    fn clear_through(&mut self, chat_id: [u8; 16], seq: i64) -> Result<(), Error> {
        self.memory.clear_through(chat_id, seq)?;
        self.queue(vec![Op::DeleteRange {
            store: MESSAGES,
            from: format!("{}:", hex(&chat_id)),
            through: message_row(chat_id, seq),
        }])
    }

    fn pin(&self, number: Number) -> Result<Option<PublicKeys>, Error> {
        self.memory.pin(number)
    }

    fn put_pin(&mut self, number: Number, keys: Option<PublicKeys>) -> Result<(), Error> {
        let row = self.person_row(number);
        let op = match &keys {
            Some(keys) => self.put(PINS, row, &(number, keys))?,
            None => Op::Delete { store: PINS, key: row },
        };
        self.memory.put_pin(number, keys)?;
        self.queue(vec![op])
    }

    fn pins(&self) -> Result<Vec<(Number, PublicKeys)>, Error> {
        self.memory.pins()
    }

    fn unsent_pins(&self) -> Result<Vec<Number>, Error> {
        self.memory.unsent_pins()
    }

    fn mark_pin_unsent(&mut self, number: Number) -> Result<(), Error> {
        let op = self.put(UNSENT_PINS, self.person_row(number), &number)?;
        self.memory.mark_pin_unsent(number)?;
        self.queue(vec![op])
    }

    fn mark_pin_sent(&mut self, number: Number) -> Result<(), Error> {
        self.memory.mark_pin_sent(number)?;
        self.queue(vec![Op::Delete { store: UNSENT_PINS, key: self.person_row(number) }])
    }

    fn devices(&self) -> Result<Vec<DeviceInfo>, Error> {
        self.memory.devices()
    }

    fn set_devices(&mut self, devices: Vec<DeviceInfo>) -> Result<(), Error> {
        let op = self.put(META, "devices".into(), &devices)?;
        self.memory.set_devices(devices)?;
        self.queue(vec![op])
    }

    fn outbox(&self) -> Result<Vec<Outgoing>, Error> {
        self.memory.outbox()
    }

    fn put_outgoing(&mut self, outgoing: Outgoing) -> Result<(), Error> {
        let id = (outgoing.chat_id, outgoing.msg_id);
        let place = self.places.get(&id).copied().unwrap_or(self.next_place);
        let op = self.put(OUTBOX, outbox_row(id.0, id.1), &(place, &outgoing))?;
        self.memory.put_outgoing(outgoing)?;
        self.places.insert(id, place);
        self.next_place = self.next_place.max(place + 1);
        self.queue(vec![op])
    }

    fn remove_outgoing(&mut self, chat_id: [u8; 16], msg_id: MsgId) -> Result<(), Error> {
        self.memory.remove_outgoing(chat_id, msg_id)?;
        self.places.remove(&(chat_id, msg_id));
        self.queue(vec![Op::Delete { store: OUTBOX, key: outbox_row(chat_id, msg_id) }])
    }

    fn replace_all(
        &mut self,
        account: Account,
        people: Vec<CachedPerson>,
        chats: Vec<CachedChat>,
        pins: Vec<(Number, PublicKeys)>,
    ) -> Result<(), Error> {
        let mut ops =
            vec![Op::Delete { store: META, key: "cursor".into() }, self.put(META, "account".into(), &account)?];
        ops.extend([PEOPLE, CHATS, MESSAGES, PINS, UNSENT_PINS].map(|store| Op::Clear { store }));
        for p in &people {
            ops.push(self.put(PEOPLE, self.person_row(p.person.number), p)?);
        }
        for c in &chats {
            ops.push(self.put(CHATS, hex(&c.info.chat_id.0), c)?);
        }
        for (number, keys) in &pins {
            ops.push(self.put(PINS, self.person_row(*number), &(number, keys))?);
        }
        self.memory.replace_all(account, people, chats, pins)?;
        self.queue(ops)
    }

    fn wipe(&mut self) -> Result<(), Error> {
        self.memory.wipe()?;
        self.places.clear();
        self.next_place = 0;
        self.queue(STORES.map(|store| Op::Clear { store }).into())
    }
}
