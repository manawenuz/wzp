//! # wzp-iroh
//!
//! Identity, contacts, presence and signaling for WarzonePhone on top of
//! [iroh](https://iroh.computer) 1.x. Replaces the former featherChat
//! integration. File bytes do **not** travel here: [`Envelope::FileInvite`]
//! only carries an opaque invite (relay address / token / candidates) that
//! the app hands to `wzp-files`.
//!
//! See `docs/IROH.md` for the design.
//!
//! ```no_run
//! # async fn demo() -> wzp_iroh::Result<()> {
//! use wzp_iroh::{Envelope, IrohConfig, IrohNode};
//! let node = IrohNode::start(&[0u8; 32], IrohConfig::with_data_dir("/tmp/wzp")).await?;
//! println!("share me: {}", node.contact_string("alice"));
//! let bob = node.add_contact("wzp1...").await?;
//! node.send(bob.id, Envelope::text("hi")).await?;
//! node.shutdown().await;
//! # Ok(()) }
//! ```

pub mod config;
pub mod contact;
pub mod error;
pub mod identity;
pub mod node;
pub mod wire;

#[cfg(test)]
mod two_nodes_tests;

pub use config::{IrohConfig, RelayChoice};
pub use contact::{CONTACT_PREFIX, Contact, ContactBook, ContactCard, Trust};
pub use error::{Error, Result};
pub use node::{IrohNode, NodeEvent, PresenceState};
pub use wire::{Envelope, PRESENCE_ALPN, SIGNAL_ALPN};

/// Re-exported so callers don't need a direct `iroh` dependency.
pub use iroh::{EndpointId, RelayUrl};
