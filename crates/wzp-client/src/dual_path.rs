//! Phase 3.5 — dual-path QUIC connect race for P2P hole-punching.
//!
//! When both peers advertised reflex addrs in the
//! DirectCallOffer/Answer flow, the relay cross-wires them into
//! `CallSetup.peer_direct_addr`. This module races a direct QUIC
//! handshake against the existing relay dial and returns whichever
//! completes first — with automatic drop of the loser via
//! `tokio::select!`.
//!
//! Role determination is deterministic and symmetric
//! (`wzp_client::reflect::determine_role`): whichever peer has the
//! lexicographically smaller reflex addr becomes the **Acceptor**
//! (listens on a server-capable endpoint), the other becomes the
//! **Dialer** (dials the peer's addr). Because the rule is
//! identical on both sides, the Acceptor's inbound QUIC session
//! and the Dialer's outbound are the SAME connection — no
//! negotiation needed, no two-conns-per-call confusion.
//!
//! Timeout policy:
//! - Direct path: 2s from the start of `race`. Cone-NAT hole-punch
//!   typically completes in < 500ms on a LAN; 2s gives us tolerance
//!   for a single QUIC Initial retry on unreliable networks.
//! - Relay path: 10s (existing behavior elsewhere in the codebase).
//! - Overall: `tokio::select!` returns as soon as either succeeds.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::reflect::Role;
use wzp_transport::QuinnTransport;

/// Which path won the race. Used by the `connect` command for
/// logging + (in the future) metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinningPath {
    Direct,
    Relay,
}

/// Attempt a direct QUIC connection to the peer in parallel with
/// the relay dial and return the winning `QuinnTransport`.
///
/// `role` selects the direction of the direct attempt:
/// - `Role::Acceptor` creates a server-capable endpoint and waits
///   for the peer to dial in.
/// - `Role::Dialer` creates a client-only endpoint and dials
///   `peer_direct_addr`.
///
/// The relay path is always attempted in parallel as a fallback so
/// the race ALWAYS produces a working transport unless both paths
/// genuinely fail (network partition). Returns
/// `Err(anyhow::anyhow!(...))` if both paths fail within the
/// timeout.
#[allow(clippy::too_many_arguments)]
pub async fn race(
    role: Role,
    peer_direct_addr: SocketAddr,
    relay_addr: SocketAddr,
    room_sni: String,
    call_sni: String,
) -> anyhow::Result<(Arc<QuinnTransport>, WinningPath)> {
    // Rustls provider must be installed before any quinn endpoint
    // is created. Install attempt is idempotent.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build the direct-path endpoint + future based on role.
    // Each future returns an already-wrapped `QuinnTransport` so we
    // don't need a direct `quinn::Connection` type in scope here
    // (this crate doesn't depend on quinn directly).
    let direct_ep: wzp_transport::Endpoint;
    let direct_fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<QuinnTransport>> + Send>,
    >;

    match role {
        Role::Acceptor => {
            let (sc, _cert_der) = wzp_transport::server_config();
            let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let ep = wzp_transport::create_endpoint(bind, Some(sc))?;
            tracing::info!(
                local_addr = ?ep.local_addr().ok(),
                "dual_path: A-role endpoint up, awaiting peer dial"
            );
            let ep_for_fut = ep.clone();
            direct_fut = Box::pin(async move {
                // `wzp_transport::accept` wraps the same
                // `endpoint.accept().await?.await?` dance we want
                // and maps errors into TransportError for us.
                let conn = wzp_transport::accept(&ep_for_fut)
                    .await
                    .map_err(|e| anyhow::anyhow!("direct accept: {e}"))?;
                Ok(QuinnTransport::new(conn))
            });
            direct_ep = ep;
        }
        Role::Dialer => {
            let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let ep = wzp_transport::create_endpoint(bind, None)?;
            tracing::info!(
                local_addr = ?ep.local_addr().ok(),
                %peer_direct_addr,
                "dual_path: D-role endpoint up, dialing peer"
            );
            let ep_for_fut = ep.clone();
            let client_cfg = wzp_transport::client_config();
            let sni = call_sni.clone();
            direct_fut = Box::pin(async move {
                let conn =
                    wzp_transport::connect(&ep_for_fut, peer_direct_addr, &sni, client_cfg)
                        .await
                        .map_err(|e| anyhow::anyhow!("direct dial: {e}"))?;
                Ok(QuinnTransport::new(conn))
            });
            direct_ep = ep;
        }
    }

    // Relay path: classic dial to the relay's media room.
    let relay_bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let relay_ep = wzp_transport::create_endpoint(relay_bind, None)?;
    let relay_ep_for_fut = relay_ep.clone();
    let relay_client_cfg = wzp_transport::client_config();
    let relay_sni = room_sni.clone();
    let relay_fut = async move {
        let conn =
            wzp_transport::connect(&relay_ep_for_fut, relay_addr, &relay_sni, relay_client_cfg)
                .await
                .map_err(|e| anyhow::anyhow!("relay dial: {e}"))?;
        Ok::<_, anyhow::Error>(QuinnTransport::new(conn))
    };

    // Race the two with a shared 2s ceiling on the direct attempt.
    // Pin both so we can poll them from multiple branches of the
    // select without moving the futures — the "direct failed, wait
    // for relay" and "relay failed, wait for direct" fallback paths
    // below need to await the OPPOSITE future after the winning
    // branch fires. Without pinning, tokio::select! moves the
    // future out and we can't touch it again.
    tracing::info!(?role, %peer_direct_addr, %relay_addr, "dual_path: racing direct vs relay");
    let direct_timed = tokio::time::timeout(Duration::from_secs(2), direct_fut);
    tokio::pin!(direct_timed, relay_fut);

    let result = tokio::select! {
        biased; // prefer direct win if both arrive in the same tick
        direct_result = &mut direct_timed => {
            match direct_result {
                Ok(Ok(transport)) => {
                    tracing::info!(%peer_direct_addr, "dual_path: direct WON");
                    Ok((Arc::new(transport), WinningPath::Direct))
                }
                Ok(Err(e)) => {
                    // Direct failed — fall back to waiting for relay.
                    tracing::warn!(error = %e, "dual_path: direct failed, awaiting relay");
                    match tokio::time::timeout(Duration::from_secs(5), &mut relay_fut).await {
                        Ok(Ok(transport)) => Ok((Arc::new(transport), WinningPath::Relay)),
                        Ok(Err(e2)) => Err(anyhow::anyhow!("both paths failed: direct={e}, relay={e2}")),
                        Err(_) => Err(anyhow::anyhow!("both paths failed: direct={e}, relay=timeout(5s)")),
                    }
                }
                Err(_elapsed) => {
                    tracing::warn!("dual_path: direct timed out (2s), awaiting relay");
                    match tokio::time::timeout(Duration::from_secs(5), &mut relay_fut).await {
                        Ok(Ok(transport)) => Ok((Arc::new(transport), WinningPath::Relay)),
                        Ok(Err(e2)) => Err(anyhow::anyhow!("direct timeout + relay failed: {e2}")),
                        Err(_) => Err(anyhow::anyhow!("direct timeout + relay timeout")),
                    }
                }
            }
        }
        relay_result = &mut relay_fut => {
            match relay_result {
                Ok(transport) => {
                    tracing::info!("dual_path: relay WON (direct still pending)");
                    Ok((Arc::new(transport), WinningPath::Relay))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "dual_path: relay failed, awaiting direct remainder");
                    match tokio::time::timeout(Duration::from_millis(1500), &mut direct_timed).await {
                        Ok(Ok(Ok(transport))) => Ok((Arc::new(transport), WinningPath::Direct)),
                        _ => Err(anyhow::anyhow!("relay failed + direct unavailable: {e}")),
                    }
                }
            }
        }
    };

    // Drop both endpoints once the winner is stored in result. The
    // winning transport owns its own connection so dropping the
    // endpoint won't kill it.
    drop(direct_ep);
    drop(relay_ep);

    result
}
