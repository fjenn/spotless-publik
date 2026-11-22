//! Tauri commands — the ONLY API the UI calls. context/11 UI contract. Most are one-line wrappers
//! over `encore_core::api` (shared with iOS); the ones left with bodies here need the window
//! system (mini player, hotkeys, diagnostics, the updater, file drops).

use std::sync::Arc;

use innertube::{
    AlbumPage, ArtistPage, BrowseItem, HistoryGroup, HomePage, MoodSection, PlaylistContinuation,
    PlaylistPage, PlaylistSort, PlaylistSuggestions, Rating, SearchResults, SearchSuggestions,
    SongItem,
};
use encore_core::api::{self, *};
use encore_core::HostExt;
use tauri::{Manager, State};

use crate::blocked::BlockedArtist;
use crate::state::AppState;

type St<'a> = State<'a, Arc<AppState>>;

#[tauri::command]
pub async fn set_volume(state: St<'_>, volume: i64) -> Result<(), String> {
    state.player.set_volume(volume).map_err(|e| e.to_string())?;
    if volume > 0 {
        crate::hotkeys::LAST_NONZERO_VOLUME.store(volume, std::sync::atomic::Ordering::Relaxed);
    } else {
        crate::hotkeys::remember_unmute_level(&state.db);
    }
    // There is one volume and there can be two windows (the mini player). Without this the one
    // that didn't move the slider keeps showing the old level and lies about what you're hearing.
    let _ = state.host.emit("volume", volume);
    state.media_set_volume(volume);
    Ok(())
}

#[tauri::command]
pub async fn set_setting(
    app: tauri::AppHandle,
    state: St<'_>,
    key: String,
    value: String,
) -> Result<(), String> {
    if !UI_SETTINGS.contains(&key.as_str()) {
        return Err(format!("unknown setting: {key}"));
    }
    // Registers/removes the login autostart entry on toggle; the OS persists it from there, and
    // startup repoints an existing entry at the running binary (lib.rs). Before the write, so a
    // failure leaves the setting as it was. A dev build would register itself, and at login its
    // window needs a vite server that isn't running.
    if key == "autostart" {
        if tauri::is_dev() {
            return Err(
                "autostart: a dev build can't register itself, use an installed build".into()
            );
        }
        use tauri_plugin_autostart::ManagerExt;
        let al = app.autolaunch();
        let res = if value == "true" {
            al.enable()
        } else if al.is_enabled().unwrap_or(false) {
            al.disable()
        } else {
            Ok(())
        };
        res.map_err(|e| format!("autostart: {e}"))?;
    }
    api::set_setting(&state, &key, &value).await?;
    #[cfg(target_os = "linux")]
    if key == "ambient_light" {
        crate::set_webgl(&app, value == "true");
    }
    // Hand the frame back to the compositor (or take it again). macOS is not on this path: its
    // titlebar style is fixed at window creation, so the setting is hidden there.
    #[cfg(not(target_os = "macos"))]
    if key == "system_titlebar" {
        use tauri::Manager;
        if let Some(w) = app.get_webview_window("main") {
            w.set_decorations(value == "true").map_err(|e| format!("decorations: {e}"))?;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn get_global_hotkeys(
    hotkeys: State<'_, Arc<crate::hotkeys::HotkeysManager>>,
) -> Result<crate::hotkeys::HotkeysConfig, String> {
    Ok(hotkeys.get_config())
}

#[tauri::command]
pub async fn set_global_hotkeys(
    app: tauri::AppHandle,
    state: St<'_>,
    hotkeys: State<'_, Arc<crate::hotkeys::HotkeysManager>>,
    config: crate::hotkeys::HotkeysConfig,
) -> Result<crate::hotkeys::HotkeyRegisterResult, String> {
    let result = hotkeys.apply_config(&app, config);
    // Saved even on partial failure: apply_config already made this the live config, and a
    // combo another app holds shouldn't cost the user every other binding on the next launch.
    crate::hotkeys::save_config(&state.db, &result.config);
    Ok(result)
}

#[tauri::command]
pub async fn reset_global_hotkeys(
    app: tauri::AppHandle,
    state: St<'_>,
    hotkeys: State<'_, Arc<crate::hotkeys::HotkeysManager>>,
) -> Result<crate::hotkeys::HotkeyRegisterResult, String> {
    // Resets the bindings only: the default has hotkeys off, and the button is on the enabled page.
    let default_config = crate::hotkeys::HotkeysConfig {
        enabled: hotkeys.get_config().enabled,
        ..Default::default()
    };
    let result = hotkeys.apply_config(&app, default_config);
    crate::hotkeys::save_config(&state.db, &result.config);
    Ok(result)
}

// ── stream: p-settings ──
#[tauri::command]
pub async fn cache_size(state: St<'_>) -> Result<u64, String> {
    api::cache_size(&state).await
}

/// Open the in-app Google sign-in webview (context/15 Path A). Completes asynchronously; the UI
/// hears back via `auth-changed` + `login-done` (success), `login-error` or `login-cancelled`. With `add_account`, the webview
/// opens Google's AddSession screen so a second account can be added even when the webview
/// already holds a Google session.
#[tauri::command]
pub async fn login_webview(
    app: tauri::AppHandle,
    state: St<'_>,
    add_account: Option<bool>,
) -> Result<(), String> {
    let state = state.inner().clone();
    crate::session::open_login(app, state, add_account.unwrap_or(false));
    Ok(())
}

/// Close the sign-in window (Cancel on the main window's "Signing in" sheet).
#[tauri::command]
pub fn cancel_login(app: tauri::AppHandle) {
    crate::session::cancel_login(&app);
}

// --- mini player (mini.rs) ------------------------------------------------------------------

/// Open the floating widget next to the main window (which stays as it is).
#[tauri::command]
pub async fn open_mini(app: tauri::AppHandle) -> Result<(), String> {
    // GTK wants window creation on the main thread, so hop and post the result back rather than
    // logging a failure the user would only see as a click that did nothing.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = tx.send(crate::mini::open(&handle));
    })
    .map_err(|e| e.to_string())?;
    rx.await.map_err(|_| "the mini player never answered".to_string())?
}

/// Close the widget, leaving the main window as it was (see `mini::dismiss`).
#[tauri::command]
pub async fn close_mini(app: tauri::AppHandle) -> Result<(), String> {
    crate::mini::dismiss(&app);
    Ok(())
}

/// The widget's shrink/expand button (#301).
#[tauri::command]
pub async fn set_mini_compact(app: tauri::AppHandle, compact: bool) -> Result<(), String> {
    crate::mini::set_compact(&app, compact)
}

/// The arguments this process was launched with, handed over once (#348). See `LAUNCH_ARGS`.
#[tauri::command]
pub fn take_launch_args() -> Vec<String> {
    // The page listens for `open-link` from here on (LinkDialog subscribes, then takes these), so
    // later links go to it as events instead of piling up here (deeplink.rs).
    crate::deeplink::UI_READY.store(true, std::sync::atomic::Ordering::SeqCst);
    std::mem::take(&mut *crate::LAUNCH_ARGS.lock().unwrap())
}

#[tauri::command]
pub async fn show_main(state: St<'_>, window: tauri::WebviewWindow) -> Result<bool, String> {
    encore_core::state::UI_MOUNTED.notify_one();
    if crate::should_start_minimized(&state.db) {
        return Ok(false);
    }
    window.show().map_err(|e| e.to_string())?;
    window.unminimize().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    crate::tray::set_main_visible(window.app_handle(), true);
    Ok(true)
}

/// A file dropped on the window. A webview drop has the bytes but no path, so they come over as
/// the raw request body, with the name in `x-file-name` (percent-encoded: headers are ASCII).
#[tauri::command]
pub async fn import_read_file(
    request: tauri::ipc::Request<'_>,
) -> Result<crate::import::Preview, String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("unreadable".into());
    };
    let name = request
        .headers()
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| urlencoding::decode(v).ok())
        .map(|v| v.into_owned())
        .unwrap_or_default();
    crate::import::read_file(bytes, &name)
}

/// Whether this build can install an update itself, or only point the user at the download.
///
/// Tauri's Linux updater knows one trick: rewrite an AppImage in place. It takes the path from
/// `Env::appimage` and, when that is unset, falls back to `current_exe()` and writes the downloaded
/// AppImage bytes over whatever it finds there. On the `.rpm` and on distro packages (the AUR's
/// `limusic-bin`) that is a package-manager-owned `/usr/bin/limusic-app`: it fails on permissions
/// rather than doing damage, but offering the button at all is a lie. Those users update through
/// their package manager, so the UI shows them a download link instead.
///
/// Reads the same `Env::appimage` the updater plugin decides on, so the two cannot disagree.
#[tauri::command]
pub fn can_self_update(app: tauri::AppHandle) -> bool {
    #[cfg(target_os = "linux")]
    {
        use tauri::Manager;
        app.env().appimage.is_some()
    }
    // Windows runs the NSIS installer and macOS swaps the .app bundle; both work however the app
    // was installed.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        true
    }
}

/// Open a link from the UI in the real browser. An `<a href>` inside the webview would navigate
/// the app itself off the SPA, with no way back.
#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    open_browser(&url)
}

/// Hand an http(s) link to the default browser. Re-serialised through the URL parser, so what
/// reaches the OS is a plain URL whatever the caller passed in.
pub(crate) fn open_browser(url: &str) -> Result<(), String> {
    let url = tauri::Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .ok_or_else(|| "only http(s) links".to_string())?;
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg(url.as_str()).spawn().map(|_| ());
    // Windows: the shell's URL handler. Not `cmd /c start`, where an `&` in the query splits the
    // command line.
    #[cfg(target_os = "windows")]
    let cmd = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url.as_str()])
        .spawn()
        .map(|_| ());
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let cmd = std::process::Command::new("xdg-open").arg(url.as_str()).spawn().map(|_| ());
    cmd.map_err(|e| format!("Couldn't open the browser: {e}"))
}

/// Real quit from the UI (Ctrl+Q / Cmd+Q). Positions are saved by the Exit handler in lib.rs.
#[tauri::command]
pub fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

// --- Diagnostics ----------------------------------------------------------------------------

/// The bug-report blob for Settings ▸ About: environment header plus the redacted tail of
/// `limusic.log`. See `crate::diagnostics`.
#[tauri::command]
pub fn diagnostics(app: tauri::AppHandle, state: St<'_>) -> String {
    crate::diagnostics::report(&app, &state.db)
}

/// The environment block on its own, for prefilling the bug form's `system` field. GitHub's query
/// parameters only reach `input` and `textarea` fields, so this is how the app tells us what it is
/// running on without the user typing it.
#[tauri::command]
pub fn diagnostics_summary(app: tauri::AppHandle, state: St<'_>) -> String {
    crate::diagnostics::summary(&app, &state.db)
}

/// The same text written to a path the user picked in a save dialog, for attaching to an issue
/// when it is too long to paste comfortably.
#[tauri::command]
pub fn save_diagnostics(app: tauri::AppHandle, state: St<'_>, path: String) -> Result<(), String> {
    std::fs::write(&path, crate::diagnostics::report(&app, &state.db)).map_err(|e| e.to_string())
}

/// What double-clicking a title bar does (System Settings > Desktop & Dock): the raw
/// `AppleActionOnDoubleClick` value ("Maximize"/"Fill" zoom, "Minimize", "None"), or "Maximize"
/// when unset or off macOS. Read per double-click, so a change in Settings applies at once.
#[tauri::command]
pub fn title_bar_double_click() -> String {
    #[cfg(target_os = "macos")]
    {
        use objc2::rc::Retained;
        use objc2::runtime::{AnyObject, Bool};
        use objc2::{class, msg_send};
        // SAFETY: standard NSUserDefaults/NSString API; standardUserDefaults falls back to the
        // global domain, where this key lives.
        unsafe {
            let d: Retained<AnyObject> = msg_send![class!(NSUserDefaults), standardUserDefaults];
            let key: Retained<AnyObject> =
                msg_send![class!(NSString), stringWithUTF8String: c"AppleActionOnDoubleClick".as_ptr()];
            let v: Option<Retained<AnyObject>> = msg_send![&*d, stringForKey: &*key];
            if let Some(v) = v {
                let p: *const std::ffi::c_char = msg_send![&*v, UTF8String];
                if !p.is_null() {
                    return std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
                }
            }
            // Older macOS: a separate "minimise on double-click" switch.
            let key: Retained<AnyObject> =
                msg_send![class!(NSString), stringWithUTF8String: c"AppleMiniaturizeOnDoubleClick".as_ptr()];
            let min: Bool = msg_send![&*d, boolForKey: &*key];
            if min.as_bool() {
                return "Minimize".into();
            }
        }
    }
    "Maximize".into()
}

/// Theater mode's fullscreen toggle (#139).
///
/// `setFullscreen` on its own is not enough on Windows. tao decides the client area in
/// WM_NCCALCSIZE: while the real Win32 placement says maximized it clamps the client to the
/// monitor's *work* area, so the "fullscreen" window sits under the taskbar with a frame-thick
/// border around it, and while the window is undecorated-with-shadow it insets the client by the
/// frame thickness. Both are decided before the fullscreen flag is, and tao's own `is_maximized`
/// reads a cached flag that can disagree with the placement, which is why unmaximizing from the
/// UI only fixed it some of the time.
///
/// So: restore from the real placement, go fullscreen, then put the window on the monitor rect
/// with SWP_FRAMECHANGED to force one recalculation with the fullscreen flag set. On the main
/// thread, where the window messages run inline and the order is guaranteed.
///
/// A window that was fullscreen already when theater opened (macOS's green button, a window
/// manager's shortcut) is left alone both ways, so closing theater doesn't drop the user out of a
/// fullscreen they chose (#485).
#[tauri::command]
pub fn theater_fullscreen(window: tauri::WebviewWindow, on: bool) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, Ordering};

    static WAS_FULLSCREEN: AtomicBool = AtomicBool::new(false);
    if on {
        let already = window.is_fullscreen().unwrap_or(false);
        WAS_FULLSCREEN.store(already, Ordering::Relaxed);
        if already {
            return Ok(());
        }
    } else if WAS_FULLSCREEN.swap(false, Ordering::Relaxed) {
        return Ok(());
    }

    #[cfg(not(target_os = "windows"))]
    {
        window.set_fullscreen(on).map_err(|e| e.to_string())
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            IsZoomed, SetWindowPos, ShowWindow, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE,
            SWP_NOZORDER, SW_MAXIMIZE, SW_RESTORE,
        };

        /// Restoring the window is what makes fullscreen work, so theater has to put the
        /// maximized state back itself on the way out.
        static WAS_MAXIMIZED: AtomicBool = AtomicBool::new(false);

        let w = window.clone();
        window
            .run_on_main_thread(move || {
                let Ok(hwnd) = w.hwnd() else { return };
                if on {
                    let zoomed = unsafe { IsZoomed(hwnd).as_bool() };
                    WAS_MAXIMIZED.store(zoomed, Ordering::Relaxed);
                    if zoomed {
                        unsafe {
                            let _ = ShowWindow(hwnd, SW_RESTORE);
                        }
                    }
                    let _ = w.set_fullscreen(true);
                    if let Ok(Some(m)) = w.current_monitor() {
                        let (p, s) = (m.position(), m.size());
                        unsafe {
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                p.x,
                                p.y,
                                s.width as i32,
                                s.height as i32,
                                SWP_FRAMECHANGED | SWP_NOZORDER,
                            );
                        }
                    }
                } else {
                    let _ = w.set_fullscreen(false);
                    if WAS_MAXIMIZED.swap(false, Ordering::Relaxed) {
                        unsafe {
                            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
                        }
                    }
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
                        );
                    }
                }
            })
            .map_err(|e| e.to_string())
    }
}

// ── Tauri wrappers over encore_core::api ──

#[tauri::command]
pub async fn search(state: St<'_>, query: String, record_history: bool) -> Result<Vec<SongItem>, String> {
    api::search(&state, query, record_history).await
}

#[tauri::command]
pub fn search_blend_artist_top(query: String, songs: Vec<SongItem>, artist: String, top: Vec<SongItem>) -> Vec<SongItem> {
    api::search_blend_artist_top(query, songs, artist, top)
}

#[tauri::command]
pub async fn search_videos(state: St<'_>, query: String) -> Result<Vec<SongItem>, String> {
    api::search_videos(&state, query).await
}

#[tauri::command]
pub async fn song(state: St<'_>, video_id: String) -> Result<SongItem, String> {
    api::song(&state, video_id).await
}

#[tauri::command]
pub async fn search_all(state: St<'_>, query: String, record_history: bool) -> Result<SearchResults, String> {
    api::search_all(&state, query, record_history).await
}

#[tauri::command]
pub async fn search_suggestions(state: St<'_>, query: String) -> Result<SearchSuggestions, String> {
    api::search_suggestions(&state, query).await
}

// ── stream: search2 ── the All view in one call, and each chip as an endless list.
#[tauri::command]
pub async fn search_overview(state: St<'_>, query: String, record_history: bool) -> Result<encore_core::search::SearchOverview, String> {
    encore_core::search::search_overview(&state, query, record_history).await
}

#[tauri::command]
pub async fn search_page(state: St<'_>, query: String, filter: String, continuation: Option<String>) -> Result<encore_core::search::SearchPage, String> {
    encore_core::search::search_page(&state, query, filter, continuation).await
}

/// Songs by their words, run alongside `search_overview` and blended late (lyricsearch.md).
#[tauri::command]
pub async fn search_lyrics(state: St<'_>, query: String) -> Result<Vec<encore_core::search::LyricMatch>, String> {
    Ok(encore_core::search::search_lyrics(&state, query).await)
}

#[tauri::command]
pub fn search_blend_lyrics(query: String, songs: Vec<SongItem>, lyric: Vec<encore_core::search::LyricMatch>, interacted: bool, top_title: Option<String>) -> encore_core::search::LyricBlend {
    encore_core::search::blend_lyrics(&query, songs, lyric, interacted, top_title.as_deref())
}

#[tauri::command]
pub async fn search_cards(state: St<'_>, query: String, category: String) -> Result<Vec<BrowseItem>, String> {
    api::search_cards(&state, query, category).await
}

#[tauri::command]
pub async fn play(state: St<'_>, item: SongItem, from: Option<String>) -> Result<(), String> {
    api::play_single(&state, item, from).await
}

#[tauri::command]
pub fn hint_play(state: St<'_>, item: SongItem) {
    api::hint_play(&state, item)
}

#[tauri::command]
pub async fn play_index(state: St<'_>, index: usize, start: Option<f64>) -> Result<(), String> {
    api::play_index(&state, index, start).await
}

#[tauri::command]
pub async fn switch_version(state: St<'_>) -> Result<(), String> {
    api::switch_version(&state).await
}

#[tauri::command]
pub async fn remove_from_queue(state: St<'_>, index: usize) -> Result<(), String> {
    api::remove_from_queue(&state, index).await
}

#[tauri::command]
pub async fn play_next(state: St<'_>, items: Vec<SongItem>, from: Option<String>) -> Result<(), String> {
    api::play_next(&state, items, from).await
}

#[tauri::command]
pub async fn move_in_queue(state: St<'_>, from: usize, to: usize) -> Result<(), String> {
    api::move_in_queue(&state, from, to).await
}

#[tauri::command]
pub async fn add_to_queue(state: St<'_>, items: Vec<SongItem>, from: Option<String>, continuation: Option<String>) -> Result<(), String> {
    api::add_to_queue(&state, items, from, continuation).await
}

#[tauri::command]
pub async fn clear_queued(state: St<'_>) -> Result<(), String> {
    api::clear_queued(&state).await
}

#[tauri::command]
pub async fn next_track(state: St<'_>) -> Result<(), String> {
    api::next_track(&state).await
}

#[tauri::command]
pub async fn prev_track(state: St<'_>) -> Result<(), String> {
    api::prev_track(&state).await
}

#[tauri::command]
pub async fn back_to_previous(state: St<'_>) -> Result<(), String> {
    api::back_to_previous(&state).await
}

#[tauri::command]
pub async fn toggle_shuffle(state: St<'_>) -> Result<(), String> {
    api::toggle_shuffle(&state).await
}

#[tauri::command]
pub async fn set_repeat(state: St<'_>, mode: String) -> Result<(), String> {
    api::set_repeat(&state, mode).await
}

#[tauri::command]
pub async fn toggle_pause(state: St<'_>) -> Result<(), String> {
    api::toggle_pause(&state).await
}

#[tauri::command]
pub async fn seek(state: St<'_>, position: f64) -> Result<(), String> {
    api::seek(&state, position).await
}

#[tauri::command]
pub async fn playback_clock(state: St<'_>) -> Result<Option<serde_json::Value>, String> {
    api::playback_clock(&state).await
}

#[tauri::command]
pub async fn set_playback_params(
    state: St<'_>,
    speed: f64,
    semitones: i32,
    reverb: player::Reverb,
    target: Option<String>,
) -> Result<(), String> {
    api::set_playback_params(&state, speed, semitones, reverb, target).await
}

#[tauri::command]
pub async fn get_queue(state: St<'_>) -> Result<serde_json::Value, String> {
    api::get_queue(&state).await
}

#[tauri::command]
pub async fn video_stream(state: St<'_>, video_id: String, max_height: i32) -> Result<Option<String>, String> {
    api::video_stream(&state, video_id, max_height).await
}

#[tauri::command]
pub async fn forget_video_stream(state: St<'_>, video_id: String) -> Result<(), String> {
    api::forget_video_stream(&state, video_id).await
}

#[tauri::command]
pub async fn get_settings(state: St<'_>) -> Result<serde_json::Value, String> {
    api::get_settings(&state).await
}

#[tauri::command]
pub fn global_hotkeys_on_wayland() -> bool {
    api::global_hotkeys_on_wayland()
}

#[tauri::command]
pub async fn get_stream_clients() -> Result<Vec<String>, String> {
    api::get_stream_clients().await
}

#[tauri::command]
pub async fn clear_caches(state: St<'_>) -> Result<(), String> {
    api::clear_caches(&state).await
}

#[tauri::command]
pub async fn reveal_in_finder(path: String) -> Result<(), String> {
    api::reveal_in_finder(path).await
}

#[tauri::command]
pub async fn save_text_file(path: String, contents: String) -> Result<(), String> {
    api::save_text_file(path, contents).await
}

#[tauri::command]
pub async fn get_account(state: St<'_>) -> Result<serde_json::Value, String> {
    api::get_account(&state).await
}

#[tauri::command]
pub async fn get_account_identities(state: St<'_>) -> Result<Vec<serde_json::Value>, String> {
    api::get_account_identities(&state).await
}

#[tauri::command]
pub async fn switch_account(state: St<'_>, selection_key: String) -> Result<serde_json::Value, String> {
    api::switch_account(&state, selection_key).await
}

#[tauri::command]
pub async fn sign_out(state: St<'_>) -> Result<(), String> {
    api::sign_out(&state).await
}

#[tauri::command]
pub async fn get_google_accounts(state: St<'_>) -> Result<Vec<serde_json::Value>, String> {
    api::get_google_accounts(&state).await
}

#[tauri::command]
pub async fn switch_google_account(state: St<'_>, id: String) -> Result<serde_json::Value, String> {
    api::switch_google_account(&state, id).await
}

#[tauri::command]
pub async fn remove_google_account(state: St<'_>, id: String) -> Result<(), String> {
    api::remove_google_account(&state, id).await
}

#[tauri::command]
pub async fn get_playback(state: St<'_>) -> Result<serde_json::Value, String> {
    api::get_playback(&state).await
}

#[tauri::command]
pub async fn get_home(state: St<'_>, params: Option<String>) -> Result<HomePage, String> {
    api::get_home(&state, params).await
}

#[tauri::command]
pub async fn get_live_now(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    api::get_live_now(&state).await
}

#[tauri::command]
pub async fn get_home_more(state: St<'_>, token: String) -> Result<HomePage, String> {
    api::get_home_more(&state, token).await
}

#[tauri::command]
pub async fn network_reachable() -> bool {
    api::network_reachable().await
}

#[tauri::command]
pub async fn get_library(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    api::get_library(&state).await
}

#[tauri::command]
pub async fn get_library_albums(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    api::get_library_albums(&state).await
}

#[tauri::command]
pub async fn get_history(state: St<'_>) -> Result<Vec<HistoryGroup>, String> {
    api::get_history(&state).await
}

#[tauri::command]
pub async fn get_library_artists(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    api::get_library_artists(&state).await
}

#[tauri::command]
pub async fn get_library_subscriptions(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    api::get_library_subscriptions(&state).await
}

#[tauri::command]
pub async fn get_playlist(state: St<'_>, id: String, sort: Option<PlaylistSort>, desc: Option<bool>) -> Result<PlaylistPage, String> {
    api::get_playlist(&state, id, sort, desc).await
}

#[tauri::command]
pub async fn set_playlist_sort(state: St<'_>, playlist_id: String, sort: PlaylistSort) -> Result<(), String> {
    api::set_playlist_sort(&state, playlist_id, sort).await
}

#[tauri::command]
pub fn play_counts(state: St<'_>) -> std::collections::HashMap<String, i64> {
    api::play_counts(&state)
}

#[tauri::command]
pub fn listening_stats(state: St<'_>, since: i64) -> crate::stats::Stats {
    api::listening_stats(&state, since)
}

#[tauri::command]
pub fn top_artists(state: St<'_>, limit: usize) -> Vec<crate::stats::TopArtist> {
    api::top_artists(&state, limit)
}

#[tauri::command]
pub fn clear_listening_history(state: St<'_>) {
    api::clear_listening_history(&state)
}

#[tauri::command]
pub async fn get_playlist_more(state: St<'_>, token: String, total: Option<usize>) -> Result<PlaylistContinuation, String> {
    api::get_playlist_more(&state, token, total).await
}

#[tauri::command]
pub async fn get_playlist_suggestions(state: St<'_>, playlist_id: String, token: Option<String>) -> Result<PlaylistSuggestions, String> {
    api::get_playlist_suggestions(&state, playlist_id, token).await
}

#[tauri::command]
pub async fn get_album(state: St<'_>, id: String) -> Result<AlbumPage, String> {
    api::get_album(&state, id).await
}

#[tauri::command]
pub async fn get_artist(state: St<'_>, id: String) -> Result<ArtistPage, String> {
    api::get_artist(&state, id).await
}

#[tauri::command]
pub async fn get_artist_appears_on(state: St<'_>, id: String, songs_id: String) -> Result<Vec<BrowseItem>, String> {
    api::get_artist_appears_on(&state, id, songs_id).await
}

#[tauri::command]
pub async fn get_moods(state: St<'_>) -> Result<Vec<MoodSection>, String> {
    api::get_moods(&state).await
}

#[tauri::command]
pub async fn get_mood_art(state: St<'_>, params: Vec<String>) -> Result<std::collections::HashMap<String, String>, String> {
    api::get_mood_art(&state, params).await
}

#[tauri::command]
pub async fn get_browse_grid(state: St<'_>, id: String, params: Option<String>) -> Result<Vec<BrowseItem>, String> {
    api::get_browse_grid(&state, id, params).await
}

#[tauri::command]
pub async fn play_playlist(state: St<'_>, items: Vec<SongItem>, start: Option<usize>, source_id: Option<String>, source_name: Option<String>, shuffle: Option<bool>, continuation: Option<String>, context: Option<crate::polish::context::PlayContext>, start_secs: Option<f64>) -> Result<(), String> {
    api::play_playlist(&state, items, start, source_id, source_name, shuffle, continuation, context, start_secs).await
}

#[tauri::command]
pub async fn start_radio(state: St<'_>, kind: String, id: String, name: Option<String>, start: Option<f64>) -> Result<(), String> {
    api::start_radio(&state, kind, id, name, start).await
}

#[tauri::command]
pub async fn rate(state: St<'_>, video_id: String, rating: Rating) -> Result<(), String> {
    api::rate(&state, video_id, rating).await
}

#[tauri::command]
pub async fn set_song_saved(state: St<'_>, token: String) -> Result<(), String> {
    api::set_song_saved(&state, token).await
}

#[tauri::command]
pub async fn set_album_saved(state: St<'_>, playlist_id: String, saved: bool) -> Result<(), String> {
    api::set_album_saved(&state, playlist_id, saved).await
}

#[tauri::command]
pub fn playlist_index(state: St<'_>) -> std::collections::HashMap<String, Vec<String>> {
    api::playlist_index(&state)
}

#[tauri::command]
pub async fn sync_playlist_index(state: St<'_>) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    api::sync_playlist_index(&state).await
}

#[tauri::command]
pub fn readonly_playlists(state: St<'_>) -> Vec<String> {
    api::readonly_playlists(&state)
}

#[tauri::command]
pub async fn add_to_playlist(state: St<'_>, playlist_id: String, video_id: String, allow_duplicates: Option<bool>) -> Result<bool, String> {
    api::add_to_playlist(&state, playlist_id, video_id, allow_duplicates).await
}

#[tauri::command]
pub async fn remove_from_playlist(state: St<'_>, playlist_id: String, video_id: String, set_video_id: String) -> Result<(), String> {
    api::remove_from_playlist(&state, playlist_id, video_id, set_video_id).await
}

#[tauri::command]
pub async fn remove_many_from_playlist(state: St<'_>, playlist_id: String, tracks: Vec<(String, String)>) -> Result<(), String> {
    api::remove_many_from_playlist(&state, playlist_id, tracks).await
}

#[tauri::command]
pub async fn move_in_playlist(state: St<'_>, playlist_id: String, set_video_id: String, before_set_video_id: Option<String>) -> Result<(), String> {
    api::move_in_playlist(&state, playlist_id, set_video_id, before_set_video_id).await
}

#[tauri::command]
pub async fn create_playlist(state: St<'_>, title: String, local: Option<bool>) -> Result<String, String> {
    api::create_playlist(&state, title, local).await
}

#[tauri::command]
pub async fn edit_playlist_details(state: St<'_>, playlist_id: String, name: Option<String>, description: Option<String>, public: Option<bool>) -> Result<(), String> {
    api::edit_playlist_details(&state, playlist_id, name, description, public).await
}

#[tauri::command]
pub async fn set_playlist_cover(state: St<'_>, playlist_id: String, path: Option<String>) -> Result<CoverResult, String> {
    api::set_playlist_cover(&state, playlist_id, path).await
}

#[tauri::command]
pub async fn delete_playlist(state: St<'_>, playlist_id: String) -> Result<(), String> {
    api::delete_playlist(&state, playlist_id).await
}

#[tauri::command]
pub async fn local_playlists(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    api::local_playlists(&state).await
}

#[tauri::command]
pub async fn add_to_local_playlist(state: St<'_>, playlist_id: String, items: Vec<SongItem>) -> Result<Vec<bool>, String> {
    api::add_to_local_playlist(&state, playlist_id, items).await
}

#[tauri::command]
pub async fn subscribe(state: St<'_>, channel_id: String, subscribed: bool) -> Result<(), String> {
    api::subscribe(&state, channel_id, subscribed).await
}

#[tauri::command]
pub async fn get_blocked_artists(state: St<'_>) -> Result<Vec<BlockedArtist>, String> {
    api::get_blocked_artists(&state).await
}

#[tauri::command]
pub async fn block_artist(state: St<'_>, id: Option<String>, name: String) -> Result<Vec<BlockedArtist>, String> {
    api::block_artist(&state, id, name).await
}

#[tauri::command]
pub async fn unblock_artist(state: St<'_>, key: String) -> Result<Vec<BlockedArtist>, String> {
    api::unblock_artist(&state, key).await
}

#[tauri::command]
pub async fn import_read(link: Option<String>, path: Option<String>) -> Result<crate::import::Preview, String> {
    api::import_read(link, path).await
}

#[tauri::command]
pub async fn import_start(state: St<'_>, lists: Vec<usize>) -> Result<crate::import::Snapshot, String> {
    api::import_start(&state, lists).await
}

/// First run: offer the import? Only for an empty library, until answered (`library::import_offer`).
#[tauri::command]
pub fn import_offer(state: St<'_>) -> bool {
    encore_core::library::import_offer_db(&state.db)
}

/// The first-run import offer was answered (imported or "Not now").
#[tauri::command]
pub fn import_offer_done(state: St<'_>) {
    encore_core::library::import_offer_done(&state.db)
}

#[tauri::command]
pub async fn import_status() -> Result<Option<crate::import::Snapshot>, String> {
    api::import_status().await
}

#[tauri::command]
pub async fn import_rows(tier: crate::import::Tier) -> Result<Vec<crate::import::ReviewRow>, String> {
    api::import_rows(tier).await
}

#[tauri::command]
pub async fn import_pick(state: St<'_>, key: String, song: Option<SongItem>) -> Result<crate::import::Snapshot, String> {
    api::import_pick(&state, key, song).await
}

#[tauri::command]
pub async fn import_create(state: St<'_>, options: crate::import::CreateOptions) -> Result<(), String> {
    api::import_create(&state, options).await
}

#[tauri::command]
pub async fn import_cancel(state: St<'_>) -> Result<(), String> {
    api::import_cancel(&state).await
}

#[tauri::command]
pub async fn import_source(state: St<'_>, playlist_id: String) -> Result<Option<String>, String> {
    api::import_source(&state, playlist_id).await
}

#[tauri::command]
pub async fn import_update(state: St<'_>, playlist_id: String) -> Result<crate::import::Snapshot, String> {
    api::import_update(&state, playlist_id).await
}

#[tauri::command]
pub async fn import_resolve(state: St<'_>, link: String) -> Result<crate::import::Resolved, String> {
    api::import_resolve(&state, link).await
}

#[tauri::command]
pub async fn get_local_library(state: St<'_>) -> Result<crate::local::LocalLibrary, String> {
    api::get_local_library(&state).await
}

#[tauri::command]
pub async fn add_local_folder(state: St<'_>, path: String) -> Result<crate::local::LocalLibrary, String> {
    api::add_local_folder(&state, path).await
}

#[tauri::command]
pub async fn remove_local_folder(state: St<'_>, path: String) -> Result<crate::local::LocalLibrary, String> {
    api::remove_local_folder(&state, path).await
}

#[tauri::command]
pub async fn get_lyrics(state: St<'_>, video_id: String, title: String, artists: String, album: Option<String>, duration: Option<f64>, searched: Option<bool>, source: Option<String>) -> Result<Option<crate::lyrics::Lyrics>, String> {
    api::get_lyrics(&state, video_id, title, artists, album, duration, searched, source).await
}

#[tauri::command]
pub async fn choose_lyrics_source(state: St<'_>, video_id: String, title: String, artists: String, album: Option<String>, duration: Option<f64>, searched: Option<bool>, source: Option<String>) -> Result<Option<crate::lyrics::Lyrics>, String> {
    api::choose_lyrics_source(&state, video_id, title, artists, album, duration, searched, source).await
}

#[tauri::command]
pub fn set_lyrics_offset(state: St<'_>, video_id: String, offset_ms: i64) -> i64 {
    api::set_lyrics_offset(&state, video_id, offset_ms)
}

#[tauri::command]
pub fn lyrics_providers(state: St<'_>) -> Vec<crate::lyrics::ProviderInfo> {
    api::lyrics_providers(&state)
}

#[tauri::command]
pub async fn lyrics_parse(text: String) -> Result<Vec<crate::lyrics::LyricLine>, String> {
    api::lyrics_parse(text).await
}


#[tauri::command]
pub fn log_ui(level: String, message: String) {
    api::log_ui(level, message)
}

// ── stream: parity3 ── "Made for <name>" (encore_core::localuser).
#[tauri::command]
pub fn local_user_name() -> Option<String> {
    encore_core::localuser::local_user_name()
}

// ── lane magic-d3 ── Liked Songs filter chips (#7) and Enhance (#13).
use encore_core::smart::{smartshuffle::Enhance, tags::LikedChips};

/// Liked Songs' genre and mood chips from the cached tags. Starts the background lookup for
/// untagged likes; `smart-tags-changed` carries the chips again as they fill in.
#[tauri::command]
pub fn smart_tags_liked(state: St<'_>) -> LikedChips {
    encore_core::smart::tags::liked_chips(state.inner())
}

/// Enhance state for a playlist (swaps in fresh suggestions once they're a day old).
#[tauri::command]
pub async fn enhance_state(state: St<'_>, playlist_id: String) -> Result<Enhance, String> {
    Ok(encore_core::smart::smartshuffle::enhance_state(state.inner(), &playlist_id).await)
}

/// Turn Enhance on (suggestions for a playlist of `len` songs) or off.
#[tauri::command]
pub async fn enhance_set(state: St<'_>, playlist_id: String, on: bool, len: usize) -> Result<Enhance, String> {
    encore_core::smart::smartshuffle::enhance_set(state.inner(), &playlist_id, on, len).await
}

/// "Remove" on an Enhance suggestion ("not for me").
#[tauri::command]
pub async fn enhance_remove(state: St<'_>, playlist_id: String, video_id: String) -> Result<Enhance, String> {
    Ok(encore_core::smart::smartshuffle::enhance_remove(state.inner(), &playlist_id, &video_id).await)
}

/// An Enhance suggestion was added to the playlist: it stops being a suggestion.
#[tauri::command]
pub fn enhance_keep(state: St<'_>, playlist_id: String, video_id: String) -> Enhance {
    encore_core::smart::smartshuffle::enhance_keep(&state, &playlist_id, &video_id)
}

// ── lane magic-d1: track actions and radio ──────────────────────────────────────────────────

/// "Go to … radio" as a page (MAGIC-GAPS #1): the radio's tracks, without playing anything.
#[tauri::command]
pub async fn radio_tracks(state: St<'_>, kind: String, id: String, name: Option<String>) -> Result<encore_core::radio_page::RadioPage, String> {
    encore_core::radio_page::radio_tracks(state.inner(), &kind, &id, name).await
}

/// A single pick from a pasted song link: plays from `start` seconds, then Autoplay.
#[tauri::command]
pub async fn play_single_at(state: St<'_>, item: SongItem, from: Option<String>, start: Option<f64>) -> Result<(), String> {
    encore_core::radio_page::play_single_at(state.inner(), item, from, start).await
}

/// "Show credits" (MAGIC-GAPS #6): YouTube Music's song credits, by role and by person.
#[tauri::command]
pub async fn song_credits(state: St<'_>, item: SongItem) -> Result<encore_core::credits::Credits, String> {
    encore_core::credits::song_credits(state.inner(), item).await
}

/// An artist's discography in Spotify's chips, from the release cards and top songs the page has.
#[tauri::command]
pub fn discography(
    albums: Vec<serde_json::Value>,
    singles: Vec<serde_json::Value>,
    top_songs: Option<Vec<SongItem>>,
) -> encore_core::discography::Discography {
    encore_core::discography::build_now(&albums, &singles, &top_songs.unwrap_or_default())
}

/// "Exclude from your taste profile" (MAGIC-GAPS #16) and its undo. `video_ids`: the tracks the
/// song or playlist covers.
#[tauri::command]
pub fn taste_set_excluded(state: St<'_>, id: String, kind: String, title: Option<String>, video_ids: Vec<String>, excluded: bool) {
    encore_core::smart::taste::set_excluded(&state.db, &id, &kind, title.as_deref(), &video_ids, excluded)
}

/// Ids currently excluded from the taste profile.
#[tauri::command]
pub fn taste_excluded(state: St<'_>) -> Vec<String> {
    encore_core::smart::taste::excluded_ids(&state.db)
}

// ── lane magic-d2 ── drag and drop into the queue, and Undo for Clear queue.

/// Songs dropped on the queue panel, inserted at the drop line (`at` = the queue index the line is
/// in front of; the core clamps it into "Next in queue").
#[tauri::command]
pub async fn add_to_queue_at(state: St<'_>, items: Vec<SongItem>, at: usize, from: Option<String>) -> Result<(), String> {
    state.inner().clone().add_to_queue_at(items, at, from).await;
    Ok(())
}

/// Undo of Clear queue: the rows the panel snapshotted before clearing come back.
#[tauri::command]
pub async fn restore_queued(state: St<'_>, items: Vec<SongItem>) -> Result<usize, String> {
    Ok(state.inner().clone().restore_queued(items).await)
}
