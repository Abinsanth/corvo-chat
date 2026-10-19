//! The parts that need a browser: its WebSocket, its timers, its clock and
//! its IndexedDB.

mod idb;
mod ws;

pub use self::idb::IdbCache;
pub use ws::WebTransport;

/// Opens `wss://` (or `ws://`) `host[:port]/path`; see [`WebTransport::connect`].
pub async fn connect(url: &str) -> Result<WebTransport, crate::TransportError> {
    WebTransport::connect(url).await
}

/// Milliseconds since the Unix epoch, from the browser's clock.
pub fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}
