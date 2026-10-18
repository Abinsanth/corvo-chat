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

pub use crypto::{CopyKind, Secrets, lock_copy, unlock_copy};
