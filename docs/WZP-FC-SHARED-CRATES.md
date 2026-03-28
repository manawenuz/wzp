# Shared Crate Strategy: WZP ↔ featherChat

**Goal:** Both projects import each other's crates directly instead of duplicating code. A change to identity derivation in featherChat automatically applies in WZP, and vice versa for call signaling types.

---

## Current Problem

- `warzone-protocol` uses workspace dependency inheritance (`Cargo.toml` has `ed25519-dalek.workspace = true`). When WZP tries to use it as a path dep, Cargo fails because it can't resolve workspace references from outside the featherChat workspace.
- WZP had to mirror featherChat's `identity.rs`, `mnemonic.rs`, and `Fingerprint` type in `wzp-crypto/src/identity.rs` — duplicate code that can drift.
- featherChat will need `wzp_proto::SignalMessage` for the `WireMessage::CallSignal` variant — another potential duplication.

## Solution: Make Key Crates Standalone-Importable

### What featherChat Needs to Do

#### FC-CRATE-1: Make `warzone-protocol` standalone-publishable

**File:** `warzone/crates/warzone-protocol/Cargo.toml`

Replace all `workspace = true` references with explicit versions:

```toml
# Before:
ed25519-dalek.workspace = true
x25519-dalek.workspace = true

# After:
ed25519-dalek = { version = "2", features = ["serde", "rand_core"] }
x25519-dalek = { version = "2", features = ["serde", "static_secrets"] }
chacha20poly1305 = "0.10"
hkdf = "0.12"
sha2 = "0.10"
rand = "0.8"
bip39 = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
bincode = "1"
thiserror = "2"
hex = "0.4"
base64 = "0.22"
uuid = { version = "1", features = ["v4"] }
zeroize = { version = "1", features = ["derive"] }
chrono = { version = "0.4", features = ["serde"] }
k256 = { version = "0.13", features = ["ecdsa", "serde"] }
tiny-keccak = { version = "2", features = ["keccak"] }
```

**Keep workspace inheritance working too** by using the `[package]` fallback pattern:
```toml
[package]
name = "warzone-protocol"
version = "0.0.20"
edition = "2021"
# Remove version.workspace and edition.workspace — use explicit values
```

This way the crate still works inside the featherChat workspace AND can be imported by WZP as a path dependency.

**Test:** From the WZP repo, this should work:
```toml
# In wzp-crypto/Cargo.toml:
warzone-protocol = { path = "../../deps/featherchat/warzone/crates/warzone-protocol" }
```

**Effort:** 30 minutes. Mechanical replacement, then `cargo build` to verify.

#### FC-CRATE-2: Add `wzp-proto` as a git dependency for `CallSignal`

**File:** `warzone/crates/warzone-protocol/Cargo.toml`

```toml
[dependencies]
# WarzonePhone signaling types (for CallSignal WireMessage variant)
wzp-proto = { git = "ssh://git@git.manko.yoga:222/manawenuz/wz-phone.git", optional = true }

[features]
default = []
wzp = ["wzp-proto"]
```

**File:** `warzone/crates/warzone-protocol/src/message.rs`

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum WireMessage {
    // ... existing variants ...

    /// Voice/video call signaling (requires "wzp" feature).
    #[cfg(feature = "wzp")]
    CallSignal {
        id: String,
        sender_fingerprint: String,
        signal: wzp_proto::SignalMessage,  // Typed, not opaque bytes
    },

    /// Voice/video call signaling (without wzp feature — opaque bytes).
    #[cfg(not(feature = "wzp"))]
    CallSignal {
        id: String,
        sender_fingerprint: String,
        signal: Vec<u8>,  // Opaque JSON bytes
    },
}
```

**Alternative (simpler):** Always use `Vec<u8>` for the signal field and let the consumer deserialize. This avoids the feature flag complexity:

```rust
CallSignal {
    id: String,
    sender_fingerprint: String,
    signal_json: String,  // JSON-serialized wzp_proto::SignalMessage
},
```

featherChat server treats it as opaque. WZP client deserializes it to `SignalMessage`.

**Effort:** 1-2 hours.

#### FC-CRATE-3: Extract shared identity types to a micro-crate (optional, long-term)

Create `warzone-identity` crate containing only:
- `Seed` (generation, from_bytes, from_hex, from_mnemonic, to_mnemonic)
- `IdentityKeyPair` (derive from seed)
- `PublicIdentity` (verifying key, encryption key, fingerprint)
- `Fingerprint` (SHA-256 truncated, display format)
- `hkdf_derive()` helper

Both `warzone-protocol` and `wzp-crypto` depend on `warzone-identity` instead of each implementing their own. This is the cleanest long-term solution but requires more refactoring.

**Crate structure:**
```
warzone-identity/
├── Cargo.toml  (standalone, no workspace inheritance)
├── src/
│   ├── lib.rs
│   ├── seed.rs
│   ├── identity.rs
│   ├── fingerprint.rs
│   └── mnemonic.rs
```

**Dependencies:** ed25519-dalek, x25519-dalek, hkdf, sha2, bip39, hex, zeroize

Both projects import it:
```toml
# featherChat:
warzone-identity = { path = "../warzone-identity" }

# WZP (via submodule):
warzone-identity = { path = "deps/featherchat/warzone-identity" }
```

**Effort:** Half a day. Extract code from warzone-protocol, update imports in both projects.

---

### What WZP Needs to Do (after featherChat completes FC-CRATE-1)

#### WZP-CRATE-1: Replace identity mirror with real dependency

Once `warzone-protocol` is standalone-importable:

**File:** `crates/wzp-crypto/Cargo.toml`
```toml
# Remove bip39 and hex (now comes from warzone-protocol)
# Add:
warzone-protocol = { path = "../../deps/featherchat/warzone/crates/warzone-protocol" }
```

**File:** `crates/wzp-crypto/src/identity.rs`
Replace the entire file with re-exports:
```rust
//! featherChat identity — re-exported from warzone-protocol.
pub use warzone_protocol::identity::{IdentityKeyPair, Seed};
pub use warzone_protocol::types::Fingerprint;
```

**File:** `crates/wzp-crypto/src/handshake.rs`
Use `warzone_protocol::identity::Seed` internally instead of raw HKDF calls.

**Effort:** 1 hour (after FC-CRATE-1 is done).

#### WZP-CRATE-2: Make `wzp-proto` standalone-importable

`wzp-proto` already has explicit dependency versions (not workspace-inherited for external deps). It should work as a git dependency from featherChat. Verify:

```bash
# From a scratch project:
cargo add --git ssh://git@git.manko.yoga:222/manawenuz/wz-phone.git wzp-proto
```

If this fails, replace any remaining workspace references in `wzp-proto/Cargo.toml` with explicit versions.

**Key types featherChat needs from wzp-proto:**
- `SignalMessage` (CallOffer, CallAnswer, IceCandidate, Hangup, etc.)
- `QualityProfile` (for codec negotiation)
- `HangupReason`

**Effort:** 30 minutes to verify and fix.

---

## Recommended Order

1. **FC-CRATE-1** — Make warzone-protocol standalone (30 min, unblocks everything)
2. **WZP-CRATE-2** — Verify wzp-proto works as git dep (30 min)
3. **FC-CRATE-2** — Add CallSignal with opaque signal_json field (1-2 hours)
4. **WZP-CRATE-1** — Replace identity mirror with real dep (1 hour)
5. **FC-CRATE-3** — Extract warzone-identity micro-crate (optional, half day)

After steps 1-4, both projects share types directly:
- WZP imports `warzone-protocol` for identity/seed/fingerprint
- featherChat imports `wzp-proto` (via git) for `SignalMessage` types
- No duplicated code, no drift risk

---

## Dependency Graph After Integration

```
warzone-identity (shared micro-crate, optional step 5)
    ↑                    ↑
warzone-protocol         wzp-crypto
    ↑                    ↑
warzone-server      wzp-proto ← wzp-codec, wzp-fec, wzp-transport
    ↑                    ↑
warzone-client      wzp-client, wzp-relay, wzp-web
```
