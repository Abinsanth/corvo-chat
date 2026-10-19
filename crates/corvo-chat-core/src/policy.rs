//! What every driver decides the same way, whatever it runs on: how long to
//! wait, when a connection is over, and what to do next. The drivers only
//! supply the clock, the sockets and the cache.

use corvo_chat_proto::frames::SignedOutReason;

use crate::Error;
use crate::commands::{Command, Reply};

/// Why a connection ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// Dropped or broken: try again after the back-off.
    Lost,
    /// Signed out, for good.
    SignedOut(SignedOutReason),
    /// The server needs a newer app: trying again won't help.
    UpdateRequired,
    /// The app dropped the driver.
    Shutdown,
}

impl From<Error> for End {
    fn from(e: Error) -> End {
        match e {
            Error::SignedOut(reason) => End::SignedOut(reason),
            Error::UpdateRequired => End::UpdateRequired,
            _ => End::Lost,
        }
    }
}

/// What the driver does once a connection has ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Wait out the back-off, then connect again.
    Reconnect,
    /// Stay up on what's saved: reconnecting wouldn't help.
    StayOffline,
    /// Stop for good.
    Stop,
}

pub(crate) fn after_end(end: &End) -> Next {
    match end {
        End::Lost => Next::Reconnect,
        End::UpdateRequired => Next::StayOffline,
        End::SignedOut(_) | End::Shutdown => Next::Stop,
    }
}

/// The longest wait between tries, in seconds.
pub(crate) const MAX_BACKOFF: u64 = 30;

/// The wait after `b` seconds: 1, 2, 4 … 30.
pub(crate) fn next_backoff(b: u64) -> u64 {
    (b * 2).min(MAX_BACKOFF)
}

/// How long one command may take before the connection is given up: a
/// reply that never comes means the connection is no good.
pub(crate) const COMMAND_TIMEOUT_MS: u64 = 30_000;
/// The same for a command that proves the password again, which runs the
/// slow password hash here before the server can answer.
pub(crate) const PASSWORD_TIMEOUT_MS: u64 = 120_000;
/// How long the first sync after connecting may take before the connection
/// is dropped and made again.
pub(crate) const FIRST_SYNC_TIMEOUT_MS: u64 = 30_000;

/// How long `command` may take, in milliseconds.
pub(crate) fn time_limit_ms(command: &Command) -> u64 {
    match command {
        Command::ChangePassword { .. } | Command::NewRecoveryCode { .. } | Command::DeleteAccount { .. } => {
            PASSWORD_TIMEOUT_MS
        }
        _ => COMMAND_TIMEOUT_MS,
    }
}

/// Whether a command's outcome ends the connection, and how: a sign-out, a
/// broken connection, or a reply that never came or made no sense (a reply
/// out of step can't be trusted).
pub(crate) fn end_after(command: &Command, result: &Result<Reply, Error>, timed_out: bool) -> Option<End> {
    match result {
        _ if timed_out => Some(End::Lost),
        Ok(_) => match command {
            Command::SignOut => Some(End::SignedOut(SignedOutReason::ThisDevice)),
            Command::DeleteAccount { .. } => Some(End::SignedOut(SignedOutReason::AccountDeleted)),
            _ => None,
        },
        Err(Error::SignedOut(reason)) => Some(End::SignedOut(*reason)),
        Err(Error::Transport(_) | Error::BadServerMessage | Error::Cache(_)) => Some(End::Lost),
        Err(_) => None,
    }
}

/// Whether the connection should sync now: an account that was pending and
/// has just been found active. A check that finds it still pending can't
/// sync yet, and one already synced doesn't again.
pub(crate) fn syncs_after(was_pending: bool, command: &Command, result: &Result<Reply, Error>) -> bool {
    let activating = was_pending && matches!(command, Command::Activate { .. } | Command::CheckActive);
    activating && matches!(result, Ok(Reply::Account(a)) if a.pending_until.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TransportError;
    use corvo_chat_proto::{Number, frames::Account};
    use zeroize::Zeroizing;

    fn send() -> Command {
        Command::SendText { to: Number::from_body(222_222_222).unwrap(), text: "hi".into() }
    }

    #[test]
    fn backoff_doubles_to_thirty() {
        let mut b = 1;
        let mut seen = vec![b];
        for _ in 0..6 {
            b = next_backoff(b);
            seen.push(b);
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn password_commands_get_two_minutes() {
        let secret = || Zeroizing::new("pw".to_string());
        for command in [
            Command::ChangePassword { old: secret(), new: secret() },
            Command::NewRecoveryCode { password: secret() },
            Command::DeleteAccount { password: secret() },
        ] {
            assert_eq!(time_limit_ms(&command), 120_000);
        }
        assert_eq!(time_limit_ms(&send()), 30_000);
    }

    #[test]
    fn ends_match_the_old_driver() {
        let ok = Ok(Reply::Done);
        assert_eq!(end_after(&send(), &Err(Error::Offline), true), Some(End::Lost));
        let signed_out = Err(Error::SignedOut(SignedOutReason::ByAnotherDevice));
        assert_eq!(end_after(&send(), &signed_out, false), Some(End::SignedOut(SignedOutReason::ByAnotherDevice)));
        assert_eq!(end_after(&Command::SignOut, &ok, false), Some(End::SignedOut(SignedOutReason::ThisDevice)));
        assert_eq!(end_after(&send(), &ok, false), None);
        assert_eq!(end_after(&send(), &Err(Error::Offline), false), None);
        let broken = Err(Error::Transport(TransportError::Io("x".into())));
        assert_eq!(end_after(&send(), &broken, false), Some(End::Lost));
    }

    #[test]
    fn every_other_end_after_a_command() {
        let password = Zeroizing::new("pw".to_string());
        assert_eq!(
            end_after(&Command::DeleteAccount { password }, &Ok(Reply::Done), false),
            Some(End::SignedOut(SignedOutReason::AccountDeleted))
        );
        assert_eq!(end_after(&send(), &Err(Error::BadServerMessage), false), Some(End::Lost));
        assert_eq!(end_after(&send(), &Err(Error::Cache("full".into())), false), Some(End::Lost));
    }

    fn account(pending_until: Option<i64>) -> Result<Reply, Error> {
        Ok(Reply::Account(Account {
            username: "abin".into(),
            number: Number::from_body(111_111_111).unwrap(),
            display_name: "Abin".into(),
            pending_until,
        }))
    }

    #[test]
    fn only_a_pending_account_found_active_syncs() {
        let (active, pending) = (account(None), account(Some(1_800_000_000)));
        assert!(syncs_after(true, &Command::CheckActive, &active));
        assert!(syncs_after(true, &Command::Activate { code: "c".into() }, &active));
        assert!(!syncs_after(true, &Command::CheckActive, &pending));
        assert!(!syncs_after(false, &Command::CheckActive, &active));
        assert!(!syncs_after(true, &Command::Account, &active));
    }

    #[test]
    fn update_required_never_reconnects() {
        assert_eq!(after_end(&End::UpdateRequired), Next::StayOffline);
        assert_eq!(after_end(&End::Lost), Next::Reconnect);
        assert_eq!(after_end(&End::Shutdown), Next::Stop);
        assert_eq!(after_end(&End::SignedOut(SignedOutReason::ThisDevice)), Next::Stop);
    }
}
