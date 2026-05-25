//! `EncryptingTransport` — wraps any `MediaTransport` with a `CryptoSession`.
//!
//! All outbound `send_media` calls encrypt the payload before handing off to
//! the inner transport; all inbound `recv_media` calls decrypt after receiving.
//! Signal, quality, and close are forwarded unchanged.
//!
//! The quality report travels in plaintext so the relay can make QoS decisions
//! without being able to decrypt media content.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use wzp_proto::{
    CryptoSession, MediaHeader, MediaPacket, MediaTransport, PathQuality, SignalMessage,
    TransportError,
};

/// Wraps a `MediaTransport` and applies AEAD encryption/decryption to media payloads.
pub struct EncryptingTransport {
    inner: Arc<dyn MediaTransport>,
    session: Mutex<Box<dyn CryptoSession>>,
}

impl EncryptingTransport {
    pub fn new(inner: Arc<dyn MediaTransport>, session: Box<dyn CryptoSession>) -> Self {
        Self {
            inner,
            session: Mutex::new(session),
        }
    }
}

#[async_trait]
impl MediaTransport for EncryptingTransport {
    async fn send_media(&self, packet: &MediaPacket) -> Result<(), TransportError> {
        let mut header_bytes = Vec::with_capacity(MediaHeader::WIRE_SIZE);
        packet.header.write_to(&mut header_bytes);

        let mut ciphertext = Vec::new();
        self.session
            .lock()
            .unwrap()
            .encrypt(&header_bytes, &packet.payload, &mut ciphertext)
            .map_err(|e| TransportError::Internal(format!("encrypt: {e}")))?;

        let encrypted = MediaPacket {
            header: packet.header,
            payload: Bytes::from(ciphertext),
            quality_report: packet.quality_report.clone(),
        };
        self.inner.send_media(&encrypted).await
    }

    async fn recv_media(&self) -> Result<Option<MediaPacket>, TransportError> {
        let packet = match self.inner.recv_media().await? {
            Some(p) => p,
            None => return Ok(None),
        };

        let mut header_bytes = Vec::with_capacity(MediaHeader::WIRE_SIZE);
        packet.header.write_to(&mut header_bytes);

        let mut plaintext = Vec::new();
        self.session
            .lock()
            .unwrap()
            .decrypt(&header_bytes, &packet.payload, &mut plaintext)
            .map_err(|e| TransportError::Internal(format!("decrypt: {e}")))?;

        Ok(Some(MediaPacket {
            header: packet.header,
            payload: Bytes::from(plaintext),
            quality_report: packet.quality_report,
        }))
    }

    async fn send_signal(&self, msg: &SignalMessage) -> Result<(), TransportError> {
        self.inner.send_signal(msg).await
    }

    async fn recv_signal(&self) -> Result<Option<SignalMessage>, TransportError> {
        self.inner.recv_signal().await
    }

    fn path_quality(&self) -> PathQuality {
        self.inner.path_quality()
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.inner.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use wzp_crypto::ChaChaSession;
    use wzp_proto::{CodecId, MediaType};

    struct LoopbackTransport {
        sent: StdMutex<Vec<MediaPacket>>,
    }

    impl LoopbackTransport {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: StdMutex::new(Vec::new()),
            })
        }
        fn take_sent(&self) -> Vec<MediaPacket> {
            self.sent.lock().unwrap().drain(..).collect()
        }
    }

    #[async_trait]
    impl MediaTransport for LoopbackTransport {
        async fn send_media(&self, packet: &MediaPacket) -> Result<(), TransportError> {
            self.sent.lock().unwrap().push(packet.clone());
            Ok(())
        }
        async fn recv_media(&self) -> Result<Option<MediaPacket>, TransportError> {
            Ok(None)
        }
        async fn send_signal(&self, _msg: &SignalMessage) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv_signal(&self) -> Result<Option<SignalMessage>, TransportError> {
            Ok(None)
        }
        fn path_quality(&self) -> PathQuality {
            PathQuality::default()
        }
        async fn close(&self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    fn make_header(seq: u32) -> MediaHeader {
        MediaHeader {
            version: 2,
            flags: 0,
            media_type: MediaType::Audio,
            codec_id: CodecId::Opus24k,
            stream_id: 0,
            fec_ratio: 0,
            seq,
            timestamp: seq * 20,
            fec_block: 0,
        }
    }

    #[tokio::test]
    async fn payload_is_encrypted_on_wire() {
        let key = [0x42u8; 32];
        let session: Box<dyn CryptoSession> = Box::new(ChaChaSession::new(key));
        let loopback = LoopbackTransport::new();
        let enc = EncryptingTransport::new(loopback.clone(), session);

        let header = make_header(1);
        let plaintext = b"secret audio frame";
        let pkt = MediaPacket {
            header,
            payload: Bytes::from_static(plaintext),
            quality_report: None,
        };

        enc.send_media(&pkt).await.unwrap();

        let sent = loopback.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].header, header, "header must be preserved");
        assert_ne!(
            sent[0].payload.as_ref(),
            plaintext.as_ref(),
            "plaintext must not appear on wire"
        );
        // Ciphertext is longer by exactly the AEAD tag (16 bytes)
        assert_eq!(sent[0].payload.len(), plaintext.len() + 16);
    }

    #[tokio::test]
    async fn encrypt_then_decrypt_roundtrip() {
        let key = [0x42u8; 32];
        let send_session: Box<dyn CryptoSession> = Box::new(ChaChaSession::new(key));
        let mut recv_session = ChaChaSession::new(key);

        let loopback = LoopbackTransport::new();
        let enc = EncryptingTransport::new(loopback.clone(), send_session);

        let header = make_header(5);
        let plaintext = b"hello encrypted world";
        let pkt = MediaPacket {
            header,
            payload: Bytes::from_static(plaintext),
            quality_report: None,
        };

        enc.send_media(&pkt).await.unwrap();

        let sent = loopback.take_sent();
        let wire_pkt = &sent[0];

        let mut header_bytes = Vec::new();
        header.write_to(&mut header_bytes);
        let mut decrypted = Vec::new();
        recv_session
            .decrypt(&header_bytes, &wire_pkt.payload, &mut decrypted)
            .expect("decrypt should succeed with matching key");
        assert_eq!(&decrypted[..], plaintext);
    }
}
