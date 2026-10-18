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

mod crypto;
mod keyfile;

pub use crypto::{
    CopyKind, RecoveryCode, Secrets, check_password, lock_copy, open_pin, safety_code, seal_pin, unlock_copy,
};
pub use keyfile::LocalKey;

/// What can go wrong in the core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("The recovery code isn't right. Check it and try again.")]
    BadRecoveryCode,
    #[error("Use at least 10 characters.")]
    PasswordTooShort,
    #[error("That password is too common. Pick another.")]
    PasswordTooCommon,
    #[error("Wrong username or password.")]
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
}
