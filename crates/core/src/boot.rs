//! Bringing the core up: what the desktop's Tauri `setup` hook used to do inline, minus the
//! windows. The desktop calls these in order around its own window/tray/menu work
//! (`src-tauri/src/lib.rs`); the iOS app calls [`start_core`] (UniFFI `EncoreCore::new`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use innertube::{Clients, InnerTube, Locale, Session};
use player::{Player, PlayerEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::cipher::{CipherDeobfuscator, PlayerConfigStore};
use crate::db::Db;
use crate::host::{Host, HostExt, MediaControls};
use crate::orchestrator::Orchestrator;
use crate::potoken::PoTokenGenerator;
use crate::state::{self, AppState};

/// A startup failure the app cannot run past: `title` for the dialog, `detail` for the log.
#[derive(Debug)]
pub struct Fatal {
    pub title: &'static str,
    pub detail: String,
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.title, self.detail)
    }
}

/// Open (or create) `encore.sqlite` in `data_dir`, quarantining an unreadable one.
pub fn open_db(data_dir: &Path) -> Result<Arc<Db>, Fatal> {
    let (db, quarantined) = Db::open_or_quarantine(&data_dir.join("encore.sqlite")).map_err(|e| {
        Fatal {
            title: "Spotless could not open or create its database",
            detail: format!("{}: {e}", data_dir.display()),
        }
    })?;
    // Log only: nothing tells the UI yet, so to the user this is an empty library. A
    // notice needs a startup-event path to the UI, and there is none to reuse.
    if let Some(aside) = &quarantined {
        tracing::warn!("started with a fresh database; the previous one is at {}", aside.display());
    }
    Ok(Arc::new(db))
}

/// The proxy to use, and point the shared HTTP client at it (before the first fetch).
///
/// `LIMUSIC_PROXY` overrides the stored setting, so a region-locked surface can be tested for one
/// run without a system-wide VPN (CLAUDE.md).
pub fn resolve_proxy(db: &Db) -> Option<String> {
    let proxy = std::env::var("LIMUSIC_PROXY")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| db.get_setting("proxy").filter(|p| !p.trim().is_empty()))
        // The setting is free text from the UI. An unparseable one used to fail
        // `InnerTube::new` below, and that `expect` bricks the app: no window, so no way
        // to reach settings and undo it. Drop it once here, for every consumer.
        .filter(|p| match reqwest::Proxy::all(p.as_str()) {
            Ok(_) => true,
            // Scheme only: the URI can carry credentials in its userinfo (see http.rs).
            Err(e) => {
                let scheme = p.split_once("://").map_or("(none)", |(s, _)| s);
                tracing::warn!(scheme, "unusable proxy setting, going direct: {e}");
                false
            }
        });
    // Before the first fetch: the shared client builds itself on first use.
    crate::http::set_proxy(proxy.as_deref());
    proxy
}

/// Everything [`assemble`] built that the host may still want to hold on to.
pub struct Booted {
    pub state: Arc<AppState>,
    pub cipher: Arc<CipherDeobfuscator>,
    pub potoken: Arc<PoTokenGenerator>,
    /// The player's events, for [`start`] to pump.
    pub events: Option<UnboundedReceiver<PlayerEvent>>,
    /// The saved volume the player was opened at.
    pub volume: i64,
    needs_visitor_bootstrap: bool,
    visitor_for_prewarm: Option<String>,
}

/// Build the transport, the player and the extraction stack, and the [`AppState`] over them.
/// `media` is the OS media widget, if the host has one (`MediaControls`).
pub fn assemble(
    host: Arc<dyn Host>,
    db: Arc<Db>,
    proxy: Option<&str>,
    media: Option<Box<dyn MediaControls>>,
) -> Result<Booted, Fatal> {
    let data_dir = host.data_dir();
    let cache_dir = host.cache_dir();
    std::fs::create_dir_all(&cache_dir).ok();

    // ── stream: import2 ── cookies into the secrets store (secrets.rs), before the first read.
    crate::secrets::install(host.secret_store());
    db.seal_secrets();
    // Session bootstrap (context/15 startup ordering): load the persisted login session
    // (cookie/dataSyncId/visitorData) from settings; fetch visitorData anonymously
    // (context/04 §A) only if we've never stored one.
    let cookie = db.get_setting("session_cookie").filter(|s| !s.is_empty());
    let data_sync_id = state::persisted_data_sync_id(&db);
    let visitor_data = db.get_setting("visitor_data").filter(|s| !s.is_empty());
    // First run (no stored visitorData): bootstrap it in the background after the window is
    // up, rather than blocking setup on a network GET (up to 60s on a bad connection). See
    // [`start`].
    let needs_visitor_bootstrap = visitor_data.is_none();
    if cookie.is_some() {
        tracing::info!("loaded persisted login session");
    }

    let visitor_for_prewarm = visitor_data.clone();
    let session = Session { locale: Locale::default(), visitor_data, data_sync_id, cookie };
    let it = InnerTube::new(session, proxy).map_err(|e| Fatal {
        title: "Spotless could not start its network client",
        detail: e.to_string(),
    })?;
    // Shelf titles, mood chips and playlist subtitles are YouTube's text, so the UI's
    // language has to go out with the request (#274). Persisted rather than pushed from the
    // UI at startup, because the first home fetch is already in flight by the time the
    // webview could tell us; the UI writes it whenever it changes (`set_setting`).
    if let Some(hl) = db.get_setting("locale") {
        it.set_locale(&hl);
    }
    it.set_hide_videos(db.get_setting("hide_videos").as_deref() == Some("true"));
    it.set_blocked(crate::blocked::block_list(&db));
    let clients = Clients::bundled();

    let mut player = Player::new(&cache_dir.to_string_lossy()).map_err(|e| Fatal {
        title: "Spotless could not load libmpv, which it uses to play audio",
        detail: format!(
            "{e}. On Linux, install your distribution's mpv library \
             (Fedora: mpv-libs, Debian/Ubuntu: libmpv2)."
        ),
    })?;
    // The audio bytes are the one request that never went through the proxy setting (#241).
    if let Err(e) = player.set_http_proxy(proxy) {
        tracing::warn!("mpv refused the proxy setting: {e}");
    }
    // Before anything can play: the first track of a restored queue has to come out at the
    // level the user left, not at 100.
    let volume = state::saved_volume(&db);
    let _ = player.set_volume(volume);
    player.set_crossfade(state::saved_crossfade(&db));
    crate::audio::apply_saved(&player, &db); // stream: audio (EQ + output device)
    let events = player.take_events().ok_or_else(|| Fatal {
        title: "Spotless could not start its audio event loop",
        detail: "the player's event channel was already taken, which is a bug".into(),
    })?;

    // Phase 2 extraction stack: cipher (host JS engine) + PoToken behind the orchestrator.
    let config = Arc::new(PlayerConfigStore::new(&data_dir));
    let cipher = Arc::new(CipherDeobfuscator::new(host.js_engine(), &data_dir, config));
    let potoken = Arc::new(PoTokenGenerator::new(db.clone()));
    let orchestrator =
        Arc::new(Orchestrator::new(it.clone(), clients.clone(), cipher.clone(), potoken.clone()));

    // MPRIS starts at 1.0 and would show 100% until the first change.
    if let Some(m) = &media {
        m.set_volume(volume);
    }

    let st = Arc::new(AppState::new(
        it,
        clients,
        player,
        db,
        host,
        orchestrator,
        cache_dir,
        media,
    ));
    state::install(&st);
    Ok(Booted {
        state: st,
        cipher,
        potoken,
        events: Some(events),
        volume,
        needs_visitor_bootstrap,
        visitor_for_prewarm,
    })
}

/// The background services that hang off the state: smart/Made for you, podcasts, polish,
/// downloads, the audio loops and the two loopback proxies. Desktop: right after `app.manage`.
pub fn start_services(b: &Booted) {
    let st = &b.state;
    let db = &st.db;
    // ── stream: smart ── shuffle/taste read settings and plays without an AppState.
    crate::smart::init(db.clone());
    // ── stream: coverfix ── dated Made for you cover files → stable names, before any
    // cached Home (or page) asks for them.
    crate::smart::mfy::migrate_cover_files(db, &crate::local::covers_dir(&*st.host).join("mfy"));
    crate::smart::mfy::spawn(); // stream: smart2 (Made for you + What's New refresh)
    // ── stream: podcasts ── tables, progress tracker, background refresh of follows.
    crate::podcasts::init(db.clone());
    crate::audiobooks::init(db); // stream: audiobooks (saved books table)
    // ── stream: polish ── context log, RSS→YouTube twins; extraction health at startup.
    crate::polish::init(db.clone());
    crate::polish::health::start(st.clone());
    crate::downloads::init(st.clone()); // stream: downloads (queue, sync, offline)
    crate::audio::start(st.clone()); // stream: audio (device watch, sleep timer)

    // The player view's <video> pulls its bytes from Rust over loopback, so the webview
    // never sees a googlevideo URL (context/11). videoproxy.rs explains why a socket and
    // not a custom scheme.
    crate::videoproxy::start(st.clone());

    // mpv's audio goes through a second loopback socket so the open-ended range ffmpeg
    // sends becomes bounded ranges upstream, which is the difference between 32 KB/s and
    // several MB/s on the same URL. audioproxy.rs has the measurements.
    crate::audioproxy::start();

    // Local music artwork reaches the webview over the asset protocol, whose configured
    // scope is empty — the folders it may read are the ones the user picked (local.rs).
    crate::local::allow_music_paths(&*st.host, db);

    // Device sync: last, because its change-log triggers go on tables every module above made.
    crate::sync::start(st);
}

/// Restore the queue, bootstrap visitorData / the client version, pump player events, and run the
/// cipher/PoToken prewarm and idle-teardown loops. Desktop: at the end of `setup`.
pub fn start_playback(b: &mut Booted) {
    // Restore the last session's queue, then optionally start playback.
    {
        let st = b.state.clone();
        crate::rt::spawn(async move {
            st.restore_queue().await;
            st.play_on_start().await;
        });
    }

    // First-run visitorData bootstrap, off the startup path. `set_visitor_data` writes
    // through the shared session (Arc<RwLock>), so the orchestrator's InnerTube clone sees
    // it; resolves degrade gracefully (no PoToken) until it lands. context/04 §A.
    // ── stream: reliability ── the same request also carries the live WEB_REMIX client
    // version, cached for a day, so `clients.json` going stale never matters. The stored
    // visitorData is kept: it is this install's identity and YouTube's anonymous
    // personalisation hangs off it.
    let st = &b.state;
    crate::reliability_boot::apply_cached_version(&st.it, &st.db);
    let needs_visitor_bootstrap = b.needs_visitor_bootstrap;
    if needs_visitor_bootstrap || crate::reliability_boot::version_is_stale(&st.db) {
        let st = st.clone();
        let potoken = b.potoken.clone();
        crate::rt::spawn(async move {
            match st.it.fetch_web_config().await {
                Ok(cfg) => {
                    crate::reliability_boot::store_version(
                        &st.it,
                        &st.db,
                        cfg.web_remix_version.as_deref(),
                    );
                    if needs_visitor_bootstrap {
                        let vd = cfg.visitor_data;
                        st.it.set_visitor_data(Some(vd.clone()));
                        st.db.set_setting("visitor_data", &vd);
                        tracing::info!("visitorData bootstrapped (background)");
                        potoken.prewarm(&vd).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "web config bootstrap failed (continuing)")
                }
            }
        });
    }
    // ── end stream: reliability ──

    // Pump mpv events → UI events + queue advance. context/11 events, context/14 §TrackEnded.
    if let Some(events) = b.events.take() {
        spawn_event_pump(b.state.clone(), events);
    }

    // Prewarm the JS contexts off the first-play path (context/04 §startup). The delays let
    // the host's event loop come up first (a webview needs it pumping).
    {
        let cipher = b.cipher.clone();
        crate::rt::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            cipher.prewarm().await;
        });
    }
    if let Some(vd) = b.visitor_for_prewarm.clone() {
        let potoken = b.potoken.clone();
        crate::rt::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            potoken.prewarm(&vd).await;
        });
    }
    // Mint-and-destroy policy (Phase-0 decision), now applied to the BotGuard V8 isolate
    // rather than a webview. Measured: the live isolate costs ~92 MB RSS, of which a
    // teardown returns ~39 MB (the rest is V8 platform + arenas, retained for the life of
    // the process once BotGuard has run at all).
    //
    // The idle window has to outlast a track, not a track gap. The gapless lookahead mints
    // for the next track seconds after the current one starts, so the runtime then sits
    // idle for the whole song: at 60s it was torn down and rebuilt once per track, and a
    // cold bootstrap (~0.8-2.3s) landed on the critical path whenever a track started from
    // a stop. 10 minutes covers any normal song, so continuous playback keeps one isolate,
    // and the memory still comes back ten minutes after the user stops listening.
    //
    // The cipher webview rides the same tick, for the same reason: it is a whole
    // `WebKitWebProcess` (91 MiB PSS / 234 MiB RSS measured on Fedora) held for two
    // functions that run once per track resolve. Not on Windows, where rebuilding it
    // freezes the app: see `CipherDeobfuscator::teardown_if_idle` (issue #288).
    {
        let potoken = b.potoken.clone();
        let cipher = b.cipher.clone();
        crate::rt::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                potoken.teardown_if_idle(Duration::from_secs(600)).await;
                cipher.teardown_if_idle(Duration::from_secs(600)).await;
            }
        });
    }
}

/// The whole core, for a host with no windows of its own to set up in between (iOS): open the
/// database in `host.data_dir()`, assemble, start everything.
pub fn start_core(host: Arc<dyn Host>) -> Result<Booted, Fatal> {
    let data_dir: PathBuf = host.data_dir();
    std::fs::create_dir_all(&data_dir).ok();
    let db = open_db(&data_dir)?;
    let proxy = resolve_proxy(&db);
    let mut b = assemble(host, db, proxy.as_deref(), None)?;
    start_services(&b);
    start_playback(&mut b);
    Ok(b)
}

/// Decide whether a position tick is worth forwarding to the UI. Passes ~4 Hz of steady
/// playback through, plus any discontinuity (seek/track change) immediately so the slider
/// never lags a jump. Pure so it's testable; the pump owns the state.
// ponytail: fixed 250ms cadence; make it adaptive only if someone ever wants sub-second UI time.
pub struct PositionThrottle {
    last_emit: std::time::Instant,
    last_pos: f64,
}

impl Default for PositionThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl PositionThrottle {
    pub fn new() -> Self {
        Self {
            last_emit: std::time::Instant::now() - std::time::Duration::from_secs(1),
            last_pos: f64::NAN,
        }
    }
    pub fn should_emit(&mut self, pos: f64, now: std::time::Instant) -> bool {
        let dt = now.duration_since(self.last_emit);
        // A jump is any move that couldn't be normal playback since the last emit (+0.75s slack).
        let jumped =
            self.last_pos.is_nan() || (pos - self.last_pos).abs() > dt.as_secs_f64() + 0.75;
        // stream: firstaudio. A track (or a seek to its top) just started: the first tick that
        // shows it moving goes out at once, so the clock starts with the sound instead of up to
        // 250 ms after it. Once per start: after it `last_pos` is past the line.
        let started = self.last_pos <= 0.05 && pos > 0.05;
        if jumped || started || dt >= std::time::Duration::from_millis(250) {
            self.last_emit = now;
            self.last_pos = pos;
            return true;
        }
        false
    }
}

/// mpv events → UI events (through the host) + queue advance.
pub fn spawn_event_pump(state: Arc<AppState>, mut events: UnboundedReceiver<PlayerEvent>) {
    crate::rt::spawn(async move {
        let host = state.host.clone();
        let mut throttle = PositionThrottle::new();
        while let Some(ev) = events.recv().await {
            match ev {
                PlayerEvent::Position(p) => {
                    crate::firstaudio::position(p); // stream: firstaudio
                    // ── stream: instant ── the outgoing track's last ticks, under the new title.
                    if state.is_loading() {
                        continue;
                    }
                    if throttle.should_emit(p, std::time::Instant::now()) {
                        let _ = host.emit("position", serde_json::json!({ "position": p }));
                    }
                    state.on_position(p).await;
                }
                PlayerEvent::Duration(d) => {
                    let _ = host.emit("duration", serde_json::json!({ "duration": d }));
                    state.on_duration(d).await;
                }
                PlayerEvent::Playing(playing) => {
                    let _ = host.emit("playback-state", if playing { "playing" } else { "paused" });
                    if !playing {
                        state.flush_position(); // persist exact resume position on pause
                        let _ = host.emit(
                            "position",
                            serde_json::json!({ "position": state.current_position() }),
                        );
                    }
                    state.media_set_playing(playing);
                    // Keep the tray's toggle label honest — this arm is the same chokepoint
                    // MPRIS uses, so tray state can't drift from media-key state.
                    host.playing_changed(playing);
                }
                PlayerEvent::Paused(_) => {}
                PlayerEvent::TrackEnded => {
                    state.on_track_ended().await;
                }
                PlayerEvent::Looped => state.on_looped().await,
                PlayerEvent::TrackFailed(msg) => {
                    // The track died (dead/403 URL etc). on_track_failed records a WEB_REMIX 403
                    // (context/06 §2), evicts the poisoned cache, and retries the track once via
                    // the fallback clients — only toast the error if it gave up (advanced, or stopped after a run of failures).
                    //
                    // Read the client before the retry: it advances the queue when it gives up,
                    // and which client served the dead URL is the one fact a bug report has to
                    // carry. Nobody can read the log on Windows (no console, no log file), so it
                    // goes in the message the user can see and copy. Issue #71.
                    let client = state.current_stream_client().await;
                    tracing::warn!(error = %msg, client = ?client, "track failed");
                    use crate::state::FailOutcome;
                    let msg = match state.on_track_failed().await {
                        FailOutcome::Retried => None,
                        FailOutcome::Advanced => Some(match client {
                            Some(c) => format!("{msg} ({c})"),
                            None => msg,
                        }),
                        // One friendly line, not the last track's mpv error.
                        FailOutcome::Halted => Some(crate::state::CANT_PLAY_MESSAGE.to_owned()),
                    };
                    if let Some(msg) = msg {
                        let _ = host.emit("playback-error", serde_json::json!({ "message": msg }));
                    }
                }
                PlayerEvent::AudioDeviceLost => {
                    tracing::warn!("audio device unavailable, holding the queue where it is");
                    state.on_audio_device_lost().await;
                    let _ = host.emit(
                        "playback-error",
                        serde_json::json!({
                            "message": "Your audio device is unavailable. Press play once it's back."
                        }),
                    );
                }
                PlayerEvent::LookaheadFailed(msg) => {
                    // No toast: the user is still hearing the current track and nothing they can
                    // see has gone wrong. The only thing owed is the eviction.
                    tracing::warn!(error = %msg, "lookahead preload failed");
                    state.on_lookahead_failed().await;
                }
                PlayerEvent::VideoFailed(audio) => state.on_video_failed(&audio).await,
                PlayerEvent::VideoDropped(audio) => state.on_video_dropped(&audio),
                PlayerEvent::Error(msg) => {
                    tracing::error!(error = %msg, "player error");
                    let _ = host.emit("playback-error", serde_json::json!({ "message": msg }));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::PositionThrottle;
    use std::time::{Duration, Instant};

    #[test]
    fn steady_playback_throttles_to_250ms() {
        let mut t = PositionThrottle::new();
        let base = Instant::now();
        // First tick ever → emitted regardless of cadence.
        assert!(t.should_emit(1.0, base));
        // 100ms later, small forward move → still within the 250ms window, suppressed.
        assert!(!t.should_emit(1.1, base + Duration::from_millis(100)));
        assert!(!t.should_emit(1.2, base + Duration::from_millis(200)));
        // 250ms accumulated since last emit → emitted again.
        assert!(t.should_emit(1.25, base + Duration::from_millis(250)));
    }

    #[test]
    fn a_starting_track_shows_its_first_moving_tick_at_once() {
        let mut t = PositionThrottle::new();
        let base = Instant::now();
        assert!(t.should_emit(0.0, base));
        assert!(!t.should_emit(0.03, base + Duration::from_millis(30)));
        // First tick past 50 ms of audio: out at once, not 250 ms after the 0.
        assert!(t.should_emit(0.06, base + Duration::from_millis(60)));
        // Then the normal cadence.
        assert!(!t.should_emit(0.1, base + Duration::from_millis(100)));
        assert!(!t.should_emit(0.25, base + Duration::from_millis(250)));
        assert!(t.should_emit(0.32, base + Duration::from_millis(320)));
    }

    #[test]
    fn forward_jump_emits_immediately() {
        let mut t = PositionThrottle::new();
        let base = Instant::now();
        assert!(t.should_emit(10.0, base));
        // 50ms later but position jumped +30s (e.g. media-key seek) → emit despite short dt.
        assert!(t.should_emit(40.0, base + Duration::from_millis(50)));
    }

    #[test]
    fn backward_jump_emits_immediately() {
        let mut t = PositionThrottle::new();
        let base = Instant::now();
        assert!(t.should_emit(60.0, base));
        // 50ms later but position jumped -30s → emit despite short dt.
        assert!(t.should_emit(30.0, base + Duration::from_millis(50)));
    }

    #[test]
    fn first_tick_ever_emits() {
        let mut t = PositionThrottle::new();
        // NaN last_pos (fresh throttle) → always emits on the very first tick, even at t=now.
        assert!(t.should_emit(5.0, Instant::now()));
    }
}
