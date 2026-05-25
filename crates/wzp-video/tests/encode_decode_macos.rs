//! Round-trip integration test: synthetic I420 frame → VideoToolbox encode →
//! depacketize → VideoToolbox decode → frame.
//!
//! This test requires macOS (VideoToolbox is not available elsewhere).

#![cfg(target_os = "macos")]

use std::sync::Mutex;
use wzp_video::{VideoDecoder, VideoEncoder, VideoFrame};

/// VideoToolbox uses global encoder registry state that can race when multiple
/// sessions are created concurrently.  Serialize integration tests.
static VT_LOCK: Mutex<()> = Mutex::new(());

/// Generate a synthetic 640×360 I420 frame with a simple gradient pattern.
/// True if the Annex-B access unit contains at least one IDR slice (NAL type 5).
fn au_contains_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i < au.len() {
        // Skip start code.
        if i + 3 <= au.len() && au[i..i + 3] == [0x00, 0x00, 0x01] {
            i += 3;
        } else if i + 4 <= au.len() && au[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
            i += 4;
        } else {
            i += 1;
            continue;
        }
        if i < au.len() && (au[i] & 0x1F) == 5 {
            return true;
        }
    }
    false
}

fn synthetic_i420_frame(width: u32, height: u32) -> VideoFrame {
    let y_size = (width * height) as usize;
    let uv_size = y_size / 4;
    let mut data = vec![0u8; y_size + uv_size * 2];

    // Y plane: horizontal gradient.
    for y in 0..height {
        for x in 0..width {
            let val = ((x * 255) / width) as u8;
            data[(y * width + x) as usize] = val;
        }
    }

    // U and V planes: flat mid-grey.
    data[y_size..y_size + uv_size].fill(128);
    data[y_size + uv_size..].fill(128);

    VideoFrame {
        width,
        height,
        data,
        timestamp_ms: 0,
    }
}

#[test]
fn encode_decode_roundtrip() {
    let _guard = VT_LOCK.lock().unwrap();
    let width = 640;
    let height = 360;

    let mut encoder = wzp_video::VideoToolboxEncoder::new(width, height, 2_000_000).unwrap();
    let mut decoder = wzp_video::VideoToolboxDecoder::new(width, height).unwrap();

    let mut keyframe_seen = false;
    let mut decoded_any = false;

    for i in 0..30 {
        let mut frame = synthetic_i420_frame(width, height);
        frame.timestamp_ms = i as u64 * 33;

        if i == 0 {
            encoder.request_keyframe();
        }

        let au = encoder.encode(&frame).unwrap();
        if au.is_empty() {
            // VideoToolbox may buffer frames; not every encode() yields output.
            continue;
        }

        if au_contains_idr(&au) {
            keyframe_seen = true;
        }

        // Decode the access unit.
        let decoded = decoder.decode(&au).unwrap();
        if let Some(decoded_frame) = decoded {
            assert_eq!(decoded_frame.width, width);
            assert_eq!(decoded_frame.height, height);
            // I420 size check: Y + U + V = 1.5 * width * height
            let expected_size = (width * height * 3 / 2) as usize;
            assert!(
                decoded_frame.data.len() >= expected_size,
                "decoded frame data too small: {} < {expected_size}",
                decoded_frame.data.len()
            );
            decoded_any = true;
        }
    }

    assert!(
        keyframe_seen,
        "at least one keyframe should have been produced"
    );
    assert!(decoded_any, "at least one frame should have been decoded");
}

#[test]
fn keyframe_in_first_five_frames() {
    let _guard = VT_LOCK.lock().unwrap();
    let width = 640;
    let height = 360;

    let mut encoder = wzp_video::VideoToolboxEncoder::new(width, height, 2_000_000).unwrap();

    let mut keyframe_seen = false;

    for i in 0..5 {
        let mut frame = synthetic_i420_frame(width, height);
        frame.timestamp_ms = i as u64 * 33;

        if i == 0 {
            encoder.request_keyframe();
        }

        let au = encoder.encode(&frame).unwrap();
        if !au.is_empty() && au_contains_idr(&au) {
            keyframe_seen = true;
            break;
        }
    }

    assert!(
        keyframe_seen,
        "at least one keyframe should appear in the first 5 frames"
    );
}
