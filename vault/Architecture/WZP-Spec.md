---
tags: [architecture, wzp]
type: architecture
---

# WZP Protocol Specification (one-page reference)

> Distilled from `docs/ARCHITECTURE.md` and the `wzp-proto` crate. Authoritative wire details live in `crates/wzp-proto/src/packet.rs`.
>
> **Status:** v2 is the deployed protocol (audio + video, 16 B header, MediaType, u32 seq). v1 clients are rejected with `Hangup::ProtocolVersionMismatch`.

## Layer summary

| Layer | WZP | FaceTime equivalent |
|---|---|---|
| Transport | **QUIC datagrams** (Quinn), PLPMTUD 1200 → 1452 | RTP/SRTP over UDP, ICE |
| Signaling | `SignalMessage` (bincode) over a QUIC stream, SNI = hashed room name | APNs-tunneled binary plist |
| Identity | Ed25519 + X25519 from BIP39 seed; fingerprint = SHA-256(pubkey)[..16] | IDS RSA + ECDSA per device |
| Key agreement | X25519 DH + HKDF, Ed25519 signatures, rekey every 65,536 packets | Per-call DH signed by IDS keys |
| Bulk crypto | ChaCha20-Poly1305, 64-packet sliding anti-replay | SRTP (AES-CTR + HMAC) |
| Loss recovery | **RaptorQ FEC + Opus DRED + classical PLC** | NACK / PLI + reference-picture selection |
| Adaptive | 3-tier hysteresis (Good / Degraded / Catastrophic) + continuous DRED tuner | Per-frame bitrate ladder |
| Topology | SFU rooms + inter-relay federation + P2P via ICE | Mesh ≤ ~3, SFU above, Apple relays |
| Header | 16 B `MediaHeader` v2 / 5 B `MiniHeader` (49 of 50), 4 B `QualityReport` trailer | RTP 12 B + extensions |

## Distinctive choices

- **QUIC datagrams instead of raw UDP + SRTP.** Brings TLS 1.3, PLPMTUD, path migration, and ACK-based RTT/loss estimation for free.
- **Continuous DRED tuning.** Maps live `(loss%, RTT, jitter)` to a continuous Opus DRED lookback window. Most stacks treat DRED as discrete tiers.
- **MiniHeader (5 B for 49/50 packets).** Saves ~11 B/packet ≈ 550 B/s/stream at 50 pps vs. the full 16 B header.
- **E2E-preserving SFU.** The relay forwards encrypted datagrams; it never decrypts media. Room membership uses SNI = `hash(room_name)`.
- **Codec coordination via `QualityReport` trailer.** Receivers attach 4-byte loss/RTT/jitter/cap to media packets; the SFU broadcasts `QualityDirective` so all senders in a room converge on the same tier.

## Wire format (current — v2)

### `MediaHeader` v2 (16 bytes, byte-aligned)

```
Byte 0:      version        (u8)   0x02
Byte 1:      flags          (u8)   [T:1][Q:1][KeyFrame:1][FrameEnd:1][reserved:4]
Byte 2:      media_type     (u8)   0=audio, 1=video, 2=data, 3=control
Byte 3:      codec_id       (u8)   0-255 (see codec table)
Byte 4:      stream_id      (u8)   simulcast layer; 0=base
Byte 5:      fec_ratio      (u8)   0..200 → 0.0..2.0
Bytes 6-9:   sequence       (u32 BE)
Bytes 10-13: timestamp_ms   (u32 BE)
Bytes 14-15: fec_block_id   (u16 BE)
```

| Field | Bits | Meaning |
|---|---|---|
| version | 8 | Must be `0x02`; v1 clients receive `Hangup::ProtocolVersionMismatch` |
| T (bit 7 of flags) | 1 | 1 = FEC repair packet |
| Q (bit 6 of flags) | 1 | QualityReport trailer present |
| KeyFrame (bit 5 of flags) | 1 | Packet belongs to a video I-frame |
| FrameEnd (bit 4 of flags) | 1 | Last packet of an access unit |
| reserved (bits 3-0 of flags) | 4 | Must be zero |
| media_type | 8 | 0=audio, 1=video, 2=data, 3=control |
| codec_id | 8 | See codec table (widened from v1's 4-bit field) |
| stream_id | 8 | Simulcast layer; 0=base layer |
| fec_ratio | 8 | 0..200 → 0.0..2.0 |
| sequence | 32 | Monotonically increasing packet seq (not reset by rekey) |
| timestamp_ms | 32 | ms since session start. Monotonic across the full session; **not reset by rekey** |
| fec_block_id | 16 | FEC source block ID |

### Codec table

| ID | Codec | Bitrate | Sample | Frame |
|---|---|---|---|---|
| 0 | Opus 24k | 24 kbps | 48 kHz | 20 ms |
| 1 | Opus 16k | 16 kbps | 48 kHz | 20 ms |
| 2 | Opus 6k | 6 kbps | 48 kHz | 40 ms |
| 3 | Codec2 3200 | 3.2 kbps | 8 kHz | 20 ms |
| 4 | Codec2 1200 | 1.2 kbps | 8 kHz | 40 ms |
| 5 | ComfortNoise | 0 | 48 kHz | 20 ms |
| 6 | Opus 32k | 32 kbps | 48 kHz | 20 ms |
| 7 | Opus 48k | 48 kbps | 48 kHz | 20 ms |
| 8 | Opus 64k | 64 kbps | 48 kHz | 20 ms |
| 9 | H.264 Baseline | — | — | — |
| 10 | H.264 Main | — | — | — |
| 11 | H.265 Main | — | — | — |
| 12 | AV1 Main | — | — | — |

### `MiniHeader` v2 (5 bytes, compressed — 49 of every 50 packets)

```
[FRAME_TYPE_MINI = 0x01]
Byte 0:    seq_delta           (u8)
Bytes 1-2: timestamp_delta_ms  (u16 BE)
Bytes 3-4: payload_len         (u16 BE)
```

Full header sent every 50th packet to resync.

### `TrunkFrame` (batched, relay-internal)

```
[count: u16]
  [session_id: 2][len: u16][payload: len]   × count
```

Up to 10 entries or PMTUD-discovered MTU; flushed every 5 ms.

### `QualityReport` (4 bytes, optional inline trailer)

```
Byte 0: loss_pct          (0-255 → 0-100%)
Byte 1: rtt_4ms           (0-255 → 0-1020 ms)
Byte 2: jitter_ms         (0-255 ms)
Byte 3: bitrate_cap_kbps  (0-255 kbps)
```

### Version negotiation

- `version=0x02` in `MediaHeader` is a hard switch — there is no fallback negotiation.
- Both endpoints must speak v2. A v1 peer receives `Hangup::ProtocolVersionMismatch` immediately.
- Relays inspect only `version` and `media_type`; they never downgrade or translate between versions.

## Session lifecycle

```
Idle → Connecting → Handshaking → Active ⇄ Rekeying → Closed
```

- `CallOffer { identity_pub, ephemeral_pub, signature, profiles }`
- `CallAnswer { identity_pub, ephemeral_pub, signature, chosen_profile }`
- `session_key = HKDF(X25519_DH(eph_a, eph_b), "warzone-session-key")`
- Rekey every 65,536 packets via fresh ephemeral DH.

## SFU forwarding rules

1. Fan-out to all room participants except the sender.
2. Failed sends are skipped; forwarding is best-effort.
3. The relay never decrypts media.
4. With trunking on, packets to the same receiver are batched (flush 5 ms).
5. `QualityDirective` is broadcast when the room-wide tier degrades.

## Adaptive quality (audio, today)

| Tier | Codec | FEC | Frame |
|---|---|---|---|
| Good | Opus 24 k | 20 % | 20 ms |
| Degraded | Opus 6 k | 50 % | 40 ms |
| Catastrophic | Codec2 1200 | 100 % | 40 ms |

Hysteresis: 3 reports to downgrade (2 on cellular), 10 to upgrade.

## NAT traversal (Phase 8)

- Candidate types: Host, Port-mapped (NAT-PMP / PCP / UPnP), Server-reflexive (STUN), Relay.
- Hard-NAT port prediction with `classify_port_allocation()` → `predict_ports()` → `HardNatProbe` signal.
- Mid-call re-gather: `CandidateUpdate { generation }`.
