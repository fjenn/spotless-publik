//! The seam between the core and whatever app embeds it (docs/research/23-ios-plan.md §1.3).
//!
//! The core never talks to a window system. Everything it used to ask Tauri's `AppHandle` for goes
//! through [`Host`]: events out to the UI, the app's directories, the JavaScript engine the cipher
//! runs YouTube's `player.js` in, the secrets store, the network monitor, and a handful of
//! desktop-only hooks (tray label, asset-protocol scope, track notifications, the offline menu
//! item) that default to doing nothing.
//!
//! The desktop implements it in `src-tauri/src/host.rs` (`emit` → `AppHandle::emit_str`); the iOS
//! app implements it through UniFFI callbacks (`crates/ffi`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::js::JsEngine;

/// What the core needs from the app around it. Every method but [`Host::emit_json`] and
/// [`Host::data_dir`] has a default, so a minimal host is two methods long.
pub trait Host: Send + Sync + 'static {
    /// Send an event to the UI. `json` is the serialized payload, exactly what the desktop UI's
    /// `listen()` receives (the `api.ts` contract), so a Swift host can decode it with `Codable`.
    fn emit_json(&self, event: &str, json: String);

    /// Where the SQLite file, covers, player.js cache and downloads live.
    fn data_dir(&self) -> PathBuf;

    /// mpv's on-disk audio cache (wiped by "Clear caches"). iOS points this into `Library/Caches`.
    fn cache_dir(&self) -> PathBuf {
        self.data_dir().join("audio-cache")
    }

    /// The user's Downloads folder, for the library export's default path.
    fn download_dir(&self) -> Option<PathBuf> {
        None
    }

    /// The engine the signature/n cipher runs `player.js` in: a hidden webview on the desktop,
    /// JavaScriptCore on iOS. `None` leaves the cipher unavailable, so streams come from the
    /// clients that need none (VISIONOS first).
    fn js_engine(&self) -> Option<Arc<dyn JsEngine>> {
        None
    }

    /// The store sign-in cookies are sealed into. `None` keeps the built-in one (the macOS
    /// Keychain on a Mac, nothing elsewhere: the cookie then stays in SQLite).
    fn secret_store(&self) -> Option<Box<dyn crate::secrets::Store>> {
        None
    }

    /// Start watching the network: `cb(online, metered)` on every change. False when this
    /// platform has no monitor (the downloads manager then assumes online and unmetered).
    fn start_network_monitor(&self, cb: Box<dyn Fn(bool, bool) + Send + Sync>) -> bool {
        crate::downloads::net::start(cb)
    }

    /// The UI may load this local file (Tauri's asset-protocol scope). No-op elsewhere.
    fn allow_asset_file(&self, _path: &Path) {}

    /// The UI may load anything under this directory. No-op elsewhere.
    fn allow_asset_dir(&self, _path: &Path) {}

    /// A track started (desktop: the optional "now playing" notification). Only called when the
    /// user turned those notifications on; [`Self::now_playing_changed`] is the unconditional one.
    fn track_changed(&self, _title: &str, _artists: &str, _even_focused: bool) {}

    /// Every track start, notifications or not (desktop: the tray menu's "now playing" row).
    fn now_playing_changed(&self, _title: &str, _artists: &str) {}

    /// Playback started or paused (desktop: the tray menu's toggle label).
    fn playing_changed(&self, _playing: bool) {}

    /// Offline mode flipped (desktop: the menu bar's check mark).
    fn offline_mode_changed(&self, _on: bool) {}
}

/// The OS media widget (MPRIS / SMTC / NowPlaying on the desktop). iOS drives
/// `MPNowPlayingInfoCenter` from Swift off the `now-playing` events instead, so it passes none.
pub trait MediaControls: Send + Sync {
    fn set_metadata(&self, title: &str, artist: &str, album: Option<&str>, cover: Option<&str>);
    fn set_album(&self, album: &str);
    fn set_duration(&self, secs: f64, speed: f64);
    fn set_playback(&self, playing: bool, pos: f64, speed: f64);
    fn set_volume(&self, volume: i64);
}

type Listener = Box<dyn Fn(&str) + Send + Sync>;

/// In-process subscribers to the core's own events, called before the host hears them. The
/// downloads manager follows `library-changed` this way (it used to be Tauri's `listen_any`).
static LISTENERS: OnceLock<Mutex<Vec<(String, Arc<Listener>)>>> = OnceLock::new();

fn listeners() -> &'static Mutex<Vec<(String, Arc<Listener>)>> {
    LISTENERS.get_or_init(Default::default)
}

/// Call `f(json)` on every `event` the core emits from now on.
pub fn listen(event: &str, f: impl Fn(&str) + Send + Sync + 'static) {
    let f: Arc<Listener> = Arc::new(Box::new(f));
    listeners().lock().unwrap_or_else(|e| e.into_inner()).push((event.to_owned(), f));
}

/// Typed emit, the shape the code had under Tauri's `Emitter` (`let _ = host.emit(..)`).
pub trait HostExt {
    fn emit<S: serde::Serialize>(&self, event: &str, payload: S) -> Result<(), String>;
}

impl HostExt for dyn Host {
    fn emit<S: serde::Serialize>(&self, event: &str, payload: S) -> Result<(), String> {
        let json = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
        let hit: Vec<Arc<Listener>> = listeners()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(name, _)| name == event)
            .map(|(_, f)| f.clone())
            .collect();
        for f in hit {
            f(&json);
        }
        self.emit_json(event, json);
        Ok(())
    }
}

impl HostExt for Arc<dyn Host> {
    fn emit<S: serde::Serialize>(&self, event: &str, payload: S) -> Result<(), String> {
        HostExt::emit(&**self, event, payload)
    }
}

/// A host that drops every event, for tests and tools.
pub struct NullHost(pub PathBuf);

impl Host for NullHost {
    fn emit_json(&self, _event: &str, _json: String) {}
    fn data_dir(&self) -> PathBuf {
        self.0.clone()
    }
}

/// Request a cover big enough for a media widget or Discord card: stored thumbs are usually
/// row-sized (120px), which looks pixelated anywhere larger (#360). Mirrors `ui/src/lib/thumb.ts`:
/// only googleusercontent-style URLs carry their size in the URL; i.ytimg path-variant thumbs and
/// local `file://` covers pass through unchanged (other sizes can 404). 512 is the size Discord has
/// always requested, so it is known to be served.
pub fn cover_url(url: &str) -> String {
    static WH: OnceLock<regex::Regex> = OnceLock::new();
    static S: OnceLock<regex::Regex> = OnceLock::new();
    let wh = WH.get_or_init(|| regex::Regex::new(r"=w\d+-h\d+").expect("static regex"));
    let s = S.get_or_init(|| regex::Regex::new(r"=s\d+").expect("static regex"));
    if wh.is_match(url) {
        wh.replace(url, "=w512-h512").into_owned()
    } else if s.is_match(url) {
        s.replace(url, "=s512").into_owned()
    } else {
        url.to_owned()
    }
}
