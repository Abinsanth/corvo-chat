//! What an app asks the driver for: every client action and the two views,
//! and what comes back. Online they run on the live client; offline only
//! sending (into the outbox) and reading what's saved work.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use corvo_chat_proto::{
    Number,
    frames::{Account, DeviceInfo, InviteInfo},
};
use zeroize::Zeroizing;

use super::NativeTransport;
use crate::cache::{Cache, SqliteCache};
use crate::client::{Client, queue_text};
use crate::crypto::{MsgId, RecoveryCode, Secrets, safety_code};
use crate::view::{self, ChatRow, ShownMessage};
use crate::{Error, LocalKey, Strength};

/// The live client the driver runs.
pub(super) type Live = Client<NativeTransport, SqliteCache>;

/// One thing for the driver to do. Passwords are wiped once used.
pub enum Command {
    /// Activates this pending account with its inviter's code.
    Activate {
        code: String,
    },
    /// Invites a pending person by number.
    Invite {
        number: Number,
    },
    /// The invites still open.
    Invites,
    /// This account's devices.
    Devices,
    SignOutDevice {
        id: [u8; 16],
    },
    /// Signs this device out; the driver then stops as for any sign-out.
    SignOut,
    SetDisplayName {
        name: String,
    },
    RenameDevice {
        name: String,
    },
    /// Changes the password. The reply is the key file locked under the new
    /// one, to save in place of the old.
    ChangePassword {
        old: Zeroizing<String>,
        new: Zeroizing<String>,
    },
    NewRecoveryCode {
        password: Zeroizing<String>,
    },
    /// Deletes the account; the driver then stops as for any sign-out.
    DeleteAccount {
        password: Zeroizing<String>,
    },
    /// Sends a text; offline it waits in the outbox.
    SendText {
        to: Number,
        text: String,
    },
    /// Pages in older messages.
    Older {
        chat_id: [u8; 16],
    },
    MarkRead {
        chat_id: [u8; 16],
    },
    /// Looks a number up and pins its keys.
    Add {
        number: Number,
    },
    Accept {
        number: Number,
    },
    Reject {
        number: Number,
    },
    Block {
        number: Number,
    },
    Unblock {
        number: Number,
    },
    Remove {
        number: Number,
        clear_history: bool,
    },
    Report {
        number: Number,
        reason: Option<String>,
        block: bool,
    },
    /// The safety code for the chat with `number`.
    Verify {
        number: Number,
    },
    /// The account as last saved.
    Account,
    /// The chat list.
    Chats,
    /// A chat's messages: up to `limit` before seq `before`, the newest with `None`.
    Messages {
        chat_id: [u8; 16],
        before: Option<i64>,
        limit: usize,
    },
}

/// What a command gives back.
#[derive(Debug)]
pub enum Reply {
    Done,
    Account(Account),
    Invite(InviteInfo),
    Invites(Vec<InviteInfo>),
    Devices(Vec<DeviceInfo>),
    /// A new recovery code, to show once.
    RecoveryCode(RecoveryCode),
    /// The key file locked under a new password, to save.
    KeyFile(LocalKey),
    /// The id of a message sent or queued.
    Sent(MsgId),
    /// From `Older`: whether there's nothing older left.
    Complete(bool),
    SafetyCode(String),
    Chats(Vec<ChatRow>),
    Messages(Vec<ShownMessage>),
}

/// What locking a new key file takes, besides the password and secrets.
pub(super) struct KeyFile {
    pub server: String,
    pub username: String,
    pub device_id: [u8; 16],
    pub token: Zeroizing<[u8; 32]>,
    pub strength: Strength,
}

impl KeyFile {
    fn lock(&self, password: &str, secrets: &Secrets) -> LocalKey {
        LocalKey::lock(&self.server, &self.username, self.device_id, password, secrets, &self.token, self.strength)
    }
}

/// A new message's id and time, made before a command runs, so that a send
/// cut short can still be found in the outbox.
pub(super) struct NewMessage {
    pub id: MsgId,
    pub now_ms: u64,
}

impl NewMessage {
    pub fn now() -> NewMessage {
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        NewMessage { id: MsgId::new(now_ms), now_ms }
    }
}

/// Runs a command on the live client, to the end. A `SendText` uses `new`.
pub(super) async fn online(
    client: &mut Live,
    command: Command,
    key_file: &KeyFile,
    new: &NewMessage,
) -> Result<Reply, Error> {
    let command = match local(client.cache(), client.secrets(), client.online(), command) {
        Ok(answer) => return answer,
        Err(command) => command,
    };
    let done = |_| Reply::Done;
    match command {
        Command::Activate { code } => client.activate(&code).await.map(Reply::Account),
        Command::Invite { number } => client.invite(number).await.map(Reply::Invite),
        Command::Invites => client.invites().await.map(Reply::Invites),
        Command::Devices => client.devices().await.map(Reply::Devices),
        Command::SignOutDevice { id } => client.sign_out_device(id).await.map(Reply::Devices),
        Command::SignOut => client.sign_out().await.map(done),
        Command::SetDisplayName { name } => client.set_display_name(&name).await.map(done),
        Command::RenameDevice { name } => client.rename_device(&name).await.map(done),
        Command::ChangePassword { old, new } => {
            client.change_password(&old, &new).await?;
            Ok(Reply::KeyFile(key_file.lock(&new, client.secrets())))
        }
        Command::NewRecoveryCode { password } => client.new_recovery_code(&password).await.map(Reply::RecoveryCode),
        Command::DeleteAccount { password } => client.delete_account(&password).await.map(done),
        Command::SendText { to, text } => client.send_text_as(to, &text, new.id, new.now_ms).await.map(Reply::Sent),
        Command::Older { chat_id } => client.older(chat_id).await.map(Reply::Complete),
        Command::MarkRead { chat_id } => client.mark_read(chat_id).await.map(done),
        Command::Add { number } => client.add(number).await.map(done),
        Command::Accept { number } => client.accept(number).await.map(done),
        Command::Reject { number } => client.reject(number).await.map(done),
        Command::Block { number } => client.block(number).await.map(done),
        Command::Unblock { number } => client.unblock(number).await.map(done),
        Command::Remove { number, clear_history } => client.remove(number, clear_history).await.map(done),
        Command::Report { number, reason, block } => client.report(number, reason.as_deref(), block).await.map(done),
        Command::Verify { .. } | Command::Account | Command::Chats | Command::Messages { .. } => {
            unreachable!("answered from the cache above")
        }
    }
}

/// Answers a command with no connection: a text waits in the outbox, what's
/// saved can be read, and anything else is `Offline`.
pub(super) fn offline(cache: &mut SqliteCache, secrets: &Secrets, command: Command) -> Result<Reply, Error> {
    match local(cache, secrets, &HashSet::new(), command) {
        Ok(answer) => answer,
        Err(Command::SendText { to, text }) => {
            let me = cache.account()?.ok_or(Error::Offline)?.number;
            let new = NewMessage::now();
            queue_text(cache, me, to, &text, new.id, new.now_ms).map(Reply::Sent)
        }
        Err(_) => Err(Error::Offline),
    }
}

/// The commands that only read the cache, the same online or off. Any
/// other command comes back as `Err`.
fn local(
    cache: &dyn Cache,
    secrets: &Secrets,
    online: &HashSet<Number>,
    command: Command,
) -> Result<Result<Reply, Error>, Command> {
    Ok(match command {
        Command::Verify { number } => match cache.pin(number) {
            Ok(Some(theirs)) => Ok(Reply::SafetyCode(safety_code(&secrets.public_keys(), &theirs))),
            Ok(None) => Err(Error::NotPinned),
            Err(e) => Err(e),
        },
        Command::Account => cache.account().and_then(|a| a.ok_or(Error::Offline)).map(Reply::Account),
        Command::Chats => view::chats(cache, secrets, online).map(Reply::Chats),
        Command::Messages { chat_id, before, limit } => {
            view::messages(cache, secrets, chat_id, before, limit).map(Reply::Messages)
        }
        other => return Err(other),
    })
}
