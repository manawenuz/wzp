# wzp-iroh: identity, contacts, presence and signaling over iroh

`crates/wzp-iroh` replaces what featherChat used to do for WarzonePhone
(identity, contact list, presence, call/file signaling and rendezvous) with
[iroh](https://iroh.computer) **1.4.x** (`iroh = "1.4.1"`, MIT/Apache-2.0).
File and media bytes do **not** go over iroh: iroh only carries small
envelopes, e.g. a `FileInvite` that tells the peer which WZP relay / session
token / direct candidates to use with `wzp-files`.

```
 app (Tauri) ──► IrohNode ──► iroh Endpoint (noq QUIC, TLS 1.3 raw public keys)
                   │  ├─ ALPN wzp/signal/1    Envelope ⇄ Reply
                   │  └─ ALPN wzp/presence/1  Ping/Pong/Bye
                   ├─ ContactBook (contacts.json)
                   └─ events: Message | Presence | ContactRequest
```

## Versions and platforms

| item | value |
|---|---|
| iroh / iroh-base / iroh-relay | 1.4.1 |
| iroh-mdns-address-lookup (n0, official) | 0.6.1 |
| QUIC stack | `noq` 1.4 (n0's quinn fork), separate crate from the workspace's `quinn` 0.11 |
| MSRV | **1.91** (iroh 1.x). The workspace `rust-version` was raised from 1.85 to 1.91. `wzp-proto` keeps its own 1.85 (it does not depend on iroh). |

`noq`/`noq-proto`/`noq-udp` and `quinn`/`quinn-proto`/`quinn-udp` are
different crate names and link side by side; both use the same `rustls`
0.23 and `ring` 0.17. iroh's default features are kept (`tls-ring`,
`portmapper`, `metrics`, `fast-apple-datapath`).

Native (C) code in the `wzp-iroh` tree: only `ring` (built with `cc`), which
the workspace already needs. No OpenSSL, no aws-lc, no cmake, no bindgen.
Per target:

* **Android (aarch64-linux-android)**: `ring` needs the NDK clang (same
  as for the rest of the app). netwatch/netdev use netlink (pure Rust).
  `n0-dns-resolver` reads the system DNS servers over JNI through
  `ndk-context`; if the context isn't initialized it catches the panic
  and falls back to public resolvers. Tauri's `tao` normally initializes
  `ndk-context`; if not, call `iroh::dns::install_android_jni_context` from
  `JNI_OnLoad`. Not checked with `cargo check --target aarch64-linux-android`
  here: no NDK in the build sandbox, and `ring`'s build script needs one.
* **Windows**: `windows`/`windows-sys` crates (pure Rust bindings) plus `ring`.
* **macOS**: `objc2`, `core-foundation`, `security-framework`,
  `system-configuration` (system frameworks, no extra installs) plus `ring`.

## Identity

The iroh `SecretKey` **is** the WZP Ed25519 identity key:

```
seed (32 B, BIP39 24 words)
  └─ HKDF-SHA256(seed, info="warzone-ed25519")  ── WZP identity signing key
        └─ iroh::SecretKey::from_bytes(..)       ── EndpointId == WZP identity_pub
```

So `EndpointId` == `identity_pub` used in `SignalMessage::CallOffer`, and the
WZP fingerprint `SHA-256(pubkey)[..16]` (`xxxx:xxxx:…`, 8 groups) can be
computed from any `EndpointId` (`wzp_iroh::identity::fingerprint`). The tests
check this against `wzp_crypto::Seed::derive_identity()`.

**Why no separate HKDF label.** A separate key would need a signed binding
(WZP key signs the iroh key) that every peer must check, and two fingerprints.
Reusing the key is safe because the three things it signs can't be confused
with each other:

* iroh TLS 1.3 `CertificateVerify`: 64×`0x20` + context string + hash (130 B);
* pkarr/BEP44 DNS records: `3:seqi…e1:v…` bencoded, contents chosen by the
  node itself;
* WZP handshake: `ephemeral_pub (32 B) || "call-offer" | "call-answer"`.

The trade-off is linkability. If DNS discovery is on, the identity key is
published to `dns.iroh.link` together with the current relay URL and
addresses (see Threat notes).

## Contact string

`wzp1` + lower-case RFC 4648 base32 (no padding) of:

```
u8      version = 1
[32]    endpoint id
u8+N    display name (UTF-8, ≤ 64 B)
u8+N    relay URL (UTF-8, ≤ 200 B, 0 = none)
u8 n    direct addrs (≤ 8): u8 family 4|6, IP bytes, u16 BE port
[4]     SHA-256("wzp-contact-v1" || above)[..4]   (catches typos)
```

Parsing ignores case and surrounding whitespace. `ContactCard::to_qr_string()`
returns the upper-case form, which uses only `A–Z2–7`, so QR alphanumeric
mode applies. A typical string with a relay URL is about 120 characters.

`contact_string()` puts in the home relay URL when one is known. It adds
direct IP addresses only when relays are disabled or no relay is connected
yet, so shared strings don't leak LAN addresses when they aren't needed.
`add_contact` also takes a bare endpoint id (64 hex or z-base-32). That works
when DNS discovery or mDNS can find the peer.

### Contact book and trust

`ContactBook` is stored at `<data_dir>/contacts.json` and written atomically
(temp file + rename). Each `Contact` has `id`, `name`, `fingerprint`,
`relay_url`, `direct_addrs`, `trust`, `added_at` and `last_seen`.

* **TOFU**: a new contact is pinned as `Trust::Tofu`. Because the id *is* the
  public key, a pinned contact can't be swapped for another key. An attacker
  can only give you a *different* contact string. Show the fingerprint and
  let users compare it out of band, then call `verify_contact(id)` to set
  `Trust::Verified`.
* Adding the same id again refreshes the name and addresses but keeps the
  trust level.

## ALPNs and framing

Every frame is a 4-byte big-endian length followed by JSON, the same framing
as `wzp-transport::reliable`. The limit is 1 MiB, or 4 KiB for frames from
unknown peers. Each request uses its own bidi stream on a cached connection.

### `wzp/signal/1`

Request `Envelope`, response `Reply`:

```rust
#[serde(tag = "type", content = "body")]
enum Envelope {
    Signal(wzp_proto::SignalMessage),      // call signaling
    FileInvite(serde_json::Value),         // wzp-files FileSessionInvite, opaque here
    Text { text: String, sent_at_ms: u64 },
    Hello { name: String, contact: String },  // introduce yourself
    Ping,                                  // acked, not surfaced
}
#[serde(tag = "type")] enum Reply { Ack, Rejected { reason: String } }
```

`send()` resolves only after the peer's `Ack`. Peers that aren't contacts:

* The receiver reads at most one 4 KiB frame, emits
  `NodeEvent::ContactRequest { id, fingerprint, name, contact }`, answers
  `Rejected { "not a contact" }` and closes the connection (code 403).
  **Nothing is delivered.** `name`/`contact` are filled only from a `Hello`,
  and `contact` is kept only if its id matches the authenticated TLS peer id.
* Requests are de-duplicated per (peer, has-Hello) for 60 s.
* The user accepts with `add_contact(contact or id)`.

### `wzp/presence/1`

`PresenceMsg::{Ping, Pong, Bye}`. Only contacts get an answer. Strangers are
closed without a reply, so they can't probe whether you are online at the
app level (see Threat notes).

* Each contact is pinged every `presence_interval` (default 30 s) while online.
  After a failure the contact is `Offline` and retried with exponential
  backoff up to `presence_max_backoff` (default 5 min).
* Any incoming ping or envelope from a contact also marks them `Online`.
* `shutdown()` sends `Bye` to online contacts, so they switch to `Offline`
  right away instead of waiting for a ping to fail.
* State changes are emitted as `NodeEvent::Presence { id, state, last_seen }`.

**Why not iroh-gossip (0.102, still 0.x)?** Gossip broadcasts on topic
swarms. Presence here is a private, pairwise relation, so every contact pair
would need its own topic, or everyone would share one topic and see each
other's contact graph. Gossip would also add a second unstable API. Direct
pings cost one QUIC stream per contact per interval on a connection that
stays open anyway, and they only answer to contacts. Revisit gossip if
group rooms or very large contact lists need it.

## Configuration (`IrohConfig`)

| field | default | notes |
|---|---|---|
| `data_dir` | `None` (in-memory) | `contacts.json` lives here |
| `relay` | `RelayChoice::N0` | `N0` (n0 public relays), `Custom(vec![url..])`, `Disabled` |
| `relay_auth_token` | `None` | bearer token for a self-hosted relay with `access.shared_token` |
| `local_discovery` | `true` | mDNS (`iroh-mdns-address-lookup`, service `wzp-iroh`) |
| `dns_discovery` | `true` | pkarr publish + DNS resolve via `dns.iroh.link` |
| `bind_addr` | `None` (0.0.0.0:0 + [::]:0) | e.g. a fixed port for port forwarding |
| `presence_interval` / `presence_max_backoff` | 30 s / 300 s | |
| `request_timeout` | 15 s | per connect + request/response |

### Self-hosting an iroh-relay

```sh
cargo install iroh-relay --features server --locked   # same 1.4.x line
cat > relay.toml <<'EOF'
enable_relay = true
http_bind_addr = "[::]:80"
enable_quic_addr_discovery = true      # QAD on UDP 7842
access.shared_token = ["change-me"]    # optional: restrict who may use it
[tls]
cert_mode = "LetsEncrypt"               # or "Manual" with cert_dir
hostname = "relay.example.org"
contact = "admin@example.org"
EOF
iroh-relay --config-path relay.toml
```

Open TCP 80/443 and UDP 7842. Then use
`RelayChoice::Custom(vec!["https://relay.example.org".into()])` with
`relay_auth_token: Some("change-me".into())`, or on the CLI
`--relay https://relay.example.org --relay-token change-me`. For local testing
you can run `iroh-relay --dev`, which serves plain HTTP on :3340.

To avoid n0 infrastructure completely, also set `dns_discovery = false`.
Contacts are then reached through the relay URL and addresses in their
contact strings, plus mDNS on the LAN. You could also self-host
`iroh-dns-server` and wire it up through `PkarrPublisher`/`DnsAddressLookup`
(not exposed in `IrohConfig` yet).

## API

```rust
impl IrohNode {
    pub async fn start(seed: &[u8; 32], cfg: IrohConfig) -> Result<Self>;
    pub fn id(&self) -> EndpointId;
    pub fn fingerprint(&self) -> String;
    pub fn endpoint(&self) -> &iroh::Endpoint;
    pub fn contact_card(&self, name: &str) -> ContactCard;
    pub fn contact_string(&self, name: &str) -> String;
    pub async fn add_contact(&self, contact: &str) -> Result<Contact>;
    pub fn contacts(&self) -> Vec<Contact>;
    pub fn presence(&self, id: EndpointId) -> PresenceState;
    pub fn verify_contact(&self, id: EndpointId) -> Result<()>;
    pub async fn remove_contact(&self, id: EndpointId) -> Result<()>;
    pub async fn send(&self, to: EndpointId, env: Envelope) -> Result<()>;
    pub fn hello(&self, name: &str) -> Envelope;
    pub async fn send_hello(&self, to: EndpointId, name: &str) -> Result<()>;
    pub fn events(&self) -> tokio::sync::broadcast::Receiver<NodeEvent>;
    pub async fn shutdown(self);
}
```

`NodeEvent`, `Envelope`, `Contact`, `PresenceState` and `IrohConfig` all
implement `Serialize`, so the Tauri layer can forward them to JS directly.

## CLI demo

```sh
O="--relay disabled --no-dns --no-mdns"     # or leave out for n0 relays
wzp-iroh --dir /tmp/a $O --bind 127.0.0.1:7001 --name alice id   # prints wzp1… (A)
wzp-iroh --dir /tmp/b $O --bind 127.0.0.1:7002 --name bob id     # prints wzp1… (B)
wzp-iroh --dir /tmp/a add-contact <B>
wzp-iroh --dir /tmp/b add-contact <A>
wzp-iroh --dir /tmp/a $O --bind 127.0.0.1:7001 listen &
wzp-iroh --dir /tmp/b $O --bind 127.0.0.1:7002 send-text alice "hello"
wzp-iroh --dir /tmp/c $O --name carol hello <B>      # B sees a contact request
```

The seed is kept in `<dir>/seed.hex` (mode 0600; created on first run). Run
`add-contact` while `listen` is *not* running for that directory: the
running node keeps the book in memory and overwrites the file when it shuts
down.

## Threat notes

* **Transport security**: iroh QUIC/TLS 1.3 with raw public keys. Dialing an
  `EndpointId` authenticates the peer's key, and accepted connections give
  you the authenticated `remote_id()`. Relays only forward encrypted packets
  and never see plaintext envelopes.
* **Metadata**: relays (n0's or yours) see which endpoint ids talk to each
  other, when, and how much, as long as traffic is relayed. With DNS
  discovery, `dns.iroh.link` holds a signed record mapping your identity
  key to your home relay and, when known, your direct addresses. Anyone who
  knows your id can look it up. Turn it off (`dns_discovery = false`) for
  better privacy at the cost of reachability. mDNS announces the endpoint id
  on the LAN.
* **Online probing**: anyone who knows your id can attempt a QUIC handshake
  and learn that the endpoint is up. The presence protocol tells
  non-contacts nothing more, and signaling tells them only `Rejected`.
* **Spam / DoS**: unknown peers get one 4 KiB frame, a 5 s budget and
  de-duplicated `ContactRequest` events. Global connection rate limits are
  left to iroh (an `EndpointHooks`/`incoming_filter` can be added later).
* **Identity reuse**: compromising the seed compromises both WZP calls and
  the iroh identity. They were already tied to the same key in WZP.
* **Contact strings** aren't secret, but they *are* the trust anchor (TOFU).
  Exchange them over a channel you trust, or verify fingerprints.
* **Replay**: envelopes have no signatures or sequence numbers of their own.
  They rely on the QUIC session (replay-protected, authenticated). Envelopes
  that get forwarded or stored should carry their own signatures. For
  example, `SignalMessage::CallOffer` already signs its ephemeral key.
