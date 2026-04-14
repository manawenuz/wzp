//! Minimal RFC 5389 STUN Binding client for public STUN servers.
//!
//! Implements just enough of STUN to send a Binding Request and parse
//! the XOR-MAPPED-ADDRESS from the Binding Response. No TURN, no ICE
//! agent, no long-term credentials — just reflexive address discovery
//! over raw UDP.
//!
//! This complements the relay-based `Reflect` mechanism in
//! `reflect.rs` by providing independent reflexive discovery via
//! public STUN servers (stun.l.google.com, stun.cloudflare.com, etc.)
//! without requiring a connection to our own relay infrastructure.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;

// ── Constants ──────────────────────────────────────────────────────

/// STUN magic cookie (RFC 5389 §6).
const MAGIC_COOKIE: u32 = 0x2112_A442;

/// STUN message types.
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;

/// STUN attribute types.
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// STUN header is always 20 bytes.
const HEADER_LEN: usize = 20;

/// Maximum STUN response we'll accept (RFC says < 576 for most, but
/// we're generous).
const MAX_RESPONSE: usize = 576;

/// Well-known public STUN servers.
pub const DEFAULT_STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
];

// ── Error type ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum StunError {
    /// Network I/O error.
    Io(String),
    /// Timed out waiting for response.
    Timeout,
    /// Response packet too short or malformed.
    Malformed(String),
    /// Transaction ID mismatch (response doesn't match our request).
    TxnMismatch,
    /// Response was a STUN error response (class 0x01, method 0x01 = 0x0111).
    ErrorResponse(u16),
    /// No XOR-MAPPED-ADDRESS or MAPPED-ADDRESS in response.
    NoMappedAddress,
    /// DNS resolution failed.
    DnsError(String),
}

impl std::fmt::Display for StunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "STUN I/O: {e}"),
            Self::Timeout => write!(f, "STUN timeout"),
            Self::Malformed(e) => write!(f, "STUN malformed: {e}"),
            Self::TxnMismatch => write!(f, "STUN transaction ID mismatch"),
            Self::ErrorResponse(code) => write!(f, "STUN error response: {code}"),
            Self::NoMappedAddress => write!(f, "no MAPPED-ADDRESS in STUN response"),
            Self::DnsError(e) => write!(f, "STUN DNS: {e}"),
        }
    }
}

impl std::error::Error for StunError {}

// ── Configuration ──────────────────────────────────────────────────

/// Configuration for public STUN server probing.
#[derive(Debug, Clone)]
pub struct StunConfig {
    /// STUN servers to probe, as `host:port` strings. Resolved via
    /// tokio DNS at probe time.
    pub servers: Vec<String>,
    /// Per-server timeout.
    pub timeout: Duration,
}

impl Default for StunConfig {
    fn default() -> Self {
        Self {
            servers: DEFAULT_STUN_SERVERS.iter().map(|s| s.to_string()).collect(),
            timeout: Duration::from_secs(3),
        }
    }
}

// ── Packet encoding ────────────────────────────────────────────────

/// Generate a 12-byte STUN transaction ID.
fn gen_txn_id() -> [u8; 12] {
    let mut id = [0u8; 12];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut id);
    id
}

/// Encode a STUN Binding Request (20 bytes, no attributes).
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |0 0|     STUN Message Type     |         Message Length        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                         Magic Cookie                         |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                                                               |
/// |                     Transaction ID (96 bits)                  |
/// |                                                               |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
pub fn encode_binding_request(txn_id: &[u8; 12]) -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    // Message Type: Binding Request (0x0001)
    buf[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Message Length: 0 (no attributes)
    buf[2..4].copy_from_slice(&0u16.to_be_bytes());
    // Magic Cookie
    buf[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    // Transaction ID
    buf[8..20].copy_from_slice(txn_id);
    buf
}

/// Parse a STUN Binding Response and extract the mapped address.
///
/// Returns the XOR-MAPPED-ADDRESS if present, otherwise falls back
/// to MAPPED-ADDRESS. Returns `Err` if the response is malformed
/// or doesn't contain either attribute.
pub fn parse_binding_response(
    buf: &[u8],
    expected_txn_id: &[u8; 12],
) -> Result<SocketAddr, StunError> {
    if buf.len() < HEADER_LEN {
        return Err(StunError::Malformed(format!(
            "response too short: {} bytes",
            buf.len()
        )));
    }

    // Parse header.
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

    // Verify magic cookie.
    if cookie != MAGIC_COOKIE {
        return Err(StunError::Malformed(format!(
            "bad magic cookie: {cookie:#010x}"
        )));
    }

    // Verify it's a Binding Response (not an error response).
    if msg_type == 0x0111 {
        // Error response — try to extract error code.
        return Err(StunError::ErrorResponse(0));
    }
    if msg_type != BINDING_RESPONSE {
        return Err(StunError::Malformed(format!(
            "unexpected message type: {msg_type:#06x}"
        )));
    }

    // Verify transaction ID.
    if buf[8..20] != *expected_txn_id {
        return Err(StunError::TxnMismatch);
    }

    // Verify message length doesn't exceed buffer.
    let total_len = HEADER_LEN + msg_len;
    if buf.len() < total_len {
        return Err(StunError::Malformed(format!(
            "message length {msg_len} exceeds buffer ({} bytes after header)",
            buf.len() - HEADER_LEN
        )));
    }

    // Walk attributes looking for XOR-MAPPED-ADDRESS (preferred) or
    // MAPPED-ADDRESS (fallback). XOR-MAPPED-ADDRESS is preferred
    // because it survives ALG rewriting by broken NATs.
    let attrs = &buf[HEADER_LEN..total_len];
    let mut mapped: Option<SocketAddr> = None;
    let mut xor_mapped: Option<SocketAddr> = None;
    let mut pos = 0;

    while pos + 4 <= attrs.len() {
        let attr_type = u16::from_be_bytes([attrs[pos], attrs[pos + 1]]);
        let attr_len = u16::from_be_bytes([attrs[pos + 2], attrs[pos + 3]]) as usize;
        let value_start = pos + 4;
        let value_end = value_start + attr_len;

        if value_end > attrs.len() {
            break; // truncated attribute — stop parsing
        }

        let value = &attrs[value_start..value_end];

        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                xor_mapped = parse_xor_mapped_address(value, expected_txn_id).ok();
            }
            ATTR_MAPPED_ADDRESS => {
                mapped = parse_mapped_address(value).ok();
            }
            _ => {} // ignore unknown attributes
        }

        // Attributes are padded to 4-byte boundaries.
        pos = value_end + ((4 - (attr_len % 4)) % 4);
    }

    xor_mapped
        .or(mapped)
        .ok_or(StunError::NoMappedAddress)
}

/// Parse a MAPPED-ADDRESS attribute value (RFC 5389 §15.1).
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |0 0 0 0 0 0 0 0|    Family     |           Port                |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                                                               |
/// |                 Address (32 bits or 128 bits)                 |
/// |                                                               |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
fn parse_mapped_address(value: &[u8]) -> Result<SocketAddr, StunError> {
    if value.len() < 4 {
        return Err(StunError::Malformed("MAPPED-ADDRESS too short".into()));
    }
    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]);

    match family {
        0x01 => {
            // IPv4
            if value.len() < 8 {
                return Err(StunError::Malformed("MAPPED-ADDRESS IPv4 too short".into()));
            }
            let ip = Ipv4Addr::new(value[4], value[5], value[6], value[7]);
            Ok(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 => {
            // IPv6
            if value.len() < 20 {
                return Err(StunError::Malformed("MAPPED-ADDRESS IPv6 too short".into()));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            let ip = Ipv6Addr::from(octets);
            Ok(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => Err(StunError::Malformed(format!(
            "unknown address family: {family:#04x}"
        ))),
    }
}

/// Parse an XOR-MAPPED-ADDRESS attribute value (RFC 5389 §15.2).
///
/// Same layout as MAPPED-ADDRESS but port and address are XORed:
/// - Port: XOR with top 16 bits of magic cookie
/// - IPv4 address: XOR with magic cookie
/// - IPv6 address: XOR with magic cookie || transaction ID
fn parse_xor_mapped_address(
    value: &[u8],
    txn_id: &[u8; 12],
) -> Result<SocketAddr, StunError> {
    if value.len() < 4 {
        return Err(StunError::Malformed("XOR-MAPPED-ADDRESS too short".into()));
    }
    let family = value[1];
    let xport = u16::from_be_bytes([value[2], value[3]]);
    let port = xport ^ (MAGIC_COOKIE >> 16) as u16;

    match family {
        0x01 => {
            // IPv4: XOR with magic cookie (big-endian)
            if value.len() < 8 {
                return Err(StunError::Malformed(
                    "XOR-MAPPED-ADDRESS IPv4 too short".into(),
                ));
            }
            let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
            let ip = Ipv4Addr::new(
                value[4] ^ cookie_bytes[0],
                value[5] ^ cookie_bytes[1],
                value[6] ^ cookie_bytes[2],
                value[7] ^ cookie_bytes[3],
            );
            Ok(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 => {
            // IPv6: XOR with magic cookie (4 bytes) || txn ID (12 bytes)
            if value.len() < 20 {
                return Err(StunError::Malformed(
                    "XOR-MAPPED-ADDRESS IPv6 too short".into(),
                ));
            }
            let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
            let mut xor_key = [0u8; 16];
            xor_key[..4].copy_from_slice(&cookie_bytes);
            xor_key[4..16].copy_from_slice(txn_id);

            let mut octets = [0u8; 16];
            for i in 0..16 {
                octets[i] = value[4 + i] ^ xor_key[i];
            }
            let ip = Ipv6Addr::from(octets);
            Ok(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => Err(StunError::Malformed(format!(
            "unknown address family: {family:#04x}"
        ))),
    }
}

// ── Public async API ───────────────────────────────────────────────

/// Send a STUN Binding Request to `server` over `socket` and return
/// the server-reflexive address from the response.
///
/// The socket should be a `UdpSocket` bound to `0.0.0.0:0` (or a
/// specific port if you want to test the same source port as QUIC).
/// The function does NOT connect the socket — it uses `send_to` /
/// `recv_from` so the socket can be reused for multiple servers.
pub async fn stun_reflect(
    socket: &UdpSocket,
    server: SocketAddr,
    timeout: Duration,
) -> Result<SocketAddr, StunError> {
    let txn_id = gen_txn_id();
    let request = encode_binding_request(&txn_id);

    socket
        .send_to(&request, server)
        .await
        .map_err(|e| StunError::Io(e.to_string()))?;

    let mut buf = [0u8; MAX_RESPONSE];

    // Retry once: some NATs drop the first UDP packet to a new
    // destination (the "first-packet problem"). A single retry at
    // half the timeout covers this without adding excessive delay.
    let half = timeout / 2;
    let addr = match tokio::time::timeout(half, socket.recv_from(&mut buf)).await {
        Ok(Ok((len, from))) => {
            // Verify response is from the server we queried.
            if from.ip() != server.ip() {
                return Err(StunError::Malformed(format!(
                    "response from unexpected source: {from} (expected {server})"
                )));
            }
            parse_binding_response(&buf[..len], &txn_id)?
        }
        Ok(Err(e)) => return Err(StunError::Io(e.to_string())),
        Err(_) => {
            // First attempt timed out — retry.
            socket
                .send_to(&request, server)
                .await
                .map_err(|e| StunError::Io(e.to_string()))?;

            let (len, _from) = tokio::time::timeout(half, socket.recv_from(&mut buf))
                .await
                .map_err(|_| StunError::Timeout)?
                .map_err(|e| StunError::Io(e.to_string()))?;

            parse_binding_response(&buf[..len], &txn_id)?
        }
    };

    Ok(addr)
}

/// Resolve a STUN server hostname to a `SocketAddr`.
///
/// Uses tokio's DNS resolver. Returns the first IPv4 address found,
/// or the first IPv6 if no IPv4 is available.
pub async fn resolve_stun_server(host_port: &str) -> Result<SocketAddr, StunError> {
    use tokio::net::lookup_host;

    let mut addrs = lookup_host(host_port)
        .await
        .map_err(|e| StunError::DnsError(format!("{host_port}: {e}")))?;

    // Prefer IPv4 for STUN since our QUIC endpoint is currently
    // IPv4-only (Phase 7 IPv6 is still flaky).
    let mut first_v6: Option<SocketAddr> = None;
    while let Some(addr) = addrs.next() {
        if addr.is_ipv4() {
            return Ok(addr);
        }
        if first_v6.is_none() {
            first_v6 = Some(addr);
        }
    }
    first_v6.ok_or_else(|| StunError::DnsError(format!("{host_port}: no addresses resolved")))
}

/// Probe multiple public STUN servers in parallel and return the
/// reflexive address from the first successful response.
///
/// This is the high-level entry point for Phase 1 STUN integration.
/// Call it during call setup alongside (or instead of) the relay-
/// based `probe_reflect_addr`.
pub async fn discover_reflexive(config: &StunConfig) -> Result<SocketAddr, StunError> {
    if config.servers.is_empty() {
        return Err(StunError::Io("no STUN servers configured".into()));
    }

    let mut set = tokio::task::JoinSet::new();

    for server_str in &config.servers {
        let server_str = server_str.clone();
        let timeout = config.timeout;
        // We can't share &UdpSocket across spawned tasks (not Send
        // on all platforms), so each task creates its own socket.
        // For NAT classification purposes this is actually fine — if
        // the NAT is cone, all sockets see the same IP; if symmetric,
        // they'll differ (and we'll detect that in classify_nat).
        set.spawn(async move {
            let sock = UdpSocket::bind("0.0.0.0:0")
                .await
                .map_err(|e| StunError::Io(format!("bind: {e}")))?;
            let addr = resolve_stun_server(&server_str).await?;
            stun_reflect(&sock, addr, timeout).await
        });
    }

    // Return first success. Collect errors for diagnostics.
    let mut last_err: Option<StunError> = None;
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok(Ok(addr)) => {
                set.abort_all();
                return Ok(addr);
            }
            Ok(Err(e)) => {
                last_err = Some(e);
            }
            Err(_join_err) => {
                last_err = Some(StunError::Io("STUN task panicked".into()));
            }
        }
    }

    Err(last_err.unwrap_or(StunError::Io("no STUN servers responded".into())))
}

/// Probe multiple STUN servers and return per-server results suitable
/// for feeding into `classify_nat` alongside relay-based probes.
///
/// Unlike `discover_reflexive` (which returns on first success), this
/// waits for ALL servers and returns individual results — needed for
/// NAT type classification which requires 2+ observations.
pub async fn probe_stun_servers(
    config: &StunConfig,
) -> Vec<crate::reflect::NatProbeResult> {
    use std::time::Instant;

    let mut set = tokio::task::JoinSet::new();
    for server_str in &config.servers {
        let server_str = server_str.clone();
        let timeout = config.timeout;
        set.spawn(async move {
            let start = Instant::now();
            let sock = match UdpSocket::bind("0.0.0.0:0").await {
                Ok(s) => s,
                Err(e) => {
                    return crate::reflect::NatProbeResult {
                        relay_name: format!("stun:{server_str}"),
                        relay_addr: server_str,
                        observed_addr: None,
                        latency_ms: None,
                        error: Some(format!("bind: {e}")),
                    };
                }
            };
            let resolved = match resolve_stun_server(&server_str).await {
                Ok(a) => a,
                Err(e) => {
                    return crate::reflect::NatProbeResult {
                        relay_name: format!("stun:{server_str}"),
                        relay_addr: server_str,
                        observed_addr: None,
                        latency_ms: None,
                        error: Some(e.to_string()),
                    };
                }
            };
            match stun_reflect(&sock, resolved, timeout).await {
                Ok(addr) => crate::reflect::NatProbeResult {
                    relay_name: format!("stun:{server_str}"),
                    relay_addr: resolved.to_string(),
                    observed_addr: Some(addr.to_string()),
                    latency_ms: Some(start.elapsed().as_millis() as u32),
                    error: None,
                },
                Err(e) => crate::reflect::NatProbeResult {
                    relay_name: format!("stun:{server_str}"),
                    relay_addr: resolved.to_string(),
                    observed_addr: None,
                    latency_ms: None,
                    error: Some(e.to_string()),
                },
            }
        });
    }

    let mut results = Vec::with_capacity(config.servers.len());
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok(result) => results.push(result),
            Err(_) => results.push(crate::reflect::NatProbeResult {
                relay_name: "stun:<panicked>".into(),
                relay_addr: "unknown".into(),
                observed_addr: None,
                latency_ms: None,
                error: Some("STUN probe task panicked".into()),
            }),
        }
    }
    results
}

// ── Port allocation pattern detection ──────────────────────────────

/// NAT port allocation pattern, detected by probing multiple STUN
/// servers from a single socket and analyzing the observed external
/// port sequence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum PortAllocation {
    /// Same external port for all destinations — cone-like NAT.
    /// Standard hole-punching works; no hard NAT techniques needed.
    PortPreserving,
    /// Ports increment by a consistent delta per new flow.
    /// Port prediction is viable: next_port = last_port + delta.
    Sequential { delta: i16 },
    /// No discernible pattern — truly random allocation.
    /// Only birthday attack or relay can traverse this.
    Random,
    /// Not enough data to classify (< 3 successful probes).
    Unknown,
}

impl std::fmt::Display for PortAllocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PortPreserving => write!(f, "port-preserving"),
            Self::Sequential { delta } => write!(f, "sequential(delta={delta})"),
            Self::Random => write!(f, "random"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Result of port allocation analysis.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PortAllocationResult {
    /// Detected allocation pattern.
    pub allocation: PortAllocation,
    /// Observed external ports (one per successful STUN probe),
    /// in probe order.
    pub observed_ports: Vec<u16>,
    /// External IP (consensus from probes, if available).
    pub external_ip: Option<IpAddr>,
}

/// Detect the NAT's port allocation pattern by sending STUN probes
/// to multiple servers from a **single socket**.
///
/// Unlike `probe_stun_servers` (which creates one socket per server
/// for NAT type classification), this uses one socket so we see how
/// the NAT maps the SAME source port to different destinations.
///
/// - Same external port for all → `PortPreserving` (cone-like)
/// - Consistent delta → `Sequential { delta }`
/// - No pattern → `Random`
///
/// Requires at least 3 servers for reliable classification.
pub async fn detect_port_allocation(
    config: &StunConfig,
) -> PortAllocationResult {
    if config.servers.len() < 2 {
        return PortAllocationResult {
            allocation: PortAllocation::Unknown,
            observed_ports: vec![],
            external_ip: None,
        };
    }

    // Single socket — all probes share the same source port.
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => {
            return PortAllocationResult {
                allocation: PortAllocation::Unknown,
                observed_ports: vec![],
                external_ip: None,
            };
        }
    };

    // Probe servers SEQUENTIALLY (not parallel) so the NAT sees
    // distinct flows in order. Parallel probes could arrive out-of-
    // order and confuse sequential delta detection.
    let mut observed: Vec<SocketAddr> = Vec::new();
    for server_str in &config.servers {
        let resolved = match resolve_stun_server(server_str).await {
            Ok(a) => a,
            Err(_) => continue,
        };
        match stun_reflect(&socket, resolved, config.timeout).await {
            Ok(addr) => observed.push(addr),
            Err(_) => continue,
        }
    }

    let ports: Vec<u16> = observed.iter().map(|a| a.port()).collect();
    let external_ip = observed.first().map(|a| a.ip());
    let allocation = classify_port_allocation(&ports);

    tracing::info!(
        ?allocation,
        ?ports,
        external_ip = ?external_ip,
        "stun: port allocation detected"
    );

    PortAllocationResult {
        allocation,
        observed_ports: ports,
        external_ip,
    }
}

/// Pure-function classifier — split out for unit testing.
pub fn classify_port_allocation(ports: &[u16]) -> PortAllocation {
    if ports.len() < 2 {
        return PortAllocation::Unknown;
    }

    // All same port?
    if ports.iter().all(|&p| p == ports[0]) {
        return PortAllocation::PortPreserving;
    }

    if ports.len() < 3 {
        // With only 2 different ports we can't distinguish
        // sequential from random reliably.
        return PortAllocation::Unknown;
    }

    // Compute deltas between consecutive ports.
    let deltas: Vec<i16> = ports
        .windows(2)
        .map(|w| w[1] as i32 - w[0] as i32)
        .map(|d| {
            // Handle wraparound: if delta is huge negative (e.g.,
            // 65535 -> 2 = -65533), treat as +3. And vice versa.
            if d > 32768 {
                (d - 65536) as i16
            } else if d < -32768 {
                (d + 65536) as i16
            } else {
                d as i16
            }
        })
        .collect();

    // Check if all deltas are the same (sequential pattern).
    let first_delta = deltas[0];
    if first_delta == 0 {
        // All same port was already handled above, this means
        // mixed same/different — not sequential.
        return PortAllocation::Random;
    }

    // Allow small jitter: if all deltas are within ±1 of each other,
    // consider it sequential with the median delta.
    let all_close = deltas.iter().all(|&d| (d - first_delta).unsigned_abs() <= 1);
    if all_close {
        // Use the most common delta (mode).
        let median_delta = first_delta;
        return PortAllocation::Sequential { delta: median_delta };
    }

    // Check for consistent delta with occasional skip (some NATs
    // skip a port when another flow grabs it concurrently).
    // If most deltas (>= 60%) agree on the same value, call it
    // sequential.
    let mut delta_counts = std::collections::HashMap::new();
    for &d in &deltas {
        *delta_counts.entry(d).or_insert(0u32) += 1;
    }
    if let Some((&most_common, &count)) = delta_counts.iter().max_by_key(|(_, v)| *v) {
        let threshold = (deltas.len() as f64 * 0.6).ceil() as u32;
        if count >= threshold && most_common != 0 {
            return PortAllocation::Sequential { delta: most_common };
        }
    }

    PortAllocation::Random
}

/// Predict the next N external ports for a sequential NAT.
///
/// Given the last observed port and the delta, returns a range of
/// predicted ports centered around the most likely next value.
/// The `offset` parameter accounts for additional flows that may
/// open between the probe and the actual connection attempt.
pub fn predict_ports(
    last_port: u16,
    delta: i16,
    offset: u16,
    spread: u16,
) -> Vec<u16> {
    let base = last_port as i32 + (delta as i32 * (offset as i32 + 1));
    let mut ports = Vec::with_capacity((spread * 2 + 1) as usize);
    for i in -(spread as i32)..=(spread as i32) {
        let p = base + (i * delta as i32);
        // Wrap to valid port range (1..=65535)
        let p = ((p % 65536) + 65536) % 65536;
        if p > 0 && p <= 65535 {
            ports.push(p as u16);
        }
    }
    ports.sort();
    ports.dedup();
    ports
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_binding_request_is_20_bytes() {
        let txn_id = [1u8; 12];
        let pkt = encode_binding_request(&txn_id);
        assert_eq!(pkt.len(), 20);
        // First two bytes: Binding Request type
        assert_eq!(pkt[0], 0x00);
        assert_eq!(pkt[1], 0x01);
        // Bytes 2-3: message length = 0
        assert_eq!(pkt[2], 0x00);
        assert_eq!(pkt[3], 0x00);
        // Bytes 4-7: magic cookie
        assert_eq!(&pkt[4..8], &MAGIC_COOKIE.to_be_bytes());
        // Bytes 8-19: transaction ID
        assert_eq!(&pkt[8..20], &txn_id);
    }

    #[test]
    fn parse_xor_mapped_address_ipv4() {
        let txn_id = [0u8; 12];
        // Build a minimal Binding Response with XOR-MAPPED-ADDRESS
        // for 203.0.113.5:12345
        let ip = Ipv4Addr::new(203, 0, 113, 5);
        let port: u16 = 12345;

        // XOR the port and IP
        let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
        let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
        let ip_octets = ip.octets();
        let xip = [
            ip_octets[0] ^ cookie_bytes[0],
            ip_octets[1] ^ cookie_bytes[1],
            ip_octets[2] ^ cookie_bytes[2],
            ip_octets[3] ^ cookie_bytes[3],
        ];

        // Attribute: XOR-MAPPED-ADDRESS (type 0x0020, length 8)
        let mut attr = Vec::new();
        attr.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attr.extend_from_slice(&8u16.to_be_bytes()); // length
        attr.push(0x00); // reserved
        attr.push(0x01); // family: IPv4
        attr.extend_from_slice(&xport.to_be_bytes());
        attr.extend_from_slice(&xip);

        // Build full response
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attr);

        let result = parse_binding_response(&pkt, &txn_id).unwrap();
        assert_eq!(result, SocketAddr::new(IpAddr::V4(ip), port));
    }

    #[test]
    fn parse_xor_mapped_address_ipv6() {
        let txn_id = [0xAB; 12];
        let ip = Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1);
        let port: u16 = 54321;

        let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
        let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
        let ip_octets = ip.octets();
        let mut xor_key = [0u8; 16];
        xor_key[..4].copy_from_slice(&cookie_bytes);
        xor_key[4..16].copy_from_slice(&txn_id);
        let mut xip = [0u8; 16];
        for i in 0..16 {
            xip[i] = ip_octets[i] ^ xor_key[i];
        }

        let mut attr = Vec::new();
        attr.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attr.extend_from_slice(&20u16.to_be_bytes()); // length
        attr.push(0x00); // reserved
        attr.push(0x02); // family: IPv6
        attr.extend_from_slice(&xport.to_be_bytes());
        attr.extend_from_slice(&xip);

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attr);

        let result = parse_binding_response(&pkt, &txn_id).unwrap();
        assert_eq!(result, SocketAddr::new(IpAddr::V6(ip), port));
    }

    #[test]
    fn parse_mapped_address_fallback() {
        let txn_id = [0u8; 12];
        let ip = Ipv4Addr::new(198, 51, 100, 42);
        let port: u16 = 8080;

        // Attribute: MAPPED-ADDRESS (type 0x0001, length 8)
        let mut attr = Vec::new();
        attr.extend_from_slice(&ATTR_MAPPED_ADDRESS.to_be_bytes());
        attr.extend_from_slice(&8u16.to_be_bytes());
        attr.push(0x00); // reserved
        attr.push(0x01); // family: IPv4
        attr.extend_from_slice(&port.to_be_bytes());
        attr.extend_from_slice(&ip.octets());

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attr);

        let result = parse_binding_response(&pkt, &txn_id).unwrap();
        assert_eq!(result, SocketAddr::new(IpAddr::V4(ip), port));
    }

    #[test]
    fn parse_rejects_wrong_txn_id() {
        let txn_id = [1u8; 12];
        let wrong_txn = [2u8; 12];

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&wrong_txn);

        let err = parse_binding_response(&pkt, &txn_id).unwrap_err();
        assert!(matches!(err, StunError::TxnMismatch));
    }

    #[test]
    fn parse_rejects_too_short() {
        let txn_id = [0u8; 12];
        let err = parse_binding_response(&[0u8; 10], &txn_id).unwrap_err();
        assert!(matches!(err, StunError::Malformed(_)));
    }

    #[test]
    fn parse_rejects_bad_cookie() {
        let txn_id = [0u8; 12];
        let mut pkt = [0u8; 20];
        pkt[0..2].copy_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt[4..8].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        pkt[8..20].copy_from_slice(&txn_id);

        let err = parse_binding_response(&pkt, &txn_id).unwrap_err();
        assert!(matches!(err, StunError::Malformed(_)));
    }

    #[test]
    fn parse_no_mapped_address() {
        let txn_id = [0u8; 12];
        // Valid response with zero-length body (no attributes)
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);

        let err = parse_binding_response(&pkt, &txn_id).unwrap_err();
        assert!(matches!(err, StunError::NoMappedAddress));
    }

    #[test]
    fn xor_mapped_preferred_over_mapped() {
        let txn_id = [0u8; 12];

        // Build two attributes: MAPPED-ADDRESS with one IP, then
        // XOR-MAPPED-ADDRESS with a different IP.
        let mapped_ip = Ipv4Addr::new(10, 0, 0, 1);
        let xor_ip = Ipv4Addr::new(203, 0, 113, 5);
        let port: u16 = 9999;

        let mut attrs = Vec::new();

        // MAPPED-ADDRESS
        attrs.extend_from_slice(&ATTR_MAPPED_ADDRESS.to_be_bytes());
        attrs.extend_from_slice(&8u16.to_be_bytes());
        attrs.push(0x00);
        attrs.push(0x01);
        attrs.extend_from_slice(&port.to_be_bytes());
        attrs.extend_from_slice(&mapped_ip.octets());

        // XOR-MAPPED-ADDRESS
        let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
        let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
        let xip_octets = xor_ip.octets();
        let xip = [
            xip_octets[0] ^ cookie_bytes[0],
            xip_octets[1] ^ cookie_bytes[1],
            xip_octets[2] ^ cookie_bytes[2],
            xip_octets[3] ^ cookie_bytes[3],
        ];
        attrs.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attrs.extend_from_slice(&8u16.to_be_bytes());
        attrs.push(0x00);
        attrs.push(0x01);
        attrs.extend_from_slice(&xport.to_be_bytes());
        attrs.extend_from_slice(&xip);

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attrs);

        let result = parse_binding_response(&pkt, &txn_id).unwrap();
        // XOR-MAPPED-ADDRESS should win
        assert_eq!(result, SocketAddr::new(IpAddr::V4(xor_ip), port));
    }

    // ── Additional edge-case tests ────────────────────────────────

    #[test]
    fn encode_txn_id_is_random() {
        let a = gen_txn_id();
        let b = gen_txn_id();
        // Extremely unlikely to collide (96-bit random).
        assert_ne!(a, b, "two txn IDs should differ");
    }

    #[test]
    fn parse_error_response_0x0111() {
        let txn_id = [0u8; 12];
        let mut pkt = Vec::new();
        // Error response type = 0x0111
        pkt.extend_from_slice(&0x0111u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);

        let err = parse_binding_response(&pkt, &txn_id).unwrap_err();
        assert!(matches!(err, StunError::ErrorResponse(_)));
    }

    #[test]
    fn parse_unknown_message_type() {
        let txn_id = [0u8; 12];
        let mut pkt = Vec::new();
        // Some unknown type 0x0042
        pkt.extend_from_slice(&0x0042u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);

        let err = parse_binding_response(&pkt, &txn_id).unwrap_err();
        assert!(matches!(err, StunError::Malformed(_)));
    }

    #[test]
    fn parse_truncated_attribute_is_handled() {
        let txn_id = [0u8; 12];
        // Attribute header says length=100 but buffer ends after 4 bytes
        let mut attr = Vec::new();
        attr.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attr.extend_from_slice(&100u16.to_be_bytes()); // claims 100 bytes
        // No actual value bytes — truncated

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attr);

        // Should NOT panic — truncated attribute is skipped, then NoMappedAddress
        let err = parse_binding_response(&pkt, &txn_id).unwrap_err();
        assert!(matches!(err, StunError::NoMappedAddress));
    }

    #[test]
    fn parse_unknown_attributes_skipped() {
        let txn_id = [0u8; 12];
        let ip = Ipv4Addr::new(192, 0, 2, 99);
        let port: u16 = 5000;

        let mut attrs = Vec::new();

        // Unknown attribute type 0x8000 (comprehension-optional), 4 bytes
        attrs.extend_from_slice(&0x8000u16.to_be_bytes());
        attrs.extend_from_slice(&4u16.to_be_bytes());
        attrs.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        // The real XOR-MAPPED-ADDRESS after the unknown one
        let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
        let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
        let xip = [
            ip.octets()[0] ^ cookie_bytes[0],
            ip.octets()[1] ^ cookie_bytes[1],
            ip.octets()[2] ^ cookie_bytes[2],
            ip.octets()[3] ^ cookie_bytes[3],
        ];
        attrs.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attrs.extend_from_slice(&8u16.to_be_bytes());
        attrs.push(0x00);
        attrs.push(0x01);
        attrs.extend_from_slice(&xport.to_be_bytes());
        attrs.extend_from_slice(&xip);

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attrs);

        let result = parse_binding_response(&pkt, &txn_id).unwrap();
        assert_eq!(result, SocketAddr::new(IpAddr::V4(ip), port));
    }

    #[test]
    fn parse_message_length_exceeds_buffer() {
        let txn_id = [0u8; 12];
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        // Claims 500 bytes of attributes but buffer is only header
        pkt.extend_from_slice(&500u16.to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);

        let err = parse_binding_response(&pkt, &txn_id).unwrap_err();
        assert!(matches!(err, StunError::Malformed(_)));
    }

    #[test]
    fn parse_xor_mapped_ipv4_high_port() {
        // Port 65535 — tests boundary of u16 XOR
        let txn_id = [0xFF; 12];
        let ip = Ipv4Addr::new(255, 255, 255, 255);
        let port: u16 = 65535;

        let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
        let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
        let xip = [
            ip.octets()[0] ^ cookie_bytes[0],
            ip.octets()[1] ^ cookie_bytes[1],
            ip.octets()[2] ^ cookie_bytes[2],
            ip.octets()[3] ^ cookie_bytes[3],
        ];

        let mut attr = Vec::new();
        attr.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attr.extend_from_slice(&8u16.to_be_bytes());
        attr.push(0x00);
        attr.push(0x01);
        attr.extend_from_slice(&xport.to_be_bytes());
        attr.extend_from_slice(&xip);

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attr);

        let result = parse_binding_response(&pkt, &txn_id).unwrap();
        assert_eq!(result.port(), 65535);
        assert_eq!(result.ip(), IpAddr::V4(ip));
    }

    #[test]
    fn parse_mapped_address_ipv6() {
        let txn_id = [0u8; 12];
        let ip = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x42);
        let port: u16 = 3478;

        let mut attr = Vec::new();
        attr.extend_from_slice(&ATTR_MAPPED_ADDRESS.to_be_bytes());
        attr.extend_from_slice(&20u16.to_be_bytes());
        attr.push(0x00);
        attr.push(0x02); // family: IPv6
        attr.extend_from_slice(&port.to_be_bytes());
        attr.extend_from_slice(&ip.octets());

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attr);

        let result = parse_binding_response(&pkt, &txn_id).unwrap();
        assert_eq!(result, SocketAddr::new(IpAddr::V6(ip), port));
    }

    #[test]
    fn parse_mapped_address_unknown_family() {
        let txn_id = [0u8; 12];
        let mut attr = Vec::new();
        attr.extend_from_slice(&ATTR_MAPPED_ADDRESS.to_be_bytes());
        attr.extend_from_slice(&8u16.to_be_bytes());
        attr.push(0x00);
        attr.push(0x03); // unknown family
        attr.extend_from_slice(&1234u16.to_be_bytes());
        attr.extend_from_slice(&[1, 2, 3, 4]);

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attr);

        // Unknown family in the only attribute → NoMappedAddress
        let err = parse_binding_response(&pkt, &txn_id).unwrap_err();
        assert!(matches!(err, StunError::NoMappedAddress));
    }

    #[test]
    fn parse_attribute_with_padding() {
        // Attribute with length=5 gets padded to 8 bytes boundary.
        // Then a real XOR-MAPPED-ADDRESS follows.
        let txn_id = [0u8; 12];
        let ip = Ipv4Addr::new(10, 1, 2, 3);
        let port: u16 = 7777;

        let mut attrs = Vec::new();

        // SOFTWARE attribute (type 0x8022) with 5 bytes of data
        attrs.extend_from_slice(&0x8022u16.to_be_bytes());
        attrs.extend_from_slice(&5u16.to_be_bytes());
        attrs.extend_from_slice(b"hello");
        // 3 bytes padding to reach next 4-byte boundary
        attrs.extend_from_slice(&[0, 0, 0]);

        // XOR-MAPPED-ADDRESS
        let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
        let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
        let xip = [
            ip.octets()[0] ^ cookie_bytes[0],
            ip.octets()[1] ^ cookie_bytes[1],
            ip.octets()[2] ^ cookie_bytes[2],
            ip.octets()[3] ^ cookie_bytes[3],
        ];
        attrs.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attrs.extend_from_slice(&8u16.to_be_bytes());
        attrs.push(0x00);
        attrs.push(0x01);
        attrs.extend_from_slice(&xport.to_be_bytes());
        attrs.extend_from_slice(&xip);

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&txn_id);
        pkt.extend_from_slice(&attrs);

        let result = parse_binding_response(&pkt, &txn_id).unwrap();
        assert_eq!(result, SocketAddr::new(IpAddr::V4(ip), port));
    }

    #[test]
    fn stun_error_display() {
        assert!(StunError::Timeout.to_string().contains("timeout"));
        assert!(StunError::TxnMismatch.to_string().contains("mismatch"));
        assert!(StunError::NoMappedAddress.to_string().contains("MAPPED"));
        assert!(StunError::Io("test".into()).to_string().contains("test"));
        assert!(StunError::DnsError("bad".into()).to_string().contains("bad"));
        assert!(StunError::ErrorResponse(420).to_string().contains("420"));
        assert!(StunError::Malformed("x".into()).to_string().contains("x"));
    }

    #[test]
    fn default_stun_config_has_servers() {
        let cfg = StunConfig::default();
        assert!(cfg.servers.len() >= 3);
        assert!(cfg.timeout.as_secs() > 0);
    }

    #[tokio::test]
    async fn discover_reflexive_empty_servers_errors() {
        let cfg = StunConfig {
            servers: vec![],
            timeout: Duration::from_secs(1),
        };
        let err = discover_reflexive(&cfg).await.unwrap_err();
        assert!(matches!(err, StunError::Io(_)));
    }

    // ── Port allocation classification tests ────────────────────

    #[test]
    fn classify_port_preserving() {
        let ports = vec![4433, 4433, 4433, 4433, 4433];
        assert_eq!(classify_port_allocation(&ports), PortAllocation::PortPreserving);
    }

    #[test]
    fn classify_sequential_delta_1() {
        let ports = vec![40001, 40002, 40003, 40004, 40005];
        assert_eq!(
            classify_port_allocation(&ports),
            PortAllocation::Sequential { delta: 1 }
        );
    }

    #[test]
    fn classify_sequential_delta_2() {
        let ports = vec![50000, 50002, 50004, 50006];
        assert_eq!(
            classify_port_allocation(&ports),
            PortAllocation::Sequential { delta: 2 }
        );
    }

    #[test]
    fn classify_sequential_negative_delta() {
        // Some NATs decrement
        let ports = vec![50000, 49999, 49998, 49997];
        assert_eq!(
            classify_port_allocation(&ports),
            PortAllocation::Sequential { delta: -1 }
        );
    }

    #[test]
    fn classify_random() {
        let ports = vec![40001, 52847, 19432, 61203, 8847];
        assert_eq!(classify_port_allocation(&ports), PortAllocation::Random);
    }

    #[test]
    fn classify_too_few_ports() {
        assert_eq!(classify_port_allocation(&[]), PortAllocation::Unknown);
        assert_eq!(classify_port_allocation(&[4433]), PortAllocation::Unknown);
    }

    #[test]
    fn classify_two_same_is_preserving() {
        let ports = vec![4433, 4433];
        assert_eq!(classify_port_allocation(&ports), PortAllocation::PortPreserving);
    }

    #[test]
    fn classify_two_different_is_unknown() {
        // Can't distinguish sequential from random with only 2 points
        let ports = vec![4433, 4434];
        assert_eq!(classify_port_allocation(&ports), PortAllocation::Unknown);
    }

    #[test]
    fn classify_sequential_with_jitter() {
        // Delta is mostly 1 but one jump of 2 (concurrent flow grabbed a port)
        let ports = vec![40001, 40002, 40004, 40005, 40006];
        // Deltas: [1, 2, 1, 1] — 3 out of 4 are delta=1, above 60% threshold
        assert_eq!(
            classify_port_allocation(&ports),
            PortAllocation::Sequential { delta: 1 }
        );
    }

    #[test]
    fn classify_sequential_wraparound() {
        // Port wraps from 65534 -> 65535 -> 1 -> 2
        let ports = vec![65534, 65535, 1, 2];
        // Deltas: [1, -65534(→+2), 1] — wraparound handling
        let alloc = classify_port_allocation(&ports);
        // Should detect as sequential with delta ~1
        assert!(
            matches!(alloc, PortAllocation::Sequential { delta: 1 }),
            "wraparound should be sequential, got: {alloc:?}"
        );
    }

    #[test]
    fn predict_ports_sequential() {
        // Last port 40005, delta 1, offset 0, spread 2
        let predicted = predict_ports(40005, 1, 0, 2);
        assert!(predicted.contains(&40006)); // most likely next
        assert!(predicted.contains(&40004)); // spread -2
        assert!(predicted.contains(&40008)); // spread +2
    }

    #[test]
    fn predict_ports_delta_2() {
        let predicted = predict_ports(50000, 2, 0, 1);
        assert!(predicted.contains(&50002)); // next
        assert!(predicted.contains(&50000)); // spread -1*delta
        assert!(predicted.contains(&50004)); // spread +1*delta
    }

    #[test]
    fn predict_ports_with_offset() {
        // offset=2 means 2 extra flows will open before our dial,
        // so prediction jumps further: 40005 + 1*(2+1) = 40008
        let predicted = predict_ports(40005, 1, 2, 1);
        assert!(predicted.contains(&40008));
    }

    #[test]
    fn predict_ports_wraparound() {
        let predicted = predict_ports(65534, 1, 0, 2);
        // Should handle the u16 wraparound gracefully
        assert!(predicted.contains(&65535));
        assert!(!predicted.is_empty());
    }

    #[test]
    fn port_allocation_display() {
        assert_eq!(PortAllocation::PortPreserving.to_string(), "port-preserving");
        assert_eq!(PortAllocation::Sequential { delta: 1 }.to_string(), "sequential(delta=1)");
        assert_eq!(PortAllocation::Random.to_string(), "random");
        assert_eq!(PortAllocation::Unknown.to_string(), "unknown");
    }

    #[test]
    fn port_allocation_serde() {
        let alloc = PortAllocation::Sequential { delta: 3 };
        let json = serde_json::to_string(&alloc).unwrap();
        assert!(json.contains("Sequential"));
        assert!(json.contains("3"));
    }

    #[test]
    fn port_allocation_result_serde() {
        let result = PortAllocationResult {
            allocation: PortAllocation::Sequential { delta: 1 },
            observed_ports: vec![40001, 40002, 40003],
            external_ip: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5))),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("Sequential"));
        assert!(json.contains("40001"));
        assert!(json.contains("203.0.113.5"));
    }

    /// Integration test: detect port allocation on real network.
    #[tokio::test]
    #[ignore]
    async fn integration_detect_port_allocation() {
        let config = StunConfig::default();
        let result = detect_port_allocation(&config).await;
        println!("Port allocation: {:?}", result.allocation);
        println!("Observed ports: {:?}", result.observed_ports);
        println!("External IP: {:?}", result.external_ip);
        assert!(!result.observed_ports.is_empty());
    }

    /// Integration test: actually query stun.l.google.com.
    /// Ignored by default since it requires network access.
    #[tokio::test]
    #[ignore]
    async fn integration_stun_google() {
        let config = StunConfig {
            servers: vec!["stun.l.google.com:19302".into()],
            timeout: Duration::from_secs(5),
        };
        let addr = discover_reflexive(&config).await.unwrap();
        // Should be a public IPv4 address.
        assert!(addr.ip().is_ipv4() || addr.ip().is_ipv6());
        assert!(addr.port() > 0);
        println!("STUN reflexive address: {addr}");
    }

    /// Integration test: probe multiple servers and get NAT probes.
    #[tokio::test]
    #[ignore]
    async fn integration_probe_stun_servers() {
        let config = StunConfig::default();
        let probes = probe_stun_servers(&config).await;
        assert!(!probes.is_empty());
        let successes: Vec<_> = probes.iter().filter(|p| p.observed_addr.is_some()).collect();
        assert!(
            !successes.is_empty(),
            "at least one STUN server should respond"
        );
        for p in &probes {
            println!(
                "{}: addr={:?} latency={:?}ms err={:?}",
                p.relay_name, p.observed_addr, p.latency_ms, p.error
            );
        }
    }
}
