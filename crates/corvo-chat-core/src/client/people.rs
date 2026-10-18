//! The people actions: adding by number, answering requests, blocking,
//! reporting, and comparing safety codes.

use corvo_chat_proto::{
    Number,
    chat::{Person, Relation},
    frames::{ClientFrame, ServerFrame},
};

use super::Client;
use crate::cache::{Cache, CachedPerson};
use crate::crypto::safety_code;
use crate::{Error, Transport};

/// Reads a number typed by a person. A wrong check digit is
/// `Error::NumberTypo`; anything else that isn't a number, `Error::BadNumber`.
pub fn parse_number(text: &str) -> Result<Number, Error> {
    Number::parse(text).map_err(|e| match e {
        corvo_chat_proto::NumberError::CheckDigit => Error::NumberTypo,
        _ => Error::BadNumber,
    })
}

impl<T: Transport, C: Cache> Client<T, C> {
    /// Looks a number up and pins its keys, so a first message can be sealed
    /// and a safety code shown. A person not cached yet is cached as a
    /// stranger. A lookup counts toward the server's hourly limit.
    pub async fn add(&mut self, number: Number) -> Result<(), Error> {
        let keys = self.pinned(number).await?;
        if self.cache.person(number)?.is_none() {
            let person =
                Person { number, display_name: None, public_keys: keys, relation: Relation::None, deleted: false };
            self.cache.put_person(CachedPerson { person, mismatch: false })?;
        }
        Ok(())
    }

    /// Accepts their request: you become contacts.
    pub async fn accept(&mut self, number: Number) -> Result<(), Error> {
        self.request(ClientFrame::Accept { number }).await.map(drop)
    }

    /// Rejects their request: your side of the chat is cleared, and they
    /// aren't told.
    pub async fn reject(&mut self, number: Number) -> Result<(), Error> {
        self.request(ClientFrame::Reject { number }).await.map(drop)
    }

    pub async fn block(&mut self, number: Number) -> Result<(), Error> {
        self.request(ClientFrame::Block { number }).await.map(drop)
    }

    /// Makes a blocked person a stranger again.
    pub async fn unblock(&mut self, number: Number) -> Result<(), Error> {
        self.request(ClientFrame::Unblock { number }).await.map(drop)
    }

    /// Removes a contact; with `clear_history`, your side of the chat goes too.
    pub async fn remove(&mut self, number: Number, clear_history: bool) -> Result<(), Error> {
        self.request(ClientFrame::Remove { number, clear_history }).await.map(drop)
    }

    /// Reports a person to the admin, blocking them too if `block`.
    pub async fn report(&mut self, number: Number, reason: Option<&str>, block: bool) -> Result<(), Error> {
        let frame = ClientFrame::Report { number, reason: reason.map(Into::into), block };
        match self.request(frame).await? {
            ServerFrame::Reported => Ok(()),
            _ => Err(Error::BadServerMessage),
        }
    }

    /// The safety code for the chat with `number`, from the keys pinned for
    /// them and my own. Both sides get the same code.
    pub fn verify(&self, number: Number) -> Result<String, Error> {
        let theirs = self.cache.pin(number)?.ok_or(Error::NotPinned)?;
        Ok(safety_code(&self.secrets.public_keys(), &theirs))
    }
}
