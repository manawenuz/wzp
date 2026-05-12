//! Video quality controller — maps bandwidth estimate + priority mode to
//! encoder target parameters (bitrate, fps, resolution).
//!
//! See `docs/PRD/PRD-video-quality-priority.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering::Relaxed};

use wzp_proto::BandwidthEstimator;
use wzp_proto::PriorityMode;

/// Target parameters for the video encoder.
///
/// A `bitrate_kbps` of `0` means video is disabled (not enough bandwidth).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTarget {
    /// Target bitrate in kilobits per second.
    pub bitrate_kbps: u32,
    /// Target frame rate.
    pub fps: u8,
    /// Frame width in pixels.
    pub width: u16,
    /// Frame height in pixels.
    pub height: u16,
}

impl VideoTarget {
    /// Disabled video — zero budget.
    pub const DISABLED: Self = Self {
        bitrate_kbps: 0,
        fps: 0,
        width: 0,
        height: 0,
    };
}

/// Step in the bitrate -> (resolution, fps) lookup table.
struct Step {
    min_budget_kbps: u32,
    width: u16,
    height: u16,
    fps: u8,
}

/// H.264 baseline step table.  Each step is the minimum budget required to
/// sustain the corresponding resolution + frame rate.
///
/// Steps are ordered from highest to lowest budget.  The first step whose
/// `min_budget_kbps` is <= the available video budget wins.
static STEP_TABLE: &[Step] = &[
    Step {
        min_budget_kbps: 4000,
        width: 1280,
        height: 720,
        fps: 30,
    },
    Step {
        min_budget_kbps: 2000,
        width: 640,
        height: 480,
        fps: 30,
    },
    Step {
        min_budget_kbps: 1000,
        width: 480,
        height: 360,
        fps: 30,
    },
    Step {
        min_budget_kbps: 500,
        width: 480,
        height: 360,
        fps: 15,
    },
    Step {
        min_budget_kbps: 250,
        width: 320,
        height: 240,
        fps: 15,
    },
    Step {
        min_budget_kbps: 150,
        width: 320,
        height: 240,
        fps: 10,
    },
    Step {
        min_budget_kbps: 100,
        width: 240,
        height: 180,
        fps: 10,
    },
    Step {
        min_budget_kbps: 50,
        width: 240,
        height: 180,
        fps: 5,
    },
];

/// Audio floor budgets per priority mode (kbps).
const AUDIO_FLOOR_KBPS: u32 = 24;
const AUDIO_FLOOR_SCREENCAST_KBPS: u32 = 16;

/// Proportion of total budget allocated to audio in `Balanced` mode.
const BALANCED_AUDIO_RATIO: f64 = 0.15;

/// Maximum bitrate change ratio per second (2x up or down).
const MAX_CHANGE_RATIO_PER_SEC: f64 = 2.0;

/// Video quality controller.
///
/// Consumes a [`BandwidthEstimator`] and a [`PriorityMode`] and produces
/// [`VideoTarget`] recommendations for the encoder.  The controller is
/// thread-safe: `mode`, `loss_pct`, and `rtt_ms` can be updated from any
/// thread while `tick()` runs on the encoder thread.
pub struct VideoQualityController {
    bwe: Arc<BandwidthEstimator>,
    mode: AtomicU8, // PriorityMode as u8
    loss_pct: AtomicU8,
    rtt_ms: AtomicU32,
    last_target: std::sync::Mutex<VideoTarget>,
    last_tick_ms: AtomicU32,
}

impl VideoQualityController {
    /// Create a new controller.
    pub fn new(bwe: Arc<BandwidthEstimator>) -> Self {
        Self {
            bwe,
            mode: AtomicU8::new(PriorityMode::AudioFirst as u8),
            loss_pct: AtomicU8::new(0),
            rtt_ms: AtomicU32::new(0),
            last_target: std::sync::Mutex::new(VideoTarget::DISABLED),
            last_tick_ms: AtomicU32::new(0),
        }
    }

    /// Set the current priority mode (mid-call override).
    pub fn set_mode(&self, mode: PriorityMode) {
        self.mode.store(mode as u8, Relaxed);
    }

    /// Update network observables.
    pub fn update_network(&self, loss_pct: u8, rtt_ms: u32) {
        self.loss_pct.store(loss_pct, Relaxed);
        self.rtt_ms.store(rtt_ms, Relaxed);
    }

    /// Read the current priority mode.
    pub fn mode(&self) -> PriorityMode {
        match self.mode.load(Relaxed) {
            1 => PriorityMode::VideoFirst,
            2 => PriorityMode::ScreenShare,
            3 => PriorityMode::Balanced,
            _ => PriorityMode::AudioFirst,
        }
    }

    /// Compute audio and video budgets from the current BWE and priority mode.
    ///
    /// Returns `(audio_budget_kbps, video_budget_kbps)`.
    pub fn allocate(&self) -> (u32, u32) {
        let bwe_kbps = (self.bwe.target_send_bps() / 1000) as u32;
        let mode = self.mode();

        match mode {
            PriorityMode::AudioFirst => {
                let audio = AUDIO_FLOOR_KBPS.min(bwe_kbps);
                let video = bwe_kbps.saturating_sub(audio);
                (audio, video)
            }
            PriorityMode::VideoFirst => {
                // Video floor: enough for the lowest step (240x180 @ 5fps).
                let video_floor = STEP_TABLE.last().map(|s| s.min_budget_kbps).unwrap_or(50);
                let video = video_floor.min(bwe_kbps);
                let audio = bwe_kbps.saturating_sub(video);
                (audio, video)
            }
            PriorityMode::ScreenShare => {
                let audio = AUDIO_FLOOR_SCREENCAST_KBPS.min(bwe_kbps);
                let video = bwe_kbps.saturating_sub(audio);
                (audio, video)
            }
            PriorityMode::Balanced => {
                let audio = ((bwe_kbps as f64) * BALANCED_AUDIO_RATIO) as u32;
                let video = bwe_kbps.saturating_sub(audio);
                (audio, video)
            }
        }
    }

    /// Map a video budget to a `(bitrate_kbps, width, height, fps)` target.
    ///
    /// Uses the static step table.  If budget is below the lowest step,
    /// returns [`VideoTarget::DISABLED`].
    fn derive_target(&self, video_budget_kbps: u32) -> VideoTarget {
        for step in STEP_TABLE {
            if video_budget_kbps >= step.min_budget_kbps {
                return VideoTarget {
                    bitrate_kbps: video_budget_kbps,
                    fps: step.fps,
                    width: step.width,
                    height: step.height,
                };
            }
        }
        VideoTarget::DISABLED
    }

    /// Smooth the target to avoid jumps larger than `MAX_CHANGE_RATIO_PER_SEC`
    /// over one second.
    ///
    /// `dt_ms` is the elapsed time since the last tick.
    fn smooth(&self, raw: VideoTarget, dt_ms: u32) -> VideoTarget {
        if raw.bitrate_kbps == 0 {
            return raw;
        }

        let last = *self.last_target.lock().unwrap();
        if last.bitrate_kbps == 0 {
            return raw;
        }

        let dt_s = dt_ms as f64 / 1000.0;
        let max_ratio = MAX_CHANGE_RATIO_PER_SEC.powf(dt_s);

        let min_br = (last.bitrate_kbps as f64 / max_ratio) as u32;
        let max_br = (last.bitrate_kbps as f64 * max_ratio) as u32;

        let clamped_br = raw.bitrate_kbps.clamp(min_br, max_br);

        VideoTarget {
            bitrate_kbps: clamped_br,
            ..raw
        }
    }

    /// Run one controller tick.
    ///
    /// `now_ms` is a monotonic timestamp (e.g. `timestamp_ms` from the media
    /// pipeline).  Returns the current [`VideoTarget`] which the caller should
    /// pass to the encoder.
    pub fn tick(&self, now_ms: u32) -> VideoTarget {
        let (_audio_budget, video_budget) = self.allocate();
        let raw = self.derive_target(video_budget);

        let prev = self.last_tick_ms.swap(now_ms, Relaxed);
        let dt_ms = if prev == 0 {
            1000
        } else {
            now_ms.saturating_sub(prev)
        };

        let smoothed = self.smooth(raw, dt_ms);
        *self.last_target.lock().unwrap() = smoothed;
        smoothed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_bwe(bps: u64) -> Arc<BandwidthEstimator> {
        let bwe = BandwidthEstimator::new((bps / 1000) as f64, 10.0, 100_000.0);
        // Seed cwnd so target_send_bps() returns a non-zero value.
        // cwnd_bps = cwnd_bytes * 8 / rtt_s.  For 1s RTT: cwnd_bytes = bps / 8.
        let cwnd_bytes = bps / 8;
        bwe.update_from_path(cwnd_bytes, 0, 1000);
        Arc::new(bwe)
    }

    #[test]
    fn audio_first_reserves_floor() {
        let bwe = dummy_bwe(100_000); // 100 kbps
        let ctrl = VideoQualityController::new(bwe);
        let (audio, video) = ctrl.allocate();
        // BWE target is 90% of raw = 90 kbps.
        assert_eq!(audio, 24, "audio floor is 24 kbps");
        assert_eq!(video, 66, "video gets remainder after 90% BWE factor");
    }

    #[test]
    fn audio_first_floor_not_below_bwe() {
        let bwe = dummy_bwe(10_000); // 10 kbps
        let ctrl = VideoQualityController::new(bwe);
        let (audio, video) = ctrl.allocate();
        // BWE target is 90% of raw = 9 kbps.
        assert_eq!(audio, 9, "audio cannot exceed bwe");
        assert_eq!(video, 0, "video gets nothing");
    }

    #[test]
    fn screen_share_clamps_audio() {
        let bwe = dummy_bwe(200_000); // 200 kbps
        let ctrl = VideoQualityController::new(bwe);
        ctrl.set_mode(PriorityMode::ScreenShare);
        let (audio, video) = ctrl.allocate();
        // BWE target is 90% of raw = 180 kbps.
        assert_eq!(audio, 16, "screen-share audio clamped to 16 kbps");
        assert_eq!(video, 164);
    }

    #[test]
    fn balanced_split() {
        let bwe = dummy_bwe(1_000_000); // 1 Mbps
        let ctrl = VideoQualityController::new(bwe);
        ctrl.set_mode(PriorityMode::Balanced);
        let (audio, video) = ctrl.allocate();
        // BWE target is 90% of raw = 900 kbps.
        assert_eq!(audio, 135, "15% of 900 kbps = 135 kbps audio");
        assert_eq!(video, 765);
    }

    #[test]
    fn derive_target_disabled_below_floor() {
        let bwe = dummy_bwe(1_000_000);
        let ctrl = VideoQualityController::new(bwe);
        let target = ctrl.derive_target(10); // below lowest step (50 kbps)
        assert_eq!(target, VideoTarget::DISABLED);
    }

    #[test]
    fn derive_target_lowest_step() {
        let bwe = dummy_bwe(1_000_000);
        let ctrl = VideoQualityController::new(bwe);
        let target = ctrl.derive_target(50);
        assert_eq!(target.width, 240);
        assert_eq!(target.height, 180);
        assert_eq!(target.fps, 5);
    }

    #[test]
    fn derive_target_highest_step() {
        let bwe = dummy_bwe(10_000_000); // 10 Mbps
        let ctrl = VideoQualityController::new(bwe);
        let target = ctrl.derive_target(5000);
        assert_eq!(target.width, 1280);
        assert_eq!(target.height, 720);
        assert_eq!(target.fps, 30);
    }

    #[test]
    fn smoothing_limits_jump() {
        let bwe = dummy_bwe(10_000_000);
        let ctrl = VideoQualityController::new(bwe);

        // First tick establishes baseline at 720p.
        let t0 = ctrl.tick(0);
        assert!(t0.bitrate_kbps > 0);

        // Simulate a BWE drop from 10 Mbps to 1 Mbps.
        let bwe2 = dummy_bwe(1_000_000);
        let ctrl2 = VideoQualityController::new(bwe2);
        // Pre-seed last_target so smoothing has something to compare against.
        *ctrl2.last_target.lock().unwrap() = VideoTarget {
            bitrate_kbps: 4000,
            ..VideoTarget::DISABLED
        };
        ctrl2.last_tick_ms.store(0, Relaxed);

        let t1 = ctrl2.tick(1000); // 1 s later
        // Max change per second is 2x, so 4000 -> min 2000.
        assert!(
            t1.bitrate_kbps >= 2000,
            "smoothing should prevent >2x drop in 1s"
        );
        // Raw budget after 1 Mbps drop is ~900 kbps; smoothing clamps to 2000.
        assert!(
            t1.bitrate_kbps < 4000,
            "smoothing should also cap upward jumps"
        );
    }

    #[test]
    fn mode_roundtrip() {
        let bwe = dummy_bwe(1_000_000);
        let ctrl = VideoQualityController::new(bwe);
        assert_eq!(ctrl.mode(), PriorityMode::AudioFirst);
        ctrl.set_mode(PriorityMode::ScreenShare);
        assert_eq!(ctrl.mode(), PriorityMode::ScreenShare);
    }
}
