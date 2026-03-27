//! QUIC connection lifecycle management.
//!
//! Provides helpers for creating endpoints, connecting to peers, and accepting connections.

use std::net::SocketAddr;

use wzp_proto::TransportError;

/// Create a QUIC endpoint bound to the given address.
///
/// If `server_config` is provided, the endpoint can accept incoming connections.
pub fn create_endpoint(
    bind_addr: SocketAddr,
    server_config: Option<quinn::ServerConfig>,
) -> Result<quinn::Endpoint, TransportError> {
    let endpoint = if let Some(sc) = server_config {
        quinn::Endpoint::server(sc, bind_addr)?
    } else {
        quinn::Endpoint::client(bind_addr)?
    };
    Ok(endpoint)
}

/// Connect to a remote peer using the given client configuration.
pub async fn connect(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    server_name: &str,
    config: quinn::ClientConfig,
) -> Result<quinn::Connection, TransportError> {
    let connecting = endpoint.connect_with(config, addr, server_name).map_err(|e| {
        TransportError::Internal(format!("connect error: {e}"))
    })?;

    let connection = connecting.await.map_err(|e| {
        TransportError::Internal(format!("connection failed: {e}"))
    })?;

    Ok(connection)
}

/// Accept the next incoming connection on an endpoint.
pub async fn accept(endpoint: &quinn::Endpoint) -> Result<quinn::Connection, TransportError> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or(TransportError::ConnectionLost)?;

    let connection = incoming.await.map_err(|e| {
        TransportError::Internal(format!("accept failed: {e}"))
    })?;

    Ok(connection)
}
