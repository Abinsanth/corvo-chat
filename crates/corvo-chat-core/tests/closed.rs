use std::collections::VecDeque;

use corvo_chat_core::{Client, Error, MemoryCache, Secrets, SignedIn, Transport, TransportError};
use corvo_chat_proto::{
    Number,
    frames::{Account, ClientFrame, ServerFrame, SignedOutReason},
};
use zeroize::Zeroizing;

mod common;

/// A connection the server already closed: every send fails, and `recv`
/// gives what was still waiting, then `Closed`.
struct Closed {
    waiting: VecDeque<ServerFrame>,
}

impl Transport for Closed {
    async fn send(&mut self, _frame: &ClientFrame) -> Result<(), TransportError> {
        Err(TransportError::Io("connection reset".into()))
    }

    async fn recv(&mut self) -> Result<ServerFrame, TransportError> {
        self.waiting.pop_front().ok_or(TransportError::Closed)
    }
}

fn client(waiting: Vec<ServerFrame>) -> Client<Closed, MemoryCache> {
    let account = Account {
        username: "abin-7k3q".into(),
        number: Number::from_body(111_111_111).unwrap(),
        display_name: "Abin".into(),
        pending_until: None,
    };
    let signed_in = SignedIn { token: Zeroizing::new([0; 32]), account, secrets: Secrets::generate() };
    let t = Closed { waiting: waiting.into() };
    Client::new(t, signed_in, common::device("laptop"), common::S, MemoryCache::default())
}

#[tokio::test]
async fn a_waiting_sign_out_is_read_when_the_send_fails() {
    let mut client = client(vec![ServerFrame::SignedOut { reason: SignedOutReason::PasswordChanged }]);
    let err = client.devices().await.unwrap_err();
    assert!(matches!(err, Error::SignedOut(SignedOutReason::PasswordChanged)), "{err:?}");
}

#[tokio::test]
async fn a_failed_send_with_nothing_waiting_is_the_transport_error() {
    let mut client = client(vec![]);
    let err = client.devices().await.unwrap_err();
    assert!(matches!(err, Error::Transport(TransportError::Io(_))), "{err:?}");
}
