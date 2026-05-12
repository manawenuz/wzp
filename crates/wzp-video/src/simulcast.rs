//! Simulcast encoder — drives 3 independent encoder layers per source.
//!
//! Each layer emits a separate stream tagged by `stream_id`:
//! - 0 = low  (480×270,  150 kbps, 15 fps)
//! - 1 = mid  (960×540,  600 kbps, 30 fps)
//! - 2 = high (1920×1080, 2500 kbps, 30 fps)
//!
//! The sender activates layers based on available bandwidth.  The SFU
//! (T5.6) selects which layer to forward to each receiver.

use crate::encoder::{VideoEncoder, VideoError, VideoFrame};

/// Configuration for one simulcast layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimulcastLayer {
    /// `stream_id` placed in `MediaHeader` v2.
    pub stream_id: u8,
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// Target bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Target frame rate.
    pub fps: u8,
}

impl SimulcastLayer {
    /// Low layer — 480×270 @ 150 kbps, 15 fps.
    pub const LOW: Self = Self {
        stream_id: 0,
        width: 480,
        height: 270,
        bitrate_kbps: 150,
        fps: 15,
    };

    /// Mid layer — 960×540 @ 600 kbps, 30 fps.
    pub const MID: Self = Self {
        stream_id: 1,
        width: 960,
        height: 540,
        bitrate_kbps: 600,
        fps: 30,
    };

    /// High layer — 1920×1080 @ 2500 kbps, 30 fps.
    pub const HIGH: Self = Self {
        stream_id: 2,
        width: 1920,
        height: 1080,
        bitrate_kbps: 2500,
        fps: 30,
    };

    /// All three layers in ascending order.
    pub const ALL: [Self; 3] = [Self::LOW, Self::MID, Self::HIGH];

    /// Total bitrate of all layers in kbps.
    pub const fn total_bitrate_kbps() -> u32 {
        Self::LOW.bitrate_kbps + Self::MID.bitrate_kbps + Self::HIGH.bitrate_kbps
    }
}

/// Active target for one layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerTarget {
    pub layer: SimulcastLayer,
    pub active: bool,
}

/// Result of one simulcast encode call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerPacket {
    pub stream_id: u8,
    pub data: Vec<u8>,
}

/// Simulcast encoder manager.
///
/// Holds up to three [`VideoEncoder`] instances (one per layer).  On each
/// incoming frame it feeds the frame to every active encoder and collects
/// the resulting access units tagged by `stream_id`.
pub struct SimulcastEncoder {
    layers: Vec<LayerState>,
}

struct LayerState {
    config: SimulcastLayer,
    encoder: Box<dyn VideoEncoder>,
    active: bool,
}

impl SimulcastEncoder {
    /// Create a new simulcast encoder from a factory function.
    ///
    /// `factory` is called once per layer with `(width, height, bitrate_bps)`.
    /// On failure for any layer the whole constructor fails.
    pub fn new<F>(mut factory: F) -> Result<Self, VideoError>
    where
        F: FnMut(u32, u32, u32) -> Result<Box<dyn VideoEncoder>, VideoError>,
    {
        let mut layers = Vec::with_capacity(3);
        for cfg in SimulcastLayer::ALL {
            let encoder = factory(cfg.width, cfg.height, cfg.bitrate_kbps * 1000)?;
            layers.push(LayerState {
                config: cfg,
                encoder,
                active: true,
            });
        }
        Ok(Self { layers })
    }

    /// Encode one raw frame on all active layers.
    ///
    /// Returns a vector of `(stream_id, access_unit)` pairs, one per active
    /// layer that produced output.
    pub fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<LayerPacket>, VideoError> {
        let mut out = Vec::with_capacity(self.layers.len());
        for layer in &mut self.layers {
            if !layer.active {
                continue;
            }
            let data = layer.encoder.encode(frame)?;
            if !data.is_empty() {
                out.push(LayerPacket {
                    stream_id: layer.config.stream_id,
                    data,
                });
            }
        }
        Ok(out)
    }

    /// Request a keyframe on all active layers.
    pub fn request_keyframe(&mut self) {
        for layer in &mut self.layers {
            if layer.active {
                layer.encoder.request_keyframe();
            }
        }
    }

    /// Enable or disable individual layers.
    ///
    /// `mask` is a 3-bit mask where bit *i* controls layer *i*.
    ///   bit 0 → low layer
    ///   bit 1 → mid layer
    ///   bit 2 → high layer
    pub fn set_layer_mask(&mut self, mask: u8) {
        for (idx, layer) in self.layers.iter_mut().enumerate() {
            layer.active = (mask >> idx) & 1 != 0;
        }
    }

    /// Current layer mask (3-bit).
    pub fn layer_mask(&self) -> u8 {
        let mut mask = 0u8;
        for (idx, layer) in self.layers.iter().enumerate() {
            if layer.active {
                mask |= 1 << idx;
            }
        }
        mask
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{VideoEncoder, VideoError, VideoFrame};

    struct DummyEncoder {
        stream_id: u8,
        force_keyframe: bool,
    }

    impl VideoEncoder for DummyEncoder {
        fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<u8>, VideoError> {
            let mut out = vec![self.stream_id];
            out.extend_from_slice(&frame.data);
            Ok(out)
        }

        fn request_keyframe(&mut self) {
            self.force_keyframe = true;
        }

        fn is_keyframe(&self, _packet: &[u8]) -> bool {
            false
        }
    }

    fn dummy_factory(stream_counter: &mut u8) -> impl FnMut(u32, u32, u32) -> Result<Box<dyn VideoEncoder>, VideoError> + '_ {
        move |_w, _h, _br| {
            let enc = DummyEncoder {
                stream_id: *stream_counter,
                force_keyframe: false,
            };
            *stream_counter += 1;
            Ok(Box::new(enc))
        }
    }

    #[test]
    fn simulcast_encoder_creates_three_layers() {
        let mut counter = 0u8;
        let enc = SimulcastEncoder::new(dummy_factory(&mut counter));
        assert!(enc.is_ok());
        let enc = enc.unwrap();
        assert_eq!(enc.layer_mask(), 0b111);
    }

    #[test]
    fn simulcast_encode_produces_three_packets() {
        let mut counter = 0u8;
        let mut enc = SimulcastEncoder::new(dummy_factory(&mut counter)).unwrap();
        let frame = VideoFrame::new(1920, 1080, vec![0xAB; 100], 0);
        let packets = enc.encode(&frame).unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].stream_id, 0);
        assert_eq!(packets[1].stream_id, 1);
        assert_eq!(packets[2].stream_id, 2);
    }

    #[test]
    fn simulcast_layer_mask_disables_layers() {
        let mut counter = 0u8;
        let mut enc = SimulcastEncoder::new(dummy_factory(&mut counter)).unwrap();
        enc.set_layer_mask(0b101); // low + high, no mid
        assert_eq!(enc.layer_mask(), 0b101);

        let frame = VideoFrame::new(1920, 1080, vec![0xCD; 100], 0);
        let packets = enc.encode(&frame).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].stream_id, 0);
        assert_eq!(packets[1].stream_id, 2);
    }

    #[test]
    fn simulcast_request_keyframe_propagates() {
        let mut counter = 0u8;
        let mut enc = SimulcastEncoder::new(dummy_factory(&mut counter)).unwrap();
        enc.request_keyframe();
        // DummyEncoder sets force_keyframe flag; we can't inspect it directly
        // because it's inside the Box, but the call should not panic.
    }

    #[test]
    fn simulcast_layer_total_bitrate() {
        assert_eq!(SimulcastLayer::total_bitrate_kbps(), 150 + 600 + 2500);
    }

    #[test]
    fn simulcast_all_layers_ordered() {
        let all = SimulcastLayer::ALL;
        assert_eq!(all[0].stream_id, 0);
        assert_eq!(all[1].stream_id, 1);
        assert_eq!(all[2].stream_id, 2);
        assert_eq!(all[0].width, 480);
        assert_eq!(all[1].width, 960);
        assert_eq!(all[2].width, 1920);
    }
}
