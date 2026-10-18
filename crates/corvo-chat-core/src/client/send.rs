//! Sending through the outbox (Decision 14), paging older messages, and
//! marking a chat read.

use std::collections::HashSet;

use corvo_chat_proto::{
    B64, Number, PublicKeys,
    chat::{ChatInfo, NewChat, Relation},
    frames::{ClientFrame, ErrorCode, ServerFrame},
};
use rand_core::{OsRng, RngCore};

use super::Client;
use crate::cache::{Cache, OutState, Outgoing};
use crate::crypto::{ChatKey, MsgId, encrypt_message, new_chat, open_key};
use crate::sync::{Update, pin_new};
use crate::view::{Opened, open_chat};
use crate::{Error, Transport};

/// Messages one `older` call asks for.
const PAGE: u32 = 50;

/// The `send` frame for an outbox entry.
fn send_frame(o: &Outgoing) -> Option<ClientFrame> {
    Some(ClientFrame::Send {
        chat_id: B64(o.chat_id),
        msg_id: B64(o.msg_id.0),
        body: o.body.clone()?,
        new_chat: o.new_chat.clone(),
    })
}

/// What sealing an outbox entry came to.
enum Sealed {
    /// Encrypted for its chat, ready to send.
    Ready(Outgoing),
    /// Not yet: no chat or pin to seal against, or a request not accepted.
    Wait,
    /// Never: their keys changed, or the chat doesn't check out.
    Refused(Error),
}

/// What a chat's key came to (Decision 12).
enum Keyed {
    Ready,
    Unpinned,
    Broken,
}

impl<T: Transport, C: Cache> Client<T, C> {
    /// Sends a text to `to`, in the chat with them or, for a first message,
    /// a new one, sealed to their pinned keys. The message goes into the
    /// outbox before it's sent, so it's never lost: an `Err` after that
    /// leaves it waiting, to go again after the next sync. A refusal isn't
    /// an `Err`: the message shows it.
    ///
    /// Before `synced`, nothing is pinned, sealed or sent: the message waits
    /// in the outbox, and the sync seals and sends it against the pins it
    /// brings. Once synced, a person not known yet is looked up first.
    pub async fn send_text(&mut self, to: Number, text: &str, now_ms: u64) -> Result<MsgId, Error> {
        if self.cache.person(to)?.is_some_and(|p| p.mismatch) {
            return Err(Error::KeyMismatch);
        }
        let msg_id = MsgId::new(now_ms);
        let draft = Outgoing {
            chat_id: self.chat_id_for(to)?,
            msg_id,
            to,
            text: text.into(),
            body: None,
            new_chat: None,
            created_ms: now_ms,
            state: OutState::Waiting,
        };
        if !self.synced {
            // Too long is still told now, with a key that's thrown away.
            encrypt_message(&ChatKey::generate(), &draft.chat_id, self.account.number, &msg_id, now_ms, text)?;
            self.cache.put_outgoing(draft)?;
            self.outbox_changes += 1;
            return Ok(msg_id);
        }
        self.pinned(to).await?;
        let entry = match self.seal(draft, false)? {
            Sealed::Ready(entry) => entry,
            Sealed::Wait => return Err(Error::KeyMismatch),
            Sealed::Refused(e) => return Err(e),
        };
        self.cache.put_outgoing(entry.clone())?;
        // Sent on this connection: a failure waits for the next sync.
        self.sent_here.insert((entry.chat_id, msg_id));
        if let Some(frame) = send_frame(&entry) {
            self.request(frame).await?;
        }
        Ok(msg_id)
    }

    /// The chat id a new message to `to` starts with: the chat's, that of an
    /// earlier message still waiting for its chat, or a new one.
    fn chat_id_for(&self, to: Number) -> Result<[u8; 16], Error> {
        if let Some(chat) = self.cache.chat_with(to)? {
            return Ok(chat.info.chat_id.0);
        }
        for o in self.cache.outbox()? {
            let taken = matches!(o.state, OutState::Refused { code: ErrorCode::BadFrame, .. });
            if o.to == to && !taken && self.cache.chat(o.chat_id)?.is_none() {
                return Ok(o.chat_id);
            }
        }
        let mut chat_id = [0u8; 16];
        OsRng.fill_bytes(&mut chat_id);
        Ok(chat_id)
    }

    /// `to`'s pinned keys (Decision 13), once synced. Keys not pinned yet
    /// are pinned now: the cached person's, or a lookup's, which pins them
    /// on its way in.
    pub(super) async fn pinned(&mut self, to: Number) -> Result<PublicKeys, Error> {
        if let Some(keys) = self.cache.pin(to)? {
            return Ok(keys);
        }
        let keys = match self.cache.person(to)? {
            Some(cached) => cached.person.public_keys,
            None => match self.request(ClientFrame::Lookup { number: to }).await? {
                ServerFrame::Found { public_keys, .. } => public_keys,
                _ => return Err(Error::BadServerMessage),
            },
        };
        if self.cache.pin(to)?.is_none() && pin_new(&mut self.cache, vec![(to, keys)])? {
            self.pins_due = true;
        }
        if self.cache.person(to)?.is_some_and(|p| p.mismatch) {
            return Err(Error::KeyMismatch);
        }
        self.cache.pin(to)?.ok_or(Error::KeyMismatch)
    }

    /// Opens a chat's key once and keeps it (Decision 12), with the pinned
    /// keys only. A chat that doesn't check out is kept as broken.
    fn key_state(&mut self, info: &ChatInfo) -> Result<Keyed, Error> {
        let id = info.chat_id.0;
        if let Some(key) = self.keys.get(&id) {
            return Ok(if key.is_some() { Keyed::Ready } else { Keyed::Broken });
        }
        Ok(match open_chat(&self.cache, &self.secrets, self.account.number, info)? {
            Opened::Key(key) => {
                self.keys.insert(id, Some(key));
                Keyed::Ready
            }
            Opened::Broken => {
                self.keys.insert(id, None);
                Keyed::Broken
            }
            Opened::Unpinned => Keyed::Unpinned,
        })
    }

    /// Encrypts an entry for the chat with its person, against the pins and
    /// with no request: into the cached chat (`new_chat` none), or else a
    /// new chat. A message `moving` from a chat that lost (`chat_exists`)
    /// waits for that chat, and for my accept if they asked first, since
    /// the server refuses it until then.
    fn seal(&mut self, o: Outgoing, moving: bool) -> Result<Sealed, Error> {
        if self.cache.person(o.to)?.is_some_and(|p| p.mismatch) {
            return Ok(Sealed::Refused(Error::KeyMismatch));
        }
        let me = self.account.number;
        let (chat_id, new) = match self.cache.chat_with(o.to)? {
            Some(chat) => {
                let asked = self.cache.person(o.to)?.is_some_and(|p| p.person.relation == Relation::Request);
                if moving && asked {
                    return Ok(Sealed::Wait);
                }
                match self.key_state(&chat.info)? {
                    Keyed::Ready => (chat.info.chat_id.0, None),
                    Keyed::Unpinned => return Ok(Sealed::Wait),
                    Keyed::Broken => return Ok(Sealed::Refused(Error::BadChat)),
                }
            }
            None if moving => return Ok(Sealed::Wait),
            None => match self.first_message(o.to)? {
                Some((chat_id, key, new)) => {
                    self.keys.insert(chat_id, Some(key));
                    (chat_id, Some(new))
                }
                None => {
                    let Some(theirs) = self.cache.pin(o.to)? else { return Ok(Sealed::Wait) };
                    let (key, new) = new_chat(&self.secrets, me, &o.chat_id, o.to, &theirs);
                    self.keys.insert(o.chat_id, Some(key));
                    (o.chat_id, Some(new))
                }
            },
        };
        let Some(Some(key)) = self.keys.get(&chat_id) else { return Ok(Sealed::Wait) };
        let body = encrypt_message(key, &chat_id, me, &o.msg_id, o.created_ms, &o.text)?;
        Ok(Sealed::Ready(Outgoing { chat_id, body: Some(body), new_chat: new, state: OutState::Waiting, ..o }))
    }

    /// The id, key and `new_chat` of an earlier first message to `to` still
    /// in the outbox, so a second shares its chat (unless its id was taken).
    fn first_message(&self, to: Number) -> Result<Option<([u8; 16], ChatKey, NewChat)>, Error> {
        let me = self.account.number;
        for o in self.cache.outbox()? {
            let taken = matches!(o.state, OutState::Refused { code: ErrorCode::BadFrame, .. });
            if o.to == to
                && !taken
                && let Some(new) = o.new_chat
                && self.cache.chat(o.chat_id)?.is_none()
                && let Some(key) = open_key(&new.my_key, &o.chat_id, me, &self.secrets)
            {
                return Ok(Some((o.chat_id, key, new)));
            }
        }
        Ok(None)
    }

    /// Refuses an entry here, without the server: their keys changed, or
    /// the chat doesn't check out.
    fn refuse(&mut self, o: Outgoing, why: Error) -> Result<(), Error> {
        let message = why.to_string();
        self.pending.push(Update::Refused { chat_id: o.chat_id, msg_id: o.msg_id, message: message.clone() });
        let state = OutState::Refused { code: ErrorCode::NotAllowed, message };
        self.cache.put_outgoing(Outgoing { state, ..o })
    }

    /// Once synced, looks up the people of messages written before it whom
    /// the cache doesn't know, so they can be sealed: their keys pin on the
    /// way in. A number that's unknown or banned refuses those messages
    /// with the server's reason.
    /// Only `sync` calls this, since it waits for replies.
    pub(super) async fn look_up_outbox(&mut self) -> Result<(), Error> {
        let mut asked = HashSet::new();
        for o in self.cache.outbox()? {
            let unsealed = o.state == OutState::Waiting && o.body.is_none();
            if !unsealed
                || asked.contains(&o.to)
                || self.cache.pin(o.to)?.is_some()
                || self.cache.person(o.to)?.is_some()
                || self.cache.chat_with(o.to)?.is_some()
            {
                continue;
            }
            let number = o.to;
            asked.insert(number);
            match self.request(ClientFrame::Lookup { number }).await {
                Ok(_) => {}
                // A number that isn't there, or is banned; anything else is tried after the next sync.
                Err(Error::Server { code: code @ (ErrorCode::NumberUnknown | ErrorCode::Banned), message, .. }) => {
                    for p in self.cache.outbox()? {
                        if p.to == number && p.state == OutState::Waiting && p.body.is_none() {
                            let update =
                                Update::Refused { chat_id: p.chat_id, msg_id: p.msg_id, message: message.clone() };
                            self.pending.push(update);
                            let state = OutState::Refused { code, message: message.clone() };
                            self.cache.put_outgoing(Outgoing { state, ..p })?;
                        }
                    }
                }
                Err(Error::Server { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// At a safe point once synced, behind one barrier, with no other
    /// request: seals what waited for the sync, moves each message whose new
    /// chat lost (`chat_exists`) to the chat now cached, and sends every
    /// waiting message not yet sent on this connection. The server dedupes
    /// by `msg_id`, so a message sent twice (a dropped call) is stored once.
    /// A message to someone whose keys changed is refused here.
    pub(super) async fn send_outbox(&mut self) -> Result<(), Error> {
        if !self.synced || self.outbox_seen == self.outbox_changes {
            return Ok(());
        }
        let seen = self.outbox_changes;
        let (mut frames, mut keys) = (Vec::new(), Vec::new());
        for o in self.cache.outbox()? {
            let moving = o.state == OutState::NeedsChat;
            if !moving && o.state != OutState::Waiting {
                continue;
            }
            let o = if moving || o.body.is_none() {
                match self.seal(o.clone(), moving)? {
                    Sealed::Ready(sealed) => {
                        // Added before the old one goes, so a failure in between loses nothing.
                        self.cache.put_outgoing(sealed.clone())?;
                        if sealed.chat_id != o.chat_id {
                            self.cache.remove_outgoing(o.chat_id, o.msg_id)?;
                        }
                        sealed
                    }
                    Sealed::Wait => continue,
                    Sealed::Refused(why) => {
                        self.refuse(o, why)?;
                        continue;
                    }
                }
            } else if self.cache.person(o.to)?.is_some_and(|p| p.mismatch) {
                self.refuse(o, Error::KeyMismatch)?;
                continue;
            } else {
                o
            };
            if !self.sent_here.contains(&(o.chat_id, o.msg_id))
                && let Some(frame) = send_frame(&o)
            {
                frames.push(frame);
                keys.push((o.chat_id, o.msg_id));
            }
        }
        if !frames.is_empty() {
            // Their errors leave them waiting for the next sync.
            self.behind_barrier(&frames).await?;
            self.sent_here.extend(keys);
        }
        self.outbox_seen = seen;
        Ok(())
    }

    /// Pages in up to 50 messages older than the oldest cached one. Gives
    /// whether there's nothing older left.
    pub async fn older(&mut self, chat_id: [u8; 16]) -> Result<bool, Error> {
        if self.cache.chat(chat_id)?.is_some_and(|c| c.complete) {
            return Ok(true);
        }
        let before = self.cache.oldest(chat_id)?.unwrap_or(i64::MAX);
        match self.request(ClientFrame::History { chat_id: B64(chat_id), before, limit: PAGE }).await? {
            ServerFrame::History { complete, .. } => Ok(complete),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// Marks the chat read through its last visible message: tells the
    /// server (which tells their side and my other devices) and clears the
    /// unread count here.
    pub async fn mark_read(&mut self, chat_id: [u8; 16]) -> Result<(), Error> {
        let Some(chat) = self.cache.chat(chat_id)? else { return Ok(()) };
        let Some(last) = self.cache.messages(chat_id, i64::MAX, 1)?.pop() else { return Ok(()) };
        if last.seq <= chat.info.read_through {
            return Ok(());
        }
        self.request_quietly(ClientFrame::Read { chat_id: B64(chat_id), through: last.seq }).await?;
        // Read again: the wait may have changed it.
        if let Some(mut chat) = self.cache.chat(chat_id)? {
            chat.info.read_through = chat.info.read_through.max(last.seq);
            self.cache.put_chat(chat)?;
        }
        Ok(())
    }
}
