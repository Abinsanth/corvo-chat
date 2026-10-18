//! The cache in a SQLite file. Every value is sealed under a key made from the
//! account's data key, so the file holds nothing readable: people are found by
//! a keyed hash of their number, and chat ids and seqs are the only plain parts.

use std::path::Path;

use corvo_chat_proto::{
    Body, Number, PublicKeys,
    chat::{Message, NewChat},
    frames::{Account, DeviceInfo, ErrorCode},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::Zeroizing;

use super::{Cache, CachedChat, CachedPerson, OutState, Outgoing};
use crate::Error;
use crate::crypto::{MsgId, Secrets, open, seal, subkey, tag};

/// Migration 1: every table. Values are sealed blobs; `person` and `with_key`
/// are keyed hashes of a number.
const MIGRATIONS: &[&str] = &["
CREATE TABLE meta (key TEXT PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE people (person BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE chats (chat_id BLOB PRIMARY KEY, with_key BLOB NOT NULL, value BLOB NOT NULL) WITHOUT ROWID;
CREATE INDEX chats_by_person ON chats (with_key);
CREATE TABLE messages (
  chat_id BLOB NOT NULL, seq INTEGER NOT NULL, value BLOB NOT NULL,
  PRIMARY KEY (chat_id, seq)
) WITHOUT ROWID;
CREATE TABLE pins (person BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE unsent_pins (person BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE outbox (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  chat_id BLOB NOT NULL, msg_id BLOB NOT NULL, value BLOB NOT NULL,
  UNIQUE (chat_id, msg_id)
);
"];

const CACHE_KEY_LABEL: &[u8] = b"corvo-chat cache v1";

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::Cache(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Error {
        Error::Cache(e.to_string())
    }
}

/// What an outbox entry keeps in its sealed value; the ids stay in columns.
#[derive(Serialize, Deserialize)]
struct StoredOutgoing {
    to: Number,
    text: String,
    body: Option<Body>,
    new_chat: Option<NewChat>,
    created_ms: u64,
    state: StoredState,
}

#[derive(Serialize, Deserialize)]
enum StoredState {
    Waiting,
    NeedsChat,
    Sent { seq: i64 },
    Refused { code: ErrorCode, message: String },
}

impl From<&OutState> for StoredState {
    fn from(state: &OutState) -> StoredState {
        match state {
            OutState::Waiting => StoredState::Waiting,
            OutState::NeedsChat => StoredState::NeedsChat,
            OutState::Sent { seq } => StoredState::Sent { seq: *seq },
            OutState::Refused { code, message } => StoredState::Refused { code: *code, message: message.clone() },
        }
    }
}

impl From<StoredState> for OutState {
    fn from(state: StoredState) -> OutState {
        match state {
            StoredState::Waiting => OutState::Waiting,
            StoredState::NeedsChat => OutState::NeedsChat,
            StoredState::Sent { seq } => OutState::Sent { seq },
            StoredState::Refused { code, message } => OutState::Refused { code, message },
        }
    }
}

/// A cache kept in an encrypted SQLite file.
pub struct SqliteCache {
    conn: Connection,
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for SqliteCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SqliteCache(…)")
    }
}

/// Applies the migrations not yet applied, tracked in `user_version`.
fn migrate(conn: &mut Connection) -> Result<(), Error> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > MIGRATIONS.len() as i64 {
        return Err(Error::Cache("the file is from a newer corvo-chat".into()));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// The id of a row keyed by a number: a keyed hash, so the file doesn't list them.
fn person_key(key: &[u8; 32], number: Number) -> [u8; 32] {
    tag(key, b"number", number.digits().as_bytes())
}

/// Where a sealed value belongs: its table, then its row key.
fn place(table: &str, row: &[u8]) -> Vec<u8> {
    [table.as_bytes(), row].concat()
}

fn seal_json<T: Serialize>(key: &[u8; 32], table: &str, row: &[u8], value: &T) -> Result<Vec<u8>, Error> {
    let json = Zeroizing::new(serde_json::to_vec(value)?);
    Ok(seal(key, &place(table, row), &json))
}

fn open_json<T: DeserializeOwned>(key: &[u8; 32], table: &str, row: &[u8], sealed: &[u8]) -> Result<T, Error> {
    let json = open(key, &place(table, row), sealed)
        .ok_or_else(|| Error::Cache(format!("a value in {table} won't open: wrong account or a damaged file")))?;
    Ok(serde_json::from_slice(&json)?)
}

fn seq_bytes(seq: i64) -> [u8; 8] {
    seq.to_be_bytes()
}

impl SqliteCache {
    /// Opens (creating if needed) the file at `path`, sealing with a key made
    /// from `secrets`.
    pub fn open(path: &Path, secrets: &Secrets) -> Result<SqliteCache, Error> {
        let mut conn = Connection::open(path)?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        migrate(&mut conn)?;
        Ok(SqliteCache { conn, key: subkey(secrets.data_key(), CACHE_KEY_LABEL) })
    }

    fn seal<T: Serialize>(&self, table: &str, row: &[u8], value: &T) -> Result<Vec<u8>, Error> {
        seal_json(&self.key, table, row, value)
    }

    fn open_value<T: DeserializeOwned>(&self, table: &str, row: &[u8], sealed: &[u8]) -> Result<T, Error> {
        open_json(&self.key, table, row, sealed)
    }

    fn person_key(&self, number: Number) -> [u8; 32] {
        person_key(&self.key, number)
    }

    fn meta<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, Error> {
        let sealed: Option<Vec<u8>> =
            self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [name], |r| r.get(0)).optional()?;
        sealed.map(|s| self.open_value("meta", name.as_bytes(), &s)).transpose()
    }

    fn set_meta<T: Serialize>(&self, name: &str, value: &T) -> Result<(), Error> {
        let sealed = self.seal("meta", name.as_bytes(), value)?;
        self.conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![name, sealed])?;
        Ok(())
    }

    fn chat_where(&self, sql: &str, arg: &[u8]) -> Result<Option<CachedChat>, Error> {
        let row: Option<(Vec<u8>, Vec<u8>)> =
            self.conn.query_row(sql, [arg], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        row.map(|(id, sealed)| self.open_value("chats", &id, &sealed)).transpose()
    }

    fn stored_outgoing(&self, chat_id: &[u8], msg_id: &[u8], sealed: &[u8]) -> Result<Outgoing, Error> {
        let row = [chat_id, msg_id].concat();
        let s: StoredOutgoing = self.open_value("outbox", &row, sealed)?;
        let bad = || Error::Cache("an outbox row has a bad id".into());
        Ok(Outgoing {
            chat_id: chat_id.try_into().map_err(|_| bad())?,
            msg_id: MsgId(msg_id.try_into().map_err(|_| bad())?),
            to: s.to,
            text: s.text,
            body: s.body,
            new_chat: s.new_chat,
            created_ms: s.created_ms,
            state: s.state.into(),
        })
    }

    /// Writes the pins and chats and so on of a snapshot, in one transaction.
    fn write_snapshot(
        &mut self,
        account: &Account,
        people: &[CachedPerson],
        chats: &[CachedChat],
        pins: &[(Number, PublicKeys)],
    ) -> Result<(), Error> {
        let key = self.key.clone();
        let tx = self.conn.transaction()?;
        for table in ["people", "chats", "messages", "pins", "unsent_pins"] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.execute("DELETE FROM meta WHERE key = 'cursor'", [])?;
        put_meta(&tx, &key, "account", account)?;
        for p in people {
            let row = person_key(&key, p.person.number);
            tx.execute(
                "INSERT OR REPLACE INTO people (person, value) VALUES (?1, ?2)",
                params![row, seal_json(&key, "people", &row, p)?],
            )?;
        }
        for c in chats {
            insert_chat(&tx, &key, c)?;
        }
        for (number, keys) in pins {
            let row = person_key(&key, *number);
            tx.execute(
                "INSERT OR REPLACE INTO pins (person, value) VALUES (?1, ?2)",
                params![row, seal_json(&key, "pins", &row, keys)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn put_meta<T: Serialize>(tx: &Transaction, key: &[u8; 32], name: &str, value: &T) -> Result<(), Error> {
    let sealed = seal_json(key, "meta", name.as_bytes(), value)?;
    tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![name, sealed])?;
    Ok(())
}

fn insert_chat(tx: &Transaction, key: &[u8; 32], chat: &CachedChat) -> Result<(), Error> {
    let id = chat.info.chat_id.0;
    tx.execute(
        "INSERT OR REPLACE INTO chats (chat_id, with_key, value) VALUES (?1, ?2, ?3)",
        params![id, person_key(key, chat.info.with), seal_json(key, "chats", &id, chat)?],
    )?;
    Ok(())
}

impl Cache for SqliteCache {
    fn cursor(&self) -> Result<Option<i64>, Error> {
        self.meta("cursor")
    }

    fn set_cursor(&mut self, cursor: i64) -> Result<(), Error> {
        self.set_meta("cursor", &cursor)
    }

    fn account(&self) -> Result<Option<Account>, Error> {
        self.meta("account")
    }

    fn set_account(&mut self, account: Account) -> Result<(), Error> {
        self.set_meta("account", &account)
    }

    fn person(&self, number: Number) -> Result<Option<CachedPerson>, Error> {
        let row = self.person_key(number);
        let sealed: Option<Vec<u8>> =
            self.conn.query_row("SELECT value FROM people WHERE person = ?1", [row], |r| r.get(0)).optional()?;
        sealed.map(|s| self.open_value("people", &row, &s)).transpose()
    }

    fn put_person(&mut self, person: CachedPerson) -> Result<(), Error> {
        let row = self.person_key(person.person.number);
        let sealed = self.seal("people", &row, &person)?;
        self.conn.execute("INSERT OR REPLACE INTO people (person, value) VALUES (?1, ?2)", params![row, sealed])?;
        Ok(())
    }

    fn people(&self) -> Result<Vec<CachedPerson>, Error> {
        let mut stmt = self.conn.prepare("SELECT person, value FROM people")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut people = Vec::new();
        for row in rows {
            let (id, sealed) = row?;
            people.push(self.open_value::<CachedPerson>("people", &id, &sealed)?);
        }
        people.sort_by_key(|p| p.person.number.digits());
        Ok(people)
    }

    fn chat(&self, chat_id: [u8; 16]) -> Result<Option<CachedChat>, Error> {
        self.chat_where("SELECT chat_id, value FROM chats WHERE chat_id = ?1", &chat_id)
    }

    fn chat_with(&self, number: Number) -> Result<Option<CachedChat>, Error> {
        let with = self.person_key(number);
        self.chat_where("SELECT chat_id, value FROM chats WHERE with_key = ?1 ORDER BY chat_id LIMIT 1", &with)
    }

    fn put_chat(&mut self, chat: CachedChat) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        insert_chat(&tx, &self.key, &chat)?;
        tx.commit()?;
        Ok(())
    }

    fn chats(&self) -> Result<Vec<CachedChat>, Error> {
        let mut stmt = self.conn.prepare("SELECT chat_id, value FROM chats ORDER BY chat_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut chats = Vec::new();
        for row in rows {
            let (id, sealed) = row?;
            chats.push(self.open_value("chats", &id, &sealed)?);
        }
        Ok(chats)
    }

    fn put_message(&mut self, message: Message) -> Result<(), Error> {
        let row = [message.chat_id.0.as_slice(), &seq_bytes(message.seq)].concat();
        let sealed = self.seal("messages", &row, &message)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO messages (chat_id, seq, value) VALUES (?1, ?2, ?3)",
            params![message.chat_id.0, message.seq, sealed],
        )?;
        Ok(())
    }

    fn messages(&self, chat_id: [u8; 16], before: i64, limit: usize) -> Result<Vec<Message>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, value FROM messages WHERE chat_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows =
            stmt.query_map(params![chat_id, before, limit], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut page = Vec::new();
        for row in rows {
            let (seq, sealed) = row?;
            let id = [chat_id.as_slice(), &seq_bytes(seq)].concat();
            page.push(self.open_value::<Message>("messages", &id, &sealed)?);
        }
        page.reverse();
        Ok(page)
    }

    fn clear_through(&mut self, chat_id: [u8; 16], seq: i64) -> Result<(), Error> {
        self.conn.execute("DELETE FROM messages WHERE chat_id = ?1 AND seq <= ?2", params![chat_id, seq])?;
        Ok(())
    }

    fn pin(&self, number: Number) -> Result<Option<PublicKeys>, Error> {
        let row = self.person_key(number);
        let sealed: Option<Vec<u8>> =
            self.conn.query_row("SELECT value FROM pins WHERE person = ?1", [row], |r| r.get(0)).optional()?;
        sealed.map(|s| self.open_value("pins", &row, &s)).transpose()
    }

    fn put_pin(&mut self, number: Number, keys: Option<PublicKeys>) -> Result<(), Error> {
        let row = self.person_key(number);
        match keys {
            Some(keys) => {
                let sealed = self.seal("pins", &row, &keys)?;
                self.conn
                    .execute("INSERT OR REPLACE INTO pins (person, value) VALUES (?1, ?2)", params![row, sealed])?;
            }
            None => {
                self.conn.execute("DELETE FROM pins WHERE person = ?1", [row])?;
            }
        }
        Ok(())
    }

    fn unsent_pins(&self) -> Result<Vec<Number>, Error> {
        let mut stmt = self.conn.prepare("SELECT person, value FROM unsent_pins")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut numbers = Vec::new();
        for row in rows {
            let (id, sealed) = row?;
            numbers.push(self.open_value::<Number>("unsent_pins", &id, &sealed)?);
        }
        numbers.sort_by_key(Number::digits);
        Ok(numbers)
    }

    fn mark_pin_unsent(&mut self, number: Number) -> Result<(), Error> {
        let row = self.person_key(number);
        let sealed = self.seal("unsent_pins", &row, &number)?;
        self.conn
            .execute("INSERT OR REPLACE INTO unsent_pins (person, value) VALUES (?1, ?2)", params![row, sealed])?;
        Ok(())
    }

    fn mark_pin_sent(&mut self, number: Number) -> Result<(), Error> {
        self.conn.execute("DELETE FROM unsent_pins WHERE person = ?1", [self.person_key(number)])?;
        Ok(())
    }

    fn devices(&self) -> Result<Vec<DeviceInfo>, Error> {
        Ok(self.meta("devices")?.unwrap_or_default())
    }

    fn set_devices(&mut self, devices: Vec<DeviceInfo>) -> Result<(), Error> {
        self.set_meta("devices", &devices)
    }

    fn outbox(&self) -> Result<Vec<Outgoing>, Error> {
        let mut stmt = self.conn.prepare("SELECT chat_id, msg_id, value FROM outbox ORDER BY id")?;
        let rows =
            stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?)))?;
        let mut entries = Vec::new();
        for row in rows {
            let (chat_id, msg_id, sealed) = row?;
            entries.push(self.stored_outgoing(&chat_id, &msg_id, &sealed)?);
        }
        Ok(entries)
    }

    fn put_outgoing(&mut self, outgoing: Outgoing) -> Result<(), Error> {
        let stored = StoredOutgoing {
            to: outgoing.to,
            text: outgoing.text,
            body: outgoing.body,
            new_chat: outgoing.new_chat,
            created_ms: outgoing.created_ms,
            state: (&outgoing.state).into(),
        };
        let row = [outgoing.chat_id.as_slice(), &outgoing.msg_id.0].concat();
        let sealed = self.seal("outbox", &row, &stored)?;
        self.conn.execute(
            "INSERT INTO outbox (chat_id, msg_id, value) VALUES (?1, ?2, ?3)
             ON CONFLICT (chat_id, msg_id) DO UPDATE SET value = excluded.value",
            params![outgoing.chat_id, outgoing.msg_id.0, sealed],
        )?;
        Ok(())
    }

    fn remove_outgoing(&mut self, chat_id: [u8; 16], msg_id: MsgId) -> Result<(), Error> {
        self.conn.execute("DELETE FROM outbox WHERE chat_id = ?1 AND msg_id = ?2", params![chat_id, msg_id.0])?;
        Ok(())
    }

    fn replace_all(
        &mut self,
        account: Account,
        people: Vec<CachedPerson>,
        chats: Vec<CachedChat>,
        pins: Vec<(Number, PublicKeys)>,
    ) -> Result<(), Error> {
        self.write_snapshot(&account, &people, &chats, &pins)
    }

    fn wipe(&mut self) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        for table in ["meta", "people", "chats", "messages", "pins", "unsent_pins", "outbox"] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use corvo_chat_proto::{B64, chat::Person, chat::Relation};

    use super::*;
    use crate::cache::{CachedPerson, OutState};

    const NAME: &str = "Rahul Secret";
    const TEXT: &str = "hello plaintext";

    fn number() -> Number {
        Number::from_body(222_222_222).unwrap()
    }

    fn fill(cache: &mut SqliteCache, secrets: &Secrets) {
        cache
            .put_person(CachedPerson {
                person: Person {
                    number: number(),
                    display_name: Some(NAME.into()),
                    public_keys: secrets.public_keys(),
                    relation: Relation::Contact,
                    deleted: false,
                },
                mismatch: false,
            })
            .unwrap();
        cache
            .put_outgoing(Outgoing {
                chat_id: [7; 16],
                msg_id: MsgId([1; 16]),
                to: number(),
                text: TEXT.into(),
                body: None,
                new_chat: None,
                created_ms: 1,
                state: OutState::Waiting,
            })
            .unwrap();
        cache.mark_pin_unsent(number()).unwrap();
        cache.set_cursor(4).unwrap();
        cache
            .put_message(Message {
                chat_id: B64([7; 16]),
                seq: 1,
                msg_id: B64([1; 16]),
                from: number(),
                body: corvo_chat_proto::Body(vec![9; 40]),
                sent_at: 1,
            })
            .unwrap();
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn file_holds_no_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let secrets = Secrets::generate();
        let digits = number().digits();
        {
            let mut cache = SqliteCache::open(&path, &secrets).unwrap();
            fill(&mut cache, &secrets);
            // Check the file while it is still open (the WAL holds the data)...
            for name in ["cache.db", "cache.db-wal", "cache.db-shm"] {
                if let Ok(bytes) = std::fs::read(dir.path().join(name)) {
                    for needle in [NAME, TEXT, digits.as_str()] {
                        assert!(!contains(&bytes, needle.as_bytes()), "{needle} found in {name}");
                    }
                }
            }
            // ...and again once everything is written into the main file.
            cache.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
        }
        let main = std::fs::read(&path).unwrap();
        assert!(main.len() > 4096, "the main file should hold the data now");
        for name in ["cache.db", "cache.db-wal", "cache.db-shm"] {
            if let Ok(bytes) = std::fs::read(dir.path().join(name)) {
                for needle in [NAME, TEXT, digits.as_str()] {
                    assert!(!contains(&bytes, needle.as_bytes()), "{needle} found in {name}");
                }
            }
        }
    }

    #[test]
    fn data_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let secrets = Secrets::generate();
        {
            let mut cache = SqliteCache::open(&path, &secrets).unwrap();
            fill(&mut cache, &secrets);
        }
        let cache = SqliteCache::open(&path, &secrets).unwrap();
        assert_eq!(cache.cursor().unwrap(), Some(4));
        assert_eq!(cache.person(number()).unwrap().unwrap().person.display_name.as_deref(), Some(NAME));
        assert_eq!(cache.outbox().unwrap()[0].text, TEXT);
    }

    #[test]
    fn another_accounts_key_cannot_read_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let secrets = Secrets::generate();
        {
            let mut cache = SqliteCache::open(&path, &secrets).unwrap();
            fill(&mut cache, &secrets);
        }
        let other = SqliteCache::open(&path, &Secrets::generate()).unwrap();
        assert!(matches!(other.cursor(), Err(Error::Cache(_))));
        assert!(matches!(other.people(), Err(Error::Cache(_))));
        assert!(matches!(other.outbox(), Err(Error::Cache(_))));
        assert!(matches!(other.messages([7; 16], i64::MAX, 10), Err(Error::Cache(_))));
        assert!(matches!(other.unsent_pins(), Err(Error::Cache(_))));
        // The person's row is looked up by this key's hash, so it isn't found.
        assert_eq!(other.person(number()).unwrap(), None);
    }

    #[test]
    fn a_damaged_value_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let secrets = Secrets::generate();
        let mut cache = SqliteCache::open(&path, &secrets).unwrap();
        fill(&mut cache, &secrets);
        cache.conn.execute("UPDATE meta SET value = zeroblob(10)", []).unwrap();
        assert!(matches!(cache.cursor(), Err(Error::Cache(_))));
    }

    #[test]
    fn a_value_moved_to_another_row_will_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets::generate();
        let mut cache = SqliteCache::open(&dir.path().join("cache.db"), &secrets).unwrap();
        fill(&mut cache, &secrets);
        cache
            .set_account(Account {
                username: "a".into(),
                number: number(),
                display_name: "A".into(),
                pending_until: None,
            })
            .unwrap();
        cache
            .conn
            .execute("UPDATE meta SET value = (SELECT value FROM meta WHERE key = 'account') WHERE key = 'cursor'", [])
            .unwrap();
        assert!(matches!(cache.cursor(), Err(Error::Cache(_))));
    }

    #[test]
    fn migrating_twice_is_harmless_and_a_newer_file_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(matches!(migrate(&mut conn), Err(Error::Cache(_))));
    }
}
