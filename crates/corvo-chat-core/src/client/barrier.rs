//! Frames the server answers only with an error (or an event): they go
//! behind a `list_devices` barrier, whose answer marks where their errors
//! end. A barrier a dropped future left behind is finished before anything
//! else is sent; if it's unknown whether the barrier went out, a
//! `list_invites` marks the end instead, since its answer can't be taken
//! for the barrier's.

use corvo_chat_proto::{
    chat::Pin,
    frames::{ClientFrame, DeviceInfo, ErrorCode, ServerFrame},
};

use super::Client;
use crate::cache::Cache;
use crate::crypto::seal_pin;
use crate::{Error, Transport, TransportError};

/// A `list_devices` barrier still to be answered, so that if the future
/// waiting for it is dropped, its answer isn't taken for the next request's.
pub(super) struct Owed {
    /// Frames sent before it, each of which may fail first.
    before: usize,
    /// How many errors have been read for them so far.
    failed: usize,
    /// Whether they name a chat, so `chat_unknown` may be theirs.
    names_chat: bool,
    /// The answer that marks the end.
    end: End,
}

/// Which answer ends an owed barrier.
#[derive(Clone, Copy)]
enum End {
    /// Not known yet: the barrier may or may not have gone out.
    Unsent,
    /// The barrier went out, so its `devices`.
    Devices,
    /// A `list_invites` went out after it, so its `invites`.
    Invites,
}

/// A server fault partway through a barrier. It can't be told whose it
/// is, so the answer that ends the barrier may still be on its way: the
/// connection is given up rather than read out of step.
fn fault() -> Error {
    Error::Transport(TransportError::Io("The server failed partway through a request.".into()))
}

impl<T: Transport, C: Cache> Client<T, C> {
    /// Sends the pins the server hasn't confirmed, behind one barrier. The
    /// echoed `pin` event confirms each; past the pin limit, one stays only
    /// here and isn't tried again.
    pub(super) async fn send_pins(&mut self) -> Result<(), Error> {
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

    /// Sends frames the server answers only with an error, or, once synced,
    /// with an event, then a `list_devices` behind them. The server handles
    /// frames in order, so every error before that reply is one of theirs.
    /// Gives the devices and those errors.
    pub(super) async fn behind_barrier(
        &mut self,
        frames: &[ClientFrame],
    ) -> Result<(Vec<DeviceInfo>, Vec<Error>), Error> {
        self.settle().await?;
        let names_chat = frames.iter().any(|f| {
            matches!(f, ClientFrame::History { .. } | ClientFrame::Read { .. } | ClientFrame::Received { .. })
        });
        // Noted before anything is sent, so a dropped future leaves it to `settle`.
        self.owed = Some(Owed { before: frames.len(), failed: 0, names_chat, end: End::Unsent });
        for frame in frames {
            self.send_checked(frame).await?;
        }
        self.send_checked(&ClientFrame::ListDevices).await?;
        if let Some(owed) = &mut self.owed {
            owed.end = End::Devices;
        }
        self.barrier_answer().await
    }

    /// Reads up to the owed barrier's answer, counting the errors of the
    /// frames before it. `list_devices` itself fails only as `internal`, which
    /// ends the connection, or as `not_active`, which every frame before it
    /// got too.
    pub(super) async fn barrier_answer(&mut self) -> Result<(Vec<DeviceInfo>, Vec<Error>), Error> {
        let barrier = ClientFrame::ListDevices;
        let mut failed = Vec::new();
        let result = loop {
            let Some(names_chat) = self.owed.as_ref().map(|o| o.names_chat) else {
                break Err(Error::BadServerMessage);
            };
            match self.wait_for(&barrier, names_chat).await {
                Ok(ServerFrame::Devices { devices }) => break Ok((devices, failed)),
                Ok(_) => break Err(Error::BadServerMessage),
                Err(Error::Server { code: ErrorCode::Internal, .. }) => break Err(fault()),
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
    /// is sent or read. The left frames' errors are dropped: a pin not
    /// confirmed is sent again after the next sync.
    ///
    /// If it's unknown whether the barrier itself went out, a `list_invites`
    /// goes instead, and everything up to its `invites` is skipped: the
    /// barrier's `devices`, if it came, and the frames' errors. An error past
    /// as many as could be theirs is `list_invites`' own.
    pub(super) async fn settle(&mut self) -> Result<(), Error> {
        let Some(owed) = &self.owed else { return Ok(()) };
        let result = match owed.end {
            End::Devices => self.barrier_answer().await.map(drop),
            End::Unsent => {
                self.send_checked(&ClientFrame::ListInvites).await?;
                if let Some(owed) = &mut self.owed {
                    owed.end = End::Invites;
                }
                self.invites_answer().await
            }
            End::Invites => self.invites_answer().await,
        };
        match result {
            Ok(()) | Err(Error::Server { .. } | Error::BadServerMessage) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Reads up to the answer of the `list_invites` sent in place of a
    /// barrier, skipping what comes before it.
    async fn invites_answer(&mut self) -> Result<(), Error> {
        let result = loop {
            let Some(owed) = &self.owed else { break Ok(()) };
            let (names_chat, most) = (owed.names_chat, owed.before + 1);
            match self.wait_for(&ClientFrame::ListInvites, names_chat).await {
                Ok(ServerFrame::Invites { .. }) => break Ok(()),
                Ok(_) => {}
                Err(Error::Server { code: ErrorCode::Internal, .. }) => break Err(fault()),
                Err(e @ Error::Server { .. }) => match &mut self.owed {
                    Some(owed) if owed.failed < most => owed.failed += 1,
                    _ => break Err(e),
                },
                Err(e) => break Err(e),
            }
        };
        self.owed = None;
        result
    }

    /// One frame behind the barrier: its error, or the devices.
    pub(super) async fn request_quietly(&mut self, frame: ClientFrame) -> Result<Vec<DeviceInfo>, Error> {
        let (devices, failed) = self.behind_barrier(&[frame]).await?;
        match failed.into_iter().next() {
            Some(e) => Err(e),
            None => Ok(devices),
        }
    }
}
