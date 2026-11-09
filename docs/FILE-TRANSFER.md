# File transfer over WZP (`wzp-files`)

Status: data path **B (reliable QUIC streams)** implemented, over a relay
and direct. Data path **A (RaptorQ over datagrams)** is a follow-up (see
the end of this document).

Crates touched:

| crate | what |
|---|---|
| `wzp-proto` | `SignalMessage::{FileOffer, FileAccept, FileReject, FileProgress, FileComplete, FileCancel, ChunkNack}`, `sanitize_file_name()` |
| `wzp-files` (new) | sessions, E2E handshake, framing, manifest, sender/receiver state machines, storage |
| `wzp-relay` | `file_relay`: uni-stream splicer for two-party rooms, `MediaType::Data` datagram limiter, File* signal forwarding |
| `wzp-client` | `send-file` / `receive-files` CLI |

## 1. Sessions

A wzp-files session is one QUIC connection (WZP transport config, but with
bulk flow-control windows) plus an end-to-end handshake between the two
file peers. Three ways to get one:

* **Relay**: `connect_via_relay(relay, room_or_token, seed)`. The client
  connects with SNI = room name (`file-<token>`; tokens without a
  `file-`/`call-` prefix get `file-` prepended), runs the usual WZP relay
  handshake (`CallOffer`/`CallAnswer`; the derived hop key is discarded),
  then waits for a `RoomUpdate` with `count >= 2`.
* **Direct**: `listen_direct(bind, seed)` + `DirectListener::accept()` on
  one side, `connect_direct(addr, seed, opts)` on the other.
* **Existing connection**: `Session::over_transport(&QuinnTransport, seed,
  context, relayed, opts)` runs only the E2E handshake, e.g. on the QUIC
  connection of a running direct call (`call-*` room). wzp-files uses only
  *unidirectional* streams, so it never competes with the transport's
  bidirectional signal streams.

`FileSessionInvite` (serde/JSON) is what the sender hands the receiver,
e.g. over iroh:

```json
{"version":1,
 "relay":{"addr":"203.0.113.1:4433","room":"file-6f1c…","auth_token":null},
 "direct":["192.168.1.20:50000"],
 "peer_fingerprint":"3ae1d83f…"}
```

`join_invite()` tries the direct candidates (5 s each) then the relay;
`host_invite()` races an optional `DirectListener` against the relay room.
The joiner always enforces `peer_fingerprint`.

### E2E handshake

Each side opens one uni stream, the **control stream**:

```
"WZPC" | ver=1 | Hello
Hello = identity_pub(32, Ed25519) | ephemeral_pub(32, X25519) | salt(16) | sig(64)
sig   = Ed25519(identity, "wzp-files/v1/hello" | SHA256("wzp-files/v1/context" | context) | ephemeral_pub | salt)
```

`context` is the relay room name, or `"wzp-files-direct"`, binding the
signature to the rendezvous. Each side verifies the peer's signature,
optionally checks `fingerprint = SHA256(identity_pub)[:16]` against the
expected one, rejects non-contributory DH results, and derives:

```
shared     = X25519(my_eph, peer_eph)
transcript = SHA256("wzp-files/v1/transcript" | ctx_hash | min(eph_a, eph_b) | max(eph_a, eph_b))
key(eph)   = HKDF-SHA256(salt=transcript, ikm=shared, info="wzp-files/v1/key/" | eph)
send_key   = key(my_eph);  recv_key = key(peer_eph)
SAS        = HKDF(…, info="wzp-files/v1/sas") as u32 % 10000   (4-digit verify code)
```

Keys are per direction, so the peers never share a (key, nonce) pair.

### Stream framing

After the preamble every stream is a sequence of frames
`len: u32 BE | XChaCha20-Poly1305(key, nonce = salt(16) | counter u64 BE, aad = magic)`.
Data streams start with `"WZPD" | ver=1 | salt(16)` (no Hello). Receivers
reject reused salts per session, out-of-range frame lengths (control:
8 MiB + 1 KiB, data: 64 KiB + 64) and any authentication failure.

Control frame plaintext: `0x00 | JSON(SignalMessage)` or
`0x01 | transfer_id(16) | chunk hashes (32 × n)` (the manifest).
Data frame plaintext: `0x01 | transfer_id(16) | chunk_index u32 BE`
(chunk start) followed by `0x02 | bytes` frames until the chunk length is
reached.

## 2. Wire messages

All carry `version: u8` (`#[serde(default = "default_signal_version")]`),
like every other `SignalMessage`. Peers that predate them fail to
deserialize the unknown variant; the existing signal loops treat
`TransportError::Deserialize` as "log and continue", so old peers ignore
them. wzp-files sends them **inside the encrypted control stream**; the
relay never parses them there.

| message | direction | meaning |
|---|---|---|
| `FileOffer {transfer_id:[u8;16], name, size:u64, blake3_root:[u8;32], mime:Option<String>, chunk_size:u32}` | S→R | propose a file |
| `FileAccept {transfer_id, resume_bitmap:Option<Vec<u8>>}` | R→S | consent; bitmap = chunks already held (1 bit per chunk, LSB-first) |
| `FileReject {transfer_id, reason}` | R→S | declined / too large / busy / timeout |
| `FileProgress {transfer_id, chunks_done:u64}` | R→S | verified chunks on disk (≤ 4/s) |
| `FileComplete {transfer_id, blake3_root}` | R→S | file verified and renamed into place |
| `FileCancel {transfer_id}` | both | abort |
| `ChunkNack {transfer_id, chunks:Vec<u32>}` | R→S | resend these (failed hash, or missing after idle) |

The relay additionally forwards these variants when a client sends them
on the plain relay signal channel in a two-party room (rate limited to
20/s, burst 200); wzp-files does not use that path because it would
expose names and sizes to the relay.

## 3. Integrity

```
n          = ceil(size / chunk_size)          (0 for an empty file)
hash_i     = BLAKE3(chunk_i)
root       = BLAKE3("wzp-files/v1/manifest" | size u64 LE | chunk_size u32 LE | hash_0 | … | hash_{n-1})
```

The sender hashes the file once (streaming, one chunk buffer) before
offering. After `FileAccept` it sends the manifest; the receiver checks it
against the offered root and from then on verifies **each chunk on
arrival**. A bad chunk is NACKed immediately (and re-read + re-hashed by
the sender, which aborts with "source file changed" if its own data no
longer matches). A chunk failing more than `max_chunk_retries` (5) times
fails the transfer. A 10 GiB file never fails only at the end.

## 4. State machines

Sender (per transfer):

```
send() ─hash─▶ Offered ──FileAccept──▶ Sending ──FileComplete(root ok)──▶ Completed
                 │  └──FileReject──▶ Rejected        │  ▲
                 └──offer_timeout──▶ Failed          │  └─ ChunkNack: requeue chunks
                                                      ├──FileCancel / cancel()──▶ Cancelled
                                                      └──session lost / read error──▶ Failed
```

In `Sending`, `streams_per_transfer` (4) workers each own one data stream
and pull chunk indices from a shared queue; they stay open for NACKed
chunks until the transfer ends.

Receiver (per transfer):

```
FileOffer ─validate─▶ AwaitingDecision ──accept()──▶ Receiving ──all chunks verified──▶ Finishing ──rename──▶ Completed (FileComplete sent)
   │ (too large, bad chunk size,  │ reject()/drop/timeout ─▶ Rejected     │
   │  too many chunks/offers,     │                                       ├─ bad chunk ─▶ ChunkNack
   │  duplicate id)               │                                       ├─ idle nack_idle (2 s) ─▶ ChunkNack(missing)
   └──▶ FileReject (no prompt)                                            ├─ FileCancel ─▶ Cancelled (partial deleted)
                                                                          └─ session lost ─▶ Failed (partial kept)
```

Chunks for a transfer that is not in `Receiving` (never offered, not yet
accepted, finished) make the receiver `STOP` the stream with code
`0x5701` (unsolicited); no byte is written.

## 5. Storage and resume

* On accept the receiver creates `<name>.wzpart` (pre-sized, `create_new`)
  and `<name>.wzpart.state` (JSON: name, size, chunk_size, root, bitmap).
  `<name>` is the sanitized offer name made collision-free
  (`a.txt`, `a (1).txt`, …) against existing files and partials.
* Verified chunks are written at their offset; the state file is replaced
  atomically (after `fsync` of the data) at most once per second and when
  the session ends.
* A later offer with the same `(root, size, chunk_size)` finds the state
  file, sends its bitmap in `FileAccept.resume_bitmap`, and the sender only
  queues the missing chunks.
* Completion: `fsync`, reserve the final name with `create_new`, `rename`
  the partial over the placeholder, delete the state file.
* Cancel deletes partial + state; a lost session keeps them.

Name sanitizing (`wzp_proto::sanitize_file_name`): last path component
only (both `/` and `\`), control characters and `<>:"|?*` removed, leading
dots/spaces and trailing dots/spaces trimmed, Windows device names (`CON`,
`NUL`, `COM1`, …) prefixed with `_`, capped at 200 bytes (on a char
boundary, keeping a short extension), `"file"` if nothing is left.

## 6. Limits (defaults, `FileConfig`)

| limit | default |
|---|---|
| `max_size` (send and accept) | 10 GiB |
| `chunk_size` (send) / accepted range | 1 MiB / 16 KiB – 4 MiB |
| max chunks per file | 262 144 (manifest ≤ 8 MiB) |
| `streams_per_transfer` | 4 |
| `max_pending_offers` / `max_active_incoming` | 8 / 8 |
| `offer_timeout` (both ends) | 5 min |
| `nack_idle` | 2 s |
| `max_chunk_retries` | 5 |
| `max_send_rate` | unlimited |

Memory is bounded: the sender holds one chunk per worker, the receiver one
chunk per incoming stream (≤ 4 MiB each) plus the manifest.

## 7. Relay behaviour

* Rooms prefixed `file-` (like `call-`) are two-party: a third client is
  refused.
* In `file-*` and `call-*` rooms the relay runs
  `wzp_relay::file_relay::run_stream_splicer` per participant: every uni
  stream a participant opens is copied to a fresh uni stream on the other
  participant (FIN, `RESET` and `STOP_SENDING` are propagated). The relay
  only sees ciphertext and frame lengths.
* Per session: token-bucket shaping (`file_rate_bytes_per_sec`, default
  32 MiB/s, burst 8 MiB; shaping, not dropping), total byte cap
  (`file_session_byte_cap`, 64 GiB), per-stream cap
  (`file_stream_byte_cap`, 10 GiB + 128 MiB), max 32 concurrent spliced
  streams; a stream opened with no peer present waits 10 s and is then
  refused. All three caps are `RelayConfig` (TOML) fields.
* `MediaType::Data` datagrams in any room go through a per-session
  8 MiB/s (burst 16 MiB) limiter in both forwarding loops; over-budget
  packets are dropped.

Known relay-side limitation: the relay's QUIC server config keeps the
VoIP flow-control windows (256 KiB per connection, 64 KiB per stream), so
the *upload* leg (sender → relay) is bounded by roughly
256 KiB / RTT. The download leg uses the receiver's bulk windows.
Raising these for file sessions needs either a separate relay listener or
per-connection transport config (quinn `Incoming::accept_with`) — left for
a follow-up.

## 8. CLI

```
wzp-client send-file <path>... --relay <addr> [--room <token>] [--peer <fp>] [--seed <hex>]
wzp-client send-file <path>... --direct <addr>
wzp-client receive-files --relay <addr> --room <token> --dir <dir> [--auto-accept] [--peer <fp>]
wzp-client receive-files --listen <addr> --dir <dir> [--auto-accept]
```

Both sides print their own fingerprint, the peer's fingerprint and the
4-digit verify code. Without `--auto-accept` the receiver prompts per offer.
`send-file` without `--room` creates a random room and prints the matching
receive command.

## 9. Follow-up: data path A (RaptorQ over datagrams)

Not implemented. Reliable QUIC streams already recover loss with ACK-based
retransmission and work through the relay splice. Path A would help on
very lossy / high-RTT links where QUIC retransmission stalls:

* per chunk, RaptorQ-encode (`wzp-fec`) into symbols sized to
  `max_datagram_size()` minus `MediaHeader` + AEAD tag;
* send as `MediaType::Data` datagrams (`MediaHeader.seq` = per-transfer
  symbol counter, `stream_id` = transfer slot), encrypted with the session
  `send_key` (nonce from seq, *not* the call `ChaChaSession`, which uses
  the same nonce space for both directions);
* receiver decodes per chunk, verifies the BLAKE3 hash, and acks decoded
  chunks with `ChunkNack`-style bitmaps over the control stream; the sender
  keeps emitting repair symbols for un-acked chunks, paced by a
  loss-based rate controller;
* the relay already forwards and rate-limits `MediaType::Data`.

Selection would happen in `FileEndpoint` per transfer (e.g. measured loss
> 10 % or RTT > 300 ms → A), behind the same public API.
