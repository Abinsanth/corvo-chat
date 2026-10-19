//! What an app asks the driver for: every client action and the two views,
//! and what comes back. Online they run on the live client; offline only
//! sending (into the outbox) and reading what's saved work.

use std::collections::HashSet;
use std::fmt;

use corvo_chat_proto::{
    Number,
    frames::{Account, DeviceInfo, InviteInfo},
};
use zeroize::Zeroizing;

use crate::cache::Cache;
use crate::client::{Client, queue_text};
use crate::crypto::{MsgId, RecoveryCode, Secrets, safety_code};
use crate::keyfile::KeyFile;
use crate::view::{self, ChatRow, PersonRow, ShownMessage};
use crate::{Error, LocalKey, Transport};

/// One thing for the driver to do. Passwords are wiped once used, and
/// `{:?}` never shows them, the invite code or a message's text.
#[derive(Clone, PartialEq, Eq)]
pub enum Command {
    /// Activates this pending account with its inviter's code.
    Activate {
        code: String,
    },
    /// Whether this pending account has been activated elsewhere (by the
    /// server's admin). The reply is the account, active or still pending.
    CheckActive,
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
    /// Everyone known here but me.
    People,
    /// A chat's messages: up to `limit` before seq `before`, the newest with `None`.
    Messages {
        chat_id: [u8; 16],
        before: Option<i64>,
        limit: usize,
    },
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn number(f: &mut fmt::Formatter<'_>, name: &str, number: &Number) -> fmt::Result {
            f.debug_struct(name).field("number", number).finish()
        }
        fn chat(f: &mut fmt::Formatter<'_>, name: &str, chat_id: &[u8; 16]) -> fmt::Result {
            f.debug_struct(name).field("chat_id", chat_id).finish()
        }
        fn hidden(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
            f.debug_struct(name).finish_non_exhaustive()
        }
        match self {
            Command::Activate { .. } => hidden(f, "Activate"),
            Command::CheckActive => f.write_str("CheckActive"),
            Command::Invite { number: n } => number(f, "Invite", n),
            Command::Invites => f.write_str("Invites"),
            Command::Devices => f.write_str("Devices"),
            Command::SignOutDevice { id } => f.debug_struct("SignOutDevice").field("id", id).finish(),
            Command::SignOut => f.write_str("SignOut"),
            Command::SetDisplayName { name } => f.debug_struct("SetDisplayName").field("name", name).finish(),
            Command::RenameDevice { name } => f.debug_struct("RenameDevice").field("name", name).finish(),
            Command::ChangePassword { .. } => hidden(f, "ChangePassword"),
            Command::NewRecoveryCode { .. } => hidden(f, "NewRecoveryCode"),
            Command::DeleteAccount { .. } => hidden(f, "DeleteAccount"),
            Command::SendText { to, .. } => f.debug_struct("SendText").field("to", to).finish_non_exhaustive(),
            Command::Older { chat_id } => chat(f, "Older", chat_id),
            Command::MarkRead { chat_id } => chat(f, "MarkRead", chat_id),
            Command::Add { number: n } => number(f, "Add", n),
            Command::Accept { number: n } => number(f, "Accept", n),
            Command::Reject { number: n } => number(f, "Reject", n),
            Command::Block { number: n } => number(f, "Block", n),
            Command::Unblock { number: n } => number(f, "Unblock", n),
            Command::Remove { number, clear_history } => {
                f.debug_struct("Remove").field("number", number).field("clear_history", clear_history).finish()
            }
            Command::Report { number, block, .. } => {
                f.debug_struct("Report").field("number", number).field("block", block).finish_non_exhaustive()
            }
            Command::Verify { number: n } => number(f, "Verify", n),
            Command::Account => f.write_str("Account"),
            Command::Chats => f.write_str("Chats"),
            Command::People => f.write_str("People"),
            Command::Messages { chat_id, before, limit } => f
                .debug_struct("Messages")
                .field("chat_id", chat_id)
                .field("before", before)
                .field("limit", limit)
                .finish(),
        }
    }
}

/// What a command gives back. `{:?}` never shows a recovery code, a key
/// file, an invite code or a message's text.
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
    People(Vec<PersonRow>),
    Messages(Vec<ShownMessage>),
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn many(f: &mut fmt::Formatter<'_>, name: &str, n: usize) -> fmt::Result {
            write!(f, "{name}([{n} items])")
        }
        match self {
            Reply::Done => f.write_str("Done"),
            Reply::Account(account) => f.debug_tuple("Account").field(account).finish(),
            Reply::Invite(invite) => f.debug_struct("Invite").field("number", &invite.number).finish_non_exhaustive(),
            Reply::Invites(invites) => many(f, "Invites", invites.len()),
            Reply::Devices(devices) => many(f, "Devices", devices.len()),
            Reply::RecoveryCode(_) => f.write_str("RecoveryCode(..)"),
            Reply::KeyFile(_) => f.write_str("KeyFile(..)"),
            Reply::Sent(id) => f.debug_tuple("Sent").field(id).finish(),
            Reply::Complete(complete) => f.debug_tuple("Complete").field(complete).finish(),
            Reply::SafetyCode(code) => f.debug_tuple("SafetyCode").field(code).finish(),
            Reply::Chats(rows) => many(f, "Chats", rows.len()),
            Reply::People(rows) => many(f, "People", rows.len()),
            Reply::Messages(messages) => many(f, "Messages", messages.len()),
        }
    }
}

/// A new message's id and time, made before a command runs, so that a send
/// cut short can still be found in the outbox.
pub(crate) struct NewMessage {
    pub id: MsgId,
    pub now_ms: u64,
}

/// The new message for a command run at `now_ms`. There's no clock in here:
/// each driver reads its own.
pub(crate) fn at(now_ms: u64) -> NewMessage {
    NewMessage { id: MsgId::new(now_ms), now_ms }
}

/// Runs a command on the live client, to the end. A `SendText` uses `new`.
pub(crate) async fn online<T: Transport, C: Cache>(
    client: &mut Client<T, C>,
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
        Command::CheckActive => client.check_active().await.map(Reply::Account),
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
        Command::Verify { .. } | Command::Account | Command::Chats | Command::People | Command::Messages { .. } => {
            unreachable!("answered from the cache above")
        }
    }
}

/// Answers a command with no connection: a text waits in the outbox, what's
/// saved (the devices too) can be read, and anything else is `Offline`.
pub(crate) fn offline<C: Cache>(
    cache: &mut C,
    secrets: &Secrets,
    command: Command,
    new: &NewMessage,
) -> Result<Reply, Error> {
    match local(cache, secrets, &HashSet::new(), command) {
        Ok(answer) => answer,
        Err(Command::Devices) => cache.devices().map(Reply::Devices),
        Err(Command::SendText { to, text }) => {
            let me = cache.account()?.ok_or(Error::Offline)?.number;
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
        Command::People => view::people(cache, online).map(Reply::People),
        Command::Messages { chat_id, before, limit } => {
            view::messages(cache, secrets, chat_id, before, limit).map(Reply::Messages)
        }
        other => return Err(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::MemoryCache;
    use corvo_chat_proto::frames::Account;

    #[test]
    fn offline_queues_a_message_in_any_cache() {
        let me = Number::from_body(111_111_111).unwrap();
        let to = Number::from_body(222_222_222).unwrap();
        let mut cache = MemoryCache::new();
        cache
            .set_account(Account {
                username: "abin".into(),
                number: me,
                display_name: "Abin".into(),
                pending_until: None,
            })
            .unwrap();
        let reply = offline(&mut cache, &Secrets::generate(), Command::SendText { to, text: "hi".into() }, &at(1_000));
        let Ok(Reply::Sent(id)) = reply else { panic!("{reply:?}") };
        let outbox = cache.outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!((outbox[0].msg_id, outbox[0].created_ms), (id, 1_000));
    }

    #[test]
    fn offline_refuses_what_needs_the_server() {
        let number = Number::from_body(222_222_222).unwrap();
        let reply = offline(&mut MemoryCache::new(), &Secrets::generate(), Command::Invite { number }, &at(1));
        assert!(matches!(reply, Err(Error::Offline)));
    }

    #[test]
    fn debug_never_shows_secrets_or_message_text() {
        let secret = || Zeroizing::new("correct horse battery".to_string());
        let number = Number::parse("482 910 5572").unwrap();
        for command in [
            Command::Activate { code: "123456".into() },
            Command::ChangePassword { old: secret(), new: secret() },
            Command::NewRecoveryCode { password: secret() },
            Command::DeleteAccount { password: secret() },
            Command::SendText { to: number, text: "meet at 123456".into() },
            Command::Report { number, reason: Some("correct horse battery".into()), block: true },
        ] {
            let shown = format!("{command:?}");
            assert!(!shown.contains("horse") && !shown.contains("123456"), "{shown}");
        }
        assert_eq!(format!("{:?}", Command::Chats), "Chats");
        let messages = Command::Messages { chat_id: [0; 16], before: Some(7), limit: 50 };
        assert!(format!("{messages:?}").contains("before: Some(7)"));
        assert_eq!(messages.clone(), messages);
    }

    #[test]
    fn reply_debug_never_shows_codes_keys_or_text() {
        let number = Number::parse("482 910 5572").unwrap();
        let invite = InviteInfo { number, display_name: "ana".into(), code: "123456".into(), expires_at: 0 };
        let message = ShownMessage {
            chat_id: [1; 16],
            seq: Some(1),
            msg_id: MsgId([2; 16]),
            mine: true,
            from: number,
            text: Ok("meet at 123456".into()),
            sent_at_ms: 0,
            mark: None,
        };
        for reply in [Reply::Invite(invite.clone()), Reply::Invites(vec![invite]), Reply::Messages(vec![message])] {
            let shown = format!("{reply:?}");
            assert!(!shown.contains("123456"), "{shown}");
        }
        assert_eq!(format!("{:?}", Reply::Complete(true)), "Complete(true)");
    }
}
