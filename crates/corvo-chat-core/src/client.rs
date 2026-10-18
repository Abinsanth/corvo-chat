//! The live client: a signed-in connection that sends a request and waits for
//! its reply, keeps the cache in step with what the server pushes, and the
//! account actions built on that.

use std::collections::{BTreeMap, HashMap, HashSet};

use corvo_chat_proto::{
    B64, Number, PublicKeys,
    chat::Pin,
    frames::{Account, ClientFrame, Device, DeviceInfo, ErrorCode, InviteInfo, ServerFrame, SignedOutReason},
    opaque::recovery_ksf,
};

use crate::account::{SignedIn, next};
use crate::cache::Cache;
use crate::crypto::{CopyKind, RecoveryCode, Secrets, check_password, lock_copy, normalize_password, opaque, seal_pin};
use crate::sync::{Update, apply, pin_new};
use crate::{Error, Strength, Transport};

/// A signed-in connection. It owns the transport, so it is the only thing that
/// reads or writes frames from here on.
pub struct Client<T: Transport, C: Cache> {
    t: T,
    account: Account,
    secrets: Secrets,
    device: Device,
    strength: Strength,
    cache: C,
    /// Contacts online now, from `presence`; it starts over with each sync.
    online: HashSet<Number>,
    /// Past `synced`: the log has been replayed, so new keys can be pinned
    /// and acks sent at once.
    synced: bool,
    /// Keys seen with no pin, waiting for `synced`.
    unpinned: Vec<(Number, PublicKeys)>,
    /// Acks waiting to be sent: the server reads nothing during a sync.
    acks: BTreeMap<[u8; 16], i64>,
    /// The last `received` sent per chat on this connection.
    acked: HashMap<[u8; 16], i64>,
    /// Unconfirmed pins may need sending.
    pins_due: bool,
    /// Pins already sent on this connection, waiting for their echo.
    pins_sent: HashSet<Number>,
    /// A barrier whose answer hasn't been read, left by a dropped future.
    owed: Option<Owed>,
    /// `devices` answers that may still come for barriers sent twice; any
    /// that arrives unasked is skipped.
    stray: usize,
    /// What changed and hasn't been handed out by `next` yet.
    pending: Vec<Update>,
}

/// A `list_devices` barrier still to be answered, so that if the future
/// waiting for it is dropped, its answer isn't taken for the next request's.
struct Owed {
    /// Frames sent before it, each of which may fail first.
    before: usize,
    /// How many of them have failed so far.
    failed: usize,
    /// Whether they name a chat, so `chat_unknown` may be theirs.
    names_chat: bool,
    /// Whether the barrier surely went out.
    sent: bool,
}

/// Frames the server sends on its own, not as the reply to a request.
fn is_push(frame: &ServerFrame) -> bool {
    matches!(
        frame,
        ServerFrame::Events { .. }
            | ServerFrame::Presence { .. }
            | ServerFrame::Synced { .. }
            | ServerFrame::Snapshot { .. }
            | ServerFrame::Sent { .. }
            | ServerFrame::NotSent { .. }
            | ServerFrame::History { .. }
    )
}

/// Whether `frame` answers `request` with something the cache keeps too:
/// `sent` and `history` can also come unasked (a resend, a snapshot), and
/// `found` brings keys to pin.
fn answers(request: &ClientFrame, frame: &ServerFrame) -> bool {
    match (request, frame) {
        (ClientFrame::Send { chat_id, msg_id, .. }, ServerFrame::Sent { chat_id: c, msg_id: m, .. })
        | (ClientFrame::Send { chat_id, msg_id, .. }, ServerFrame::NotSent { chat_id: c, msg_id: m, .. }) => {
            chat_id == c && msg_id == m
        }
        (ClientFrame::History { chat_id, .. }, ServerFrame::History { chat_id: c, .. }) => chat_id == c,
        (ClientFrame::Lookup { number }, ServerFrame::Found { number: n, .. }) => number == n,
        _ => false,
    }
}

impl<T: Transport, C: Cache> Client<T, C> {
    /// Takes over a connection that has just signed in. The token is not kept:
    /// the caller stores it for `resume`.
    pub fn new(t: T, signed_in: SignedIn, device: Device, strength: Strength, mut cache: C) -> Client<T, C> {
        // So my own messages are never taken for theirs. `sync` stores it
        // again and reports a failure there.
        let _ = cache.set_account(signed_in.account.clone());
        Client {
            t,
            account: signed_in.account,
            secrets: signed_in.secrets,
            device,
            strength,
            cache,
            online: HashSet::new(),
            synced: false,
            unpinned: Vec::new(),
            acks: BTreeMap::new(),
            acked: HashMap::new(),
            pins_due: false,
            pins_sent: HashSet::new(),
            owed: None,
            stray: 0,
            pending: Vec::new(),
        }
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    pub fn secrets(&self) -> &Secrets {
        &self.secrets
    }

    pub fn cache(&self) -> &C {
        &self.cache
    }

    /// Gives the cache back, to carry it to the next connection.
    pub fn into_cache(self) -> C {
        self.cache
    }

    /// The contacts online now.
    pub fn online(&self) -> &HashSet<Number> {
        &self.online
    }

    /// Brings the cache up to date: sends `sync` from its cursor (none for an
    /// empty cache, which gets a snapshot) and applies everything up to
    /// `synced`, then sends the acks and pins that waited for it. What
    /// changed comes from the next `next`.
    ///
    /// Not cancel-safe: if it's dropped, drop the connection too.
    pub async fn sync(&mut self) -> Result<(), Error> {
        self.settle().await?;
        // The account just signed in is the newest copy.
        self.cache.set_account(self.account.clone())?;
        self.online.clear();
        self.synced = false;
        let cursor = self.cache.cursor()?;
        self.t.send(&ClientFrame::Sync { cursor }).await?;
        loop {
            let frame = self.read(false).await?;
            let done = matches!(frame, ServerFrame::Synced { .. });
            self.handle(frame).await?;
            if done {
                return self.send_pins().await;
            }
        }
    }

    /// Waits for the next pushed frame, applies it, and gives what changed.
    /// Updates held back while a request waited come first.
    ///
    /// Cancel-safe, so it can sit in a `select!`. A dropped call loses no
    /// frame and no update: a frame is applied, and its updates kept, before
    /// anything else is awaited; acks not yet sent stay queued; a pin barrier
    /// cut short is finished by the next call that talks to the server.
    pub async fn next(&mut self) -> Result<Vec<Update>, Error> {
        self.settle().await?;
        self.send_pins().await?;
        if self.pending.is_empty() {
            let frame = self.read(false).await?;
            self.handle(frame).await?;
        }
        Ok(std::mem::take(&mut self.pending))
    }

    /// The next frame. A stray `chat_unknown` (an ack for a chat the server no
    /// longer shows) is skipped unless what's waiting names a chat itself. A
    /// sign-out wipes the cache, and comes back as `Err` even if the wipe
    /// fails.
    async fn read(&mut self, names_chat: bool) -> Result<ServerFrame, Error> {
        loop {
            match next(&mut self.t).await {
                Err(Error::Server { code: ErrorCode::ChatUnknown, .. }) if !names_chat => {}
                Err(Error::SignedOut(reason)) => {
                    let _ = self.cache.wipe();
                    return Err(Error::SignedOut(reason));
                }
                frame => return frame,
            }
        }
    }

    /// Applies a frame, keeps presence and the account up to date, and adds
    /// what changed to `pending`. Once synced, it pins new keys (sent later,
    /// from `next`) and sends acks that move forward. Nothing here waits for
    /// a reply, and everything is kept before the first await.
    async fn handle(&mut self, frame: ServerFrame) -> Result<(), Error> {
        if let ServerFrame::Presence { number, online } = &frame {
            if *online {
                self.online.insert(*number);
            } else {
                self.online.remove(number);
            }
        }
        let synced = matches!(frame, ServerFrame::Synced { .. });
        let start = self.pending.len();
        let applied = apply(&mut self.cache, &self.secrets, frame, &mut self.pending)?;
        if self.pending[start..].contains(&Update::Profile)
            && let Some(account) = self.cache.account()?
        {
            self.account = account;
        }
        // An echo confirms a pin; if it's ever made again, it goes again.
        if !self.pins_sent.is_empty() {
            let unsent = self.cache.unsent_pins()?;
            self.pins_sent.retain(|number| unsent.contains(number));
        }
        self.unpinned.extend(applied.unpinned);
        for (chat_id, through) in applied.acks {
            let ack = self.acks.entry(chat_id).or_insert(through);
            *ack = (*ack).max(through);
        }
        if synced {
            self.synced = true;
            // Pins from earlier connections that never came back.
            self.pins_due = true;
        }
        if self.synced {
            if pin_new(&mut self.cache, std::mem::take(&mut self.unpinned))? {
                self.pins_due = true;
            }
            // Each ack leaves the queue only once it's sent.
            while let Some((&chat_id, &through)) = self.acks.iter().next() {
                if through > self.acked.get(&chat_id).copied().unwrap_or(0) {
                    self.t.send(&ClientFrame::Received { chat_id: B64(chat_id), through }).await?;
                    self.acked.insert(chat_id, through);
                }
                self.acks.remove(&chat_id);
            }
        }
        Ok(())
    }

    /// Sends the pins the server hasn't confirmed, behind one barrier. The
    /// echoed `pin` event confirms each; past the pin limit, one stays only
    /// here and isn't tried again.
    async fn send_pins(&mut self) -> Result<(), Error> {
        if !self.pins_due {
            return Ok(());
        }
        self.pins_due = false;
        let (mut numbers, mut frames) = (Vec::new(), Vec::new());
        for number in self.cache.unsent_pins()? {
            if self.pins_sent.contains(&number) {
                continue;
            }
            let Some(keys) = self.cache.pin(number)? else {
                self.cache.mark_pin_sent(number)?;
                continue;
            };
            let pin = Some(seal_pin(&self.secrets, number, &keys));
            frames.push(ClientFrame::SetPin { pin: Pin { number, pin } });
            numbers.push(number);
        }
        if frames.is_empty() {
            return Ok(());
        }
        // Noted first: if this is dropped, the echo (or the next sync) settles it.
        self.pins_sent.extend(numbers.iter().copied());
        let (_, failed) = self.behind_barrier(&frames).await?;
        // Errors don't say which pin they're for; at the limit, none of the
        // new ones can go, and the rest come back as events anyway.
        if failed.iter().any(|e| matches!(e, Error::Server { code: ErrorCode::NotAllowed, .. })) {
            for number in numbers {
                self.cache.mark_pin_sent(number)?;
            }
        }
        Ok(())
    }

    /// Sends a frame and returns the reply, applying pushed frames on the way
    /// (their updates wait for `next`). A reply the cache keeps too is applied
    /// as well. Server errors and sign-outs come back as `Err`.
    ///
    /// Cancel-safe only until the frame is sent: dropped after that, its reply
    /// would be taken for the next request's, so drop the connection too.
    pub async fn request(&mut self, frame: ClientFrame) -> Result<ServerFrame, Error> {
        self.settle().await?;
        self.t.send(&frame).await?;
        let names_chat = matches!(frame, ClientFrame::History { .. });
        self.wait_for(&frame, names_chat).await
    }

    async fn wait_for(&mut self, request: &ClientFrame, names_chat: bool) -> Result<ServerFrame, Error> {
        let wants_devices = matches!(request, ClientFrame::ListDevices | ClientFrame::SignOutDevice { .. });
        loop {
            let frame = self.read(names_chat).await?;
            if matches!(frame, ServerFrame::Devices { .. }) && !wants_devices && self.stray > 0 {
                self.stray -= 1;
                continue;
            }
            let answered = answers(request, &frame);
            if answered || is_push(&frame) {
                self.handle(frame.clone()).await?;
            }
            if answered || !is_push(&frame) {
                return Ok(frame);
            }
        }
    }

    /// Sends frames the server answers only with an error, or, once synced,
    /// with an event, then a `list_devices` behind them. The server handles
    /// frames in order, so every error before that reply is one of theirs.
    /// Gives the devices and those errors.
    async fn behind_barrier(&mut self, frames: &[ClientFrame]) -> Result<(Vec<DeviceInfo>, Vec<Error>), Error> {
        self.settle().await?;
        let names_chat = frames.iter().any(|f| matches!(f, ClientFrame::History { .. } | ClientFrame::Read { .. }));
        // Noted before anything is sent, so a dropped future leaves it to `settle`.
        self.owed = Some(Owed { before: frames.len(), failed: 0, names_chat, sent: false });
        for frame in frames {
            self.t.send(frame).await?;
        }
        self.t.send(&ClientFrame::ListDevices).await?;
        if let Some(owed) = &mut self.owed {
            owed.sent = true;
        }
        self.barrier_answer().await
    }

    /// Reads up to the owed barrier's answer, counting the errors of the
    /// frames before it. `list_devices` itself fails only as `internal`, or
    /// as `not_active`, which every frame before it got too.
    async fn barrier_answer(&mut self) -> Result<(Vec<DeviceInfo>, Vec<Error>), Error> {
        let barrier = ClientFrame::ListDevices;
        let mut failed = Vec::new();
        let result = loop {
            let Some(names_chat) = self.owed.as_ref().map(|o| o.names_chat) else {
                break Err(Error::BadServerMessage);
            };
            match self.wait_for(&barrier, names_chat).await {
                Ok(ServerFrame::Devices { devices }) => break Ok((devices, failed)),
                Ok(_) => break Err(Error::BadServerMessage),
                Err(e @ Error::Server { code: ErrorCode::Internal, .. }) => break Err(e),
                Err(e @ Error::Server { .. }) => match &mut self.owed {
                    Some(owed) if owed.failed < owed.before => {
                        owed.failed += 1;
                        failed.push(e);
                    }
                    _ => break Err(e),
                },
                Err(e) => break Err(e),
            }
        };
        self.owed = None;
        result
    }

    /// Finishes a barrier a dropped future left behind, before anything else
    /// is sent or read. If it's unknown whether the barrier itself went out,
    /// another goes, and the extra answer is skipped when it comes. The left
    /// frames' errors are dropped: a pin not confirmed is sent again.
    async fn settle(&mut self) -> Result<(), Error> {
        let Some(owed) = &self.owed else { return Ok(()) };
        if !owed.sent {
            self.stray += 1;
            self.t.send(&ClientFrame::ListDevices).await?;
            if let Some(owed) = &mut self.owed {
                owed.sent = true;
            }
        }
        match self.barrier_answer().await {
            Ok(_) | Err(Error::Server { .. } | Error::BadServerMessage) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// One frame behind the barrier: its error, or the devices.
    async fn request_quietly(&mut self, frame: ClientFrame) -> Result<Vec<DeviceInfo>, Error> {
        let (devices, failed) = self.behind_barrier(&[frame]).await?;
        match failed.into_iter().next() {
            Some(e) => Err(e),
            None => Ok(devices),
        }
    }

    /// Activates this pending account with the code its inviter passed on.
    pub async fn activate(&mut self, code: &str) -> Result<Account, Error> {
        match self.request(ClientFrame::Activate { code: code.into() }).await? {
            ServerFrame::Activated { account } => {
                self.account = account.clone();
                Ok(account)
            }
            _ => Err(Error::BadServerMessage),
        }
    }

    /// Invites a pending person by number. Pass on the code in the result.
    pub async fn invite(&mut self, number: Number) -> Result<InviteInfo, Error> {
        match self.request(ClientFrame::Invite { number }).await? {
            ServerFrame::Invited { invite } => Ok(invite),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// The invites still open, oldest first.
    pub async fn invites(&mut self) -> Result<Vec<InviteInfo>, Error> {
        match self.request(ClientFrame::ListInvites).await? {
            ServerFrame::Invites { invites } => Ok(invites),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// This account's devices, oldest first.
    pub async fn devices(&mut self) -> Result<Vec<DeviceInfo>, Error> {
        match self.request(ClientFrame::ListDevices).await? {
            ServerFrame::Devices { devices } => Ok(devices),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// Signs out another device. Gives the devices that are left.
    pub async fn sign_out_device(&mut self, id: [u8; 16]) -> Result<Vec<DeviceInfo>, Error> {
        match self.request(ClientFrame::SignOutDevice { id: B64(id) }).await? {
            ServerFrame::Devices { devices } => Ok(devices),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// Signs this device out. The connection is closed after that.
    pub async fn sign_out(&mut self) -> Result<(), Error> {
        match self.request(ClientFrame::SignOut).await {
            Err(Error::SignedOut(SignedOutReason::ThisDevice)) => Ok(()),
            Ok(_) => Err(Error::BadServerMessage),
            Err(e) => Err(e),
        }
    }

    pub async fn set_display_name(&mut self, name: &str) -> Result<(), Error> {
        self.request_quietly(ClientFrame::SetDisplayName { display_name: name.into() }).await?;
        self.account.display_name = name.trim().into();
        Ok(())
    }

    pub async fn rename_device(&mut self, name: &str) -> Result<(), Error> {
        let devices = self.request_quietly(ClientFrame::RenameDevice { name: name.into() }).await?;
        if let Some(this) = devices.iter().find(|d| d.this_device) {
            self.device.name = this.name.clone();
        }
        Ok(())
    }

    /// Proves the password again, which opens a 5 minute window for the
    /// frames that change a secret. A wrong password sends nothing more.
    async fn reauth(&mut self, password: &str) -> Result<(), Error> {
        let password = normalize_password(password);
        let (state, request) = opaque::login_start(password.as_bytes());
        let response = match self.request(ClientFrame::ReauthStart { request }).await? {
            ServerFrame::LoginOffer { response } => response,
            _ => return Err(Error::BadServerMessage),
        };
        let (finalization, _) =
            opaque::login_finish(state, password.as_bytes(), &response, &opaque::password_ksf(self.strength))?;
        match self.request(ClientFrame::ReauthFinish { finalization }).await? {
            ServerFrame::Reauthed => Ok(()),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// Changes the password and locks the secrets again under the new one.
    /// Every other device is signed out. The new password is checked before
    /// anything is sent.
    pub async fn change_password(&mut self, old: &str, new: &str) -> Result<(), Error> {
        let new = normalize_password(new);
        check_password(&new)?;
        self.reauth(old).await?;
        let (state, request) = opaque::register_start(new.as_bytes());
        let response = match self.request(ClientFrame::Reregister { password: Some(request), recovery: None }).await? {
            ServerFrame::Registration { password: Some(p), .. } => p,
            _ => return Err(Error::BadServerMessage),
        };
        let (upload, wrap_key) =
            opaque::register_finish(state, new.as_bytes(), &response, &opaque::password_ksf(self.strength))?;
        let password_copy = lock_copy(&self.secrets, &wrap_key, CopyKind::Password);
        match self.request(ClientFrame::ChangePassword { password: upload, password_copy }).await? {
            ServerFrame::PasswordChanged => Ok(()),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// Makes a new recovery code; the old one stops working. Show it once.
    pub async fn new_recovery_code(&mut self, password: &str) -> Result<RecoveryCode, Error> {
        self.reauth(password).await?;
        let code = RecoveryCode::generate();
        let (state, request) = opaque::register_start(code.secret());
        let response = match self.request(ClientFrame::Reregister { password: None, recovery: Some(request) }).await? {
            ServerFrame::Registration { recovery: Some(r), .. } => r,
            _ => return Err(Error::BadServerMessage),
        };
        let (upload, wrap_key) = opaque::register_finish(state, code.secret(), &response, &recovery_ksf())?;
        let recovery_copy = lock_copy(&self.secrets, &wrap_key, CopyKind::Recovery);
        match self.request(ClientFrame::NewRecoveryCode { recovery: upload, recovery_copy }).await? {
            ServerFrame::RecoveryCodeChanged => Ok(code),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// Deletes the account for good. The server answers by signing every
    /// device out, which is the success here.
    pub async fn delete_account(&mut self, password: &str) -> Result<(), Error> {
        self.reauth(password).await?;
        match self.request(ClientFrame::DeleteAccount).await {
            Err(Error::SignedOut(SignedOutReason::AccountDeleted)) => Ok(()),
            Ok(_) => Err(Error::BadServerMessage),
            Err(e) => Err(e),
        }
    }
}
