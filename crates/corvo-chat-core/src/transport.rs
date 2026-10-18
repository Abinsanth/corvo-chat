//! How a client talks to the server: one frame at a time, in and out. The
//! native build uses TLS WebSockets; the browser build brings its own.

use std::future::Future;

use corvo_chat_proto::frames::{ClientFrame, ServerFrame};

/// `Send` where threads exist, so a tokio task can own a client; nothing on
/// the browser build.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}

#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// Why a frame couldn't be sent or received.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("The connection is closed.")]
    Closed,
    #[error("Connection problem: {0}")]
    Io(String),
    #[error("The server sent something that isn't a frame: {0}")]
    BadFrame(String),
}

pub trait Transport {
    fn send(&mut self, frame: &ClientFrame) -> impl Future<Output = Result<(), TransportError>> + MaybeSend;

    /// The next frame; pings and pongs are handled inside.
    fn recv(&mut self) -> impl Future<Output = Result<ServerFrame, TransportError>> + MaybeSend;
}
