//! Node configuration.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

/// Which iroh relay servers to use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "urls", rename_all = "snake_case")]
pub enum RelayChoice {
    /// number 0's public production relays (`iroh::RelayMode::Default`).
    #[default]
    N0,
    /// Your own `iroh-relay` server(s), e.g. `https://relay.example.org`.
    Custom(Vec<String>),
    /// No relays: only direct (LAN / public-IP / hole-punch-free) paths.
    Disabled,
}

/// Configuration for [`crate::IrohNode::start`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrohConfig {
    /// Directory for `contacts.json`. `None` keeps contacts in memory only.
    pub data_dir: Option<PathBuf>,
    pub relay: RelayChoice,
    /// Bearer token for a self-hosted `iroh-relay` started with
    /// `access.shared_token = [...]`. Only used with [`RelayChoice::Custom`].
    #[serde(default)]
    pub relay_auth_token: Option<String>,
    /// mDNS discovery of peers on the local network (default: on).
    pub local_discovery: bool,
    /// Publish our addresses to, and resolve peers from, number 0's
    /// DNS/pkarr service (`dns.iroh.link`). Lets contacts dial us by id only
    /// (default: on). Turning it off means peers need a relay URL or direct
    /// address from the contact string, or mDNS.
    pub dns_discovery: bool,
    /// Bind a specific IPv4 or IPv6 socket instead of `0.0.0.0:0` + `[::]:0`.
    pub bind_addr: Option<SocketAddr>,
    /// Ping interval for online contacts; also the initial retry backoff.
    pub presence_interval: Duration,
    /// Maximum retry backoff for offline contacts.
    pub presence_max_backoff: Duration,
    /// Timeout for one connect + request/response exchange.
    pub request_timeout: Duration,
}

impl Default for IrohConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            relay: RelayChoice::N0,
            relay_auth_token: None,
            local_discovery: true,
            dns_discovery: true,
            bind_addr: None,
            presence_interval: Duration::from_secs(30),
            presence_max_backoff: Duration::from_secs(300),
            request_timeout: Duration::from_secs(15),
        }
    }
}

impl IrohConfig {
    /// Defaults with contacts persisted in `dir`.
    pub fn with_data_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: Some(dir.into()),
            ..Self::default()
        }
    }
}
