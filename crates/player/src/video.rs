//! Music video, played by mpv itself.
//!
//! The picture is one more track of the audio file mpv is already playing (`video-add`), so picture
//! and sound run off one clock: a seek, a pause or a tempo change moves both, and there is nothing
//! to keep in step. mpv draws it one of two ways, and nothing here knows what it draws into:
//!
//! - through its render API into a GL surface the app owns (a GtkGLArea on Linux,
//!   `src-tauri/src/nativevideo.rs`): the app hands in a GL loader and a wake-up, and calls
//!   [`VideoRenderer::render`] on its GL thread;
//! - with its own video output, into a native window the app hands over
//!   ([`Player::set_video_window`], Windows: `src-tauri/src/nativevideo_windows.rs`).
//!
//! Videos are keyed by the audio URL mpv was handed, never by position in mpv's playlist, so the
//! gapless-next track's video can be registered minutes early and attaches itself the moment that
//! file becomes the one playing.

use std::collections::VecDeque;
use std::ffi::{c_void, CStr};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};

use crate::{quoted, Decks, Error, Player};

/// The playing track and the gapless-next one are all that matter; the rest covers a skip landing
/// while a resolve is in flight.
const MAX_VIDEOS: usize = 4;

#[derive(Default)]
pub(crate) struct Videos {
    /// `(audio URL exactly as mpv was handed it, video URL)`, newest last.
    by_audio: VecDeque<(String, String)>,
    /// The file each deck has open, as of its last `FileLoaded`.
    loaded: [Option<String>; 2],
    /// The audio file whose picture stalled playback and was dropped (see [`watch`]). Its
    /// picture is not selected again until the view asks for it anew.
    dropped: Option<String>,
}

impl Videos {
    /// Remember `video` for `audio`, replacing an older entry for the same file.
    fn insert(&mut self, audio: &str, video: &str) {
        self.by_audio.retain(|(a, _)| a != audio);
        if self.by_audio.len() >= MAX_VIDEOS {
            self.by_audio.pop_front();
        }
        self.by_audio.push_back((audio.to_owned(), video.to_owned()));
    }

    /// The file `deck` has open had its picture dropped.
    fn dropped_on(&self, deck: usize) -> bool {
        self.dropped.is_some() && self.dropped == self.loaded[deck]
    }

    fn video_for(&self, deck: usize) -> Option<String> {
        let path = self.loaded[deck].as_deref()?;
        self.by_audio.iter().rev().find(|(a, _)| a == path).map(|(_, v)| v.clone())
    }
}

/// The display connection the app's GL context runs on. mpv needs it for zero-copy hardware
/// decoding (VA-API); without it that falls back to decoding on the CPU, which still plays.
pub enum GlDisplay {
    X11(*const c_void),
    Wayland(*const c_void),
}

impl Player {
    /// `video_url` is the picture for the audio file mpv was (or will be) handed as `audio_url`.
    /// Attached now if that file is the one playing, otherwise when it starts.
    pub fn set_video_for(&self, audio_url: &str, video_url: &str) {
        let deck = self.decks.active.load(Ordering::SeqCst);
        let now = {
            let mut v = self.decks.videos.lock().unwrap();
            v.insert(audio_url, video_url);
            v.loaded[deck].as_deref() == Some(audio_url)
        };
        if now {
            add_video(&self.decks, deck, audio_url.to_owned(), video_url.to_owned());
        }
    }

    /// The file mpv is playing, exactly as it was handed it.
    pub fn current_path(&self) -> Option<String> {
        self.mpv().get_property::<String>("path").ok()
    }

    /// Whether anyone can see the picture. Off, mpv decodes no video at all; back on, it picks the
    /// track up again at the current position, already in step.
    ///
    /// Shown again after being hidden, a picture dropped for stalling is tried once more (the
    /// user switched back to video, or the connection is back), and the stall watch restarts.
    pub fn set_video_visible(&self, on: bool) {
        if self.decks.video_visible.swap(on, Ordering::SeqCst) != on {
            if on {
                self.decks.videos.lock().unwrap().dropped = None;
            }
            apply_vid(&self.decks);
            if on {
                watch(self.decks.clone());
            } else {
                self.decks.video_watch.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// The playing file's picture was dropped because it stalled playback (see [`watch`]).
    pub fn video_dropped(&self) -> bool {
        let deck = self.decks.active.load(Ordering::SeqCst);
        self.decks.videos.lock().unwrap().dropped_on(deck)
    }

    /// Draw the picture into the native window `wid` (mpv's `wid`, an HWND on Windows) with mpv's
    /// own video output, instead of through the render API. Every deck, including the crossfade
    /// deck built later. An error leaves the render API output in place, which with no context
    /// shows nothing: never a window of mpv's own.
    pub fn set_video_window(&self, wid: i64) -> Result<(), Error> {
        for deck in 0..2 {
            if let Some(m) = self.decks.mpv(deck) {
                embed(m, wid)?;
            }
        }
        self.decks.wid.store(wid, Ordering::SeqCst);
        Ok(())
    }

    /// Whether the deck being heard has its video output up, i.e. mpv's window (black until a
    /// frame) is in the one handed to [`Player::set_video_window`].
    pub fn video_output_up(&self) -> bool {
        self.mpv().get_property::<bool>("vo-configured").unwrap_or(false)
    }

    /// The picture's health, read off the deck being heard: for a UI that waits for the first
    /// frame, and for the A/V sync measurements (docs/streams/ios-video.md).
    pub fn video_stats(&self) -> VideoStats {
        let m = self.mpv();
        let f = |k: &str| m.get_property::<f64>(k).ok();
        let i = |k: &str| m.get_property::<i64>(k).ok();
        VideoStats {
            output_up: m.get_property::<bool>("vo-configured").unwrap_or(false),
            width: i("video-out-params/w").unwrap_or(0),
            height: i("video-out-params/h").unwrap_or(0),
            avsync: f("avsync"),
            frames_dropped: i("frame-drop-count").unwrap_or(0)
                + i("vo-drop-frame-count").unwrap_or(0),
            frames_delayed: i("vo-delayed-frame-count").unwrap_or(0),
            fps: f("estimated-vf-fps").or_else(|| f("container-fps")),
            hwdec: m.get_property::<String>("hwdec-current").unwrap_or_default(),
            codec: m.get_property::<String>("video-codec").unwrap_or_default(),
        }
    }

    /// A copy of the picture shrunk to at most `max_width` pixels wide, for a glow drawn around it.
    /// `None` with no picture up, or when playback still sits at `unless_at` (paused on the frame
    /// already taken). Costs a full-size conversion inside mpv plus a pass over it here, so it is
    /// for a caller that asks a few times a second, not per frame.
    pub fn video_thumbnail(&self, max_width: usize, unless_at: Option<f64>) -> Option<Thumbnail> {
        let mpv = self.mpv();
        let pts = mpv.get_property::<f64>("time-pos").ok()?;
        // No picture out yet (the video track is still being added): mpv would refuse the
        // screenshot and log an error for every ask.
        if unless_at == Some(pts) || mpv.get_property::<i64>("video-out-params/w").is_err() {
            return None;
        }
        let (width, height, rgba) = screenshot_shrunk(mpv, max_width)?;
        Some(Thumbnail { pts, width, height, rgba })
    }

    /// The renderer, for the app's GL thread. `get_proc_address` resolves GL functions for the
    /// context that thread renders with; `wake` is called from any thread when there is a new frame
    /// to draw (or a new deck to give a context), and must get [`VideoRenderer::render`] called
    /// soon on the GL thread. Creates nothing yet: see [`VideoRenderer::ensure_contexts`].
    pub fn video_renderer(
        &self,
        get_proc_address: fn(&(), &str) -> *mut c_void,
        display: Option<GlDisplay>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> VideoRenderer {
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let w = wake.clone();
        let _ = self.decks.on_new_deck.set(Box::new(move || w()));
        VideoRenderer {
            ctx: [None, None],
            decks: self.decks.clone(),
            get_proc_address,
            display,
            wake,
        }
    }
}

/// `vid` on every deck: on only for the one being heard, and only while someone can see it.
///
/// Under the `videos` lock, like the selection after a `video-add` (see [`add_video`]): the two
/// race, and whichever writes last has to be the one that read the current visibility.
///
/// In a native window (`wid`) mpv's window *is* the picture, and the app's window is see-through
/// around it, so `force-window` follows `vid`: the output stays up, black, between two tracks and
/// while the next one's video is still being added, instead of closing and letting the desktop
/// show through. Off with the picture, which closes the output and frees what it held.
pub(crate) fn apply_vid(decks: &Decks) {
    let serial = decks.videos.lock().unwrap();
    let active = decks.active.load(Ordering::SeqCst);
    let on = decks.video_visible.load(Ordering::SeqCst);
    let embedded = decks.wid.load(Ordering::SeqCst) != 0;
    for deck in 0..2 {
        if let Some(m) = decks.mpv(deck) {
            let show = on && deck == active && !serial.dropped_on(deck);
            if embedded {
                let _ = m.set_property("force-window", if show { "yes" } else { "no" });
            }
            let _ = m.set_property("vid", if show { "auto" } else { "no" });
        }
    }
}

/// Point one deck at the native window `wid`. The window first: an output switched to before it
/// would open a window of its own. `video-timing-offset` goes back to mpv's default, which the
/// render API setup in `new_mpv` zeroed for its own reasons.
pub(crate) fn embed(mpv: &libmpv2::Mpv, wid: i64) -> Result<(), Error> {
    mpv.set_property("wid", wid)?;
    // iOS: `wid` is a CAMetalLayer, which only Vulkan through MoltenVK can draw into (MPVKit has
    // no GL output on iOS). VideoToolbox decodes, and libplacebo maps its frames without a copy.
    #[cfg(target_os = "ios")]
    {
        mpv.set_property("gpu-api", "vulkan")?;
        mpv.set_property("gpu-context", "moltenvk")?;
        let _ = mpv.set_property("hwdec", "videotoolbox");
    }
    mpv.set_property("vo", "gpu-next,gpu")?;
    let _ = mpv.set_property("video-timing-offset", 0.05);
    Ok(())
}

/// See [`Player::video_stats`].
#[derive(Debug, Clone, Default)]
pub struct VideoStats {
    /// mpv's video output is open (it is only while the picture is selected).
    pub output_up: bool,
    /// The decoded picture's size; 0 until the first frame.
    pub width: i64,
    pub height: i64,
    /// mpv's `avsync`: the last displayed frame's distance from the audio clock, in seconds.
    pub avsync: Option<f64>,
    /// Frames dropped by the decoder plus by the output, since the file opened.
    pub frames_dropped: i64,
    /// Frames the output showed late.
    pub frames_delayed: i64,
    pub fps: Option<f64>,
    /// The hardware decoder in use (`videotoolbox`), empty for software decoding.
    pub hwdec: String,
    pub codec: String,
}

// ── stream: video-pods ── Picture in Picture on iOS.
//
// mpv draws through Metal, and a backgrounded app may not use the GPU, so PiP cannot keep mpv's
// own output. While PiP is up the active deck switches to `vo=null` with copy-back decoding: mpv
// still decodes and times every frame against the audio clock, nothing touches the GPU, and the app
// pulls each frame as BGRA ([`Player::pip_frame`]) into the `AVSampleBufferDisplayLayer` that PiP
// shows. Back in the app it returns to `gpu-next` and hardware decoding.

/// One decoded frame for Picture in Picture: BGRA, rows top-down, `width * 4` bytes a row, alpha
/// opaque.
pub struct PipFrame {
    /// The playback position the frame belongs to, to hand back as `unless_at`.
    pub pts: f64,
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u8>,
}

impl Player {
    /// Picture in Picture on or off: see the block comment above. No-op without a video window.
    pub fn set_video_pip(&self, on: bool) {
        if self.decks.wid.load(Ordering::SeqCst) == 0 {
            return;
        }
        let deck = self.decks.active.load(Ordering::SeqCst);
        let Some(m) = self.decks.mpv(deck) else { return };
        let (vo, hwdec) = if on { ("null", "videotoolbox-copy,auto-copy") } else { ("gpu-next,gpu", PIP_OFF_HWDEC) };
        if let Err(e) = m.set_property("hwdec", hwdec) {
            tracing::warn!(error = %e, "pip: mpv refused hwdec={hwdec}");
        }
        if let Err(e) = m.set_property("vo", vo) {
            tracing::warn!(error = %e, "pip: mpv refused vo={vo}");
        }
        tracing::info!(on, "pip: video output switched");
    }

    /// The newest decoded frame, at most `max_width` wide (halved until it fits), or `None` with
    /// no picture, outside PiP mode, or when playback still sits at `unless_at`. mpv converts the frame on the CPU,
    /// so this works in `vo=null` and in the background.
    pub fn pip_frame(&self, max_width: usize, unless_at: Option<f64>) -> Option<PipFrame> {
        let mpv = self.mpv();
        // Only from `vo=null`: on `gpu-next` a screenshot is a GPU readback (MoltenVK aborted in
        // one on the simulator), and the switch lands a moment after `set_video_pip`.
        if mpv.get_property::<String>("current-vo").ok().as_deref() != Some("null") {
            return None;
        }
        let pts = mpv.get_property::<f64>("time-pos").ok()?;
        if unless_at == Some(pts) || mpv.get_property::<i64>("video-params/w").is_err() {
            return None;
        }
        let (width, height, bgra) = screenshot_bgra(mpv, max_width)?;
        Some(PipFrame { pts, width, height, bgra })
    }
}

#[cfg(target_os = "ios")]
const PIP_OFF_HWDEC: &str = "videotoolbox";
#[cfg(not(target_os = "ios"))]
const PIP_OFF_HWDEC: &str = "auto-safe";

/// `screenshot-raw video` as top-down, opaque BGRA, shrunk by [`shrink_bgra`].
fn screenshot_bgra(mpv: &libmpv2::Mpv, max_width: usize) -> Option<(usize, usize, Vec<u8>)> {
    with_screenshot(mpv, |d, w, h, stride| shrink_bgra(d, w, h, stride, max_width))
}

/// Run `f` over mpv's `screenshot-raw video` (`bgr0`): data, width, height, stride.
fn with_screenshot<T>(
    mpv: &libmpv2::Mpv,
    f: impl FnOnce(&[u8], usize, usize, usize) -> Option<T>,
) -> Option<T> {
    use libmpv2_sys as sys;
    let mut args = [c"screenshot-raw".as_ptr(), c"video".as_ptr(), std::ptr::null()];
    let mut node: sys::mpv_node = unsafe { std::mem::zeroed() };
    if unsafe { sys::mpv_command_ret(mpv.ctx.as_ptr(), args.as_mut_ptr(), &mut node) } < 0 {
        return None;
    }
    let (mut w, mut h, mut stride, mut data) = (0i64, 0i64, 0i64, None);
    if node.format == sys::mpv_format_MPV_FORMAT_NODE_MAP {
        unsafe {
            let list = &*node.u.list;
            for i in 0..list.num.max(0) as usize {
                let key = CStr::from_ptr(*list.keys.add(i)).to_bytes();
                let v = &*list.values.add(i);
                match (key, v.format) {
                    (b"w", sys::mpv_format_MPV_FORMAT_INT64) => w = v.u.int64,
                    (b"h", sys::mpv_format_MPV_FORMAT_INT64) => h = v.u.int64,
                    (b"stride", sys::mpv_format_MPV_FORMAT_INT64) => stride = v.u.int64,
                    (b"data", sys::mpv_format_MPV_FORMAT_BYTE_ARRAY) => {
                        let ba = &*v.u.ba;
                        data = Some(std::slice::from_raw_parts(ba.data as *const u8, ba.size));
                    }
                    _ => {}
                }
            }
        }
    }
    let out = data.and_then(|d| f(d, w.max(0) as usize, h.max(0) as usize, stride.max(0) as usize));
    unsafe { sys::mpv_free_node_contents(&mut node) };
    out
}

/// A `bgr0` image averaged down by the power of two that brings it to at most `max_width` wide,
/// as top-down BGRA with opaque alpha (`bgr0`'s fourth byte is undefined).
fn shrink_bgra(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    max_width: usize,
) -> Option<(usize, usize, Vec<u8>)> {
    let mut f = 1;
    while w / f > max_width.max(1) {
        f *= 2;
    }
    let (lw, lh) = (w / f, h / f);
    if lw == 0 || lh == 0 || stride < w * 4 || src.len() < stride * (h - 1) + w * 4 {
        return None;
    }
    let mut out = vec![0u8; lw * lh * 4];
    if f == 1 {
        for y in 0..lh {
            let row = &src[y * stride..y * stride + w * 4];
            let dst = &mut out[y * lw * 4..(y + 1) * lw * 4];
            dst.copy_from_slice(row);
            for px in dst.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        return Some((lw, lh, out));
    }
    let n = (f * f) as u32;
    for oy in 0..lh {
        for ox in 0..lw {
            let mut acc = [0u32; 3];
            for y in oy * f..(oy + 1) * f {
                let at = y * stride + ox * f * 4;
                for px in src[at..at + f * 4].chunks_exact(4) {
                    acc[0] += px[0] as u32;
                    acc[1] += px[1] as u32;
                    acc[2] += px[2] as u32;
                }
            }
            let o = (oy * lw + ox) * 4;
            out[o..o + 4].copy_from_slice(&[
                (acc[0] / n) as u8,
                (acc[1] / n) as u8,
                (acc[2] / n) as u8,
                255,
            ]);
        }
    }
    Some((lw, lh, out))
}
// ── end stream: video-pods ──

/// See [`Player::video_thumbnail`].
pub struct Thumbnail {
    /// The playback position it was taken at, to hand back as `unless_at`.
    pub pts: f64,
    pub width: usize,
    pub height: usize,
    /// RGBA, rows bottom-up (GL's order, as the Linux GL readback produces them).
    pub rgba: Vec<u8>,
}

/// `screenshot-raw` of the decoded frame (no OSD, mpv's default `bgr0`), shrunk with [`shrink`]
/// before mpv's full-size copy is freed.
///
/// Rendered by the video output, so it works on hardware-decoded frames, which mpv's software
/// screenshot refuses (NVDEC answered `MPV_ERROR_COMMAND`). Measured on Linux/NVIDIA, 720p VP9:
/// ~8.5 ms of CPU a grab, and the call returns with mpv's next frame (~40 ms at 24 fps). The
/// software path costs ~6 ms but only with copy-back decoding of every frame, so it is not used.
fn screenshot_shrunk(mpv: &libmpv2::Mpv, max_width: usize) -> Option<(usize, usize, Vec<u8>)> {
    with_screenshot(mpv, |d, w, h, stride| shrink(d, w, h, stride, max_width))
}

/// A `bgr0` image averaged down by the power of two that brings it to at most `max_width` wide
/// (the same halving the Linux GPU chain does, so the glow looks the same), as RGBA rows
/// bottom-up. Every source pixel counts: a sparse sample of a frame shimmers.
fn shrink(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    max_width: usize,
) -> Option<(usize, usize, Vec<u8>)> {
    let mut f = 1;
    while w / f > max_width.max(1) {
        f *= 2;
    }
    let (lw, lh) = (w / f, h / f);
    if lw == 0 || lh == 0 || stride < w * 4 || src.len() < stride * (h - 1) + w * 4 {
        return None;
    }
    let n = (f * f) as u32;
    let mut out = vec![0u8; lw * lh * 4];
    for oy in 0..lh {
        let dst_row = (lh - 1 - oy) * lw * 4;
        for ox in 0..lw {
            let mut acc = [0u32; 3];
            for y in oy * f..(oy + 1) * f {
                let at = y * stride + ox * f * 4;
                for px in src[at..at + f * 4].chunks_exact(4) {
                    acc[0] += px[2] as u32;
                    acc[1] += px[1] as u32;
                    acc[2] += px[0] as u32;
                }
            }
            let o = dst_row + ox * 4;
            out[o..o + 4].copy_from_slice(&[
                (acc[0] / n) as u8,
                (acc[1] / n) as u8,
                (acc[2] / n) as u8,
                255,
            ]);
        }
    }
    Some((lw, lh, out))
}

/// A deck finished opening a file. Reads which one on its own thread: the caller is the event
/// loop, and a synchronous property read there can stall the pump mid-transition.
pub(crate) fn file_loaded(decks: &Arc<Decks>, deck: usize) {
    let Some(mpv) = decks.mpv(deck).cloned() else { return };
    let decks = decks.clone();
    let _ = std::thread::Builder::new().name("mpv-video".into()).spawn(move || {
        let path = mpv.get_property::<String>("path").ok();
        let video = {
            let mut v = decks.videos.lock().unwrap();
            v.loaded[deck] = path.clone();
            (deck == decks.active.load(Ordering::SeqCst)).then(|| v.video_for(deck)).flatten()
        };
        if let (Some(audio), Some(url)) = (path, video) {
            add_video(&decks, deck, audio, url);
        }
    });
}

/// A crossfade made the other deck the one being heard: move the picture over with the sound.
pub(crate) fn deck_swapped(decks: &Arc<Decks>) {
    apply_vid(decks);
    let deck = decks.active.load(Ordering::SeqCst);
    let (audio, video) = {
        let v = decks.videos.lock().unwrap();
        (v.loaded[deck].clone(), v.video_for(deck))
    };
    if let (Some(audio), Some(url)) = (audio, video) {
        add_video(decks, deck, audio, url);
    }
}

/// `video-add`, off the calling thread: it returns only once mpv has opened the file, which is a
/// network round trip (mpv opens it on a thread of its own, so playback never waits on it).
///
/// Always added unselected, and selected afterwards by id if someone can see it by then. Choosing
/// `select` or `auto` up front read the visibility before the round trip: the view asking for the
/// picture during it set `vid=auto` while there was no track to pick, the track then arrived
/// unselected, and setting `auto` again is a no-op in mpv. That was the black box after a track
/// change or an opened link, which only closing and reopening the view (`no` then `auto`) cleared.
///
/// `audio` is the file the picture belongs to, which a failure names.
fn add_video(decks: &Arc<Decks>, deck: usize, audio: String, url: String) {
    let Some(mpv) = decks.mpv(deck).cloned() else { return };
    let decks = decks.clone();
    let _ = std::thread::Builder::new().name("mpv-video-add".into()).spawn(move || {
        if let Err(e) = mpv.command("video-add", &[&quoted(&url), "auto"]) {
            tracing::warn!(deck, error = %e, "video: mpv could not open the picture");
            let _ = decks.tx.send(crate::PlayerEvent::VideoFailed(audio));
            return;
        }
        let serial = decks.videos.lock().unwrap();
        let shown = decks.video_visible.load(Ordering::SeqCst)
            && decks.active.load(Ordering::SeqCst) == deck
            && !serial.dropped_on(deck);
        let track = shown.then(|| newest_video_track(&mpv)).flatten();
        let _ = match track {
            Some(id) => mpv.set_property("vid", id),
            None => mpv.set_property("vid", "no"),
        };
        tracing::debug!(deck, ?track, "video: attached");
    });
}

/// How long playback may sit still, with the picture selected and the sound itself ready, before
/// the picture is blamed and dropped.
const STALL_LIMIT: std::time::Duration = std::time::Duration::from_millis(2500);
const WATCH_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// The picture is a second stream mpv reads alongside the sound (`video-add`), and mpv only plays
/// when every selected stream has data. After a seek it has to fetch the video at the new offset
/// over the network: offline, or on a dead or crawling link, that never comes and the sound waits
/// with it, silent, while the very same file plays at once with the picture off (`vid=no`). Seen
/// with a downloaded podcast episode in video mode: scrub, and playback stops until audio-only.
///
/// So while the picture is shown, a thread watches the deck being heard. Playing, not paused, the
/// picture selected, and the clock stuck (or mpv paused for cache) for [`STALL_LIMIT`] while the
/// sound itself is not the one waiting (a local file, or seconds of audio already buffered): the
/// picture is to blame. It is dropped for that file, the sound plays on, and the app is told.
pub(crate) fn watch(decks: Arc<Decks>) {
    let gen = decks.video_watch.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = std::thread::Builder::new().name("mpv-video-watch".into()).spawn(move || {
        let mut last: Option<f64> = None;
        let mut stuck = std::time::Duration::ZERO;
        loop {
            std::thread::sleep(WATCH_TICK);
            if decks.video_watch.load(Ordering::SeqCst) != gen
                || !decks.video_visible.load(Ordering::SeqCst)
            {
                return;
            }
            let deck = decks.active.load(Ordering::SeqCst);
            let Some(mpv) = decks.mpv(deck).cloned() else { continue };
            let selected = mpv.get_property::<String>("vid").map_or(false, |v| v != "no");
            let paused = mpv.get_property::<bool>("pause").unwrap_or(true);
            let idle = mpv.get_property::<bool>("idle-active").unwrap_or(true);
            let pos = mpv.get_property::<f64>("time-pos").ok();
            let waiting = mpv.get_property::<bool>("paused-for-cache").unwrap_or(false);
            let moved = match (last, pos) {
                (Some(a), Some(b)) => (b - a).abs() > 0.02,
                _ => false,
            };
            last = pos;
            if !selected || paused || idle || (moved && !waiting) {
                stuck = std::time::Duration::ZERO;
                continue;
            }
            stuck += WATCH_TICK;
            if stuck < STALL_LIMIT {
                continue;
            }
            stuck = std::time::Duration::ZERO;
            let Some(path) = mpv.get_property::<String>("path").ok() else { continue };
            if !sound_ready(&mpv, &path) {
                continue;
            }
            {
                let mut v = decks.videos.lock().unwrap();
                if v.loaded[deck].as_deref() != Some(path.as_str()) {
                    continue;
                }
                v.dropped = Some(path.clone());
            }
            tracing::warn!(deck, "video: the picture stalled playback, dropped; the sound plays on");
            apply_vid(&decks);
            let _ = decks.tx.send(crate::PlayerEvent::VideoDropped(path));
        }
    });
}

/// The sound is not what playback is waiting for: a file on disk, or a stream with seconds of
/// audio already buffered ahead (mpv's `demuxer-cache-duration` is the main, audio, demuxer's).
fn sound_ready(mpv: &libmpv2::Mpv, path: &str) -> bool {
    is_local(path) || mpv.get_property::<f64>("demuxer-cache-duration").map_or(false, |d| d >= 2.0)
}

fn is_local(path: &str) -> bool {
    !path.contains("://") || path.starts_with("file://")
}

/// The id of the last video track in the file, which is the one `video-add` just appended.
fn newest_video_track(mpv: &libmpv2::Mpv) -> Option<i64> {
    let n = mpv.get_property::<i64>("track-list/count").ok()?;
    (0..n).rev().find_map(|i| {
        let kind = mpv.get_property::<String>(&format!("track-list/{i}/type")).ok()?;
        if kind != "video" {
            return None;
        }
        mpv.get_property::<i64>(&format!("track-list/{i}/id")).ok()
    })
}

/// mpv's render contexts, one per deck, living on the app's GL thread.
pub struct VideoRenderer {
    // Before `decks`: fields drop in order, and mpv requires every render context to be freed
    // before its core is destroyed, which dropping the last `Decks` would do.
    ctx: [Option<RenderContext>; 2],
    decks: Arc<Decks>,
    get_proc_address: fn(&(), &str) -> *mut c_void,
    display: Option<GlDisplay>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl VideoRenderer {
    /// Give every deck that exists a render context. On the GL thread, with its context current,
    /// and before any picture is asked for: a deck with no context cannot start its video output.
    pub fn ensure_contexts(&mut self) -> Result<(), Error> {
        for deck in 0..2 {
            if self.ctx[deck].is_some() {
                continue;
            }
            let Some(mpv) = self.decks.mpv(deck).cloned() else { continue };
            let mut params = vec![
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address: self.get_proc_address,
                    ctx: (),
                }),
            ];
            match self.display {
                Some(GlDisplay::X11(p)) => params.push(RenderParam::X11Display(p)),
                Some(GlDisplay::Wayland(p)) => params.push(RenderParam::WaylandDisplay(p)),
                None => {}
            }
            // SAFETY: the handle is live for as long as `self.decks` is, and the field order above
            // frees this context first.
            let handle = unsafe { &mut *mpv.ctx.as_ptr() };
            let mut ctx = RenderContext::new(handle, params)?;
            let wake = self.wake.clone();
            ctx.set_update_callback(move || wake());
            self.ctx[deck] = Some(ctx);
        }
        Ok(())
    }

    /// Draw the playing deck's current frame into `fbo` (`width` x `height` pixels). On the GL
    /// thread, with its context current.
    pub fn render(&mut self, fbo: i32, width: i32, height: i32) -> Result<(), Error> {
        self.ensure_contexts()?;
        let deck = self.decks.active.load(Ordering::SeqCst);
        if let Some(ctx) = &self.ctx[deck] {
            // Flipped: a GL framebuffer's origin is bottom-left, a video frame's top-left.
            ctx.render::<()>(fbo, width, height, true)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A video belongs to the file a deck has open, by exact URL, and a re-resolve replaces it.
    #[test]
    fn local_files_are_told_from_streams() {
        assert!(super::is_local("/var/mobile/Containers/Data/Application/x/Downloads/ep.m4a"));
        assert!(super::is_local("file:///tmp/ep.webm"));
        assert!(super::is_local("C:\\Users\\me\\Music\\ep.m4a"));
        assert!(!super::is_local("http://127.0.0.1:8123/stream/abc"));
        assert!(!super::is_local("https://rr1---sn.googlevideo.com/videoplayback?x"));
    }

    #[test]
    fn a_dropped_picture_belongs_to_its_file_only() {
        let mut v = super::Videos::default();
        v.loaded[0] = Some("a".into());
        v.loaded[1] = Some("b".into());
        assert!(!v.dropped_on(0));
        v.dropped = Some("a".into());
        assert!(v.dropped_on(0));
        assert!(!v.dropped_on(1));
        v.loaded[0] = Some("c".into());
        assert!(!v.dropped_on(0), "the next file's picture is not dropped");
    }

    #[test]
    fn a_video_follows_its_audio_file() {
        let mut v = Videos::default();
        v.insert("a1", "v1");
        v.insert("a2", "v2");
        assert_eq!(v.video_for(0), None, "nothing loaded yet");
        v.loaded[0] = Some("a2".into());
        v.loaded[1] = Some("a1".into());
        assert_eq!(v.video_for(0).as_deref(), Some("v2"));
        assert_eq!(v.video_for(1).as_deref(), Some("v1"));
        v.insert("a2", "v2b");
        assert_eq!(v.video_for(0).as_deref(), Some("v2b"));
        assert_eq!(v.by_audio.len(), 2);
        for i in 0..MAX_VIDEOS {
            v.insert(&format!("x{i}"), "x");
        }
        assert_eq!(v.by_audio.len(), MAX_VIDEOS);
        assert_eq!(v.video_for(0), None, "evicted by newer tracks");
    }

    /// bgr0 in, RGBA out, each output pixel the mean of its block, rows flipped to bottom-up, and
    /// a padded stride skipped rather than read as pixels.
    #[test]
    fn shrink_averages_swaps_and_flips() {
        // 4x4, stride 20 (a 4-byte pad per row, filled with junk). Top half blue-ish, bottom red.
        let px = |b: u8, g: u8, r: u8| [b, g, r, 0];
        let mut src = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                let p = if y < 2 { px(200 + x as u8, 10, 0) } else { px(0, 20, 100) };
                src.extend_from_slice(&p);
            }
            src.extend_from_slice(&[99; 4]);
        }
        let (w, h, out) = shrink(&src, 4, 4, 20, 2).unwrap();
        assert_eq!((w, h), (2, 2));
        // Bottom-up: the first row out is the source's bottom (red) half.
        assert_eq!(&out[0..8], &[100, 20, 0, 255, 100, 20, 0, 255]);
        // Top row: blue averages 200,201 then 202,203 (integer mean).
        assert_eq!(&out[8..16], &[0, 10, 200, 255, 0, 10, 202, 255]);
        // Already narrow enough: copied through, just swapped and flipped.
        let (w, h, _) = shrink(&src, 4, 4, 20, 128).unwrap();
        assert_eq!((w, h), (4, 4));
        assert!(shrink(&src, 4, 4, 12, 2).is_none(), "a stride shorter than a row is refused");
        assert!(shrink(&src[..40], 4, 4, 20, 2).is_none(), "a short buffer is refused");
    }

    /// stream: video-pods. PiP frames stay top-down BGRA, with opaque alpha, averaged the same way.
    #[test]
    fn shrink_bgra_keeps_order_and_makes_alpha_opaque() {
        let mut src = Vec::new();
        for y in 0..4u8 {
            for x in 0..4u8 {
                src.extend_from_slice(&[10 * y, x, 7, 0]);
            }
            src.extend_from_slice(&[99; 4]);
        }
        let (w, h, out) = shrink_bgra(&src, 4, 4, 20, 128).unwrap();
        assert_eq!((w, h), (4, 4));
        assert_eq!(&out[0..4], &[0, 0, 7, 255], "top row first, alpha opaque");
        assert_eq!(&out[12 * 4..12 * 4 + 4], &[30, 0, 7, 255]);
        let (w, h, out) = shrink_bgra(&src, 4, 4, 20, 2).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&out[0..4], &[5, 0, 7, 255]); // mean of rows 0-1, cols 0-1
        assert_eq!(&out[12..16], &[25, 2, 7, 255]); // rows 2-3, cols 2-3 (x mean 2.5 floors)
        assert!(shrink_bgra(&src[..40], 4, 4, 20, 2).is_none());
    }
}
