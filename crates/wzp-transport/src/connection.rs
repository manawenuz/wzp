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

/// Create an IPv6-only QUIC endpoint with `IPV6_V6ONLY=1`.
///
/// Tries `[::]:preferred_port` first (same port as the IPv4 signal
/// endpoint — allowed on Linux/Android when the AFs differ and
/// V6ONLY is set). Falls back to `[::]:0` (OS-assigned) if the
/// preferred port is already taken.
///
/// Must be called from within a tokio runtime (quinn needs the
/// async runtime handle for its I/O driver).
pub fn create_ipv6_endpoint(
    preferred_port: u16,
    server_config: Option<quinn::ServerConfig>,
) -> Result<quinn::Endpoint, TransportError> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{Ipv6Addr, SocketAddrV6};

    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| TransportError::Internal(format!("ipv6 socket: {e}")))?;

    // Critical: IPv6-only so this socket never intercepts IPv4.
    // On Android some kernels default to V6ONLY=1 anyway, but we
    // set it explicitly for cross-platform consistency.
    sock.set_only_v6(true)
        .map_err(|e| TransportError::Internal(format!("set_only_v6: {e}")))?;

    sock.set_reuse_address(true)
        .map_err(|e| TransportError::Internal(format!("set_reuse_address: {e}")))?;

    // Try the preferred port (same as IPv4 signal endpoint), fall
    // back to ephemeral if the OS rejects it.
    let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, preferred_port, 0, 0);
    if let Err(e) = sock.bind(&bind_addr.into()) {
        if preferred_port != 0 {
            tracing::debug!(
                preferred_port,
                error = %e,
                "ipv6 bind to preferred port failed, falling back to ephemeral"
            );
            let fallback = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
            sock.bind(&fallback.into())
                .map_err(|e| TransportError::Internal(format!("ipv6 bind fallback: {e}")))?;
        } else {
            return Err(TransportError::Internal(format!("ipv6 bind: {e}")));
        }
    }

    sock.set_nonblocking(true)
        .map_err(|e| TransportError::Internal(format!("set_nonblocking: {e}")))?;

    let udp_socket: std::net::UdpSocket = sock.into();

    let runtime = quinn::default_runtime()
        .ok_or_else(|| TransportError::Internal("no async runtime for ipv6 endpoint".into()))?;

    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        server_config,
        udp_socket,
        runtime,
    )
    .map_err(|e| TransportError::Internal(format!("ipv6 endpoint: {e}")))?;

    Ok(endpoint)
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
