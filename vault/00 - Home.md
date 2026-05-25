---
tags: [home, wzp]
type: index
---

# WarzonePhone Vault

WarzonePhone (WZP) is a custom lossy VoIP protocol and application stack built in Rust. It features a 7-crate workspace, Opus + Codec2 audio codecs, RaptorQ FEC, QUIC transport, and a Tauri-based Android client. The project spans relay infrastructure, P2P direct calling, AV1 video, and federated relay gossip.

---

## Architecture

- [[Architecture/Architecture|Architecture Overview]]
- [[Architecture/WZP-Spec|WZP Protocol Spec]]
- [[Architecture/Protocol-Audit|Protocol Audit]]
- [[Architecture/Design|Design Doc]]
- [[Architecture/WS-Relay-Spec|WebSocket Relay Spec]]
- [[Architecture/Extensibility|Extensibility]]
- [[Architecture/Road-To-Video|Road to Video]]
- [[Architecture/Attack-Surface-Relay-Abuse|Attack Surface: Relay Abuse]]
- [[Architecture/Refactor-Codebase-Audit|Refactor: Codebase Audit]]
- [[Architecture/Refactor-Relay-Concurrency|Refactor: Relay Concurrency]]
- [[Architecture/Branch-Desktop-Audio-Rewrite|Branch: Desktop Audio Rewrite]]

---

## Active Work

- [[Reference/Handoff-2026-05-12|Handoff 2026-05-12]] — current state handoff doc
- [[PRDs/TASKS|TASKS — Status Board]]
- [[Audit/Audit-2026-05-25|Audit 2026-05-25]]

---

## PRDs

### Audio & Codec
- [[PRDs/PRD-adaptive-quality|Adaptive Quality]]
- [[PRDs/PRD-bluetooth-audio|Bluetooth Audio]]
- [[PRDs/PRD-coordinated-codec|Coordinated Codec]]
- [[PRDs/PRD-dred-integration|DRED Integration]]
- [[PRDs/PRD-studio-quality|Studio Quality]]

### Networking & P2P
- [[PRDs/PRD-p2p-direct|P2P Direct Calling]]
- [[PRDs/PRD-hard-nat|Hard NAT Traversal]]
- [[PRDs/PRD-ice-regather|ICE Regather]]
- [[PRDs/PRD-mtu-discovery|MTU Discovery]]
- [[PRDs/PRD-netcheck|Network Check]]
- [[PRDs/PRD-network-awareness|Network Awareness]]
- [[PRDs/PRD-portmap|Port Mapping]]
- [[PRDs/PRD-public-stun|Public STUN]]
- [[PRDs/PRD-transport-feedback-bwe|Transport Feedback BWE]]

### Relay
- [[PRDs/PRD-relay-concurrency|Relay Concurrency]]
- [[PRDs/PRD-relay-conformance|Relay Conformance]]
- [[PRDs/PRD-relay-federation|Relay Federation]]
- [[PRDs/PRD-relay-federation-gossip|Relay Federation Gossip]]
- [[PRDs/PRD-relay-selection|Relay Selection]]

### Video
- [[PRDs/PRD-video-v1|Video V1]]
- [[PRDs/PRD-video-multicodec|Video Multicodec]]
- [[PRDs/PRD-video-quality-priority|Video Quality Priority]]
- [[PRDs/PRD-video-simulcast|Video Simulcast]]

### Protocol & Security
- [[PRDs/PRD-protocol-hardening|Protocol Hardening]]
- [[PRDs/PRD-protocol-analyzer|Protocol Analyzer]]
- [[PRDs/PRD-wire-format-v2|Wire Format V2]]
- [[PRDs/PRD-delegated-trust|Delegated Trust]]

### Other
- [[PRDs/PRD-engine-dedup|Engine Dedup]]
- [[PRDs/PRD-local-recording|Local Recording]]

---

## Android

- [[Android/Architecture|Android Architecture]]
- [[Android/Build-Guide|Build Guide]]
- [[Android/Roadmap|Android Roadmap]]
- [[Android/Debugging|Debugging]]
- [[Android/Maintenance|Maintenance]]
- [[Android/Fix-Audio-Ring-Desync|Fix: Audio Ring Desync]]
- [[Android/Fix-Capture-Thread-Crash|Fix: Capture Thread Crash]]
- [[Android/README|Android README]]

---

## Reference

- [[Reference/API|API Reference]]
- [[Reference/Usage|Usage]]
- [[Reference/User-Guide|User Guide]]
- [[Reference/Administration|Administration]]
- [[Reference/Telemetry|Telemetry]]
- [[Reference/Progress|Progress]]
- [[Reference/Featherchat-Integration|FeatherChat Integration]]
- [[Reference/Featherchat|FeatherChat]]
- [[Reference/WZP-FC-Shared-Crates|WZP-FC Shared Crates]]
- [[Reference/Integration-Tasks|Integration Tasks]]

---

## Reports

### Approved
- [[Reports/T1.1-report|T1.1]] · [[Reports/T1.1.1-report|T1.1.1]] · [[Reports/T1.1.2-report|T1.1.2]]
- [[Reports/T1.2-report|T1.2]] · [[Reports/T1.2.1-report|T1.2.1]]
- [[Reports/T1.3-report|T1.3]] · [[Reports/T1.4-report|T1.4]] · [[Reports/T1.4.1-report|T1.4.1]]
- [[Reports/T1.5-report|T1.5]] · [[Reports/T1.5.1-report|T1.5.1]] · [[Reports/T1.5.2-report|T1.5.2]]
- [[Reports/T1.6-report|T1.6]] · [[Reports/T1.7-report|T1.7]] · [[Reports/T1.8-report|T1.8]]
- [[Reports/T2.1-report|T2.1]] · [[Reports/T2.2-report|T2.2]]
- [[Reports/T4.2-report|T4.2]] · [[Reports/T4.2.1-report|T4.2.1]] · [[Reports/T4.3-report|T4.3]] · [[Reports/T4.3.1-report|T4.3.1]]
- [[Reports/T4.4-report|T4.4]] · [[Reports/T4.5-report|T4.5]] · [[Reports/T4.6-report|T4.6]] · [[Reports/T4.7-report|T4.7]]
- [[Reports/T5.1-report|T5.1]] · [[Reports/T5.2-report|T5.2]] · [[Reports/T5.3-report|T5.3]]

### Pending Review
- [[Reports/T2.3-report|T2.3]] · [[Reports/T2.4-report|T2.4]] · [[Reports/T2.5-report|T2.5]] · [[Reports/T2.6-report|T2.6]]
- [[Reports/T3.1-report|T3.1]] · [[Reports/T3.2-report|T3.2]] · [[Reports/T3.3-report|T3.3]] · [[Reports/T3.4-report|T3.4]] · [[Reports/T3.5-report|T3.5]]
- [[Reports/T4.1-report|T4.1]]
- [[Reports/T5.1.1-report|T5.1.1]] · [[Reports/T5.4-report|T5.4]] · [[Reports/T5.5-report|T5.5]] · [[Reports/T5.6-report|T5.6]]
- [[Reports/T5.7-report|T5.7]] · [[Reports/T5.7.1-report|T5.7.1]] · [[Reports/T5.8-report|T5.8]]
- [[Reports/T6.1-report|T6.1]] · [[Reports/T6.1.2-report|T6.1.2]] · [[Reports/T6.2-report|T6.2]]
