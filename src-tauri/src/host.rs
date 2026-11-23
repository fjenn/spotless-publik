//! The desktop's [`encore_core::Host`]: the core's events go out through Tauri's `emit_str`, its
//! JavaScript engine is the hidden cipher webview (`webview.rs`), and the desktop-only hooks reach
//! the tray, the menu bar, the asset-protocol scope and the track notifications.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use encore_core::js::JsEngine;
use encore_core::{Host, MediaControls};
use tauri::{AppHandle, Emitter, Manager};

pub struct TauriHost {
    app: AppHandle,
    data_dir: PathBuf,
}

impl TauriHost {
    pub fn new(app: AppHandle, data_dir: PathBuf) -> Self {
        Self { app, data_dir }
    }
}

impl Host for TauriHost {
    fn emit_json(&self, event: &str, json: String) {
        let _ = self.app.emit_str(event, json);
    }

    fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    fn download_dir(&self) -> Option<PathBuf> {
        self.app.path().download_dir().ok()
    }

    fn js_engine(&self) -> Option<Arc<dyn JsEngine>> {
        Some(Arc::new(crate::webview::WebviewEngine::new(self.app.clone())))
    }

    fn allow_asset_file(&self, path: &Path) {
        let _ = self.app.asset_protocol_scope().allow_file(path);
    }

    fn allow_asset_dir(&self, path: &Path) {
        let _ = self.app.asset_protocol_scope().allow_directory(path, true);
    }

    fn track_changed(&self, title: &str, artists: &str, even_focused: bool) {
        crate::notify::track_changed(&self.app, title, artists, even_focused);
    }

    fn now_playing_changed(&self, title: &str, artists: &str) {
        crate::tray::set_track(&self.app, title, artists);
    }

    fn playing_changed(&self, playing: bool) {
        crate::tray::set_playing(&self.app, playing);
    }

    fn offline_mode_changed(&self, on: bool) {
        crate::menu::sync_offline_mode(&self.app, on);
    }
}

impl MediaControls for crate::media::MediaHandle {
    fn set_metadata(&self, title: &str, artist: &str, album: Option<&str>, cover: Option<&str>) {
        crate::media::MediaHandle::set_metadata(self, title, artist, album, cover)
    }
    fn set_album(&self, album: &str) {
        crate::media::MediaHandle::set_album(self, album)
    }
    fn set_duration(&self, secs: f64, speed: f64) {
        crate::media::MediaHandle::set_duration(self, secs, speed)
    }
    fn set_playback(&self, playing: bool, pos: f64, speed: f64) {
        crate::media::MediaHandle::set_playback(self, playing, pos, speed)
    }
    fn set_volume(&self, volume: i64) {
        crate::media::MediaHandle::set_volume(self, volume)
    }
}
