//! The live client: a signed-in connection that sends a request and waits for
//! its reply, keeps the cache in step with what the server pushes, and the
//! account actions built on that.

use std::collections::{HashMap, HashSet};

use corvo_chat_proto::{
    B64, Number,
    frames::{Account, ClientFrame, Device, DeviceInfo, ErrorCode, InviteInfo, ServerFrame, SignedOutReason},
    opaque::recovery_ksf,
};

use crate::account::{SignedIn, next};
use crate::cache::Cache;
use crate::crypto::{CopyKind, RecoveryCode, Secrets, check_password, lock_copy, normalize_password, opaque};
use crate::sync::{Update, apply};
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
    /// The last `received` sent per chat on this connection.
    acked: HashMap<[u8; 16], i64>,
    /// Updates from frames pushed while waiting for a reply; `next` gives them first.
    pending: Vec<Update>,
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

/// Whether `frame` answers `request`, for the replies that can also come
/// unasked: `sent` for a resend, `history` from a snapshot.
fn answers(request: &ClientFrame, frame: &ServerFrame) -> bool {
    match (request, frame) {
        (ClientFrame::Send { chat_id, msg_id, .. }, ServerFrame::Sent { chat_id: c, msg_id: m, .. })
        | (ClientFrame::Send { chat_id, msg_id, .. }, ServerFrame::NotSent { chat_id: c, msg_id: m, .. }) => {
            chat_id == c && msg_id == m
        }
        (ClientFrame::History { chat_id, .. }, ServerFrame::History { chat_id: c, .. }) => chat_id == c,
        _ => false,
    }
}

impl<T: Transport, C: Cache> Client<T, C> {
    /// Takes over a connection that has just signed in. The token is not kept:
    /// the caller stores it for `resume`.
    pub fn new(t: T, signed_in: SignedIn, device: Device, strength: Strength, cache: C) -> Client<T, C> {
        Client {
            t,
            account: signed_in.account,
            secrets: signed_in.secrets,
            device,
            strength,
            cache,
            online: HashSet::new(),
            acked: HashMap::new(),
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

    /// The contacts online now.
    pub fn online(&self) -> &HashSet<Number> {
        &self.online
    }

    /// Brings the cache up to date: sends `sync` from its cursor (none for an
    /// empty cache, which gets a snapshot) and applies everything up to
    /// `synced`. What changed comes from the next `next`.
    pub async fn sync(&mut self) -> Result<(), Error> {
        // The account just signed in is the newest copy, and tells `apply`
        // which messages are mine.
        self.cache.set_account(self.account.clone())?;
        self.online.clear();
        let cursor = self.cache.cursor()?;
        self.t.send(&ClientFrame::Sync { cursor }).await?;
        loop {
            let frame = self.read().await?;
            let done = matches!(frame, ServerFrame::Synced { .. });
            let updates = self.handle(frame).await?;
            self.pending.extend(updates);
            if done {
                return Ok(());
            }
        }
    }

    /// Waits for the next pushed frame, applies it, and gives what changed.
    /// Updates held back while a request waited come first.
    pub async fn next(&mut self) -> Result<Vec<Update>, Error> {
        if !self.pending.is_empty() {
            return Ok(std::mem::take(&mut self.pending));
        }
        let frame = self.read().await?;
        self.handle(frame).await
    }

    /// The next frame. A sign-out wipes the cache before it comes back as `Err`.
    async fn read(&mut self) -> Result<ServerFrame, Error> {
        let frame = next(&mut self.t).await;
        if let Err(Error::SignedOut(_)) = &frame {
            self.cache.wipe()?;
        }
        frame
    }

    /// Applies a frame, keeps presence and the account up to date, and sends
    /// what `apply` asks for: pins, and acks that move forward.
    async fn handle(&mut self, frame: ServerFrame) -> Result<Vec<Update>, Error> {
        if let ServerFrame::Presence { number, online } = &frame {
            if *online {
                self.online.insert(*number);
            } else {
                self.online.remove(number);
            }
        }
        let mut updates = Vec::new();
        let replies = apply(&mut self.cache, &self.secrets, frame, &mut updates)?;
        if updates.contains(&Update::Profile)
            && let Some(account) = self.cache.account()?
        {
            self.account = account;
        }
        for reply in replies {
            if let ClientFrame::Received { chat_id, through } = &reply {
                let acked = self.acked.entry(chat_id.0).or_insert(0);
                if *through <= *acked {
                    continue;
                }
                *acked = *through;
            }
            self.t.send(&reply).await?;
        }
        Ok(updates)
    }

    /// Sends a frame and returns the reply, applying pushed frames on the way
    /// (their updates wait for `next`). Server errors and sign-outs come back
    /// as `Err`.
    pub async fn request(&mut self, frame: ClientFrame) -> Result<ServerFrame, Error> {
        self.t.send(&frame).await?;
        self.reply(&frame).await
    }

    async fn reply(&mut self, request: &ClientFrame) -> Result<ServerFrame, Error> {
        loop {
            let frame = self.read().await?;
            if answers(request, &frame) || !is_push(&frame) {
                return Ok(frame);
            }
            let updates = self.handle(frame).await?;
            self.pending.extend(updates);
        }
    }

    /// Sends a frame the server answers only with an error, or, once synced,
    /// with an event. A `list_devices` right behind it marks the end: the
    /// server handles frames in order, so an error comes before that reply.
    async fn request_quietly(&mut self, frame: ClientFrame) -> Result<Vec<DeviceInfo>, Error> {
        self.t.send(&frame).await?;
        let barrier = ClientFrame::ListDevices;
        self.t.send(&barrier).await?;
        match self.reply(&barrier).await {
            Ok(ServerFrame::Devices { devices }) => Ok(devices),
            Ok(_) => Err(Error::BadServerMessage),
            // Only the first frame fails these ways (a pending account fails
            // both), so the barrier's own answer is still to come.
            Err(
                e @ Error::Server {
                    code:
                        ErrorCode::BadDisplayName | ErrorCode::BadDevice | ErrorCode::RateLimited | ErrorCode::NotActive,
                    ..
                },
            ) => {
                let _ = self.reply(&barrier).await;
                Err(e)
            }
            Err(e) => Err(e),
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
