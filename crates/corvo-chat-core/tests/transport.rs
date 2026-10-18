mod common;

use corvo_chat_core::{
    Transport,
    native::{NativeTransport, Roots},
};
use corvo_chat_proto::{
    PROTOCOL_VERSION,
    frames::{ClientFrame, ServerFrame},
};

#[tokio::test]
async fn hello_reaches_the_server() {
    let server = common::start().await;
    let mut t = common::connect(&server).await;
    t.send(&ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, app_version: "t".into() }).await.unwrap();
    assert!(matches!(t.recv().await.unwrap(), ServerFrame::Ready { .. }));
}

#[tokio::test]
async fn plain_ws_and_unknown_certificates_are_refused() {
    assert!(NativeTransport::connect("ws://127.0.0.1:1/ws", Roots::Web).await.is_err());
    let server = common::start().await;
    assert!(NativeTransport::connect(&server.url, Roots::Web).await.is_err());
}

#[tokio::test]
async fn addresses_without_a_host_or_with_a_bad_port_are_refused() {
    assert!(NativeTransport::connect("wss:///ws", Roots::Web).await.is_err());
    assert!(NativeTransport::connect("wss://localhost:notaport/ws", Roots::Web).await.is_err());
}
