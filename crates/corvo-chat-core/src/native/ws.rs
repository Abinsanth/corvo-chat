//! The TLS WebSocket transport.

use std::sync::Arc;

use corvo_chat_proto::frames::{ClientFrame, ServerFrame};
use futures::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use crate::{Transport, TransportError};

/// Which certificates the server may present.
#[derive(Clone, Debug)]
pub enum Roots {
    /// The usual public authorities.
    Web,
    /// Only these, for tests.
    Only(Vec<CertificateDer<'static>>),
}

/// A connection to the server's `/ws`.
pub struct NativeTransport {
    ws: WebSocketStream<TlsStream<TcpStream>>,
}

fn io(error: impl std::fmt::Display) -> TransportError {
    TransportError::Io(error.to_string())
}

/// The host, port and path of a `wss://host[:port]/path` URL.
fn parse(url: &str) -> Result<(&str, u16, &str), TransportError> {
    let rest = url.strip_prefix("wss://").ok_or_else(|| io("Use a wss:// address."))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| io("The port isn't a number."))?),
        None => (authority, 443),
    };
    if host.is_empty() {
        return Err(io("The address has no host."));
    }
    Ok((host, port, path))
}

impl NativeTransport {
    /// Opens `wss://host[:port]/path`, checking the certificate against `roots`.
    pub async fn connect(url: &str, roots: Roots) -> Result<NativeTransport, TransportError> {
        let (host, port, _) = parse(url)?;
        let mut store = rustls::RootCertStore::empty();
        match roots {
            Roots::Web => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            Roots::Only(certs) => {
                for cert in certs {
                    store.add(cert).map_err(io)?;
                }
            }
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(io)?
            .with_root_certificates(store)
            .with_no_client_auth();
        let name = ServerName::try_from(host.to_string()).map_err(io)?;
        let tcp = TcpStream::connect((host, port)).await.map_err(io)?;
        let tls = TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(io)?;
        let (ws, _) = tokio_tungstenite::client_async(url, tls).await.map_err(io)?;
        Ok(NativeTransport { ws })
    }
}

impl Transport for NativeTransport {
    async fn send(&mut self, frame: &ClientFrame) -> Result<(), TransportError> {
        let text = serde_json::to_string(frame).map_err(io)?;
        self.ws.send(Message::text(text)).await.map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<ServerFrame, TransportError> {
        loop {
            match self.ws.next().await {
                None | Some(Ok(Message::Close(_))) => return Err(TransportError::Closed),
                Some(Err(error)) => return Err(io(error)),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text).map_err(|e| TransportError::BadFrame(e.to_string()));
                }
                Some(Ok(_)) => return Err(TransportError::BadFrame("a binary message".into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn urls_split_into_host_port_and_path() {
        assert_eq!(parse("wss://chat.corvo.live/ws").unwrap(), ("chat.corvo.live", 443, "/ws"));
        assert_eq!(parse("wss://localhost:8443/ws").unwrap(), ("localhost", 8443, "/ws"));
        assert!(parse("ws://localhost/ws").is_err());
        assert!(parse("wss:///ws").is_err());
    }
}
