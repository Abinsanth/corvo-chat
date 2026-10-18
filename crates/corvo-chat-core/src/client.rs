//! The live client: a signed-in connection that sends a request and waits for
//! its reply, and the account actions built on that.

use corvo_chat_proto::{
    B64, Number,
    frames::{Account, ClientFrame, Device, DeviceInfo, InviteInfo, ServerFrame, SignedOutReason},
    opaque::recovery_ksf,
};

use crate::account::{SignedIn, next};
use crate::crypto::{CopyKind, RecoveryCode, Secrets, check_password, lock_copy, normalize_password, opaque};
use crate::{Error, Strength, Transport};

/// A signed-in connection. It owns the transport, so it is the only thing that
/// reads or writes frames from here on.
pub struct Client<T: Transport> {
    t: T,
    account: Account,
    secrets: Secrets,
    device: Device,
    strength: Strength,
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
            // No request here asks for history, so any that arrives is a push.
            | ServerFrame::History { .. }
    )
}

impl<T: Transport> Client<T> {
    /// Takes over a connection that has just signed in. The token is not kept:
    /// the caller stores it for `resume`.
    pub fn new(t: T, signed_in: SignedIn, device: Device, strength: Strength) -> Client<T> {
        Client { t, account: signed_in.account, secrets: signed_in.secrets, device, strength }
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    pub fn secrets(&self) -> &Secrets {
        &self.secrets
    }

    /// Where pushed frames go. Nothing uses them yet.
    fn on_push(&mut self, _frame: ServerFrame) {}

    /// Sends a frame and returns the reply, handing pushed frames on the way
    /// to `on_push`. Server errors and sign-outs come back as `Err`.
    pub async fn request(&mut self, frame: ClientFrame) -> Result<ServerFrame, Error> {
        self.t.send(&frame).await?;
        self.reply().await
    }

    async fn reply(&mut self) -> Result<ServerFrame, Error> {
        loop {
            let frame = next(&mut self.t).await?;
            if is_push(&frame) {
                self.on_push(frame);
            } else {
                return Ok(frame);
            }
        }
    }

    /// Sends a frame the server answers only with an error, or, once synced,
    /// with an event. A `list_devices` right behind it marks the end: the
    /// server handles frames in order, so an error comes before that reply.
    async fn request_quietly(&mut self, frame: ClientFrame) -> Result<Vec<DeviceInfo>, Error> {
        self.t.send(&frame).await?;
        match self.request(ClientFrame::ListDevices).await {
            Ok(ServerFrame::Devices { devices }) => Ok(devices),
            Ok(_) => Err(Error::BadServerMessage),
            Err(e @ Error::Server { .. }) => {
                // The `list_devices` reply is still on its way.
                let _ = self.reply().await;
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
