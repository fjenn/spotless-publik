//! Music videos on iOS (stream ios-video): mpv draws the picture itself, into a `CAMetalLayer` the
//! app owns, with the same mpv instance that plays the sound. Picture and sound therefore share one
//! clock and the EQ chain stays in place; there is nothing to keep in step from Swift.
//!
//! The core already does the rest on every non-macOS host: a video row's picture is resolved and
//! handed to mpv as one more track of the playing file (`AppState::attach_video`,
//! `player::video`). The app only says where to draw and whether anyone can see it.

use crate::{on_rt, EncoreCore, R};

/// The picture's health, for the Now Playing view (first frame up) and the sync measurements.
#[derive(uniffi::Record)]
pub struct VideoStats {
    /// mpv's video output is open (only while the picture is selected).
    pub output_up: bool,
    /// The decoded picture's size; 0 until the first frame.
    pub width: i64,
    pub height: i64,
    /// mpv's `avsync` (seconds): the last frame's distance from the audio clock.
    pub avsync: Option<f64>,
    pub frames_dropped: i64,
    pub frames_delayed: i64,
    pub fps: Option<f64>,
    /// `videotoolbox` when the hardware decodes, empty for software.
    pub hwdec: String,
    pub codec: String,
    /// The playing file's picture stalled playback and was dropped: the sound plays on alone.
    /// Showing the picture again (visible off, then on) retries it.
    pub dropped: bool,
}

/// stream: video-pods. One frame for Picture in Picture: BGRA, rows top-down, `width * 4` a row.
#[derive(uniffi::Record)]
pub struct PipFrame {
    pub pts: f64,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[uniffi::export]
impl EncoreCore {
    /// Hand mpv the layer to draw into: the address of a `CAMetalLayer` (mpv's `wid`). Once per
    /// process, with a layer that lives as long as the process does: mpv keeps the raw pointer.
    pub fn set_video_layer(&self, layer: u64) -> R<()> {
        self.state.player.set_video_window(layer as i64).map_err(|e| crate::CoreError::Failed(e.to_string()))
    }

    /// Whether anyone can see the picture. Off, mpv decodes no video and closes its output (the
    /// background, a locked phone, Now Playing closed); back on, it picks the picture up at the
    /// current position, already in step with the sound.
    pub fn set_video_visible(&self, on: bool) {
        self.state.player.set_video_visible(on);
    }

    pub fn video_stats(&self) -> VideoStats {
        let s = self.state.player.video_stats();
        VideoStats {
            output_up: s.output_up,
            width: s.width,
            height: s.height,
            avsync: s.avsync,
            frames_dropped: s.frames_dropped,
            frames_delayed: s.frames_delayed,
            fps: s.fps,
            hwdec: s.hwdec,
            codec: s.codec,
            dropped: self.state.player.video_dropped(),
        }
    }

    /// The loopback URL of `video_id`'s already-resolved picture (the desktop `<video>` path),
    /// `None` until mpv's own picture resolved it. Only for the engine comparison (an AVPlayer on
    /// the same stream); the app itself never needs it.
    pub fn video_proxy_url(&self, video_id: String) -> Option<String> {
        self.state.video_url(&video_id)?;
        encore_core::videoproxy::url_for(&video_id)
    }

    /// Now Playing's "Switch to video" / "Switch to audio": play the current track's other
    /// version (its `counterpart`) from the same moment (`api::switch_version`, #442).
    pub async fn switch_version(&self) -> R<()> {
        let st = self.state.clone();
        on_rt(async move { encore_core::api::switch_version(&st).await }).await
    }

    /// stream: video-pods. Picture in Picture on or off. On, mpv stops drawing through Metal
    /// (`vo=null`, copy-back decoding: no GPU work in the background) and the app pulls frames with
    /// [`Self::pip_frame`]; off, it draws into the layer again.
    pub fn set_video_pip(&self, on: bool) {
        self.state.player.set_video_pip(on);
    }

    /// stream: video-pods. The newest frame, at most `max_width` wide, unless playback still sits
    /// at `unless_at` (the last frame's `pts`).
    pub fn pip_frame(&self, max_width: u32, unless_at: Option<f64>) -> Option<PipFrame> {
        let f = self.state.player.pip_frame(max_width as usize, unless_at)?;
        Some(PipFrame { pts: f.pts, width: f.width as u32, height: f.height as u32, bgra: f.bgra })
    }
}
