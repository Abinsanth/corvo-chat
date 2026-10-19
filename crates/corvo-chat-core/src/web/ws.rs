//! The browser's WebSocket as a transport. TLS, pings and pongs are the
//! browser's business.

use std::pin::Pin;
use std::task::{Context, Poll};

use corvo_chat_proto::frames::{ClientFrame, ServerFrame};
use futures::{FutureExt, Sink, SinkExt, StreamExt};
use gloo_net::websocket::{Message, State, WebSocketError, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;

use crate::{Transport, TransportError};

/// How long opening a connection may take, so an address that never answers
/// fails instead of hanging.
pub(crate) const CONNECT_TIMEOUT_MS: u32 = 10_000;

/// How long a connection may go with no frame from the server. The browser
/// answers the server's pings itself and never shows them, so unlike the
/// native transport's limit, this one counts frames only.
const QUIET_LIMIT_MS: u32 = 75_000;

/// A connection to the server's `/ws`.
pub struct WebTransport {
    ws: WebSocket,
}

fn io(error: impl std::fmt::Display) -> TransportError {
    TransportError::Io(error.to_string())
}

/// Waits until the socket is no longer connecting: it opened, or it failed.
struct Settled<'a>(&'a mut WebSocket);

impl std::future::Future for Settled<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // gloo-net's sink is ready once the socket leaves `Connecting`, open or not.
        Pin::new(&mut *self.0).poll_ready(cx).map(|_| ())
    }
}

impl WebTransport {
    /// Opens `wss://host[:port]/path` (or `ws://`, for tests on this
    /// machine). Gives up after [`CONNECT_TIMEOUT_MS`].
    pub async fn connect(url: &str) -> Result<WebTransport, TransportError> {
        if !(url.starts_with("wss://") || url.starts_with("ws://")) {
            return Err(io("Use a wss:// address."));
        }
        let mut ws = WebSocket::open(url).map_err(io)?;
        futures::select! {
            () = Settled(&mut ws).fuse() => {}
            () = TimeoutFuture::new(CONNECT_TIMEOUT_MS).fuse() => {
                return Err(io("The server didn't answer in time."));
            }
        }
        match ws.state() {
            State::Open => Ok(WebTransport { ws }),
            _ => Err(io("Couldn't reach the server.")),
        }
    }
}

impl Transport for WebTransport {
    async fn send(&mut self, frame: &ClientFrame) -> Result<(), TransportError> {
        // A closed browser socket drops what it's given without a word.
        if !matches!(self.ws.state(), State::Open) {
            return Err(TransportError::Closed);
        }
        let text = serde_json::to_string(frame).map_err(io)?;
        self.ws.send(Message::Text(text)).await.map_err(|_| TransportError::Closed)
    }

    /// Fails after [`QUIET_LIMIT_MS`] with no frame from the server.
    async fn recv(&mut self) -> Result<ServerFrame, TransportError> {
        let next = futures::select! {
            next = self.ws.next().fuse() => next,
            () = TimeoutFuture::new(QUIET_LIMIT_MS).fuse() => return Err(io("The server went quiet.")),
        };
        match next {
            None | Some(Err(WebSocketError::ConnectionClose(_))) => Err(TransportError::Closed),
            Some(Err(error)) => Err(io(error)),
            Some(Ok(Message::Text(text))) => {
                serde_json::from_str(&text).map_err(|e| TransportError::BadFrame(e.to_string()))
            }
            Some(Ok(Message::Bytes(_))) => Err(TransportError::BadFrame("a binary message".into())),
        }
    }
}
