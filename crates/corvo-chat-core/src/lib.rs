//! The corvo-chat client core: the crypto, the server connection and the sync
//! logic that every client (terminal now, web and phone later) builds on.
//!
//! The parts that need threads, sockets, files or a clock sit behind the
//! `native` feature, so the crate still builds for the browser without it.

/// How hard the slow password steps work. Tests use the fast one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strength {
    Normal,
    #[cfg(feature = "insecure-test-kdf")]
    FastInsecure,
}

mod account;
mod cache;
mod client;
mod crypto;
mod keyfile;
#[cfg(feature = "native")]
pub mod native;
mod sync;
mod transport;

pub use account::{Recovery, SignedIn, SignedUp, hello, resume, sign_in, sign_up};
pub use cache::{Cache, CachedChat, CachedPerson, MemoryCache, OutState, Outgoing};
pub use client::Client;
pub use crypto::{
    ChatKey, CopyKind, MsgId, Plain, RecoveryCode, Secrets, chat_key, check_password, decrypt_message, encrypt_message,
    lock_copy, new_chat, open_pin, safety_code, seal_pin, unlock_copy,
};
pub use keyfile::LocalKey;
pub use sync::Update;
pub use transport::{MaybeSend, Transport, TransportError};

use corvo_chat_proto::frames::{ErrorCode, SignedOutReason};

/// What can go wrong in the core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("The recovery code isn't right. Check it and try again.")]
    BadRecoveryCode,
    #[error("Use at least 10 characters.")]
    PasswordTooShort,
    #[error("That password is too common. Pick another.")]
    PasswordTooCommon,
    #[error("Username or password is wrong")]
    WrongCredentials,
    #[error("The server sent something unexpected.")]
    BadServerMessage,
    #[error("This chat's keys don't check out.")]
    BadChat,
    #[error("That message is too long.")]
    TooLong,
    #[error("Couldn't decrypt that message.")]
    Undecryptable,
    #[error("Wrong password.")]
    WrongPassword,
    #[error("The key file is damaged.")]
    DamagedKeyFile,
    #[error("This app is too old. Update it.")]
    UpdateRequired,
    #[error("{message}")]
    Server { code: ErrorCode, message: String, until: Option<i64> },
    #[error("This device was signed out.")]
    SignedOut(SignedOutReason),
    #[error(transparent)]
    Transport(#[from] TransportError),
}
