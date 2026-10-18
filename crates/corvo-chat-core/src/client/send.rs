//! Sending through the outbox (Decision 14), paging older messages, and
//! marking a chat read.

use corvo_chat_proto::{
    B64, Number, PublicKeys,
    chat::{ChatInfo, NewChat},
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

impl<T: Transport, C: Cache> Client<T, C> {
    /// Sends a text to `to`, in the chat with them or, for a first message,
    /// a new one. Their keys are the pinned ones; a person not known yet is
    /// looked up and pinned first. The message goes into the outbox before
    /// it's sent, so it's never lost: an `Err` after that leaves it waiting,
    /// to go again after the next sync. A refusal isn't an `Err`: the
    /// message shows it.
    pub async fn send_text(&mut self, to: Number, text: &str, now_ms: u64) -> Result<MsgId, Error> {
        if self.cache.person(to)?.is_some_and(|p| p.mismatch) {
            return Err(Error::KeyMismatch);
        }
        let me = self.account.number;
        let msg_id = MsgId::new(now_ms);
        let (chat_id, new) = match self.cache.chat_with(to)? {
            Some(chat) => {
                self.open_key(&chat.info).await?;
                (chat.info.chat_id.0, None)
            }
            None => {
                let (chat_id, key, new) = self.start_chat(to).await?;
                self.keys.insert(chat_id, Some(key));
                (chat_id, Some(new))
            }
        };
        let Some(Some(key)) = self.keys.get(&chat_id) else { return Err(Error::BadChat) };
        let body = encrypt_message(key, &chat_id, me, &msg_id, now_ms, text)?;
        let entry = Outgoing {
            chat_id,
            msg_id,
            to,
            text: text.into(),
            body: Some(body),
            new_chat: new,
            created_ms: now_ms,
            state: OutState::Waiting,
        };
        self.cache.put_outgoing(entry.clone())?;
        // Sent on this connection: a failure waits for the next sync.
        self.sent_here.insert((chat_id, msg_id));
        if let Some(frame) = send_frame(&entry) {
            self.request(frame).await?;
        }
        Ok(msg_id)
    }

    /// The id, key and `new_chat` for a first message to `to`: those of an
    /// earlier first message still in the outbox, so the two share a chat
    /// (unless its id was taken), or new ones sealed to their pinned keys.
    async fn start_chat(&mut self, to: Number) -> Result<([u8; 16], ChatKey, NewChat), Error> {
        let me = self.account.number;
        for o in self.cache.outbox()? {
            let taken = matches!(o.state, OutState::Refused { code: ErrorCode::BadFrame, .. });
            if o.to == to
                && !taken
                && let Some(new) = o.new_chat
                && self.cache.chat(o.chat_id)?.is_none()
                && let Some(key) = open_key(&new.my_key, &o.chat_id, me, &self.secrets)
            {
                return Ok((o.chat_id, key, new));
            }
        }
        let theirs = self.pinned(to).await?;
        let mut chat_id = [0u8; 16];
        OsRng.fill_bytes(&mut chat_id);
        let (key, new) = new_chat(&self.secrets, me, &chat_id, to, &theirs);
        Ok((chat_id, key, new))
    }

    /// `to`'s pinned keys (Decision 13). Keys not pinned yet are pinned now:
    /// the cached person's, or a lookup's. Once synced, the lookup's answer
    /// pins them on its way in; before that, pinning would wait for the
    /// replay, which sending can't.
    async fn pinned(&mut self, to: Number) -> Result<PublicKeys, Error> {
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

    /// Opens a chat's key once and keeps it (Decision 12). A chat that
    /// doesn't check out is kept as broken: `Err(BadChat)`, and nothing is
    /// sent to it.
    async fn open_key(&mut self, info: &ChatInfo) -> Result<(), Error> {
        let id = info.chat_id.0;
        if !self.keys.contains_key(&id) {
            self.pinned(info.with).await?;
            let opened = open_chat(&self.cache, &self.secrets, self.account.number, info)?;
            self.keys.insert(id, if let Opened::Key(key) = opened { Some(key) } else { None });
        }
        match self.keys.get(&id) {
            Some(Some(_)) => Ok(()),
            _ => Err(Error::BadChat),
        }
    }

    /// At a safe point once synced, behind one barrier: moves each message
    /// whose new chat lost (`chat_exists`) to the chat now cached, and sends
    /// every waiting message not yet sent on this connection. The server
    /// dedupes by `msg_id`, so a message sent twice (a dropped call) is
    /// stored once.
    pub(super) async fn send_outbox(&mut self) -> Result<(), Error> {
        if !self.synced || self.outbox_seen == self.outbox_changes {
            return Ok(());
        }
        let seen = self.outbox_changes;
        let (mut frames, mut keys) = (Vec::new(), Vec::new());
        for o in self.cache.outbox()? {
            let o = match o.state {
                OutState::NeedsChat => match self.move_to_chat(o).await? {
                    Some(moved) => moved,
                    None => continue,
                },
                OutState::Waiting => o,
                _ => continue,
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

    /// Encrypts a message again for the chat with its person, same `msg_id`
    /// and no `new_chat`, once that chat is cached. `None` while it waits:
    /// no chat yet, or their keys changed. A broken chat refuses it.
    async fn move_to_chat(&mut self, o: Outgoing) -> Result<Option<Outgoing>, Error> {
        let Some(chat) = self.cache.chat_with(o.to)? else { return Ok(None) };
        if self.cache.person(o.to)?.is_some_and(|p| p.mismatch) {
            return Ok(None);
        }
        match self.open_key(&chat.info).await {
            Ok(()) => {}
            Err(Error::KeyMismatch) => return Ok(None),
            Err(Error::BadChat) => {
                let message = Error::BadChat.to_string();
                let refused = OutState::Refused { code: ErrorCode::ChatExists, message: message.clone() };
                self.pending.push(Update::Refused { chat_id: o.chat_id, msg_id: o.msg_id, message });
                self.cache.put_outgoing(Outgoing { state: refused, ..o })?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        let chat_id = chat.info.chat_id.0;
        let Some(Some(key)) = self.keys.get(&chat_id) else { return Ok(None) };
        let body = encrypt_message(key, &chat_id, self.account.number, &o.msg_id, o.created_ms, &o.text)?;
        let (old, msg_id) = (o.chat_id, o.msg_id);
        let moved = Outgoing { chat_id, body: Some(body), new_chat: None, state: OutState::Waiting, ..o };
        // Added before the old one goes, so a failure in between loses nothing.
        self.cache.put_outgoing(moved.clone())?;
        if old != chat_id {
            self.cache.remove_outgoing(old, msg_id)?;
        }
        Ok(Some(moved))
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
