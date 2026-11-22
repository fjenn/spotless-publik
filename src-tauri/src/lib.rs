//! Spotless's desktop app: a thin Tauri host over `encore_core` (crates/core). The core owns the
//! transport, player, database, queue and every feature; this crate adds the windows, tray, menu,
//! hotkeys, sign-in webview and the command boundary, and implements the core's `Host` (host.rs).

// The core's modules, re-exported under their old paths so `crate::state::AppState` and friends
// keep resolving in the desktop-only files.
#[allow(unused_imports)]
pub(crate) use encore_core::{
    api, audioproxy, blocked, canonical, cipher, db, firstaudio, history, identity, import, local,
    localuser, lyrics, orchestrator, potoken, reliability_boot, romanize, secrets, spotify, state,
    stats, videoproxy,
};

// Core modules that also carry this crate's Tauri commands (`<module>/commands.rs`).
pub(crate) mod downloads {
    pub use encore_core::downloads::*;
    pub mod commands;
}
pub(crate) mod podcasts {
    pub use encore_core::podcasts::*;
    pub mod commands;
}
pub(crate) mod polish {
    pub use encore_core::polish::*;
    pub mod commands;
}
pub(crate) mod smart {
    pub use encore_core::smart::*;
    pub mod commands;
}
pub(crate) mod http {
    pub use crate::webproxy::*;
    #[allow(unused_imports)]
    pub use encore_core::http::*;
}
pub(crate) mod audio {
    pub use crate::audio_cmds::*;
}

mod audio_cmds;
mod audiobooks; // stream: audiobooks
mod commands;
mod deeplink;
mod diagnostics;
mod host;
mod hotkeys;
#[cfg(target_os = "macos")]
mod install; // stream: installer (move to /Applications on first launch)
mod library;
mod media;
mod menu;
mod migrate;
mod mini;
mod notify;
mod session;
mod sync; // device sync (docs/SYNC.md)
mod tray;
mod webproxy;
mod webview;
mod winframe; // stream: parity2 (main window frame survives full screen / no clean exit)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use db::Db;
use state::AppState;

/// Hand glibc's freed-but-retained heap back to the OS every few minutes.
///
/// glibc gives each thread its own arena and never returns those pages on `free`, so this process
/// (45 threads across tokio, GTK, mpv and souvlaki) accumulates empty heap it will never reuse.
/// Measured against a running 0.3.2 build: `malloc_trim(0)` dropped it from 211 MiB to 160 MiB PSS
/// and the slack came back at roughly 15 MiB per 15 minutes, so a periodic trim keeps it flat.
///
/// ponytail: trim only. `mallopt(M_ARENA_MAX, 2)` would cap the sprawl at the source, but it
/// serialises allocation across all those threads for a win the trim already gets. Reach for it
/// only if RSS starts climbing between trims.
#[cfg(target_os = "linux")]
fn spawn_heap_trimmer() {
    tauri::async_runtime::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(180)).await;
            // Safe: no arguments, no allocation, glibc walks its own arenas.
            unsafe { libc::malloc_trim(0) };
        }
    });
}

/// Pull WebKitGTK off its full-browser defaults, which wry never touches.
///
/// **Caches.** WebKitGTK defaults to `WEBKIT_CACHE_MODEL_WEB_BROWSER` ("cache a very large number of
/// resources and previously viewed content"), sized against total system RAM. A music client
/// browsing YTM shelves fills that with thumbnails: measured 627 MiB of on-disk WebKitCache and a
/// web process that would not give the memory back (`malloc_trim` there freed 1 MiB, so it is all
/// live cache). `DocumentBrowser` keeps a working cache but drops dead resources instead of
/// hoarding them. wry also hard-enables the back/forward page cache (`webkitgtk/mod.rs:438`), which
/// keeps whole previous documents alive; this is a SvelteKit SPA doing client-side routing, so it
/// never gets a back/forward navigation to restore and that memory is pure waste.
///
/// **Subsystems.** Audio is libmpv's job and the UI has no `<audio>`, `AudioContext`,
/// `getUserMedia` or WebGL anywhere in it (only 2D canvas, in `theme.svelte.ts`), yet every web
/// process boots the media and 3D stacks regardless: GStreamer, libLLVM and Mesa's gallium are all
/// mapped into it. Measured A/B in `cargo tauri dev`, same build otherwise, home feed loaded:
/// **259 MiB → 247 MiB** PSS at T+180s (236 → 223 at T+60s).
///
/// `media` is the one exception, and only the main window passes `true`: the player view draws a
/// `<video>` for music videos (plan 031). That is a plain `<video src>`, so `mediasource`,
/// `media_stream`, `media_capabilities`, `encrypted_media`, `webaudio`, `webrtc` and `webgl` all
/// stay off. The mini player has no video surface, so it keeps the whole media stack off. WebGL
/// comes back on in the main window only while the ambient light is on ([`set_webgl`]).
///
/// Applies to one webview, because WebKit settings are per-view: the main window and the mini
/// player each cost their own web process, so each has to be told. The hidden cipher/PoToken
/// webviews are deliberately left at the defaults, since the fingerprinting code they exist to run
/// is entitled to probe whatever it likes.
#[cfg(target_os = "linux")]
fn tune_webview(win: &tauri::WebviewWindow, media: bool) {
    use webkit2gtk::{
        CacheModel, NetworkProxyMode, NetworkProxySettings, SettingsExt, WebContextExt, WebViewExt,
        WebsiteDataManagerExt,
    };

    let label = win.label().to_owned();
    let res = win.with_webview(move |wv| {
        let webview = wv.inner();
        // Context-wide, so the second call is a no-op. Set here anyway: whichever window comes up
        // first should not depend on the other existing.
        if let Some(ctx) = WebViewExt::context(&webview) {
            ctx.set_cache_model(CacheModel::DocumentBrowser);
            // The proxy setting, for what the webviews load themselves (#465). Not tauri's
            // `proxy_url`: wry gives WebKit no bypass list, and WebKit then sends loopback through
            // the proxy too (unlike WebView2), so the dev server and the music-video proxy would
            // be out of reach of a remote one.
            if let (Some(url), Some(dm)) = (http::webview_proxy(), ctx.website_data_manager()) {
                let bypass = ["localhost", "127.0.0.0/8", "::1"];
                let mut proxy = NetworkProxySettings::new(Some(url.as_str()), &bypass);
                dm.set_network_proxy_settings(NetworkProxyMode::Custom, Some(&mut proxy));
            }
        }
        if let Some(settings) = WebViewExt::settings(&webview) {
            settings.set_enable_page_cache(false);
            settings.set_enable_media(media);
            settings.set_enable_mediasource(false);
            settings.set_enable_media_stream(false);
            settings.set_enable_media_capabilities(false);
            settings.set_enable_encrypted_media(false);
            settings.set_enable_webaudio(false);
            settings.set_enable_webrtc(false);
            settings.set_enable_webgl(false);
            settings.set_enable_html5_database(false); // WebSQL. localStorage is a separate switch.

            // Two-finger swipe to go back (#302). WebKit walks its own back/forward list, which
            // for this SPA is SvelteKit's pushState entries: the same ones the titlebar's back
            // button steps through. It only fires once a horizontal scroller has run out, so the
            // shelves keep their swipes. Windows has this on by default; macOS sets
            // WKWebView's allowsBackForwardNavigationGestures (`enable_swipe_navigation`).
            settings.set_enable_back_forward_navigation_gestures(true);

            if compositor_features_off(&settings) {
                restart_web_process(&webview);
            }
        }
        heal_lost_release(&webview);
    });
    match res {
        Ok(()) => {
            tracing::info!(label, media, "webkit: DocumentBrowser cache, page cache + webgl off")
        }
        Err(e) => tracing::warn!(label, error = %e, "webkit tuning failed (continuing)"),
    }
}

/// WebKitGTK 2.54's new compositing, back to what every earlier release did (#433). Returns
/// whether anything changed.
///
/// 2.54 composites with Skia, and Skia draws a layer at its opacity from before an animation on the
/// frame the animation ends, so every fade blinks: switches, dialogs, the player view. Any GPU,
/// opaque windows too. `UseSkiaForComposition` off brings back TextureMapper. 2.54 also has the
/// compositor draw only the damaged rectangles, and on Intel/Mesa (GNOME, niri) a buffer that
/// holds nothing outside them leaves our transparent window see-through on alternate frames, so
/// both damage switches go off as well and every frame is drawn whole, as on 2.52.
/// `ui/perf/flashprobe.py` on 2.54.1: 7-8 one-frame flashes per 6.5 s of fades, 0 with these off,
/// 0 on 2.52.1.
///
/// WebKit's feature list (2.42+) has no binding in webkit2gtk 2.0, so its six functions come from
/// the already-loaded library by name. A WebKit without one of them, or without a feature, is left
/// alone.
///
/// ponytail: drop this once a WebKitGTK release fixes both; run flashprobe.py on it first.
#[cfg(target_os = "linux")]
fn compositor_features_off(settings: &webkit2gtk::Settings) -> bool {
    use gtk::glib::translate::ToGlibPtr;
    use std::ffi::{c_char, c_int, c_void, CStr};
    type AllFeatures = unsafe extern "C" fn() -> *mut c_void;
    type Length = unsafe extern "C" fn(*mut c_void) -> usize;
    type Get = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
    type Identifier = unsafe extern "C" fn(*mut c_void) -> *const c_char;
    type Enabled = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
    type SetEnabled = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int);
    type Unref = unsafe extern "C" fn(*mut c_void);
    const OFF: [&[u8]; 3] = [
        b"UseSkiaForComposition",
        b"UseDamagingInformationForCompositing",
        b"PropagateDamagingInformation",
    ];

    // SAFETY: the version calls take nothing. The rest are dlsym'd from the WebKitGTK this process
    // already links, each cast to its signature in webkit/WebKitSettings.h and WebKitFeature.h.
    // The list is ours to unref; the features in it are borrowed from the list.
    unsafe {
        if webkit2gtk::ffi::webkit_get_minor_version() < 54 {
            return false;
        }
        let sym = |name: &CStr| libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
        let syms = [
            sym(c"webkit_settings_get_all_features"),
            sym(c"webkit_feature_list_get_length"),
            sym(c"webkit_feature_list_get"),
            sym(c"webkit_feature_get_identifier"),
            sym(c"webkit_settings_get_feature_enabled"),
            sym(c"webkit_settings_set_feature_enabled"),
            sym(c"webkit_feature_list_unref"),
        ];
        if syms.iter().any(|s| s.is_null()) {
            return false;
        }
        let all: AllFeatures = std::mem::transmute(syms[0]);
        let length: Length = std::mem::transmute(syms[1]);
        let get: Get = std::mem::transmute(syms[2]);
        let identifier: Identifier = std::mem::transmute(syms[3]);
        let enabled: Enabled = std::mem::transmute(syms[4]);
        let set_enabled: SetEnabled = std::mem::transmute(syms[5]);
        let unref: Unref = std::mem::transmute(syms[6]);

        let raw: *mut webkit2gtk::ffi::WebKitSettings = settings.to_glib_none().0;
        let list = all();
        if list.is_null() {
            return false;
        }
        let mut changed = false;
        for i in 0..length(list) {
            let feature = get(list, i);
            let id = identifier(feature);
            if !id.is_null()
                && OFF.contains(&CStr::from_ptr(id).to_bytes())
                && enabled(raw.cast(), feature) != 0
            {
                set_enabled(raw.cast(), feature, 0);
                changed = true;
            }
        }
        unref(list);
        changed
    }
}

/// Builds the page again so [`compositor_features_off`] takes effect. The web process reads those
/// settings once, when it creates the page, and wry starts that load inside `build()`, before tauri
/// hands the webview over: flipped afterwards they are ignored (measured). So the process is
/// killed and the same URL loaded again. `reload()` would not do: the first load has usually not
/// committed yet, and a reload of nothing paints nothing. Costs one extra web-process start per
/// window, only on 2.54+. A later crash is not ours to answer, hence the reason check.
#[cfg(target_os = "linux")]
fn restart_web_process(webview: &webkit2gtk::WebView) {
    use webkit2gtk::{WebProcessTerminationReason, WebViewExt};

    let Some(uri) = webview.uri() else {
        tracing::warn!(
            "webkit 2.54+: no URI to rebuild the page with, compositing unchanged (#433)"
        );
        return;
    };
    webview.connect_web_process_terminated(move |view, reason| {
        if reason == WebProcessTerminationReason::TerminatedByApi {
            view.load_uri(&uri);
        }
    });
    webview.terminate_web_process();
    tracing::info!("webkit 2.54+: Skia compositing and damage tracking off, page rebuilt (#433)");
}

/// Hands WebKit a left-button release it never got. A titlebar drag starts a compositor move grab
/// (tauri's `start_dragging`, run later on the GTK loop), and the grab eats the release of whichever
/// press it caught. WebKit then still counts the button as down and turns the next press into a
/// `pointermove`, so nothing listening for `pointerdown` sees it: after a double-click on the bar,
/// the first click outside an open dialog didn't close it (#427). A second left press without a
/// release in between can only mean one was lost, so it is sent first. At (-1, -1), so its
/// `mouseup`/`click` land on `<html>` rather than on whatever is now under the old spot.
/// `ui/perf/dragpress.py` shows both the stuck press and this fix on the system WebKitGTK.
#[cfg(target_os = "linux")]
fn heal_lost_release(webview: &webkit2gtk::WebView) {
    use gtk::gdk::{self, glib::translate::ToGlibPtrMut};
    use gtk::prelude::*;
    use std::{cell::Cell, rc::Rc};

    let held = Rc::new(Cell::new(false));
    let on_press = held.clone();
    webview.connect_button_press_event(move |view, e| {
        if e.button() != 1 || e.event_type() != gdk::EventType::ButtonPress {
            return gtk::glib::Propagation::Proceed;
        }
        if on_press.get() {
            let mut release = e.clone();
            // SAFETY: `release` is our own copy of a GdkEventButton; only plain fields change.
            unsafe {
                let raw: *mut gdk::ffi::GdkEventButton = release.to_glib_none_mut().0;
                (*raw).type_ = gdk::ffi::GDK_BUTTON_RELEASE;
                (*raw).x = -1.0;
                (*raw).y = -1.0;
                (*raw).state |= gdk::ffi::GDK_BUTTON1_MASK;
            }
            view.event(&release); // runs the release handler below, which clears `held`
        }
        on_press.set(true);
        gtk::glib::Propagation::Proceed
    });
    webview.connect_button_release_event(move |_, e| {
        if e.button() == 1 {
            held.set(false);
        }
        gtk::glib::Propagation::Proceed
    });
}

/// WebGL in the main window, which [`tune_webview`] turns off: the ambient light draws its glow with
/// it (ui/src/lib/ambient.ts), so it is on exactly while that setting is. WebKit checks the setting
/// when the page asks for a context, so a change applies without a reload.
#[cfg(target_os = "linux")]
pub(crate) fn set_webgl(app: &tauri::AppHandle, on: bool) {
    use webkit2gtk::{SettingsExt, WebViewExt};

    if let Some(win) = app.get_webview_window("main") {
        let _ = win.with_webview(move |wv| {
            if let Some(settings) = WebViewExt::settings(&wv.inner()) {
                settings.set_enable_webgl(on);
            }
        });
    }
}

/// Two-finger swipe back/forward on macOS (#302), the Linux switch in [`tune_webview`]. WKWebView
/// walks its own back/forward list, which for this SPA is SvelteKit's pushState entries, so a swipe
/// is a `popstate` exactly like the titlebar's back button. The gesture only starts when the wheel
/// event begins on content that is already at that edge, so a shelf mid-scroll keeps its swipe.
/// wry does not expose `allowsBackForwardNavigationGestures`, so it is set through the objc runtime.
#[cfg(target_os = "macos")]
fn enable_swipe_navigation(win: &tauri::WebviewWindow) {
    let _ = win.with_webview(|wv| {
        let view = wv.inner() as *mut objc2::runtime::AnyObject;
        if view.is_null() {
            return;
        }
        // SAFETY: `inner()` is the live WKWebView; with_webview runs on the main thread.
        unsafe {
            let _: () = objc2::msg_send![&*view, setAllowsBackForwardNavigationGestures: objc2::runtime::Bool::YES];
        }
    });
}

/// [`tune_webview`] for a window looked up by label. No-op if it isn't up.
#[cfg(target_os = "linux")]
pub(crate) fn tune_webview_labelled(app: &tauri::AppHandle, label: &str, media: bool) {
    if let Some(win) = app.get_webview_window(label) {
        tune_webview(&win, media);
    }
}

/// Let a destroyed webview's web process exit (#572). Runs for every webview, from the plugin hook
/// in [`run`].
///
/// wry 0.55 connects its IPC handler on the view's own `UserContentManager` with a closure that
/// holds the view (tauri-apps/wry#1871), so the view is never freed. WebKitGTK 2.52 closes the
/// web process when the widget is destroyed anyway; 2.54 keeps it until the view is freed, so every
/// webview torn down at runtime (cipher idle teardown, mini player, sign-in, session refresh) left
/// a whole `WebKitWebProcess` behind, ~90 MiB PSS each. Dropping the manager's handlers when GTK
/// destroys the view breaks the cycle.
///
/// ponytail: delete once wry captures the view weakly.
#[cfg(target_os = "linux")]
fn release_on_destroy(webview: &tauri::Webview) {
    use gtk::glib::{gobject_ffi, prelude::*, translate::IntoGlib};
    use gtk::prelude::WidgetExt;
    use webkit2gtk::{UserContentManager, WebViewExt};

    let _ = webview.with_webview(|wv| {
        let view = wv.inner();
        let Some(manager) = view.user_content_manager() else { return };
        view.connect_destroy(move |_| {
            let signal = gtk::glib::subclass::signal::SignalId::lookup(
                "script-message-received",
                UserContentManager::static_type(),
            );
            if let Some(signal) = signal {
                // Safe: `manager` is a live GObject we hold a reference to, and matching on the
                // signal id alone needs none of the closure/func/data pointers.
                unsafe {
                    gobject_ffi::g_signal_handlers_disconnect_matched(
                        manager.as_ptr().cast(),
                        gobject_ffi::G_SIGNAL_MATCH_ID,
                        signal.into_glib(),
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                }
            }
        });
    });
}

/// Pull the main window back onto a display if the window-state plugin restored it somewhere that
/// no longer exists (#459). The plugin keeps a saved position when any one corner lands on a
/// monitor, so after a layout change the window could come back with only its invisible resize
/// border on screen and no titlebar to drag. Called from `setup`: the plugin has restored by then
/// (it runs as the window is built, just before), and the window is still hidden.
///
/// Wayland gives the app no real position and ignores `center`, which is fine: the compositor
/// places the window there, so the bug cannot happen.
fn keep_on_screen(w: &tauri::WebviewWindow) {
    let (Ok(p), Ok(size)) = (w.outer_position(), w.outer_size()) else { return };
    if mini::on_a_display(w, p, size) {
        fit_work_area(w, p, size);
        return;
    }
    tracing::info!(x = p.x, y = p.y, "main window restored off-screen, centering it");
    // A maximized window has to come back to its normal size to move: maximizing again then fills
    // the display it was centered on.
    let maximized = w.is_maximized().unwrap_or(false);
    if maximized {
        let _ = w.unmaximize();
    }
    let _ = w.center();
    if maximized {
        let _ = w.maximize();
    }
}

/// The whole window inside the work area of the display it's on: a window whose middle is on
/// screen but whose bottom (or title bar) is under the Dock or past the edge gets shrunk to fit and
/// centred. First launch from the DMG once opened with the welcome dialog below the screen.
fn fit_work_area(w: &tauri::WebviewWindow, p: tauri::PhysicalPosition<i32>, size: tauri::PhysicalSize<u32>) {
    let Ok(Some(m)) = w.current_monitor() else { return };
    let (a, sz) = (m.work_area().position, m.work_area().size);
    if fits_inside((p.x, p.y, size.width, size.height), (a.x, a.y, sz.width, sz.height)) {
        return;
    }
    tracing::info!(x = p.x, y = p.y, "main window not fully on screen, fitting it");
    if w.is_maximized().unwrap_or(false) || w.is_fullscreen().unwrap_or(false) {
        return;
    }
    let scale = m.scale_factor();
    let screen = sz.to_logical::<f64>(scale);
    let cur = size.to_logical::<f64>(scale);
    let _ = w.set_size(tauri::LogicalSize::new(cur.width.min(screen.width), cur.height.min(screen.height)));
    let _ = w.center();
}

/// `(x, y, w, h)` window fully inside `(x, y, w, h)` area, with a few pixels of slack for shadows.
fn fits_inside(win: (i32, i32, u32, u32), area: (i32, i32, u32, u32)) -> bool {
    const SLACK: i64 = 8;
    let (wx, wy, ww, wh) = (win.0 as i64, win.1 as i64, win.2 as i64, win.3 as i64);
    let (ax, ay, aw, ah) = (area.0 as i64, area.1 as i64, area.2 as i64, area.3 as i64);
    wx >= ax - SLACK && wy >= ay - SLACK && wx + ww <= ax + aw + SLACK && wy + wh <= ay + ah + SLACK
}

/// Set once the main window has been given its first-launch size. From then on the window-state
/// plugin's saved size/position wins. A new key (not the plugin's file) so installs that already
/// saved Limusic's small 1200x800 default get the new one once too.
/// v2 (parity2): v1 ran on installs whose large size was then never written (the plugin only
/// saved on a clean exit), so they reopened small; those get the default once more.
const DEFAULT_SIZE_KEY: &str = "window_default_size_v2";

/// Spotify opens big: 90% of the display the window is on, capped at 1600x1000 logical and never
/// under 1280x832 unless the display itself is smaller, centred.
fn first_launch_size(w: &tauri::WebviewWindow) {
    let Ok(Some(m)) = w.current_monitor().or_else(|_| w.primary_monitor()) else { return };
    let scale = m.scale_factor();
    let screen = m.work_area().size.to_logical::<f64>(scale);
    let (width, height) = default_window_size(screen.width, screen.height);
    let _ = w.set_size(tauri::LogicalSize::new(width, height));
    let _ = w.center();
}

/// Pure half of [`first_launch_size`]: (width, height) for a work area of `sw` x `sh` logical.
fn default_window_size(sw: f64, sh: f64) -> (f64, f64) {
    let fit = |want: f64, cap: f64, screen: f64| want.max(screen * 0.9).min(cap).min(screen).round();
    (fit(1280.0, 1600.0, sw), fit(832.0, 1000.0, sh))
}

#[cfg(test)]
mod default_size_tests {
    use super::{default_window_size, fits_inside};
    #[test]
    fn a_window_past_the_bottom_does_not_fit() {
        let area = (0, 74, 3456, 2160);
        assert!(fits_inside((346, 150, 3110, 1950), area));
        assert!(!fits_inside((346, 1400, 3110, 1950), area), "middle on screen, bottom under the Dock");
        assert!(!fits_inside((-400, 150, 3110, 1950), area));
    }
    #[test]
    fn default_size_follows_the_display() {
        assert_eq!(default_window_size(1440.0, 875.0), (1296.0, 832.0));
        assert_eq!(default_window_size(1728.0, 1080.0), (1555.0, 972.0));
        assert_eq!(default_window_size(2560.0, 1415.0), (1600.0, 1000.0));
        // A display smaller than the default: the window still fits on it.
        assert_eq!(default_window_size(1024.0, 740.0), (1024.0, 740.0));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// Logs to stdout **and** to `<app data>/limusic.log`, truncated each launch (the previous run is
/// kept as `limusic.log.1`).
///
/// The filter names `app_lib` and `encore_core`, the two `[lib]` names, because that is what every
/// tracing target carries (`app_lib::tray`, `encore_core::orchestrator`). It said `limusic_app`
/// until 2026-08-29, which only ever matched `main.rs`, so every `debug!` in the app was dropped.
///
/// The file is the only way a Windows or macOS user can produce a log at all: `main.rs` sets
/// `windows_subsystem = "windows"` in release, so there is no console to print to, and a bug that
/// only reproduces on their machine (issue #71) otherwise has to be debugged by guessing.
/// `RUST_LOG` still overrides the default filter for both.
fn init_logging(dir: &std::path::Path) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,app_lib=debug,encore_core=debug,ort=warn,flacenc=warn".into())
    };
    let path = dir.join("encore.log");
    let _ = std::fs::rename(&path, dir.join("encore.log.1"));
    // ponytail: one file per launch, no size cap. A run long enough to matter is a run whose log
    // someone wants anyway; add rotation if that stops being true.
    let file = std::fs::File::create(&path).ok().map(std::sync::Mutex::new);
    let file_layer = file.map(|f| {
        tracing_subscriber::fmt::layer().with_ansi(false).with_writer(f).with_filter(filter())
    });
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter()))
        .with(file_layer)
        .init();
}

/// Raise the open-file soft limit to the hard limit, capped.
///
/// WebKitGTK's DMABUF renderer spends a file descriptor per buffer, so a busy page can hold
/// hundreds. Against the 1024 soft limit a login shell often hands us, the web process runs out,
/// and a web process that cannot open an fd cannot allocate a buffer or even create a GWakeup
/// pipe: it stops painting and never recovers, while playback carries on in this process. That is
/// the "window frozen, music still playing" report, and it is a resource limit, not a driver bug.
///
/// Browsers do exactly this at startup for the same reason. The cap keeps us clear of code that
/// sizes arrays by the limit or loops over every possible fd; 64k is ~60x the headroom we need.
#[cfg(target_os = "linux")]
fn raise_fd_limit() {
    const WANT: libc::rlim_t = 65536;
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: both calls take a pointer to a live, fully initialised `rlimit`.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let want = WANT.min(lim.rlim_max);
        if lim.rlim_cur >= want {
            return;
        }
        let old = lim.rlim_cur;
        lim.rlim_cur = want;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) == 0 {
            tracing::info!(from = old, to = want, "raised open-file limit");
        }
    }
}

/// The vendor of the EGL that serves the X11 display: "NVIDIA" when the proprietary driver draws
/// the screen, "Mesa Project" when Intel or AMD does, which is a hybrid laptop's default even with
/// the NVIDIA driver loaded (#458). WebKit's UI process opens its EGL display the same way, and
/// the GPU behind it is the one the DMABUF renderer allocates from, so this asks the question
/// WebKit is about to answer. `None` when there is no EGL or it will not initialize.
#[cfg(target_os = "linux")]
fn x11_egl_vendor() -> Option<String> {
    use std::ffi::{c_char, c_void, CStr};
    type GetPlatformDisplay = unsafe extern "C" fn(u32, *mut c_void, *const isize) -> *mut c_void;
    type Initialize = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> u32;
    type Terminate = unsafe extern "C" fn(*mut c_void) -> u32;
    type QueryString = unsafe extern "C" fn(*mut c_void, i32) -> *const c_char;
    const EGL_PLATFORM_X11_KHR: u32 = 0x31D5;
    const EGL_VENDOR: i32 = 0x3053;

    // SAFETY: dlopen/dlsym by name, each symbol cast to its EGL 1.5 signature. Never dlclosed:
    // WebKit loads the same library a moment later, and vendor drivers do not like being unloaded.
    unsafe {
        let lib = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_LAZY);
        if lib.is_null() {
            return None;
        }
        let sym = |name: &CStr| libc::dlsym(lib, name.as_ptr());
        let syms = [
            sym(c"eglGetPlatformDisplay"),
            sym(c"eglInitialize"),
            sym(c"eglTerminate"),
            sym(c"eglQueryString"),
        ];
        if syms.iter().any(|s| s.is_null()) {
            return None;
        }
        let get_display: GetPlatformDisplay = std::mem::transmute(syms[0]);
        let initialize: Initialize = std::mem::transmute(syms[1]);
        let terminate: Terminate = std::mem::transmute(syms[2]);
        let query: QueryString = std::mem::transmute(syms[3]);

        // A null native display is $DISPLAY (EGL_KHR_platform_x11).
        let dpy = get_display(EGL_PLATFORM_X11_KHR, std::ptr::null_mut(), std::ptr::null());
        if dpy.is_null() || initialize(dpy, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
            return None;
        }
        let vendor = query(dpy, EGL_VENDOR);
        let vendor = (!vendor.is_null()).then(|| CStr::from_ptr(vendor).to_string_lossy().into());
        terminate(dpy);
        vendor
    }
}

/// Log a startup failure and stop, instead of panicking.
///
/// Everything this is called for happens inside Tauri's `setup`, before any window exists.
/// `main.rs` sets `windows_subsystem = "windows"` in release, so a panic here writes to a stderr
/// that is not connected to anything: the user sees a process start and disappear, with no window,
/// no console and no log line. `init_logging` has already run by the time any caller gets here, so
/// this reaches limusic.log (the writer is unbuffered, so the line lands before the exit).
///
/// There is deliberately no dialog, although the dialog plugin is registered. `setup` runs on the
/// main thread inside the event loop's `Ready` handler, and the plugin shows a message box by
/// posting a task to that same thread. Blocking on it from here (directly, or by joining a thread
/// that does) waits on a task that can never run, so the process hangs instead of exiting, and a
/// hung instance keeps the single-instance name: every later launch hands off to it and silently
/// does nothing. A dialog needs a path that does not go through the event loop.
fn fatal(what: &str, detail: &str) -> ! {
    tracing::error!("fatal at startup: {what}: {detail}");
    std::process::exit(1)
}

/// Whether the OS launched us at login, through the autostart entry's `--autostart`.
///
/// Not the argument alone: Tauri restarts (the update banner's relaunch, the tray's Restart) hand
/// the old process's arguments on, so a copy started at login would come back hidden after every
/// update. A restarted process inherits the environment instead, and finds [`RESTARTED_ENV`],
/// which `run` sets once it has read this. Windows keeps it too: the updater starts the per-user
/// NSIS installer with a plain ShellExecute, and the installer starts the new version the same way.
static AUTOSTARTED: AtomicBool = AtomicBool::new(false);
const RESTARTED_ENV: &str = "LIMUSIC_RESTARTED";

/// What a cold launch was given, so `limusic-app 'https://music.youtube.com/watch?v=…'` opens the
/// link once the SPA has mounted (#348). Taken once by `take_launch_args`. A launch while we are
/// already running reaches the single-instance callback instead, which emits `open-link`. Raw
/// strings either way: `parseYtLink` in the UI decides what is a link, so there is one parser.
pub(crate) static LAUNCH_ARGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Stay in the tray instead of showing the window: launched at login with "Start minimized to
/// tray" on, and a tray icon to come back from. Asked by the startup safety net and by
/// `show_main` once the SPA has mounted.
fn should_start_minimized(db: &Db) -> bool {
    AUTOSTARTED.load(Ordering::Relaxed)
        && db.get_setting("start_minimized").as_deref() == Some("true")
        && tray::available()
}

/// Tauri entry point. Applies the platform boot fixes (open-fd limit, NVIDIA/WebKit env), restores
/// the persisted session, wires every command and plugin, and runs the event loop. context/01
/// §startup.
pub fn run() {
    // publik edition: the app's version for publik, and its release self test (publik/README.md).
    publik::start(env!("CARGO_PKG_VERSION"));
    // One tokio runtime for the core and Tauri: the core's background loops and Tauri's commands
    // share a pool, as they did when the core spawned onto `tauri::async_runtime`.
    tauri::async_runtime::set(encore_core::rt::handle());

    // Must happen before any webview exists: the limit is inherited by the web processes WebKit
    // forks, and cannot be raised for them afterwards.
    #[cfg(target_os = "linux")]
    raise_fd_limit();

    // See `AUTOSTARTED`. Up here with the other env writes, before any thread exists to read it.
    AUTOSTARTED.store(
        std::env::args_os().any(|a| a == "--autostart")
            && std::env::var_os(RESTARTED_ENV).is_none(),
        Ordering::Relaxed,
    );
    // Not on a restart either: the relaunch replays argv, so the linked song would start over after
    // every update. `args_os`, because `args` panics on an argument that is not UTF-8.
    if std::env::var_os(RESTARTED_ENV).is_none() {
        *LAUNCH_ARGS.lock().unwrap() = std::env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            // A `spotless://` (or `encore://`) link in argv becomes the https link the page knows
            // how to open.
            .map(|a| deeplink::to_web_link(&a).filter(|_| deeplink::is_app_link(&a)).unwrap_or(a))
            .collect();
    }
    std::env::set_var(RESTARTED_ENV, "1");

    // Two separate NVIDIA/WebKitGTK failures, two separate variables. Neither substitutes for
    // the other, which is the mistake ee48c55 made.
    #[cfg(target_os = "linux")]
    {
        // Without this the window dies at startup with "Gdk-Message: Error 71 (Protocol error)
        // dispatching to Wayland display". Harmless no-op on non-NVIDIA drivers.
        if std::env::var_os("__NV_DISABLE_EXPLICIT_SYNC").is_none() {
            std::env::set_var("__NV_DISABLE_EXPLICIT_SYNC", "1");
        }
        // On XWayland the proprietary driver cannot back the DMABUF renderer at all: "Failed to
        // create GBM buffer of size WxH: Invalid argument", zero frames, the window never paints.
        // The AppImage always lands there, because linuxdeploy-plugin-gtk's AppRun hook exports
        // GDK_BACKEND=x11 and Tauri's bundler ships that hook; GDK_BACKEND is set nowhere in this
        // repo. `WEBKIT_DMABUF_RENDERER_FORCE_SHM=1` keeps the renderer, and the GPU, while
        // bypassing GBM. It costs about half the CPU of software rendering: 10% vs 18% on one
        // composited animation in `ui/perf/renderprobe.py`. A/B under `GDK_BACKEND=x11` on
        // 2026-08-31 (GTX 1060, driver 580.173.02, WebKitGTK 2.52.5): without it, a black window
        // and two GBM failures; with it, the app paints and keeps repainting across a track change.
        //
        // Native Wayland needs none of this and does not get it. The full DMABUF path is the
        // cheapest of the three (5% on that animation, 17% vs 23% scrolling 400 layered cards) and
        // it is stable: the "window frozen, music still playing" freeze this gate used to work
        // around was fd exhaustion, fixed by `raise_fd_limit` above. GPU compositing costs about
        // 90 MiB of web-process RSS, which is the whole price.
        //
        // /dev/nvidiactl is the proprietary driver's control node, present whenever it is loaded
        // and absent under nouveau, which does not have this bug. Loaded is not drawing, though:
        // a hybrid laptop has it and draws on Intel by default, where FORCE_SHM made album covers
        // flicker black on hover and plain DMABUF was fine (#458, UHD 630 + GTX 1050, X11). So the
        // last check asks EGL which vendor serves the display, and only a clear non-NVIDIA answer
        // stands the gate down: no answer keeps the old behaviour, since the failure it guards
        // against is a window that never paints. Optimus-manager's "nvidia" mode and PRIME
        // offload should both answer NVIDIA, which is right: NVIDIA draws there.
        //
        // The variable is only defaulted, and skipped if either WEBKIT_ knob is already set by
        // hand, so retesting stays possible.
        //
        // ponytail: delete this the day the AppImage stops forcing X11, which would put those
        // users on native Wayland and the full path, or the day the driver learns GBM on XWayland.
        //
        // GDK picks its backend from GDK_BACKEND when set, else Wayland when WAYLAND_DISPLAY is,
        // else X11.
        let on_x11 = match std::env::var("GDK_BACKEND") {
            Ok(b) => b.split(',').next() == Some("x11"),
            Err(_) => std::env::var_os("WAYLAND_DISPLAY").is_none(),
        };
        if on_x11
            && std::path::Path::new("/dev/nvidiactl").exists()
            && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
            && std::env::var_os("WEBKIT_DMABUF_RENDERER_FORCE_SHM").is_none()
            && x11_egl_vendor().map_or(true, |v| v.contains("NVIDIA"))
        {
            std::env::set_var("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1");
        }
    }

    let mut builder = tauri::Builder::default();

    // A launch from the DMG, Downloads or a translocation mirror offers to move to /Applications
    // (install.rs). While that offer is pending the single-instance guard stays off, or a new copy
    // could never replace an old one running from /Applications.
    #[cfg(target_os = "macos")]
    let move_offer = install::pending();
    #[cfg(target_os = "macos")]
    let single_instance = move_offer.is_none();
    #[cfg(not(target_os = "macos"))]
    let single_instance = true;

    // Must be the first plugin registered (its documented requirement). A second launch —
    // e.g. clicking the app icon while we're hidden in the tray — re-shows this instance
    // instead of spawning a second one (which would fight over SQLite and mpv).
    //
    // `LIMUSIC_MULTI=1` lifts the guard, because the guard is exactly what makes Listen Together
    // impossible to test on one machine. Pair it with `XDG_DATA_HOME` (Linux) or `APPDATA`
    // (Windows) pointing somewhere else, or the second copy opens the first one's SQLite file and
    // the two fight over it, which is what the guard exists to prevent:
    //
    //     LIMUSIC_MULTI=1 XDG_DATA_HOME=/tmp/limusic-b ./target/debug/limusic-app
    if single_instance && std::env::var_os("LIMUSIC_MULTI").is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            tray::show_main(app);
            // `Spotless <link>` against this instance (#348). argv[0] leads on every platform.
            if args.len() > 1 {
                deeplink::route(app, args.into_iter().skip(1));
            }
        }));
    }

    // Every webview, so a destroyed one gives its web process back on WebKitGTK 2.54 (#572).
    #[cfg(target_os = "linux")]
    {
        builder = builder.plugin(
            tauri::plugin::Builder::<_, ()>::new("webview-release")
                .on_webview_ready(|webview| release_on_destroy(&webview))
                .build(),
        );
    }

    // ── stream: perf ── `ENCORE_PERF=1` runs the dev-only timing probe in the main window
    // (ui/src/lib/perfprobe.ts): startup marks, page/search/playback timings, an idle phase for
    // CPU sampling and a navigation soak, each logged as a `perf:` line. Off unless the variable
    // is set, so a normal launch never sees it.
    if let Ok(mode) = std::env::var("ENCORE_PERF") {
        let mode = serde_json::to_string(&mode).unwrap_or_else(|_| "\"1\"".into());
        builder = builder.plugin(
            tauri::plugin::Builder::<_, ()>::new("perfprobe")
                .js_init_script(format!("window.__ENCORE_PERF__ = {mode};"))
                .build(),
        );
    }
    // ── end stream: perf ──

    builder
        // The hidden cipher webview's document (webview.rs). A registered scheme, because a
        // `data:` URL is not a document WebView2 will navigate to.
        .register_uri_scheme_protocol(webview::SCHEME, |_ctx, _req| {
            tauri::http::Response::builder()
                .header(tauri::http::header::CONTENT_TYPE, "text/html")
                .body(webview::HARNESS_HTML.as_bytes())
                .expect("static harness response")
        })
        // `spotless://` / `encore://` links and https links opened with the app (deeplink.rs). On macOS they arrive
        // as Apple Events (`RunEvent::Opened`), which this plugin turns into `deep-link://new-url`.
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // Folder picker for the local-music library (local.rs).
        .plugin(tauri_plugin_dialog::init())
        // Copying goes through Rust, not the webview. WebKitGTK gates JavaScript clipboard writes
        // (both `execCommand('copy')` and `navigator.clipboard`) behind its own policy, and on
        // Fedora every copy button in the app silently did nothing. The OS clipboard from the app
        // process has no such gate. See ui/src/lib/clipboard.ts.
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        // Reopen at the size/position the window was left at. Only "main": the mini widget is
        // fixed-size and the login/cipher/PoToken webviews are windows too. Size, position and
        // maximized only — VISIBLE would restore a window hidden to the tray as invisible, and
        // DECORATIONS would fight the custom titlebar.
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(winframe::FLAGS)
                .with_filter(|label| label == "main")
                .build(),
        )
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if let Some(mgr) = app.try_state::<Arc<hotkeys::HotkeysManager>>() {
                        mgr.handle_event(app, shortcut, event.state());
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let handle = app.handle().clone();
            // publik edition: sign in with publik before the app (publik/README.md).
            publik::gate::spawn(&handle);

            // Before anything opens a file or a window: a move ends this process.
            #[cfg(target_os = "macos")]
            if let Some(offer) = move_offer {
                install::offer(&handle, offer);
            }

            // GtkSettings belongs to the main thread, which is where setup runs (#481).
            #[cfg(target_os = "linux")]
            if let Some(layout) = gtk::Settings::default()
                .and_then(|s| gtk::prelude::GtkSettingsExt::gtk_decoration_layout(&s))
            {
                let _ = encore_core::api::BUTTON_LAYOUT.set(layout.into());
            }

            // App data dir for the SQLite file and mpv's on-disk audio cache (`Host::cache_dir`).
            let data_dir = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
            // Encore → Spotless: move the folders kept under the old bundle id before anything
            // (the log, the database, the first webview) opens them (migrate.rs).
            let identifier = app.config().identifier.clone();
            let moved = {
                let p = app.path();
                let dirs = migrate::app_dirs(
                    &[
                        p.app_data_dir().ok(),
                        p.app_local_data_dir().ok(),
                        p.app_cache_dir().ok(),
                        p.app_log_dir().ok(),
                        p.app_config_dir().ok(),
                    ],
                    p.home_dir().ok().as_deref(),
                    &identifier,
                );
                if migrate::pending(&dirs, &identifier, migrate::OLD_ID) && migrate::old_app_running() {
                    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                    app.dialog()
                        .message(
                            "Spotless is Encore's new name. Quit Encore, then open Spotless again: \
                             your library, likes, downloads and settings move over.",
                        )
                        .title("Quit Encore first")
                        .kind(MessageDialogKind::Info)
                        .show(|_| std::process::exit(0));
                    return Ok(());
                }
                migrate::migrate(&dirs, &identifier, migrate::OLD_ID)
            };
            std::fs::create_dir_all(&data_dir).ok();
            init_logging(&data_dir);
            for m in &moved.moved {
                tracing::info!(from = %m.from.display(), to = %m.to.display(), "rename: moved app folder");
            }
            for (from, e) in &moved.failed {
                tracing::warn!(from = %from.display(), error = %e, "rename: could not move app folder");
            }

            let db = match encore_core::boot::open_db(&data_dir) {
                Ok(db) => db,
                Err(e) => fatal(e.title, &e.detail),
            };
            if let Some(m) = moved.moved.iter().find(|m| m.to == data_dir) {
                let n = migrate::rewrite_paths(&db, &m.from, &m.to);
                tracing::info!(values = n, "rename: stored paths now point at the moved data folder");
            }
            // `LIMUSIC_PROXY` or the stored setting, applied to the shared HTTP client.
            let proxy = encore_core::boot::resolve_proxy(&db);
            // Built here instead of from the config (`create: false`), because WebView2 only takes
            // a proxy at creation and it isn't known until the database is open (#465).
            if let Some(config) = app.config().app.windows.first() {
                let w =
                    http::proxied(tauri::WebviewWindowBuilder::from_config(&handle, config)?)
                        .build()?;
                // ── stream: fix-data ── Spotify-size first launch (see `first_launch_size`).
                if db.get_setting(DEFAULT_SIZE_KEY).is_none() {
                    first_launch_size(&w);
                    db.set_setting(DEFAULT_SIZE_KEY, "1");
                }
                keep_on_screen(&w);
            }
            #[cfg_attr(target_os = "macos", allow(unused_variables))]
            let system_titlebar = db.get_setting("system_titlebar").as_deref() == Some("true");

            // OS media controls (MPRIS/SMTC/NowPlaying). Its callback resolves AppState lazily, so
            // it's fine to spawn before AppState is managed. context/16, D11.
            let media = media::spawn(handle.clone())
                .map(|m| Box::new(m) as Box<dyn encore_core::MediaControls>);
            let host: Arc<dyn encore_core::Host> =
                Arc::new(host::TauriHost::new(handle.clone(), data_dir.clone()));
            let mut booted =
                match encore_core::boot::assemble(host, db.clone(), proxy.as_deref(), media) {
                    Ok(b) => b,
                    Err(e) => fatal(e.title, &e.detail),
                };
            let volume = booted.volume;
            // The level unmute returns to: the current one, or after a muted quit the level saved
            // when it was muted.
            let unmute = if volume > 0 {
                Some(volume)
            } else {
                db.get_setting("volume_before_mute").and_then(|s| s.parse::<i64>().ok())
            };
            if let Some(v) = unmute.filter(|v| (1..=100).contains(v)) {
                hotkeys::LAST_NONZERO_VOLUME.store(v, std::sync::atomic::Ordering::Relaxed);
            }
            let app_state = booted.state.clone();
            app.manage(app_state.clone());
            // Smart, podcasts, polish, downloads, audio loops, the loopback proxies.
            encore_core::boot::start_services(&booted);

            // The login entry runs whichever binary last wrote it. A dev build that wrote it leaves
            // the login launch loading the vite server, which isn't running then: tray icon, no
            // window, "Could not connect to localhost". An AppImage moved after enabling leaves it
            // pointing at nothing. So an installed build repoints an existing entry at itself.
            // Only an existing one: `is_enabled` is false after a Task Manager disable on Windows,
            // and that choice is the user's.
            if !tauri::is_dev() {
                use tauri_plugin_autostart::ManagerExt;
                let al = app.autolaunch();
                if al.is_enabled().unwrap_or(false) {
                    if let Err(e) = al.enable() {
                        tracing::warn!(error = %e, "could not repoint the autostart entry");
                    }
                }
            }

            {
                use tauri_plugin_deep_link::DeepLinkExt;
                // An AppImage has no installer to tell the desktop about spotless:// links (the
                // .deb/.rpm ship them in their .desktop file): register them at launch, pointing
                // at this AppImage's path. Windows' NSIS installer writes them to the registry.
                #[cfg(target_os = "linux")]
                if app.env().appimage.is_some() {
                    if let Err(e) = app.deep_link().register_all() {
                        tracing::warn!(error = %e, "could not register the spotless:// link handler");
                    }
                }
                let h = handle.clone();
                app.deep_link().on_open_url(move |e| {
                    deeplink::route(&h, e.urls().into_iter().map(|u| u.to_string()));
                });
            }

            // Native menu bar (menu.rs), macOS only: Spotify on Windows has no menu bar, and a
            // Tauri menu there would draw a strip under the title bar. Off macOS the page binds the
            // same shortcuts itself (shortcuts.ts, `native` false). UI items reach the page as
            // `menu://<id>` events.
            #[cfg(target_os = "macos")]
            match menu::build(&handle) {
                Ok(m) => {
                    if let Err(e) = app.set_menu(m) {
                        tracing::warn!(error = %e, "menu bar could not be set");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "menu bar could not be built"),
            }

            // System tray: playback controls + show/quit while running in the background.
            if let Err(e) = tray::init(&handle) {
                tracing::warn!(error = %e, "tray init failed (continuing without tray)");
                tray::set_available(false);
            }

            // System-wide global hotkeys for playback control
            let hotkeys_cfg = hotkeys::load_config(&app_state.db);
            let hotkeys_mgr = Arc::new(hotkeys::HotkeysManager::new(hotkeys_cfg.clone()));
            app.manage(hotkeys_mgr.clone());
            let reg_res = hotkeys_mgr.apply_config(&handle, hotkeys_cfg);
            if !reg_res.success {
                tracing::warn!(errors = ?reg_res.errors, "some global hotkeys could not be registered on startup");
            }


            // Keep the login alive by itself (#165 / KI-2). Two signals from the transport:
            // `cookie_changed` when a response rotated `__Secure-*SIDTS` (persist it, or the next
            // launch starts from a dead snapshot again), and `session_rejected` when YouTube
            // turned the session down (re-mint it from the login webview's own Google session,
            // instead of leaving the user with a "sign in again" they have to act on).
            {
                let st = app_state.clone();
                let app_handle = handle.clone();
                let rejected = st.it.session_rejected();
                let rotated = st.it.cookie_changed();
                tauri::async_runtime::spawn(async move {
                    // Ask once, up front, rather than waiting for whatever the UI happens to
                    // request first: the app can start straight to the tray, with nothing on
                    // screen to run into the dead session and report it. One request, and the
                    // 401 it may come back with goes down the same healing path as any other.
                    if st.it.is_logged_in() {
                        if let Some(client) = st.clients.get(innertube::METADATA_CLIENT) {
                            // No heal-waiting for it: this runs *before* the loop below, so
                            // there is nobody to answer a wait yet, and a dead session would
                            // stall the healer's own startup for the whole timeout. Its 401
                            // raises the flag instead, and the loop picks that up on entry.
                            let _ = innertube::without_healing(st.it.account_menu(client)).await;
                        }
                    }
                    // Google rolls its short-lived tokens on the requests the app makes, so an
                    // idle night leaves the jar to expire on its own. Half-hourly is well inside
                    // the window and costs one request.
                    let mut keepalive = tokio::time::interval(Duration::from_secs(30 * 60));
                    keepalive.tick().await; // the first tick is immediate; the ping above covered it
                    loop {
                        tokio::select! {
                            _ = rejected.notified() => {
                                // The guard is what parked requests are watching: while it lives
                                // they keep waiting however long this takes, and dropping it
                                // releases them whether or not anything was re-minted. They retry
                                // either way, so the ones that really are dead can say so instead
                                // of holding a spinner until the timeout.
                                let _heal = st.it.begin_heal();
                                innertube::without_healing(
                                    session::refresh_session(app_handle.clone(), st.clone()),
                                )
                                .await;
                            }
                            _ = rotated.notified() => st.persist_rotated_cookie(),
                            _ = keepalive.tick() => st.keep_session_alive().await,
                        }
                    }
                });
            }

            // Queue restore, visitorData bootstrap, the player event pump, cipher/PoToken
            // prewarm and idle teardown.
            encore_core::boot::start_playback(&mut booted);

            // Hand the frame to the compositor when the user asked for it (issue #65). The window
            // is created undecorated, so this is the one place that reverses it; macOS never gets
            // here, its traffic lights come from `tauri.macos.conf.json`. Done before the SPA shows
            // the window, so the frame is already there rather than popping in.
            #[cfg(not(target_os = "macos"))]
            if system_titlebar {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.set_decorations(true);
                }
            }

            // The window starts hidden and the SPA shows it once it has mounted, so the saved size
            // is already applied by then (#45). Safety net: if the frontend never gets that far,
            // show it anyway rather than leaving the app with no window at all. Not when starting
            // in the tray, where the hidden window is the point.
            if let Some(w) = app.get_webview_window("main") {
                if !should_start_minimized(&db) {
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        if !w.is_visible().unwrap_or(true) {
                            let _ = w.show();
                        }
                    });
                }
            }

            #[cfg(target_os = "macos")]
            if let Some(w) = app.get_webview_window("main") {
                enable_swipe_navigation(&w);
            }
            #[cfg(target_os = "linux")]
            {
                tune_webview_labelled(app.handle(), "main", true);
                if db.get_setting("ambient_light").as_deref() == Some("true") {
                    set_webgl(app.handle(), true);
                }
                spawn_heap_trimmer();
            }
            Ok(())
        })
        .invoke_handler(only_ui(tauri::generate_handler![
            // publik edition: the sign-in screen (publik/README.md).
            publik::gate::publik_gate_screen,
            publik::gate::publik_gate_press,
            // ── stream: smart ──
            smart::commands::smart_settings,
            smart::commands::smart_set_setting,
            smart::commands::smart_home,
            smart::commands::smart_home_local, // stream: magic-d4
            smart::commands::podcast_home,     // stream: home-tiles-pods
            smart::commands::podcast_category, // stream: home-tiles-pods
            smart::commands::smart_mix,
            smart::commands::playlist_recommendations,
            smart::commands::explicit_flags,
            smart::commands::playable_flags,
            smart::commands::taste_profile,
            // ── end stream: smart ──
            // ── stream: smart2 ──
            smart::commands::made_for_you,
            smart::commands::whats_new,
            smart::commands::whats_new_count,
            smart::commands::whats_new_mark_seen,
            smart::commands::smart_shuffle_state,
            smart::commands::smart_shuffle_set,
            smart::commands::smart_shuffle_remove,
            smart::commands::smart_shuffle_keep,
            // ── end stream: smart2 ──
            // ── lane magic-d3 ──
            commands::smart_tags_liked,
            commands::enhance_state,
            commands::enhance_set,
            commands::enhance_remove,
            commands::enhance_keep,
            // lane magic-d1
            commands::radio_tracks,
            commands::play_single_at,
            commands::song_credits,
            commands::discography, // stream: onecore
            commands::taste_set_excluded,
            commands::taste_excluded,
            // ── stream: audiobooks ──
            audiobooks::audiobook_search,
            audiobooks::audiobook_home,
            audiobooks::audiobook_genre,
            audiobooks::audiobook_page,
            audiobooks::audiobook_for_chapter,
            audiobooks::audiobook_save,
            audiobooks::audiobook_saved,
            // ── stream: podcasts ──
            podcasts::commands::podcast_search,
            podcasts::commands::podcast_top_show, // ── stream: podtop ──
            commands::search_blend_artist_top, // ── stream: podtop ──
            podcasts::commands::podcast_show,
            podcasts::commands::podcast_show_more,
            podcasts::commands::podcast_episode,
            podcasts::commands::podcast_chapters, // ── stream: chapters ──
            podcasts::commands::podcast_episode_video, // ── stream: video-pods ──
            podcasts::commands::podcast_follow,
            podcasts::commands::podcast_unfollow,
            podcasts::commands::podcast_follows,
            podcasts::commands::podcast_add_feed,
            podcasts::commands::podcast_new_episodes,
            podcasts::commands::podcast_continue_listening,
            podcasts::commands::podcast_saved_episodes,
            podcasts::commands::podcast_save_episode,
            podcasts::commands::podcast_mark_played,
            podcasts::commands::podcast_mark_show_played,
            podcasts::commands::podcast_progress,
            podcasts::commands::podcast_refresh,
            podcasts::commands::podcast_opml_export,
            podcasts::commands::podcast_opml_import,
            podcasts::commands::podcast_settings,
            podcasts::commands::podcast_set_settings,
            // ── end stream: podcasts ──
            commands::search,
            commands::search_all,
            commands::search_suggestions,
            commands::search_cards,
            commands::search_overview, // stream: search2
            commands::search_page, // stream: search2
            commands::search_lyrics, // stream: search2
            commands::search_blend_lyrics, // stream: search2
            commands::get_live_now,
            commands::search_videos,
            commands::song,
            commands::play,
            commands::hint_play, // stream: firstaudio
            commands::play_index,
            commands::switch_version,
            commands::remove_from_queue,
            commands::clear_queued,
            commands::add_to_queue_at, // lane magic-d2
            commands::restore_queued, // lane magic-d2
            commands::add_to_queue,
            commands::move_in_queue,
            commands::play_next,
            commands::next_track,
            commands::prev_track,
            commands::back_to_previous,
            commands::toggle_shuffle,
            commands::set_repeat,
            commands::toggle_pause,
            commands::seek,
            commands::playback_clock,
            commands::set_volume,
            commands::set_playback_params,
            // ── stream: audio ──
            audio::audio_settings,
            audio::audio_eq_info,
            audio::set_eq,
            audio::set_eq_preset,
            audio::set_normalize_volume,
            audio::set_volume_level,
            audio::set_crossfade_secs,
            audio::audio_devices,
            audio::set_audio_device,
            audio::set_sleep_timer,
            audio::cancel_sleep_timer,
            audio::sleep_timer,
            // ── end stream: audio ──
            commands::get_queue,
            commands::get_playback,
            commands::video_stream,
            commands::forget_video_stream,
            commands::get_settings,
            commands::set_setting,
            commands::get_global_hotkeys,
            commands::global_hotkeys_on_wayland,
            commands::set_global_hotkeys,
            commands::reset_global_hotkeys,
            commands::get_stream_clients,
            commands::clear_caches,
            commands::cache_size, // stream: p-settings
            commands::reveal_in_finder, // stream: p-settings
            commands::save_text_file, // stream: p-settings
            commands::local_user_name, // stream: parity3
            commands::get_account,
            commands::get_account_identities,
            commands::switch_account,
            commands::sign_out,
            commands::login_webview,
            commands::cancel_login,
            commands::get_google_accounts,
            commands::switch_google_account,
            commands::remove_google_account,
            commands::open_mini,
            commands::close_mini,
            commands::set_mini_compact,
            commands::show_main,
            commands::take_launch_args,
            commands::get_home,
            commands::network_reachable,
            commands::get_home_more,
            commands::get_library,
            commands::get_library_albums,
            commands::get_library_artists,
            commands::get_library_subscriptions,
            commands::get_history,
            commands::get_playlist,
            commands::get_playlist_more,
            commands::get_playlist_suggestions,
            commands::playlist_index,
            commands::sync_playlist_index,
            commands::play_counts,
            commands::listening_stats,
            commands::top_artists,
            commands::clear_listening_history,
            commands::get_album,
            commands::get_blocked_artists,
            commands::block_artist,
            commands::unblock_artist,
            commands::get_local_library,
            commands::add_local_folder,
            commands::remove_local_folder,
            commands::get_artist,
            commands::get_artist_appears_on,
            commands::get_browse_grid,
            commands::get_moods,
            commands::get_mood_art,
            commands::play_playlist,
            commands::start_radio,
            commands::rate,
            commands::set_song_saved,
            commands::set_album_saved,
            commands::add_to_playlist,
            commands::readonly_playlists,
            commands::add_to_local_playlist,
            commands::local_playlists,
            commands::remove_from_playlist,
            commands::remove_many_from_playlist,
            commands::move_in_playlist,
            commands::create_playlist,
            commands::edit_playlist_details,
            commands::set_playlist_cover,
            commands::set_playlist_sort,
            commands::delete_playlist,
            commands::subscribe,
            commands::import_read,
            commands::import_read_file,
            commands::import_start,
            commands::import_status,
            commands::import_offer,
            commands::import_offer_done,
            commands::import_rows,
            commands::import_pick,
            commands::import_create,
            commands::import_cancel,
            commands::import_source,
            commands::import_update,
            commands::import_resolve,
            commands::get_lyrics,
            commands::choose_lyrics_source,
            commands::set_lyrics_offset,
            commands::lyrics_providers,
            commands::lyrics_parse,
            commands::theater_fullscreen,
            commands::title_bar_double_click,
            commands::can_self_update,
            commands::open_external,
            commands::quit_app,
            commands::diagnostics,
            commands::diagnostics_summary,
            commands::save_diagnostics,
            commands::log_ui,
            // ── device sync (docs/SYNC.md) ──
            sync::sync_status,
            sync::sync_start_pairing,
            sync::sync_cancel_pairing,
            sync::sync_join,
            sync::sync_unpair,
            sync::sync_now,
            sync::sync_set_active,
            // ── stream: library ──
            library::lib_snapshot,
            library::lib_liked_ids,
            library::lib_liked_songs,
            library::lib_like,
            library::lib_unlike,
            library::lib_restore_likes,
            library::lib_saved,
            library::lib_save,
            library::lib_unsave,
            library::lib_restore_saved,
            library::lib_mark_synced,
            library::lib_playlists,
            library::lib_playlist_create,
            library::lib_playlist_update,
            library::lib_playlist_delete,
            library::lib_playlist_restore,
            library::lib_deleted_playlists, // stream: magic-d4
            library::lib_playlist_entries,
            library::lib_playlist_duplicates,
            library::lib_playlist_add,
            library::lib_playlist_remove,
            library::lib_playlist_insert_entries,
            library::lib_playlist_move,
            library::lib_playlist_set_order,
            library::lib_folders,
            library::lib_folder_create,
            library::lib_folder_rename,
            library::lib_folder_move,
            library::lib_folder_delete,
            library::lib_folder_restore,
            library::lib_folder_set_item,
            library::lib_pins,
            library::lib_pin,
            library::lib_unpin,
            library::lib_pins_reorder,
            library::lib_note_played,
            library::lib_recent_contexts,
            library::lib_recent_tracks,
            library::lib_last_played,
            library::lib_hidden_songs,
            library::lib_hide_song,
            library::lib_unhide_song,
            library::lib_private_session,
            library::lib_set_private_session,
            library::lib_migrate_legacy,
            library::lib_export,
            // ── end stream: library ──
            // ── stream: polish ──
            polish::commands::get_charts,
            polish::commands::get_new_releases,
            polish::commands::get_browse_sections,
            polish::commands::playback_skip_reasons,
            polish::commands::playback_health,
            polish::commands::recent_play_contexts,
            polish::commands::podcast_youtube_twin,
            polish::commands::podcast_youtube_match,
            polish::commands::podcast_set_youtube_match,
            // ── end stream: polish ──
            // ── stream: downloads ──
            downloads::commands::downloads_snapshot,
            downloads::commands::downloads_add,
            downloads::commands::downloads_remove,
            downloads::commands::downloads_remove_all,
            downloads::commands::downloads_retry,
            downloads::commands::downloads_sync,
            downloads::commands::downloads_set_settings,
            downloads::commands::downloads_set_dir,
            downloads::commands::downloads_songs,
            // ── end stream: downloads ──
        ]))
        .on_menu_event(|app, event| menu::handle(app, event.id().as_ref()))
        .on_window_event(|window, event| {
            winframe::on_event(window, event); // ── stream: parity2 ──
            // Close-to-tray: ✕ hides the main window and playback keeps running; real quit is
            // the tray's Quit item (or the "close_to_tray=false" setting). Label-gated: the
            // hidden cipher/PoToken webviews are windows too and must close normally.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                match window.label() {
                    "main" => {
                        let hide = tray::available()
                            && window
                                .app_handle()
                                .try_state::<Arc<AppState>>()
                                .map(|s| close_hides(s.db.get_setting("close_to_tray").as_deref()))
                                .unwrap_or(true);
                        if hide {
                            api.prevent_close();
                            let _ = window.hide();
                            tray::set_main_visible(window.app_handle(), false);
                        }
                    }
                    // A WM shortcut closing the widget goes the same way as its own close button:
                    // position saved, main window left as it was (or brought back if hidden).
                    mini::LABEL => {
                        api.prevent_close();
                        mini::dismiss(window.app_handle());
                    }
                    _ => {}
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|handle, event| {
            // The hidden cipher/PoToken webviews are windows too, so closing the main window no
            // longer auto-exits the app. Quit when the main window is destroyed.
            if let tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::Destroyed,
                ..
            } = &event
            {
                if label == "main" {
                    handle.exit(0);
                }
            }

            // Dock icon clicked (macOS). ✕ hides the main window to keep music playing, so this is
            // the way back, as in every Mac app. The mini player counts as a visible window, so
            // check the main window itself rather than `has_visible_windows`.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = &event {
                let main_visible = handle
                    .get_webview_window("main")
                    .and_then(|w| w.is_visible().ok())
                    .unwrap_or(false);
                if !main_visible {
                    tray::show_main(handle);
                }
            }

            // The single place that saves positions on exit. It covers `app.exit`,
            // `request_restart`, the main window closing, and macOS Cmd+Q from the native menu.
            if let tauri::RunEvent::Exit = &event {
                if let Some(state) = handle.try_state::<Arc<AppState>>() {
                    state.flush_position();
                }
                mini::save_position(handle);
                winframe::on_exit(handle); // ── stream: parity2 ── after the plugin's save
            }
        });
}

/// Let app commands answer only the UI's own windows. Tauri checks capabilities for plugin
/// commands, but without an app ACL manifest an app command is open to any webview on a local
/// origin, and the hidden cipher webview is one: its `limusicharness://` document runs YouTube's
/// player.js. It needs no IPC, the bridge drives it through eval callbacks (`webview.rs`). The
/// login windows load remote pages, which Tauri refuses on its own.
fn only_ui<R: tauri::Runtime>(
    handler: impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static,
) -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    move |invoke| {
        if matches!(invoke.message.webview_ref().label(), "main" | mini::LABEL) {
            return handler(invoke);
        }
        invoke.resolver.reject("not allowed from this webview");
        true
    }
}

/// ✕ hides to tray unless the user explicitly set close_to_tray=false (unset → default on).
///
/// Gated by [`tray::available`] at the call site: with no tray to click, hiding would strand the
/// app with no window (#232).
fn close_hides(setting: Option<&str>) -> bool {
    setting != Some("false")
}

#[cfg(test)]
mod tests {
    use super::close_hides;

    #[tauri::command]
    fn ping() {}

    /// The hidden cipher webview runs YouTube's player.js on a local origin, so Tauri lets it
    /// reach app commands unless `only_ui` turns it away.
    #[test]
    fn app_commands_answer_only_the_ui() {
        let app = tauri::test::mock_builder()
            .invoke_handler(super::only_ui(tauri::generate_handler![ping]))
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        for (label, allowed) in [("main", true), ("mini", true), ("cipher", false)] {
            let wv =
                tauri::WebviewWindowBuilder::new(&app, label, Default::default()).build().unwrap();
            let res = tauri::test::get_ipc_response(
                &wv,
                tauri::webview::InvokeRequest {
                    cmd: "ping".into(),
                    callback: tauri::ipc::CallbackFn(0),
                    error: tauri::ipc::CallbackFn(1),
                    url: if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" }
                        .parse()
                        .unwrap(),
                    body: Default::default(),
                    headers: Default::default(),
                    invoke_key: tauri::test::INVOKE_KEY.into(),
                },
            );
            assert_eq!(res.is_ok(), allowed, "{label}");
        }
    }

    /// `tauri.macos.conf.json` overrides the main window so macOS gets real traffic lights over the
    /// app's own titlebar (issue #65). Tauri merges platform config with RFC 7386 JSON Merge Patch,
    /// which replaces arrays wholesale, so that file has to repeat every window key from
    /// `tauri.conf.json` rather than patch the few it changes. Nothing on this machine builds for
    /// macOS, so a key dropped from the copy would only surface as a wrongly sized window shipped
    /// by CI. This fails instead.
    #[test]
    fn macos_window_config_mirrors_the_base_window() {
        let base: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let mac: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.macos.conf.json")).unwrap();
        let base_win = base["app"]["windows"][0].as_object().unwrap();
        let mac_win = mac["app"]["windows"][0].as_object().unwrap();

        // Only these two may differ; the frame is the compositor's on macOS.
        let overridden = ["decorations", "transparent"];
        for (k, v) in base_win {
            let got = mac_win.get(k).unwrap_or_else(|| panic!("tauri.macos.conf.json drops `{k}`"));
            if !overridden.contains(&k.as_str()) {
                assert_eq!(got, v, "tauri.macos.conf.json disagrees on `{k}`");
            }
        }
        assert_eq!(mac_win["decorations"], serde_json::json!(true));
        assert_eq!(mac_win["titleBarStyle"], serde_json::json!("Overlay"));
        assert_eq!(mac_win["hiddenTitle"], serde_json::json!(true));

        // Catches a typo or a key Tauri would reject: WindowConfig is `deny_unknown_fields`.
        serde_json::from_value::<tauri::utils::config::WindowConfig>(
            mac["app"]["windows"][0].clone(),
        )
        .expect("macOS window config is not a valid WindowConfig");
    }

    #[test]
    fn close_hides_unless_explicitly_disabled() {
        assert!(close_hides(None)); // fresh install → tray on
        assert!(close_hides(Some("true")));
        assert!(close_hides(Some("garbage")));
        assert!(!close_hides(Some("false")));
    }
}
