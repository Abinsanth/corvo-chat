//! The parts that need sockets, threads, files or a clock.

mod ws;

pub use ws::{NativeTransport, Roots};
