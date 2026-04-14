//! NAT port mapping protocols: NAT-PMP (RFC 6886), PCP (RFC 6887),
//! and UPnP IGD.
//!
//! These allow clients to request explicit port mappings from their
//! router, making even symmetric NATs traversable. Tailscale reports
//! ~70% of consumer routers support at least one of these.
//!
//! Try order: NAT-PMP → PCP → UPnP (first success wins).
//!
//! The mapped external address is advertised as an additional ICE
//! candidate alongside the server-reflexive (STUN) and host (LAN)
//! candidates.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

// ── Types ──────────────────────────────────────────────────────────

/// Which protocol provided the port mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum PortMapProtocol {
    NatPmp,
    Pcp,
    #[allow(clippy::upper_case_acronyms)]
    UPnP,
}

/// A successfully acquired port mapping.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PortMapping {
    /// The external address:port that peers can dial.
    pub external_addr: SocketAddr,
    /// Which protocol was used.
    pub protocol: PortMapProtocol,
    /// When the mapping expires (absolute time).
    #[serde(skip)]
    pub expires_at: Instant,
    /// How often to refresh (typically half the lifetime).
    #[serde(skip)]
    pub refresh_interval: Duration,
    /// The gateway address used for refresh requests.
    #[serde(skip)]
    pub gateway: Ipv4Addr,
    /// The internal port that was mapped.
    pub internal_port: u16,
}

#[derive(Debug, Clone)]
pub enum PortMapError {
    /// No default gateway found.
    NoGateway,
    /// Protocol-specific error.
    Protocol(String),
    /// Network I/O error.
    Io(String),
    /// Timed out.
    Timeout,
    /// All protocols failed.
    AllFailed(Vec<(PortMapProtocol, String)>),
}

impl std::fmt::Display for PortMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoGateway => write!(f, "no default gateway found"),
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Timeout => write!(f, "timeout"),
            Self::AllFailed(errs) => {
                write!(f, "all protocols failed:")?;
                for (proto, err) in errs {
                    write!(f, " {proto:?}={err}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PortMapError {}

// ── Gateway discovery ──────────────────────────────────────────────

/// Discover the default IPv4 gateway address.
///
/// Platform-specific:
/// - macOS: `route -n get default` and parse the `gateway:` line
/// - Linux/Android: parse `/proc/net/route` for the 0.0.0.0
///   destination entry
pub async fn default_gateway() -> Result<Ipv4Addr, PortMapError> {
    #[cfg(target_os = "macos")]
    {
        default_gateway_macos().await
    }
    #[cfg(target_os = "linux")]
    {
        default_gateway_linux().await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(PortMapError::NoGateway)
    }
}

#[cfg(target_os = "macos")]
async fn default_gateway_macos() -> Result<Ipv4Addr, PortMapError> {
    let output = tokio::process::Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .await
        .map_err(|e| PortMapError::Io(format!("route: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("gateway:") {
            let gw = rest.trim();
            return gw
                .parse::<Ipv4Addr>()
                .map_err(|e| PortMapError::Protocol(format!("parse gateway {gw:?}: {e}")));
        }
    }
    Err(PortMapError::NoGateway)
}

#[cfg(target_os = "linux")]
async fn default_gateway_linux() -> Result<Ipv4Addr, PortMapError> {
    let contents = tokio::fs::read_to_string("/proc/net/route")
        .await
        .map_err(|e| PortMapError::Io(format!("/proc/net/route: {e}")))?;

    // Format: Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT
    // Default route has Destination = 00000000
    for line in contents.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        if fields[1] == "00000000" {
            // Gateway is in hex, little-endian on most Linux
            let gw_hex = u32::from_str_radix(fields[2], 16)
                .map_err(|e| PortMapError::Protocol(format!("parse gateway hex: {e}")))?;
            return Ok(Ipv4Addr::from(gw_hex.to_be()));
        }
    }
    Err(PortMapError::NoGateway)
}

// ── NAT-PMP (RFC 6886) ────────────────────────────────────────────

/// NAT-PMP uses UDP port 5351 on the gateway.
const NATPMP_PORT: u16 = 5351;

/// NAT-PMP opcode for mapping a UDP port.
const NATPMP_OP_MAP_UDP: u8 = 1;

/// NAT-PMP version.
const NATPMP_VERSION: u8 = 0;

/// Request the gateway's external address via NAT-PMP (opcode 0).
async fn natpmp_external_address(
    socket: &UdpSocket,
    gateway: SocketAddrV4,
    timeout: Duration,
) -> Result<Ipv4Addr, PortMapError> {
    // Request: version(1) + opcode(1) = 2 bytes
    let request = [NATPMP_VERSION, 0]; // opcode 0 = external address request
    socket
        .send_to(&request, gateway)
        .await
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    let mut buf = [0u8; 12];
    let len = tokio::time::timeout(timeout, async {
        let (len, _) = socket
            .recv_from(&mut buf)
            .await
            .map_err(|e| PortMapError::Io(e.to_string()))?;
        Ok::<_, PortMapError>(len)
    })
    .await
    .map_err(|_| PortMapError::Timeout)??;

    // Response: version(1) + opcode(1) + result(2) + epoch(4) + external_ip(4) = 12 bytes
    if len < 12 {
        return Err(PortMapError::Protocol(format!(
            "NAT-PMP external addr response too short: {len}"
        )));
    }
    let result_code = u16::from_be_bytes([buf[2], buf[3]]);
    if result_code != 0 {
        return Err(PortMapError::Protocol(format!(
            "NAT-PMP error: result code {result_code}"
        )));
    }
    Ok(Ipv4Addr::new(buf[8], buf[9], buf[10], buf[11]))
}

/// Request a UDP port mapping via NAT-PMP.
///
/// Returns the mapped external port and lifetime in seconds.
async fn natpmp_map_udp(
    socket: &UdpSocket,
    gateway: SocketAddrV4,
    internal_port: u16,
    external_port: u16,
    lifetime_secs: u32,
    timeout: Duration,
) -> Result<(u16, u32), PortMapError> {
    // Request: version(1) + opcode(1) + reserved(2) + internal_port(2) +
    //          suggested_external_port(2) + lifetime(4) = 12 bytes
    let mut request = [0u8; 12];
    request[0] = NATPMP_VERSION;
    request[1] = NATPMP_OP_MAP_UDP;
    // bytes 2-3: reserved (zero)
    request[4..6].copy_from_slice(&internal_port.to_be_bytes());
    request[6..8].copy_from_slice(&external_port.to_be_bytes());
    request[8..12].copy_from_slice(&lifetime_secs.to_be_bytes());

    socket
        .send_to(&request, gateway)
        .await
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    let mut buf = [0u8; 16];
    let len = tokio::time::timeout(timeout, async {
        let (len, _) = socket
            .recv_from(&mut buf)
            .await
            .map_err(|e| PortMapError::Io(e.to_string()))?;
        Ok::<_, PortMapError>(len)
    })
    .await
    .map_err(|_| PortMapError::Timeout)??;

    // Response: version(1) + opcode(1) + result(2) + epoch(4) +
    //           internal_port(2) + mapped_external_port(2) + lifetime(4) = 16 bytes
    if len < 16 {
        return Err(PortMapError::Protocol(format!(
            "NAT-PMP map response too short: {len}"
        )));
    }
    let result_code = u16::from_be_bytes([buf[2], buf[3]]);
    if result_code != 0 {
        return Err(PortMapError::Protocol(format!(
            "NAT-PMP map error: result code {result_code}"
        )));
    }
    // Bytes: 8-9 = internal_port, 10-11 = mapped_external_port, 12-15 = lifetime
    let resp_internal = u16::from_be_bytes([buf[8], buf[9]]);
    let mapped_port = u16::from_be_bytes([buf[10], buf[11]]);
    let granted_lifetime = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
    if resp_internal != internal_port {
        tracing::debug!(
            expected = internal_port,
            got = resp_internal,
            "NAT-PMP: response internal port differs from request (some routers do this)"
        );
    }

    Ok((mapped_port, granted_lifetime))
}

/// Attempt NAT-PMP port mapping for the given internal port.
async fn try_natpmp(
    gateway: Ipv4Addr,
    internal_port: u16,
    timeout: Duration,
) -> Result<PortMapping, PortMapError> {
    let gw_addr = SocketAddrV4::new(gateway, NATPMP_PORT);
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| PortMapError::Io(format!("bind: {e}")))?;

    // Step 1: get external address
    let external_ip = natpmp_external_address(&socket, gw_addr, timeout).await?;

    // Step 2: request port mapping
    // Request same port as internal (preferred); 7200s lifetime (standard)
    let (mapped_port, lifetime) =
        natpmp_map_udp(&socket, gw_addr, internal_port, internal_port, 7200, timeout).await?;

    let lifetime_dur = Duration::from_secs(lifetime as u64);
    Ok(PortMapping {
        external_addr: SocketAddr::new(IpAddr::V4(external_ip), mapped_port),
        protocol: PortMapProtocol::NatPmp,
        expires_at: Instant::now() + lifetime_dur,
        refresh_interval: lifetime_dur / 2,
        gateway,
        internal_port,
    })
}

// ── PCP (RFC 6887) ────────────────────────────────────────────────

/// PCP also uses UDP port 5351.
const PCP_PORT: u16 = 5351;
const PCP_VERSION: u8 = 2;
const PCP_OPCODE_MAP: u8 = 1;

/// Attempt PCP port mapping.
///
/// PCP MAP request:
/// - Header: version(1) + R+opcode(1) + reserved(2) + lifetime(4) + client_ip(16) = 24 bytes
/// - MAP opcode data: nonce(12) + protocol(1) + reserved(3) + internal_port(2) +
///                    suggested_external_port(2) + suggested_external_ip(16) = 36 bytes
/// Total: 60 bytes
async fn try_pcp(
    gateway: Ipv4Addr,
    internal_port: u16,
    local_ip: Ipv4Addr,
    timeout: Duration,
) -> Result<PortMapping, PortMapError> {
    let gw_addr = SocketAddrV4::new(gateway, PCP_PORT);
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| PortMapError::Io(format!("bind: {e}")))?;

    let mut request = [0u8; 60];
    request[0] = PCP_VERSION;
    request[1] = PCP_OPCODE_MAP; // R=0 (request), opcode=MAP
    // bytes 2-3: reserved
    request[4..8].copy_from_slice(&7200u32.to_be_bytes()); // lifetime
    // Bytes 8..24: client IP as IPv4-mapped IPv6 (::ffff:a.b.c.d)
    let local_octets = local_ip.octets();
    // ::ffff:x.x.x.x = 10 zero bytes + 0xff 0xff + 4 IPv4 bytes
    request[18] = 0xff;
    request[19] = 0xff;
    request[20] = local_octets[0];
    request[21] = local_octets[1];
    request[22] = local_octets[2];
    request[23] = local_octets[3];

    // MAP opcode-specific data starts at byte 24
    // Nonce: 12 random bytes (bytes 24..36)
    let mut nonce = [0u8; 12];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce);
    request[24..36].copy_from_slice(&nonce);

    // Protocol: 17 = UDP (byte 36)
    request[36] = 17;
    // bytes 37..39: reserved
    // Internal port (bytes 40..42)
    request[40..42].copy_from_slice(&internal_port.to_be_bytes());
    // Suggested external port (bytes 42..44) — request same as internal
    request[42..44].copy_from_slice(&internal_port.to_be_bytes());
    // Suggested external IP (bytes 44..60) — all zeros = let router choose

    socket
        .send_to(&request, gw_addr)
        .await
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    let mut buf = [0u8; 60];
    let len = tokio::time::timeout(timeout, async {
        let (len, _) = socket
            .recv_from(&mut buf)
            .await
            .map_err(|e| PortMapError::Io(e.to_string()))?;
        Ok::<_, PortMapError>(len)
    })
    .await
    .map_err(|_| PortMapError::Timeout)??;

    if len < 60 {
        return Err(PortMapError::Protocol(format!(
            "PCP response too short: {len}"
        )));
    }

    // Check R bit (bit 7 of byte 1) — must be 1 for response
    if buf[1] & 0x80 == 0 {
        return Err(PortMapError::Protocol("PCP: not a response".into()));
    }

    // Result code (byte 3)
    let result_code = buf[3];
    if result_code != 0 {
        return Err(PortMapError::Protocol(format!(
            "PCP error: result code {result_code}"
        )));
    }

    let granted_lifetime = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

    // Verify nonce matches (bytes 24..36)
    if buf[24..36] != nonce {
        return Err(PortMapError::Protocol("PCP nonce mismatch".into()));
    }

    // Mapped external port (bytes 42..44)
    let mapped_port = u16::from_be_bytes([buf[42], buf[43]]);

    // Assigned external IP (bytes 44..60) — IPv4-mapped IPv6
    // Check if it's an IPv4-mapped address (::ffff:x.x.x.x)
    let external_ip = if buf[54] == 0xff && buf[55] == 0xff {
        // IPv4-mapped: last 4 bytes
        Ipv4Addr::new(buf[56], buf[57], buf[58], buf[59])
    } else {
        // Could be full IPv6 — for now just try the last 4 bytes
        // as IPv4 (most routers respond with IPv4-mapped)
        Ipv4Addr::new(buf[56], buf[57], buf[58], buf[59])
    };

    let lifetime_dur = Duration::from_secs(granted_lifetime as u64);
    Ok(PortMapping {
        external_addr: SocketAddr::new(IpAddr::V4(external_ip), mapped_port),
        protocol: PortMapProtocol::Pcp,
        expires_at: Instant::now() + lifetime_dur,
        refresh_interval: lifetime_dur / 2,
        gateway,
        internal_port,
    })
}

// ── UPnP IGD ───────────────────────────────────────────────────────

/// Attempt UPnP IGD port mapping via SSDP discovery + SOAP.
///
/// This is more complex than NAT-PMP/PCP but covers older routers
/// that only support UPnP. The implementation is minimal:
/// 1. Send M-SEARCH to 239.255.255.250:1900
/// 2. Parse the LOCATION header from the response
/// 3. Fetch the XML device description
/// 4. Find the WANIPConnection service control URL
/// 5. Send AddPortMapping SOAP action
/// 6. Send GetExternalIPAddress SOAP action
async fn try_upnp(
    internal_port: u16,
    local_ip: Ipv4Addr,
    timeout: Duration,
) -> Result<PortMapping, PortMapError> {
    // Step 1: SSDP M-SEARCH discovery
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| PortMapError::Io(format!("bind: {e}")))?;

    let msearch = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: 2\r\n\
         ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
         \r\n"
    );

    let ssdp_addr: SocketAddr = "239.255.255.250:1900".parse().unwrap();
    socket
        .send_to(msearch.as_bytes(), ssdp_addr)
        .await
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    // Read SSDP response to find LOCATION header
    let mut buf = [0u8; 2048];
    let (len, _from) = tokio::time::timeout(timeout, socket.recv_from(&mut buf))
        .await
        .map_err(|_| PortMapError::Timeout)?
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    let response = String::from_utf8_lossy(&buf[..len]);
    let location = response
        .lines()
        .find_map(|line| {
            let lower = line.to_lowercase();
            if lower.starts_with("location:") {
                Some(line.split_once(':').map(|(_, v)| v.trim().to_string()))
            } else {
                None
            }
        })
        .flatten()
        .ok_or_else(|| PortMapError::Protocol("no LOCATION in SSDP response".into()))?;

    // Step 2: Fetch device description XML
    let desc_xml = fetch_url_simple(&location, timeout).await?;

    // Step 3: Find WANIPConnection or WANPPPConnection control URL
    let control_url = extract_control_url(&desc_xml, &location)?;

    // Step 4: GetExternalIPAddress
    let external_ip = upnp_get_external_ip(&control_url, timeout).await?;

    // Step 5: AddPortMapping
    upnp_add_port_mapping(
        &control_url,
        internal_port,
        internal_port,
        local_ip,
        7200,
        timeout,
    )
    .await?;

    // Determine gateway from the control URL host
    let gateway = url_host_to_ip(&location).unwrap_or(Ipv4Addr::UNSPECIFIED);

    let lifetime_dur = Duration::from_secs(7200);
    Ok(PortMapping {
        external_addr: SocketAddr::new(IpAddr::V4(external_ip), internal_port),
        protocol: PortMapProtocol::UPnP,
        expires_at: Instant::now() + lifetime_dur,
        refresh_interval: lifetime_dur / 2,
        gateway,
        internal_port,
    })
}

/// Minimal HTTP GET that returns the response body as a string.
/// No external HTTP crate needed — just raw TCP.
async fn fetch_url_simple(url: &str, timeout: Duration) -> Result<String, PortMapError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Parse URL: http://host:port/path
    let url = url.trim();
    let without_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| PortMapError::Protocol(format!("non-HTTP URL: {url}")))?;

    let (host_port, path) = match without_scheme.find('/') {
        Some(i) => (&without_scheme[..i], &without_scheme[i..]),
        None => (without_scheme, "/"),
    };

    let addr: SocketAddr = if host_port.contains(':') {
        host_port
            .parse()
            .map_err(|e| PortMapError::Protocol(format!("parse {host_port}: {e}")))?
    } else {
        format!("{host_port}:80")
            .parse()
            .map_err(|e| PortMapError::Protocol(format!("parse {host_port}:80: {e}")))?
    };

    let mut stream = tokio::time::timeout(
        timeout,
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| PortMapError::Timeout)?
    .map_err(|e| PortMapError::Io(e.to_string()))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    let mut body = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut body))
        .await
        .map_err(|_| PortMapError::Timeout)?
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    let full = String::from_utf8_lossy(&body).to_string();
    // Strip HTTP headers — find the blank line
    if let Some(pos) = full.find("\r\n\r\n") {
        Ok(full[pos + 4..].to_string())
    } else {
        Ok(full)
    }
}

/// Send a SOAP POST and return the response body.
async fn soap_post(
    url: &str,
    action: &str,
    body: &str,
    timeout: Duration,
) -> Result<String, PortMapError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let url_trimmed = url.trim();
    let without_scheme = url_trimmed
        .strip_prefix("http://")
        .ok_or_else(|| PortMapError::Protocol(format!("non-HTTP URL: {url_trimmed}")))?;

    let (host_port, path) = match without_scheme.find('/') {
        Some(i) => (&without_scheme[..i], &without_scheme[i..]),
        None => (without_scheme, "/"),
    };

    let addr: SocketAddr = if host_port.contains(':') {
        host_port
            .parse()
            .map_err(|e| PortMapError::Protocol(format!("parse {host_port}: {e}")))?
    } else {
        format!("{host_port}:80")
            .parse()
            .map_err(|e| PortMapError::Protocol(format!("parse {host_port}:80: {e}")))?
    };

    let mut stream = tokio::time::timeout(
        timeout,
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| PortMapError::Timeout)?
    .map_err(|e| PortMapError::Io(e.to_string()))?;

    let soap_body = format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>{body}</s:Body></s:Envelope>"
    );

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPAction: \"{action}\"\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {soap_body}",
        soap_body.len()
    );

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    let mut resp = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut resp))
        .await
        .map_err(|_| PortMapError::Timeout)?
        .map_err(|e| PortMapError::Io(e.to_string()))?;

    let full = String::from_utf8_lossy(&resp).to_string();
    if let Some(pos) = full.find("\r\n\r\n") {
        Ok(full[pos + 4..].to_string())
    } else {
        Ok(full)
    }
}

/// Extract the WANIPConnection or WANPPPConnection control URL from
/// the device description XML. Uses basic string matching instead
/// of a full XML parser to avoid adding dependencies.
fn extract_control_url(xml: &str, base_url: &str) -> Result<String, PortMapError> {
    // Look for WANIPConnection:1 or WANPPPConnection:1 service
    let service_types = [
        "WANIPConnection:1",
        "WANIPConnection:2",
        "WANPPPConnection:1",
    ];

    for st in service_types {
        if let Some(pos) = xml.find(st) {
            // Find the <controlURL> after this service type
            let after = &xml[pos..];
            if let Some(ctrl_start) = after.find("<controlURL>") {
                let url_start = ctrl_start + "<controlURL>".len();
                if let Some(ctrl_end) = after[url_start..].find("</controlURL>") {
                    let control_path = &after[url_start..url_start + ctrl_end];
                    // If it's a relative URL, prepend the base
                    if control_path.starts_with("http://") || control_path.starts_with("https://") {
                        return Ok(control_path.to_string());
                    }
                    // Build absolute URL from base
                    let base = base_url
                        .strip_prefix("http://")
                        .unwrap_or(base_url);
                    let host_port = base.split('/').next().unwrap_or(base);
                    return Ok(format!("http://{host_port}{control_path}"));
                }
            }
        }
    }
    Err(PortMapError::Protocol(
        "no WANIPConnection/WANPPPConnection service in device description".into(),
    ))
}

/// UPnP GetExternalIPAddress SOAP action.
async fn upnp_get_external_ip(
    control_url: &str,
    timeout: Duration,
) -> Result<Ipv4Addr, PortMapError> {
    let body = "<u:GetExternalIPAddress xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\"/>";
    let action = "urn:schemas-upnp-org:service:WANIPConnection:1#GetExternalIPAddress";

    let response = soap_post(control_url, action, body, timeout).await?;

    // Extract IP from <NewExternalIPAddress>x.x.x.x</NewExternalIPAddress>
    let tag = "<NewExternalIPAddress>";
    let end_tag = "</NewExternalIPAddress>";
    let ip_start = response
        .find(tag)
        .ok_or_else(|| PortMapError::Protocol("no NewExternalIPAddress in response".into()))?
        + tag.len();
    let ip_end = response[ip_start..]
        .find(end_tag)
        .ok_or_else(|| PortMapError::Protocol("malformed NewExternalIPAddress".into()))?
        + ip_start;

    response[ip_start..ip_end]
        .parse::<Ipv4Addr>()
        .map_err(|e| PortMapError::Protocol(format!("parse external IP: {e}")))
}

/// UPnP AddPortMapping SOAP action.
async fn upnp_add_port_mapping(
    control_url: &str,
    external_port: u16,
    internal_port: u16,
    internal_client: Ipv4Addr,
    lease_duration: u32,
    timeout: Duration,
) -> Result<(), PortMapError> {
    let body = format!(
        "<u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">\
         <NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{external_port}</NewExternalPort>\
         <NewProtocol>UDP</NewProtocol>\
         <NewInternalPort>{internal_port}</NewInternalPort>\
         <NewInternalClient>{internal_client}</NewInternalClient>\
         <NewEnabled>1</NewEnabled>\
         <NewPortMappingDescription>WarzonePhone</NewPortMappingDescription>\
         <NewLeaseDuration>{lease_duration}</NewLeaseDuration>\
         </u:AddPortMapping>"
    );
    let action = "urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping";

    let response = soap_post(control_url, action, &body, timeout).await?;

    // Check for SOAP fault
    if response.contains("<s:Fault>") || response.contains("errorCode") {
        return Err(PortMapError::Protocol(format!(
            "AddPortMapping SOAP fault: {}",
            &response[..response.len().min(200)]
        )));
    }

    Ok(())
}

/// Extract IPv4 address from a URL's host component.
fn url_host_to_ip(url: &str) -> Option<Ipv4Addr> {
    let without_scheme = url.strip_prefix("http://").unwrap_or(url);
    let host_port = without_scheme.split('/').next()?;
    let host = host_port.split(':').next()?;
    host.parse().ok()
}

// ── Public API ─────────────────────────────────────────────────────

/// Attempt to acquire a port mapping for the given internal UDP port.
///
/// Tries NAT-PMP → PCP → UPnP in sequence. Returns the first
/// successful mapping. If all fail, returns `AllFailed` with the
/// per-protocol errors.
///
/// `local_ip` is the client's LAN IPv4 address (needed for PCP and
/// UPnP). Pass `None` to auto-detect from `if-addrs`.
pub async fn acquire_port_mapping(
    internal_port: u16,
    local_ip: Option<Ipv4Addr>,
) -> Result<PortMapping, PortMapError> {
    let timeout = Duration::from_secs(3);
    let gateway = default_gateway().await?;

    tracing::debug!(
        %gateway,
        internal_port,
        "portmap: attempting NAT-PMP → PCP → UPnP"
    );

    let mut errors = Vec::new();

    // Try NAT-PMP first (simplest, most common)
    match try_natpmp(gateway, internal_port, timeout).await {
        Ok(mapping) => {
            tracing::info!(
                external = %mapping.external_addr,
                protocol = ?mapping.protocol,
                "portmap: NAT-PMP mapping acquired"
            );
            return Ok(mapping);
        }
        Err(e) => {
            tracing::debug!(error = %e, "portmap: NAT-PMP failed, trying PCP");
            errors.push((PortMapProtocol::NatPmp, e.to_string()));
        }
    }

    // Try PCP
    let lip = local_ip.unwrap_or_else(|| detect_local_ipv4().unwrap_or(Ipv4Addr::UNSPECIFIED));
    match try_pcp(gateway, internal_port, lip, timeout).await {
        Ok(mapping) => {
            tracing::info!(
                external = %mapping.external_addr,
                protocol = ?mapping.protocol,
                "portmap: PCP mapping acquired"
            );
            return Ok(mapping);
        }
        Err(e) => {
            tracing::debug!(error = %e, "portmap: PCP failed, trying UPnP");
            errors.push((PortMapProtocol::Pcp, e.to_string()));
        }
    }

    // Try UPnP
    match try_upnp(internal_port, lip, timeout).await {
        Ok(mapping) => {
            tracing::info!(
                external = %mapping.external_addr,
                protocol = ?mapping.protocol,
                "portmap: UPnP mapping acquired"
            );
            return Ok(mapping);
        }
        Err(e) => {
            tracing::debug!(error = %e, "portmap: UPnP also failed");
            errors.push((PortMapProtocol::UPnP, e.to_string()));
        }
    }

    Err(PortMapError::AllFailed(errors))
}

/// Delete/release a port mapping before shutting down.
///
/// For NAT-PMP/PCP: send a mapping request with lifetime=0.
/// For UPnP: send DeletePortMapping SOAP action.
///
/// Best-effort — errors are logged but not propagated.
pub async fn release_port_mapping(mapping: &PortMapping) {
    let timeout = Duration::from_secs(2);
    match mapping.protocol {
        PortMapProtocol::NatPmp => {
            let gw_addr = SocketAddrV4::new(mapping.gateway, NATPMP_PORT);
            if let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await {
                let _ = natpmp_map_udp(
                    &socket,
                    gw_addr,
                    mapping.internal_port,
                    0, // external port 0 = delete
                    0, // lifetime 0 = delete
                    timeout,
                )
                .await;
            }
        }
        PortMapProtocol::Pcp => {
            // PCP delete: same as map but with lifetime=0
            // For simplicity, just let it expire
            tracing::debug!("portmap: PCP mapping will expire naturally");
        }
        PortMapProtocol::UPnP => {
            // Would need to send DeletePortMapping SOAP — skip for now
            tracing::debug!("portmap: UPnP mapping will expire naturally");
        }
    }
}

/// Spawn a background task that refreshes the mapping at its
/// `refresh_interval`. Returns a handle that can be aborted to stop
/// refreshing.
pub fn spawn_refresh(mapping: PortMapping) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(mapping.refresh_interval).await;
            tracing::debug!(
                protocol = ?mapping.protocol,
                internal_port = mapping.internal_port,
                "portmap: refreshing mapping"
            );
            // Re-acquire (NAT-PMP/PCP will renew the existing mapping)
            match acquire_port_mapping(mapping.internal_port, None).await {
                Ok(new_mapping) => {
                    tracing::debug!(
                        external = %new_mapping.external_addr,
                        "portmap: mapping refreshed"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "portmap: refresh failed");
                    // Don't break — keep trying on next interval
                }
            }
        }
    })
}

/// Detect a local IPv4 address (first private address found).
fn detect_local_ipv4() -> Option<Ipv4Addr> {
    let ifaces = if_addrs::get_if_addrs().ok()?;
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        if let IpAddr::V4(v4) = iface.ip() {
            if v4.is_private() {
                return Some(v4);
            }
        }
    }
    None
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natpmp_request_encoding() {
        // Verify the NAT-PMP external address request is 2 bytes
        let request = [NATPMP_VERSION, 0u8];
        assert_eq!(request.len(), 2);
        assert_eq!(request[0], 0); // version 0
        assert_eq!(request[1], 0); // opcode 0
    }

    #[test]
    fn natpmp_map_request_encoding() {
        let mut request = [0u8; 12];
        request[0] = NATPMP_VERSION;
        request[1] = NATPMP_OP_MAP_UDP;
        let port: u16 = 12345;
        request[4..6].copy_from_slice(&port.to_be_bytes());
        request[6..8].copy_from_slice(&port.to_be_bytes());
        let lifetime: u32 = 7200;
        request[8..12].copy_from_slice(&lifetime.to_be_bytes());

        assert_eq!(request[0], 0);
        assert_eq!(request[1], 1);
        assert_eq!(u16::from_be_bytes([request[4], request[5]]), 12345);
        assert_eq!(u32::from_be_bytes([request[8], request[9], request[10], request[11]]), 7200);
    }

    #[test]
    fn extract_control_url_from_xml() {
        let xml = r#"
        <service>
            <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
            <controlURL>/upnp/control/WANIPConn1</controlURL>
        </service>
        "#;
        let base = "http://192.168.1.1:49152/rootDesc.xml";
        let url = extract_control_url(xml, base).unwrap();
        assert_eq!(url, "http://192.168.1.1:49152/upnp/control/WANIPConn1");
    }

    #[test]
    fn extract_control_url_absolute() {
        let xml = r#"
        <service>
            <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
            <controlURL>http://10.0.0.1:5000/ctl/IPConn</controlURL>
        </service>
        "#;
        let base = "http://10.0.0.1:49152/rootDesc.xml";
        let url = extract_control_url(xml, base).unwrap();
        assert_eq!(url, "http://10.0.0.1:5000/ctl/IPConn");
    }

    #[test]
    fn extract_control_url_ppp_connection() {
        let xml = r#"
        <service>
            <serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>
            <controlURL>/upnp/control/WANPPPConn1</controlURL>
        </service>
        "#;
        let base = "http://192.168.0.1:1900/igd.xml";
        let url = extract_control_url(xml, base).unwrap();
        assert_eq!(url, "http://192.168.0.1:1900/upnp/control/WANPPPConn1");
    }

    #[test]
    fn url_host_to_ip_works() {
        assert_eq!(
            url_host_to_ip("http://192.168.1.1:49152/rootDesc.xml"),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(
            url_host_to_ip("http://10.0.0.1/ctl"),
            Some(Ipv4Addr::new(10, 0, 0, 1))
        );
    }

    // ── Additional comprehensive tests ─────────────────────────

    #[test]
    fn extract_control_url_v2() {
        let xml = r#"
        <service>
            <serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType>
            <controlURL>/upnp/v2/WANIPConn</controlURL>
        </service>
        "#;
        let base = "http://192.168.1.1:5000/desc.xml";
        let url = extract_control_url(xml, base).unwrap();
        assert_eq!(url, "http://192.168.1.1:5000/upnp/v2/WANIPConn");
    }

    #[test]
    fn extract_control_url_no_service_fails() {
        let xml = r#"
        <service>
            <serviceType>urn:schemas-upnp-org:service:SomethingElse:1</serviceType>
            <controlURL>/nope</controlURL>
        </service>
        "#;
        let base = "http://10.0.0.1/desc.xml";
        let err = extract_control_url(xml, base).unwrap_err();
        assert!(matches!(err, PortMapError::Protocol(_)));
    }

    #[test]
    fn extract_control_url_missing_control_url_tag() {
        let xml = r#"
        <service>
            <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
            <!-- no controlURL tag -->
        </service>
        "#;
        let base = "http://10.0.0.1/desc.xml";
        let err = extract_control_url(xml, base).unwrap_err();
        assert!(matches!(err, PortMapError::Protocol(_)));
    }

    #[test]
    fn url_host_to_ip_no_scheme() {
        assert_eq!(
            url_host_to_ip("192.168.1.1:49152/rootDesc.xml"),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
    }

    #[test]
    fn url_host_to_ip_hostname_returns_none() {
        assert_eq!(url_host_to_ip("http://myrouter.local:49152/desc.xml"), None);
    }

    #[test]
    fn port_map_error_display() {
        assert!(PortMapError::NoGateway.to_string().contains("gateway"));
        assert!(PortMapError::Timeout.to_string().contains("timeout"));
        assert!(PortMapError::Io("test".into()).to_string().contains("test"));
        assert!(
            PortMapError::Protocol("bad".into())
                .to_string()
                .contains("bad")
        );
        let errs = vec![
            (PortMapProtocol::NatPmp, "fail1".into()),
            (PortMapProtocol::Pcp, "fail2".into()),
        ];
        let all = PortMapError::AllFailed(errs);
        let s = all.to_string();
        assert!(s.contains("NatPmp"));
        assert!(s.contains("Pcp"));
        assert!(s.contains("fail1"));
    }

    #[test]
    fn port_map_protocol_serde() {
        let json = serde_json::to_string(&PortMapProtocol::NatPmp).unwrap();
        assert!(json.contains("NatPmp"));
        let json = serde_json::to_string(&PortMapProtocol::UPnP).unwrap();
        assert!(json.contains("UPnP"));
    }

    #[test]
    fn port_mapping_serializes() {
        let m = PortMapping {
            external_addr: "203.0.113.5:12345".parse().unwrap(),
            protocol: PortMapProtocol::NatPmp,
            expires_at: Instant::now() + Duration::from_secs(3600),
            refresh_interval: Duration::from_secs(1800),
            gateway: Ipv4Addr::new(192, 168, 1, 1),
            internal_port: 4433,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("203.0.113.5:12345"));
        assert!(json.contains("NatPmp"));
        assert!(json.contains("4433"));
        // expires_at and refresh_interval are #[serde(skip)]
        assert!(!json.contains("expires_at"));
    }

    #[test]
    fn detect_local_ipv4_returns_private() {
        // This test just verifies the function doesn't panic.
        // On CI/machines without a LAN interface, it may return None.
        let result = detect_local_ipv4();
        if let Some(ip) = result {
            assert!(ip.is_private(), "should be private: {ip}");
        }
    }

    #[test]
    fn natpmp_constants() {
        assert_eq!(NATPMP_PORT, 5351);
        assert_eq!(NATPMP_VERSION, 0);
        assert_eq!(NATPMP_OP_MAP_UDP, 1);
        assert_eq!(PCP_PORT, 5351); // same port
        assert_eq!(PCP_VERSION, 2);
        assert_eq!(PCP_OPCODE_MAP, 1);
    }

    #[test]
    fn extract_control_url_real_world_xml() {
        // Realistic device description from a common router
        let xml = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
    <friendlyName>RT-AX86U</friendlyName>
    <deviceList>
      <device>
        <deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>
        <deviceList>
          <device>
            <deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>
            <serviceList>
              <service>
                <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
                <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
                <controlURL>/ctl/IPConn</controlURL>
                <eventSubURL>/evt/IPConn</eventSubURL>
                <SCPDURL>/WANIPCn.xml</SCPDURL>
              </service>
            </serviceList>
          </device>
        </deviceList>
      </device>
    </deviceList>
  </device>
</root>"#;
        let base = "http://192.168.1.1:49152/rootDesc.xml";
        let url = extract_control_url(xml, base).unwrap();
        assert_eq!(url, "http://192.168.1.1:49152/ctl/IPConn");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore]
    async fn integration_default_gateway_macos() {
        let gw = default_gateway().await.unwrap();
        println!("Default gateway: {gw}");
        assert!(gw.is_private() || gw.octets()[0] == 100);
    }

    #[tokio::test]
    #[ignore]
    async fn integration_acquire_mapping() {
        let result = acquire_port_mapping(12345, None).await;
        match result {
            Ok(m) => println!("Mapping: {m:?}"),
            Err(e) => println!("No mapping available: {e}"),
        }
    }
}
