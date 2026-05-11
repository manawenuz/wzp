# WZP Protocol Specification (one-page reference)

> Distilled from `docs/ARCHITECTURE.md` and the `wzp-proto` crate. Authoritative wire details live in `crates/wzp-proto/src/packet.rs`.
>
> **Status:** v1 (audio-only) is the deployed protocol. v2 (audio + video, 16 B header, MediaType, u32 seq, etc.) is specified in `ROAD-TO-VIDEO.md` Phase V1 and supersedes this document when implemented.

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
| Header | 12 B `MediaHeader` / 4 B `MiniHeader` (49 of 50), 4 B `QualityReport` trailer | RTP 12 B + extensions |

## Distinctive choices

- **QUIC datagrams instead of raw UDP + SRTP.** Brings TLS 1.3, PLPMTUD, path migration, and ACK-based RTT/loss estimation for free.
- **Continuous DRED tuning.** Maps live `(loss%, RTT, jitter)` to a continuous Opus DRED lookback window. Most stacks treat DRED as discrete tiers.
- **MiniHeader (4 B for 49/50 packets).** Saves ~8 B/packet ≈ 400 B/s/stream at 50 pps.
- **E2E-preserving SFU.** The relay forwards encrypted datagrams; it never decrypts media. Room membership uses SNI = `hash(room_name)`.
- **Codec coordination via `QualityReport` trailer.** Receivers attach 4-byte loss/RTT/jitter/cap to media packets; the SFU broadcasts `QualityDirective` so all senders in a room converge on the same tier.

## Wire format (current — v1)

### `MediaHeader` (12 bytes)

```
Byte 0:  [V:1][T:1][CodecID:4][Q:1][FecRatioHi:1]
Byte 1:  [FecRatioLo:6][unused:2]
Bytes 2-3:  sequence (u16 BE)
Bytes 4-7:  timestamp_ms (u32 BE)
Byte 8:     fec_block_id (u8)
Byte 9:     fec_symbol_idx (u8)
Byte 10:    reserved
Byte 11:    csrc_count
```

| Field | Bits | Meaning |
|---|---|---|
| V | 1 | Protocol version |
| T | 1 | 1 = FEC repair packet |
| CodecID | 4 | See codec table |
| Q | 1 | QualityReport trailer present |
| FecRatio | 7 | 0–127 → 0.0–2.0 |
| sequence | 16 | Wrapping packet seq |
| timestamp_ms | 32 | ms since session start. Monotonic across the full session; **not reset by rekey** |
| fec_block_id | 8 | FEC source block ID |
| fec_symbol_idx | 8 | Symbol index in block |

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

### `MiniHeader` (4 bytes, compressed — 49 of every 50 packets)

```
[FRAME_TYPE_MINI = 0x01]
Bytes 0-1: timestamp_delta_ms (u16 BE)
Bytes 2-3: payload_len (u16 BE)
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
