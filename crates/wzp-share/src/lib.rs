//! # wzp-share — "send a file to a contact"
//!
//! Glues [`wzp_iroh`] (identity, contacts, presence, signaling) and
//! [`wzp_files`] (E2E-encrypted file sessions over WZP) into the app-level
//! flow, so UI layers (Tauri, CLI) stay thin:
//!
//! 1. sender hosts a WZP file session: a direct listener (LAN candidates)
//!    and/or a fresh `file-*` room on the configured WZP relay;
//! 2. sends the [`FileSessionInvite`](wzp_files::FileSessionInvite) as
//!    `Envelope::FileInvite` over iroh;
//! 3. receiver (contacts only) checks the invite's fingerprint against the
//!    authenticated iroh sender, joins (direct first, relay fallback) and
//!    surfaces the offer for consent;
//! 4. bytes flow over WZP; progress arrives as [`ShareEvent`]s.
//!
//! See `docs/SHARE.md`.

mod node;
mod types;

#[cfg(test)]
mod tests;

pub use node::{ShareNode, TransferHandle};
pub use types::{
    Direction, Result, ShareConfig, ShareError, ShareEvent, ShareId, ShareInvite, TransferInfo,
    TransferState,
};
pub use wzp_files::FileConfig;
pub use wzp_iroh::{Contact, EndpointId, IrohConfig, PresenceState, RelayChoice};
