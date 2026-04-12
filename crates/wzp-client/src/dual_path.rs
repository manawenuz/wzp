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

/// Phase 6: the race now returns BOTH transports (when available)
/// so the connect command can negotiate with the peer before
/// committing. The negotiation decides which transport to use
/// based on whether BOTH sides report `direct_ok = true`.
pub struct RaceResult {
    /// The direct P2P transport, if the direct path completed.
    /// `None` if the direct dial/accept failed or timed out.
    pub direct_transport: Option<Arc<QuinnTransport>>,
    /// The relay transport, if the relay dial completed.
    /// `None` if the relay dial failed (shouldn't happen in
    /// practice since relay is always reachable).
    pub relay_transport: Option<Arc<QuinnTransport>>,
    /// Which future completed first in the local race.
    /// Informational — the actual path used is decided by the
    /// Phase 6 negotiation after both sides exchange reports.
    pub local_winner: WinningPath,
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
/// Phase 5.5 candidate bundle — full ICE-ish candidate list for
/// the peer. The race tries them all in parallel alongside the
/// relay path. At minimum this should contain the peer's
/// server-reflexive address; `local_addrs` carries LAN host
/// candidates gathered from their physical interfaces.
///
/// Empty is valid: the D-role has nothing to dial and the race
/// reduces to "relay only" + (if A-role) accepting on the
/// shared endpoint.
#[derive(Debug, Clone, Default)]
pub struct PeerCandidates {
    /// Peer's server-reflexive address (Phase 3). `None` if the
    /// peer didn't advertise one.
    pub reflexive: Option<SocketAddr>,
    /// Peer's LAN host addresses (Phase 5.5). Tried first on
    /// same-LAN pairs — direct dials to these bypass the NAT
    /// entirely.
    pub local: Vec<SocketAddr>,
}

impl PeerCandidates {
    /// Flatten into the list of addrs the D-role should dial.
    /// Order: LAN host candidates first (fastest when they
    /// work), then reflexive (covers the non-LAN case).
    pub fn dial_order(&self) -> Vec<SocketAddr> {
        let mut out = Vec::with_capacity(self.local.len() + 1);
        out.extend(self.local.iter().copied());
        if let Some(a) = self.reflexive {
            // Only add if it's not already in the list (some
            // edge cases on same-LAN could have the same addr
            // in both).
            if !out.contains(&a) {
                out.push(a);
            }
        }
        out
    }

    /// Is there anything for the D-role to dial? If not, the
    /// race reduces to relay-only.
    pub fn is_empty(&self) -> bool {
        self.reflexive.is_none() && self.local.is_empty()
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn race(
    role: Role,
    peer_candidates: PeerCandidates,
    relay_addr: SocketAddr,
    room_sni: String,
    call_sni: String,
    // Phase 5: when `Some`, reuse this endpoint for BOTH the
    // direct-path branch AND the relay dial. Pass the signal
    // endpoint. The endpoint MUST be server-capable (created
    // with a server config) for the A-role accept branch to
    // work.
    //
    // When `None`, falls back to fresh endpoints per role.
    // Used by tests.
    shared_endpoint: Option<wzp_transport::Endpoint>,
) -> anyhow::Result<RaceResult> {
    // Rustls provider must be installed before any quinn endpoint
    // is created. Install attempt is idempotent.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build the direct-path endpoint + future based on role.
    //
    // A-role: one accept future on the shared endpoint. The
    //   first incoming QUIC connection wins — we don't care
    //   which peer candidate the dialer used to reach us.
    //
    // D-role: N parallel dial futures, one per peer candidate
    //   (all LAN host addrs + the reflex addr), consolidated
    //   into a single direct_fut via FuturesUnordered-style
    //   "first OK wins" semantics. The first successful dial
    //   becomes the direct path; the losers are dropped (quinn
    //   will abort the in-flight handshakes via the dropped
    //   Connecting futures).
    //
    // Either way, direct_fut resolves to a single QuinnTransport
    // (or an error) and is raced against the relay_fut by the
    // outer tokio::select!.
    let direct_ep: wzp_transport::Endpoint;
    let direct_fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<QuinnTransport>> + Send>,
    >;

    match role {
        Role::Acceptor => {
            let ep = match shared_endpoint.clone() {
                Some(ep) => {
                    tracing::info!(
                        local_addr = ?ep.local_addr().ok(),
                        "dual_path: A-role reusing shared endpoint for accept"
                    );
                    ep
                }
                None => {
                    let (sc, _cert_der) = wzp_transport::server_config();
                    // 0.0.0.0:0 = IPv4 socket. [::]:0 dual-stack was
                    // tried but breaks on Android devices where
                    // IPV6_V6ONLY=1 (default on some kernels) —
                    // IPv4 candidates silently fail. IPv6 host
                    // candidates are skipped for now; they need a
                    // dedicated IPv6 socket alongside the v4 one
                    // (like WebRTC's dual-socket approach).
                    let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
                    let fresh = wzp_transport::create_endpoint(bind, Some(sc))?;
                    tracing::info!(
                        local_addr = ?fresh.local_addr().ok(),
                        "dual_path: A-role fresh endpoint up, awaiting peer dial"
                    );
                    fresh
                }
            };
            let ep_for_fut = ep.clone();
            direct_fut = Box::pin(async move {
                // `wzp_transport::accept` wraps the same
                // `endpoint.accept().await?.await?` dance we want.
                // If `ep_for_fut` is the shared signal endpoint,
                // this pulls the NEXT incoming connection —
                // normally that's the peer's direct-P2P dial.
                // Signal recv is done via the signal CONNECTION
                // (accept_bi), not the endpoint, so no conflict.
                let conn = wzp_transport::accept(&ep_for_fut)
                    .await
                    .map_err(|e| anyhow::anyhow!("direct accept: {e}"))?;
                Ok(QuinnTransport::new(conn))
            });
            direct_ep = ep;
        }
        Role::Dialer => {
            let ep = match shared_endpoint.clone() {
                Some(ep) => {
                    tracing::info!(
                        local_addr = ?ep.local_addr().ok(),
                        candidates = ?peer_candidates.dial_order(),
                        "dual_path: D-role reusing shared endpoint to dial peer candidates"
                    );
                    ep
                }
                None => {
                    // 0.0.0.0:0 = IPv4 socket. [::]:0 dual-stack was
                    // tried but breaks on Android devices where
                    // IPV6_V6ONLY=1 (default on some kernels) —
                    // IPv4 candidates silently fail. IPv6 host
                    // candidates are skipped for now; they need a
                    // dedicated IPv6 socket alongside the v4 one
                    // (like WebRTC's dual-socket approach).
                    let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
                    let fresh = wzp_transport::create_endpoint(bind, None)?;
                    tracing::info!(
                        local_addr = ?fresh.local_addr().ok(),
                        candidates = ?peer_candidates.dial_order(),
                        "dual_path: D-role fresh endpoint up, dialing peer candidates"
                    );
                    fresh
                }
            };
            let ep_for_fut = ep.clone();
            let dial_order = peer_candidates.dial_order();
            let sni = call_sni.clone();
            direct_fut = Box::pin(async move {
                if dial_order.is_empty() {
                    // No candidates — the race reduces to
                    // relay-only. Surface a stable error so the
                    // outer select falls through to relay_fut
                    // without a spurious "direct failed" warning.
                    // Use a pending future that never resolves so
                    // the select's "other side wins" branch is
                    // the natural outcome.
                    std::future::pending::<anyhow::Result<QuinnTransport>>().await
                } else {
                    // Fan out N parallel dials via JoinSet. First
                    // `Ok` wins; `Err` from a single candidate is
                    // not fatal — we wait for the others. Only
                    // when ALL have failed do we return Err.
                    let mut set = tokio::task::JoinSet::new();
                    for (idx, candidate) in dial_order.iter().enumerate() {
                        let ep = ep_for_fut.clone();
                        let client_cfg = wzp_transport::client_config();
                        let sni = sni.clone();
                        let candidate = *candidate;
                        set.spawn(async move {
                            let result = wzp_transport::connect(
                                &ep,
                                candidate,
                                &sni,
                                client_cfg,
                            )
                            .await;
                            (idx, candidate, result)
                        });
                    }
                    let mut last_err: Option<String> = None;
                    while let Some(join_res) = set.join_next().await {
                        let (idx, candidate, dial_res) = match join_res {
                            Ok(t) => t,
                            Err(e) => {
                                last_err = Some(format!("join {e}"));
                                continue;
                            }
                        };
                        match dial_res {
                            Ok(conn) => {
                                tracing::info!(
                                    %candidate,
                                    candidate_idx = idx,
                                    "dual_path: direct dial succeeded on candidate"
                                );
                                // Abort the remaining in-flight
                                // dials so they don't complete
                                // and leak QUIC sessions.
                                set.abort_all();
                                return Ok(QuinnTransport::new(conn));
                            }
                            Err(e) => {
                                tracing::debug!(
                                    %candidate,
                                    candidate_idx = idx,
                                    error = %e,
                                    "dual_path: direct dial failed, trying others"
                                );
                                last_err = Some(format!("candidate {candidate}: {e}"));
                            }
                        }
                    }
                    Err(anyhow::anyhow!(
                        "all {} direct candidates failed; last: {}",
                        dial_order.len(),
                        last_err.unwrap_or_else(|| "n/a".into())
                    ))
                }
            });
            direct_ep = ep;
        }
    }

    // Relay path: classic dial to the relay's media room. Phase 5:
    // reuse the shared endpoint here too so MikroTik-style NATs
    // keep a stable external port across all flows from this
    // client. Falls back to a fresh endpoint when not shared.
    let relay_ep = match shared_endpoint.clone() {
        Some(ep) => ep,
        None => {
            let relay_bind: SocketAddr = "[::]:0".parse().unwrap();
            wzp_transport::create_endpoint(relay_bind, None)?
        }
    };
    let relay_ep_for_fut = relay_ep.clone();
    let relay_client_cfg = wzp_transport::client_config();
    let relay_sni = room_sni.clone();
    // Phase 5.5 direct-path head-start: hold the relay dial for
    // 500ms before attempting it. On same-LAN cone-NAT pairs the
    // direct dial finishes in ~30-100ms, so giving direct a 500ms
    // head start means direct reliably wins when it's going to
    // work at all. The worst case adds 500ms to the fall-back-
    // to-relay scenario, which is imperceptible for users on
    // setups where direct isn't available anyway.
    //
    // Prior behavior (immediate race) caused the relay to win
    // ~105ms races on a MikroTik LAN because:
    //   - Acceptor role's direct_fut = accept() can only fire
    //     when the peer has completed its outbound LAN dial
    //   - Dialer role's parallel LAN dials need the peer's
    //     CallSetup processed + the race started on the other
    //     side before they can reach us
    //   - Meanwhile relay_fut is a plain dial that completes in
    //     whatever the client→relay RTT is (often <100ms)
    //
    // The 500ms head start is the minimum that empirically makes
    // same-LAN direct reliably beat relay, without penalizing
    // users who genuinely need the relay path.
    const DIRECT_HEAD_START: Duration = Duration::from_millis(500);
    let relay_fut = async move {
        tokio::time::sleep(DIRECT_HEAD_START).await;
        let conn =
            wzp_transport::connect(&relay_ep_for_fut, relay_addr, &relay_sni, relay_client_cfg)
                .await
                .map_err(|e| anyhow::anyhow!("relay dial: {e}"))?;
        Ok::<_, anyhow::Error>(QuinnTransport::new(conn))
    };

    // Phase 6: run both paths concurrently via tokio::spawn and
    // collect BOTH results. The old tokio::select! approach dropped
    // the loser, which meant the connect command couldn't negotiate
    // with the peer — it had to commit to whichever path won locally.
    //
    // Now we spawn both as tasks, wait for the first to complete
    // (that determines `local_winner`), then give the loser a short
    // grace period to also complete. The connect command gets a
    // RaceResult with both transports (when available) and uses the
    // Phase 6 MediaPathReport exchange to decide which one to
    // actually use for media.
    tracing::info!(
        ?role,
        candidates = ?peer_candidates.dial_order(),
        %relay_addr,
        "dual_path: racing direct vs relay"
    );

    let mut direct_task = tokio::spawn(
        tokio::time::timeout(Duration::from_secs(2), direct_fut),
    );
    let mut relay_task = tokio::spawn(async move {
        // Keep the 500ms head start so direct has a chance
        tokio::time::sleep(Duration::from_millis(500)).await;
        tokio::time::timeout(Duration::from_secs(5), relay_fut).await
    });

    // Wait for the first one to complete. This tells us the
    // local_winner — but we DON'T commit to it yet. Phase 6
    // negotiation decides the actual path.
    let (mut direct_result, mut relay_result): (
        Option<anyhow::Result<QuinnTransport>>,
        Option<anyhow::Result<QuinnTransport>>,
    ) = (None, None);

    let local_winner;

    tokio::select! {
        biased;
        d = &mut direct_task => {
            match d {
                Ok(Ok(Ok(t))) => {
                    tracing::info!("dual_path: direct completed first");
                    direct_result = Some(Ok(t));
                    local_winner = WinningPath::Direct;
                }
                Ok(Ok(Err(e))) => {
                    tracing::warn!(error = %e, "dual_path: direct failed");
                    direct_result = Some(Err(anyhow::anyhow!("{e}")));
                    local_winner = WinningPath::Relay; // direct failed → relay is our only hope
                }
                Ok(Err(_)) => {
                    tracing::warn!("dual_path: direct timed out (2s)");
                    direct_result = Some(Err(anyhow::anyhow!("direct timeout")));
                    local_winner = WinningPath::Relay;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "dual_path: direct task panicked");
                    direct_result = Some(Err(anyhow::anyhow!("direct task panic")));
                    local_winner = WinningPath::Relay;
                }
            }
        }
        r = &mut relay_task => {
            match r {
                Ok(Ok(Ok(t))) => {
                    tracing::info!("dual_path: relay completed first");
                    relay_result = Some(Ok(t));
                    local_winner = WinningPath::Relay;
                }
                Ok(Ok(Err(e))) => {
                    tracing::warn!(error = %e, "dual_path: relay failed");
                    relay_result = Some(Err(anyhow::anyhow!("{e}")));
                    local_winner = WinningPath::Direct;
                }
                Ok(Err(_)) => {
                    tracing::warn!("dual_path: relay timed out");
                    relay_result = Some(Err(anyhow::anyhow!("relay timeout")));
                    local_winner = WinningPath::Direct;
                }
                Err(e) => {
                    relay_result = Some(Err(anyhow::anyhow!("relay task panic: {e}")));
                    local_winner = WinningPath::Direct;
                }
            }
        }
    }

    // Give the loser a short grace period (1s) to also complete.
    // If it does, we have both transports for Phase 6 negotiation.
    // If it doesn't, we still proceed with just the winner.
    if direct_result.is_none() {
        match tokio::time::timeout(Duration::from_secs(1), direct_task).await {
            Ok(Ok(Ok(Ok(t)))) => { direct_result = Some(Ok(t)); }
            Ok(Ok(Ok(Err(e)))) => { direct_result = Some(Err(anyhow::anyhow!("{e}"))); }
            _ => { direct_result = Some(Err(anyhow::anyhow!("direct: no result in grace period"))); }
        }
    }
    if relay_result.is_none() {
        match tokio::time::timeout(Duration::from_secs(1), relay_task).await {
            Ok(Ok(Ok(Ok(t)))) => { relay_result = Some(Ok(t)); }
            Ok(Ok(Ok(Err(e)))) => { relay_result = Some(Err(anyhow::anyhow!("{e}"))); }
            _ => { relay_result = Some(Err(anyhow::anyhow!("relay: no result in grace period"))); }
        }
    }

    let direct_ok = direct_result.as_ref().map(|r| r.is_ok()).unwrap_or(false);
    let relay_ok = relay_result.as_ref().map(|r| r.is_ok()).unwrap_or(false);

    tracing::info!(
        ?local_winner,
        direct_ok,
        relay_ok,
        "dual_path: race finished, both results collected for Phase 6 negotiation"
    );

    if !direct_ok && !relay_ok {
        return Err(anyhow::anyhow!("both paths failed: no media transport available"));
    }

    let _ = (direct_ep, relay_ep);

    Ok(RaceResult {
        direct_transport: direct_result
            .and_then(|r| r.ok())
            .map(|t| Arc::new(t)),
        relay_transport: relay_result
            .and_then(|r| r.ok())
            .map(|t| Arc::new(t)),
        local_winner,
    })
}
