//! The UI's API (context/11 UI contract): what used to be the bodies of the Tauri commands. The
//! desktop wraps each in a `#[tauri::command]` (`src-tauri/src/commands.rs`); iOS calls them
//! through UniFFI (`crates/ffi`). No YouTube shapes leak past here; the UI never sees a stream URL.

use std::sync::Arc;

use innertube::{
    AlbumPage, ArtistPage, BrowseItem, HistoryGroup, HomePage, MoodSection, PlaylistContinuation,
    PlaylistPage, PlaylistSort, PlaylistSuggestions, Rating, SearchResults, SearchSuggestions,
    SongItem,
};
use crate::host::HostExt;

use crate::blocked::BlockedArtist;
use crate::state::{
    is_local_playlist, AppState, LOCAL_PLAYLIST_PREFIX, ON_REPEAT_ID, ON_REPEAT_LIMIT,
    ON_REPEAT_WINDOW_SECS,
};


/// `record_history`: true only for a query the user submitted. A typeahead preview passes false and
/// goes out unauthenticated, so half-typed prefixes never reach the account's search history (#203).
pub async fn search(
    state: &Arc<AppState>,
    query: String,
    record_history: bool,
) -> Result<Vec<SongItem>, String> {
    let client = state.clients.get(innertube::METADATA_CLIENT).ok_or("metadata client missing")?;
    let result =
        state.it.search_songs(client, &query, record_history).await.map_err(|e| e.to_string())?;
    // ── stream: smart ── studio version first, other versions demoted (identity re-ranker).
    Ok(crate::smart::recs::rerank_search_songs(&state.db, &query, result.items))
}

// ── stream: podtop ──
/// An artist query's Songs list with the artist page's "Top songs" order blended in
/// (`identity::apply_artist_top_songs`: Borda over the two ranks, benchmarked in search_bench).
/// `songs` is `search`'s answer, `artist` / `top` the page's name and `topSongs`. The search page
/// reads the page of `searchAll`'s artist card while `search` runs, then calls this. Pure, no I/O;
/// no artist intent or another artist's page returns `songs` unchanged.
pub fn search_blend_artist_top(
    query: String,
    songs: Vec<SongItem>,
    artist: String,
    top: Vec<SongItem>,
) -> Vec<SongItem> {
    crate::identity::apply_artist_top_songs(&query, songs, &artist, &top)
}
// ── end stream: podtop ──

/// Search video uploads only: the Videos shelf and its "Show more" page (#209, #266). Never
/// records history, the page's other searches already did.
pub async fn search_videos(state: &Arc<AppState>, query: String) -> Result<Vec<SongItem>, String> {
    let client = metadata_client(state)?;
    let result = state.it.search_videos(client, &query).await.map_err(|e| e.to_string())?;
    Ok(result.items)
}

/// One song or video by id, for a pasted YouTube link (#441). `/next` is the one call that answers
/// with the id's own title, artists and art; the seed comes back as one of its rows.
pub async fn song(state: &Arc<AppState>, video_id: String) -> Result<SongItem, String> {
    let client = metadata_client(state)?;
    let next = state.it.next(client, Some(&video_id), None).await.map_err(|e| e.to_string())?;
    next.items.into_iter().find(|i| i.video_id == video_id).ok_or_else(|| "not_found".into())
}

/// Unfiltered search → categorized sections for the search page. `record_history` as in [`search`].
pub async fn search_all(
    state: &Arc<AppState>,
    query: String,
    record_history: bool,
) -> Result<SearchResults, String> {
    let client = metadata_client(state)?;
    let results =
        state.it.search_all(client, &query, record_history).await.map_err(|e| e.to_string())?;
    // ── stream: smart ── re-rank Songs and the Top Result (identity re-ranker).
    Ok(crate::smart::recs::rerank_search_all(&state.db, &query, results))
}

/// Typeahead completions + a few matching rows, signed in (see `InnerTube::search_suggestions`).
pub async fn search_suggestions(state: &Arc<AppState>, query: String) -> Result<SearchSuggestions, String> {
    let client = metadata_client(state)?;
    state.it.search_suggestions(client, &query).await.map_err(|e| e.to_string())
}

/// Filtered "Show more" search for one category (albums / artists / playlists).
pub async fn search_cards(
    state: &Arc<AppState>,
    query: String,
    category: String,
) -> Result<Vec<BrowseItem>, String> {
    let client = metadata_client(state)?;
    state.it.search_cards(client, &query, &category).await.map_err(|e| e.to_string())
}

/// Play a track (from a search result). The UI passes the full item so we can seed the queue
/// with its metadata without another round-trip.
pub async fn play(state: &Arc<AppState>, item: SongItem) -> Result<(), String> {
    play_single(state, item, None).await
}

/// ── stream: autoplay ── A single pick: play this song only, then Autoplay (`crate::autoplay`).
/// `from` is the "Playing from" label, the search query for a search result.
pub async fn play_single(
    state: &Arc<AppState>,
    item: SongItem,
    from: Option<String>,
) -> Result<(), String> {
    let state = state.clone();
    // ── stream: podcast-upnext ── an episode gets the podcast queue (history, its show's next).
    if crate::podcasts::is_episode_item(&item) {
        return play_playlist(&state, vec![item], Some(0), None, from, None, None, None, None).await;
    }
    state.play_single(item, from).await;
    Ok(())
}

// ── stream: firstaudio ──
/// A row was hovered: resolve its track ahead of the click (rationed, background).
/// Returns at once; the work runs detached.
pub fn hint_play(state: &Arc<AppState>, item: SongItem) {
    let state = state.clone();
    crate::rt::spawn(async move { state.hint_play(item).await });
}
// ── end stream: firstaudio ──

pub async fn play_index(state: &Arc<AppState>, index: usize, start: Option<f64>) -> Result<(), String> {
    let state = state.clone();
    state.play_index_at(index, start).await;
    Ok(())
}

/// The player view's Song/Video switch: the playing track's other version, same position (#442).
pub async fn switch_version(state: &Arc<AppState>) -> Result<(), String> {
    let state = state.clone();
    state.switch_version().await;
    Ok(())
}

/// Remove an upcoming track from the queue (not the one playing). Guests are add-only — blocked
/// inside AppState.
pub async fn remove_from_queue(state: &Arc<AppState>, index: usize) -> Result<(), String> {
    state.clone().remove_from_queue(index).await;
    Ok(())
}

/// "Play next" from a ⋯ menu: one track or a whole album/playlist, inserted right after the
/// current song (behind any earlier manual adds). `from` is the album/playlist title, which heads
/// the block in the queue panel.
pub async fn play_next(
    state: &Arc<AppState>,
    items: Vec<SongItem>,
    from: Option<String>,
) -> Result<(), String> {
    state.clone().play_next(items, from).await;
    Ok(())
}

/// Drag-to-reorder in the queue panel: move the upcoming track at `from` to `to`. Out-of-range or
/// already-played indices are ignored.
pub async fn move_in_queue(state: &Arc<AppState>, from: usize, to: usize) -> Result<(), String> {
    state.clone().move_in_queue(from, to).await;
    Ok(())
}

/// "Add to queue": the tracks go at the back of the "Next in queue" block, so they play after
/// everything already queued by hand and ahead of the playing context (and its radio/filler).
/// `from` heads the block in the queue panel; `continuation` is the source page's next-page token —
/// the rest of a long playlist is walked in in the background.
pub async fn add_to_queue(
    state: &Arc<AppState>,
    items: Vec<SongItem>,
    from: Option<String>,
    continuation: Option<String>,
) -> Result<(), String> {
    state.clone().add_to_queue(items, from, continuation).await;
    Ok(())
}

/// Clear every upcoming manually-queued track (the queue panel's "Next in queue" section).
pub async fn clear_queued(state: &Arc<AppState>) -> Result<(), String> {
    state.clone().clear_queued().await;
    Ok(())
}

pub async fn next_track(state: &Arc<AppState>) -> Result<(), String> {
    state.clone().next_in_queue().await;
    Ok(())
}

pub async fn prev_track(state: &Arc<AppState>) -> Result<(), String> {
    state.clone().prev_in_queue().await;
    Ok(())
}

/// The queue panel's "Back to …": put back the queue a click replaced, at the track and position
/// it was left at. Previous does this too, but only from the top of a track; this one is the whole
/// point of the line, so it doesn't rewind first.
pub async fn back_to_previous(state: &Arc<AppState>) -> Result<(), String> {
    state.clone().restore_prev_context().await;
    Ok(())
}

pub async fn toggle_shuffle(state: &Arc<AppState>) -> Result<(), String> {
    state.clone().toggle_shuffle().await;
    Ok(())
}

/// `mode` ∈ "off" | "all" | "one".
pub async fn set_repeat(state: &Arc<AppState>, mode: String) -> Result<(), String> {
    let mode = match mode.as_str() {
        "off" => crate::state::RepeatMode::Off,
        "all" => crate::state::RepeatMode::All,
        "one" => crate::state::RepeatMode::One,
        other => return Err(format!("unknown repeat mode: {other}")),
    };
    state.clone().set_repeat(mode).await;
    Ok(())
}

pub async fn toggle_pause(state: &Arc<AppState>) -> Result<(), String> {
    let state = state.clone();
    state.resume_or_toggle().await;
    Ok(())
}

pub async fn seek(state: &Arc<AppState>, position: f64) -> Result<(), String> {
    // Routed through AppState so a Listen Together host broadcasts the seek and a guest is blocked.
    state.user_seek(position).await
}

/// A fresh position and when it was read (`AppState::playback_clock`). Async, so the read of mpv
/// happens off the main thread.
pub async fn playback_clock(state: &Arc<AppState>) -> Result<Option<serde_json::Value>, String> {
    Ok(state.playback_clock())
}

/// Tempo (`player::SPEED_RANGE`), pitch (−12..=12 semitones), and reverb. `target` is `"music"`
/// (the Speed & pitch sheet) or `"episode"` (the podcast speed menu); without one, whatever is
/// playing. The two never share a speed (speeds.rs).
pub async fn set_playback_params(
    state: &Arc<AppState>,
    speed: f64,
    semitones: i32,
    reverb: player::Reverb,
    target: Option<String>,
) -> Result<(), String> {
    let target = target.as_deref().and_then(crate::speeds::Target::parse);
    state.set_playback_params(speed, semitones, reverb, target).await
}

pub async fn get_queue(state: &Arc<AppState>) -> Result<serde_json::Value, String> {
    Ok(state.queue_snapshot().await)
}

/// Settings the UI is allowed to read *and write*. Session/auth material (`session_cookie`,
/// `selected_identity_json`, `data_sync_id`, `account_json`, `account_selection_pending`,
/// `visitor_data`) and internal blobs (`queue_json`, `queue_index`, `queue_position`) never cross
/// into the webview: they'd otherwise ship the login credential to the renderer on every open, and
/// the webview can't overwrite them either.
pub const UI_SETTINGS: [&str; 36] = [
    "volume",
    // The level unmute returns to, kept across restarts (written by Rust on mute; read-only use).
    "volume_before_mute",
    "proxy",
    "quality",
    "normalize_volume",
    "enable_history",
    "disabled_stream_clients",
    "discord_rpc",
    "discord_rpc_config",
    "close_to_tray",
    "track_notifications",
    "track_notifications_focused",
    "play_on_start",
    "autostart",
    "start_minimized",
    "autoplay",
    "hide_videos",
    "prevent_duplicates",
    "update_banner",
    "update_channel",
    "lyrics_providers",
    "lyrics_prefer_words",
    "music_videos",
    "videos_first",
    // stream: video-pods. "true" plays video podcasts as audio first (podcasts::video).
    "podcast_audio_only",
    "ambient_light",
    "sticky_shuffle",
    "shuffle_whole_queue",
    "system_titlebar",
    "lastfm_primary_artist",
    "lastfm_primary_strict",
    "lastfm_config",
    "crossfade",
    "crossfade_secs",
    "locale",
    // "false" leaves out of track lists the rows playback steps over, instead of greying them.
    "show_unplayable",
];

/// Resolve the music video for `video_id` and hand back a `limusicvideo://` URL the player view
/// can put in a `<video src>`. `None` when YouTube has no usable video stream for it, which is the
/// ordinary answer for a song and leaves the artwork in place. The real googlevideo URL never
/// leaves Rust (context/11).
pub async fn video_stream(
    state: &Arc<AppState>,
    video_id: String,
    max_height: i32,
) -> Result<Option<String>, String> {
    if crate::local::is_local_song(&video_id) {
        return Ok(None);
    }
    // Already resolved and still live: the loopback URL is a pure function of the videoId, so
    // there is nothing left to do. Re-resolving costs up to two `/player` round trips, and the
    // player view pays them on every reopen otherwise.
    if state.video_url(&video_id).is_some() {
        return Ok(crate::videoproxy::url_for(&video_id));
    }
    // The webview picks the height from its own box, so clamp it here rather than trusting it.
    let max_height = max_height.clamp(144, 1080);
    match state.orchestrator.resolve_video(&video_id, max_height, &state.disabled_clients()).await {
        Some(url) => {
            state.put_video_url(&video_id, url);
            Ok(crate::videoproxy::url_for(&video_id))
        }
        None => Ok(None),
    }
}

/// Forget a resolved music-video URL, so the next `video_stream` for this id resolves a fresh one.
/// The player view calls this when the `<video>` element fails to load, which is what an expired
/// or revoked googlevideo link looks like from the webview. It also sends that track's next resolve
/// to VISIONOS first, so a WEB_REMIX URL that failed is not rebuilt from the same reply.
pub async fn forget_video_stream(state: &Arc<AppState>, video_id: String) -> Result<(), String> {
    state.forget_video_url(&video_id);
    state.orchestrator.mark_video_failed(&video_id);
    Ok(())
}

/// Who draws the window frame, as the SPA needs to know it (issue #65). Read-only, derived: the
/// stored `system_titlebar` preference on Linux/Windows, and always `overlay` on macOS, where the
/// traffic lights come from `tauri.macos.conf.json`'s `titleBarStyle: Overlay` and there is no
/// preference to make. Anything but `off` means the app hides its own window buttons, drops its
/// corner rounding and leaves resizing to the compositor.
pub fn native_chrome(db: &crate::db::Db) -> &'static str {
    if cfg!(target_os = "macos") {
        "overlay"
    } else if db.get_setting("system_titlebar").as_deref() == Some("true") {
        "on"
    } else {
        "off"
    }
}

/// GTK's `gtk-decoration-layout` (`menu:close`, `:minimize,maximize,close`), read once on the main
/// thread in `setup`. Linux only: the titlebar draws minimize and maximize only when it names
/// them, as GTK's own headerbars do (#481). Tiling compositors (Hyprland, sway, niri) ignore
/// minimize and often drop maximize, and their unconfigured layout is `menu:close`.
pub static BUTTON_LAYOUT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Store one of the UI's settings and apply it to whatever is live (the desktop also handles its
/// window-system keys around this: autostart, ambient light, the system titlebar).
pub async fn set_setting(state: &Arc<AppState>, key: &str, value: &str) -> Result<(), String> {
    if !UI_SETTINGS.contains(&key) {
        return Err(format!("unknown setting: {key}"));
    }
    let quality_changed =
        key == "quality" && state.db.get_setting("quality").as_deref() != Some(value);
    state.db.set_setting(key, value);
    setting_applied(state, key, value, quality_changed).await;
    Ok(())
}

/// Apply a stored setting to whatever is live. `set_setting` calls it after a change from this
/// device's UI, and device sync (sync/mod.rs) after a change that came from another device.
pub async fn setting_applied(state: &Arc<AppState>, key: &str, value: &str, quality_changed: bool) {
    // Cached stream URLs were picked at the old quality: drop them, and the primed next track.
    if quality_changed && key == "quality" {
        state.quality_changed().await;
    }
    // A music video track already playing gets its picture now rather than from the next track.
    if key == "music_videos" && value == "true" {
        state.attach_current_video().await;
    }
    // And the rest of the queue moves to the version the settings now pick.
    if key == "music_videos" || key == "videos_first" {
        state.apply_version_preference().await;
    }
    // Retune the track that's playing. Unlike crossfade below, this one has to apply to what the
    // user is hearing right now: the switch exists so they can A/B the same loud section (#298).
    if key == "normalize_volume" {
        state.reapply_gain().await;
    }
    // The queue panel's switch sits right above the tracks it adds, so they come and go with it.
    // Spawned: turning it on can mean a radio fetch, and the switch shouldn't wait on the network.
    if key == "autoplay" {
        let (state, on) = (state.clone(), value != "false");
        crate::rt::spawn(async move { state.autoplay_changed(on).await });
    }
    // Both halves are one player setting. Applies from the next track change: the transition the
    // user is already hearing keeps the length it started with.
    if key == "crossfade" || key == "crossfade_secs" {
        state.apply_crossfade().await;
    }
    // The language YouTube answers in (#274). The SPA writes it whenever the two disagree, which is
    // also how a fresh install's language gets here at all. It drops its own browse cache and
    // remounts the route afterwards, so what is already on screen follows without a restart.
    if key == "locale" {
        state.it.set_locale(value);
    }
    // Applies to what's fetched from here on: the live queue keeps whatever is already in it.
    if key == "hide_videos" {
        state.it.set_hide_videos(value == "true");
    }
    // Cached lyrics outlive the order that produced them, so a reorder would otherwise reach only
    // songs never played. Songs whose source was picked by hand keep it.
    if key == "lyrics_providers" || key == "lyrics_prefer_words" {
        state.db.clear_lyrics_cache();
    }
}

pub async fn get_settings(state: &Arc<AppState>) -> Result<serde_json::Value, String> {
    let mut map: serde_json::Map<String, serde_json::Value> = state
        .db
        .all_settings()
        .into_iter()
        .filter(|(k, _)| UI_SETTINGS.contains(&k.as_str()))
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    map.insert("native_chrome".into(), native_chrome(&state.db).into());
    // Windows 11 rounds an undecorated window with a shadow (our main window) by itself, so the
    // SPA's own rounding would leave transparent slivers inside DWM's curve (#504). Windows 10
    // rounds nothing, and square is its native corner anyway.
    map.insert("os_corners".into(), cfg!(target_os = "windows").to_string().into());
    map.insert("native_video".into(), "false".into());
    if let Some(layout) = BUTTON_LAYOUT.get() {
        map.insert("button_layout".into(), layout.clone().into());
    }
    Ok(serde_json::Value::Object(map))
}

/// A Wayland session, where the X11 grab the hotkeys use only fires if the compositor passes keys
/// on to XWayland (KDE Plasma does, GNOME does not). The GDK backend doesn't matter, the session does.
pub fn global_hotkeys_on_wayland() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// The streamable client keys the orchestrator tries, for the "disabled clients" setting. Names
/// come from the innertube crate so the UI stays free of YouTube-shaped identity strings.
pub async fn get_stream_clients() -> Result<Vec<String>, String> {
    let mut v = vec![innertube::MAIN_CLIENT.to_string()];
    v.extend(innertube::STREAM_FALLBACK_ORDER.iter().map(|s| s.to_string()));
    Ok(v)
}

/// Wipe both cache tiers (URL cache + mpv on-disk audio cache). context/14.
pub async fn clear_caches(state: &Arc<AppState>) -> Result<(), String> {
    state.clear_caches();
    Ok(())
}

/// Bytes on disk in the audio cache (`Host::cache_dir`), for Settings › Storage.
pub async fn cache_size(state: &Arc<AppState>) -> Result<u64, String> {
    let dir = state.host.cache_dir();
    fn walk(p: &std::path::Path) -> u64 {
        let Ok(rd) = std::fs::read_dir(p) else { return 0 };
        rd.flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => walk(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    Ok(walk(&dir))
}

/// Show a file in Finder (the export toast's "Show in Finder"). Only existing paths.
pub async fn reveal_in_finder(path: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    if !p.exists() {
        return Err("not_found".into());
    }
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(&p).spawn().map(|_| ());
    // Windows: Explorer with the file selected, like Finder's reveal.
    #[cfg(target_os = "windows")]
    let r = {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(&p);
        std::process::Command::new("explorer").arg(arg).spawn().map(|_| ())
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open")
        .arg(p.parent().unwrap_or(&p))
        .spawn()
        .map(|_| ());
    r.map_err(|e| e.to_string())
}

/// Write a text file the user picked in a save dialog (OPML export). Only `.opml`/`.xml`.
pub async fn save_text_file(path: String, contents: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !matches!(ext.as_str(), "opml" | "xml") {
        return Err("unsupported_extension".into());
    }
    std::fs::write(&p, contents).map_err(|e| e.to_string())
}
// ── end stream: p-settings ──

// --- auth (context/15) ---------------------------------------------------------------------

pub async fn get_account(state: &Arc<AppState>) -> Result<serde_json::Value, String> {
    Ok(state.account_snapshot())
}

pub async fn get_account_identities(state: &Arc<AppState>) -> Result<Vec<serde_json::Value>, String> {
    state.account_identities().await
}

pub async fn switch_account(
    state: &Arc<AppState>,
    selection_key: String,
) -> Result<serde_json::Value, String> {
    state.switch_account(&selection_key).await
}

pub async fn sign_out(state: &Arc<AppState>) -> Result<(), String> {
    let state = state.clone();
    state.sign_out().await;
    Ok(())
}

// --- saved Google accounts (multi-account, context/15) ---------------------------------------

/// Saved Google accounts for the account menu. Display fields only — cookies, delegated ids and
/// visitorData never cross the Tauri boundary.
pub async fn get_google_accounts(state: &Arc<AppState>) -> Result<Vec<serde_json::Value>, String> {
    Ok(state.google_accounts())
}

/// Activate a saved Google account without a Google re-login. Fails if the stored cookie no
/// longer authenticates, in which case the caller should re-sign-in with that account.
pub async fn switch_google_account(state: &Arc<AppState>, id: String) -> Result<serde_json::Value, String> {
    state.switch_google_account(&id).await
}

/// Delete a saved account. Removing the active one signs out; the others stay listed.
pub async fn remove_google_account(state: &Arc<AppState>, id: String) -> Result<(), String> {
    state.remove_google_account(&id).await;
    Ok(())
}

/// The current track, play state, position and duration in one shot. Events are the normal
/// channel; this is for a webview that started after them (the mini player, or the main window
/// on a cold start, where the queue is restored before the UI subscribes).
pub async fn get_playback(state: &Arc<AppState>) -> Result<serde_json::Value, String> {
    Ok(state.playback_snapshot().await)
}

// --- browse / library (context/08) ---------------------------------------------------------

pub fn metadata_client(state: &Arc<AppState>) -> Result<&innertube::YouTubeClient, String> {
    state.clients.get(innertube::METADATA_CLIENT).ok_or_else(|| "metadata client missing".into())
}

pub async fn get_home(state: &Arc<AppState>, params: Option<String>) -> Result<HomePage, String> {
    let client = metadata_client(state)?;
    state.it.home(client, params.as_deref()).await.map_err(|e| e.to_string())
}

/// Home's Live section (#424): broadcasts on air from official artist channels, most watched
/// first. YouTube curates no such list (its Live hub is gaming, news and sports, the Music hub
/// only recordings, and YouTube Music's search marks nothing as live), so this is a few music
/// searches with YouTube's Live filter. Only rows with the official-artist badge survive: the rest
/// were mostly re-uploads of the same few streams by unrelated channels. Cached for an hour, so
/// home costs at most three searches an hour however often it is opened.
pub async fn get_live_now(state: &Arc<AppState>) -> Result<Vec<BrowseItem>, String> {
    use std::time::{Duration, Instant};
    static CACHE: std::sync::Mutex<Option<(Instant, Vec<BrowseItem>)>> =
        std::sync::Mutex::new(None);
    const TTL: Duration = Duration::from_secs(60 * 60);
    if let Some((at, items)) = CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < TTL {
            return Ok(items.clone());
        }
    }
    let client = state.clients.get("WEB").ok_or("WEB client missing")?;
    let searches = LIVE_QUERIES.iter().map(|q| state.it.live_now(client, q));
    let results = futures_util::future::join_all(searches).await;
    if results.iter().all(Result::is_err) {
        let first = results.into_iter().find_map(Result::err);
        return Err(first.map(|e| e.to_string()).unwrap_or_default());
    }
    let items = pick_live(results.into_iter().flatten().flatten().collect());
    *CACHE.lock().unwrap() = Some((Instant::now(), items.clone()));
    Ok(items)
}

/// What `get_live_now` asks for. Broad on purpose: each brings a different handful of official
/// channels (labels' radio streams, Lofi Girl and its kin, the jazz café channels).
pub const LIVE_QUERIES: [&str; 3] = ["music radio", "lofi", "jazz"];
pub const LIVE_MAX: usize = 12;

/// Official channels only, one card per stream (the same broadcast turns up under two queries, and
/// a label runs one title on several of its channels), at most three per channel (Lofi Girl alone
/// has eight on air at once), most watched first.
pub fn pick_live(mut all: Vec<innertube::LiveVideo>) -> Vec<BrowseItem> {
    const PER_CHANNEL: usize = 3;
    all.retain(|l| l.official);
    all.sort_by_key(|l| std::cmp::Reverse(l.watching));
    let (mut ids, mut titles) =
        (std::collections::HashSet::new(), std::collections::HashSet::new());
    let mut per_channel = std::collections::HashMap::<Option<String>, usize>::new();
    all.into_iter()
        .filter(|l| ids.insert(l.item.id.clone()) && titles.insert(l.item.title.to_lowercase()))
        .filter(|l| {
            let n = per_channel.entry(l.item.subtitle.clone()).or_default();
            *n += 1;
            *n <= PER_CHANNEL
        })
        .map(|l| l.item)
        .take(LIVE_MAX)
        .collect()
}

pub async fn get_home_more(state: &Arc<AppState>, token: String) -> Result<HomePage, String> {
    let client = metadata_client(state)?;
    state.it.home_continuation(client, &token).await.map_err(|e| e.to_string())
}

/// Whether YouTube Music answers at all, for the UI's offline retries (#483): it asks this until
/// the answer is yes and only then reloads the page, so a dead connection doesn't flash skeletons
/// on every try. Any status counts, only getting no reply means the network is still down.
pub async fn network_reachable() -> bool {
    crate::http::network_reachable().await
}


pub async fn get_library(state: &Arc<AppState>) -> Result<Vec<BrowseItem>, String> {
    let client = metadata_client(state)?;
    // Signed out there is no YouTube library to ask for (the browse would come back as a sign-in
    // shell), but Your Rotation is built from this machine's play history and is still real.
    let mut items = if state.it.is_logged_in() {
        state.it.library_playlists(client).await.map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    // Your Rotation leads the library once there's anything in it. Hidden while empty rather than
    // shown as a dead tile on a fresh install.
    let songs = on_repeat_songs(state);
    if !songs.is_empty() {
        items.insert(
            0,
            BrowseItem {
                kind: "playlist",
                id: ON_REPEAT_ID.into(),
                title: "Your Rotation".into(),
                subtitle: Some(format!("{} songs", songs.len())),
                thumbnail: None, // the UI draws an icon cover for this one
                duration: None,
                album_id: None,
                artist_runs: Vec::new(),
                play_count: None,
                is_video: false,
                is_upload: false,
                explicit: false,
            },
        );
    }
    // The playlists on this machine next, signed in or not: they belong to no account.
    let at = usize::from(items.first().is_some_and(|i| i.id == ON_REPEAT_ID));
    let local: Vec<BrowseItem> =
        state.db.local_playlists().iter().map(|p| local_playlist_card(state, p)).collect();
    items.splice(at..at, local);
    // ── stream: library ── Liked Songs leads the library, from local likes, not YouTube's.
    items.retain(|i| i.id != crate::library::LIKED_ID);
    items.insert(0, crate::library::liked_card(&state.db.conn()));
    // ── end stream: library ──
    // A card has nowhere to put two images, so a custom cover simply is the artwork here.
    for item in &mut items {
        if let Some(cover) = custom_cover(state, &item.id) {
            item.thumbnail = Some(cover);
        }
    }
    Ok(items)
}

/// Empty rather than an error when signed out: the Library page merges the user's local saves into
/// these grids, so "nothing of yours on YouTube" is an answer, not a failure.
pub async fn get_library_albums(state: &Arc<AppState>) -> Result<Vec<BrowseItem>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(state)?;
    state.it.library_albums(client).await.map_err(|e| e.to_string())
}

/// The account's YouTube Music play history, grouped by day. Empty when signed out, same as the
/// library grids: history lives on the account, and there is nothing to fail about not having one.
pub async fn get_history(state: &Arc<AppState>) -> Result<Vec<HistoryGroup>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(state)?;
    state.it.history(client).await.map_err(|e| e.to_string())
}

pub async fn get_library_artists(state: &Arc<AppState>) -> Result<Vec<BrowseItem>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(state)?;
    state.it.library_artists(client).await.map_err(|e| e.to_string())
}

pub async fn get_library_subscriptions(state: &Arc<AppState>) -> Result<Vec<BrowseItem>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(state)?;
    state.it.library_subscriptions(client).await.map_err(|e| e.to_string())
}

/// A playlist or album page. `id` is the browseId (`VL…` / `MPRE…`); Liked Songs is `VLLM`, and
/// `LIMUSIC_ON_REPEAT` is the local auto-playlist rather than anything YouTube knows about.
///
/// `sort` asks YouTube for the tracks in a given order; `None` gets whatever order the account
/// already has the list in, which is what a fresh visit wants (it matches YouTube Music).
pub async fn get_playlist(
    state: &Arc<AppState>,
    id: String,
    sort: Option<PlaylistSort>,
    desc: Option<bool>,
) -> Result<PlaylistPage, String> {
    if id == ON_REPEAT_ID {
        let items = on_repeat_songs(state);
        return Ok(PlaylistPage {
            title: Some("Your Rotation".into()),
            subtitle: Some(format!("{} songs you've played most this month", items.len())),
            thumbnail: None,
            description: None,
            privacy: None,
            cover: None,
            items,
            continuation: None,
            owned: false, // nothing to rename or delete; it rebuilds itself from what you play
            collaborative: false,
            sort_menu: None, // built from local history, so YouTube has no order to give
            suggestions: None,
        });
    }
    if is_local_playlist(&id) {
        return local_playlist_page(state, &id);
    }
    // ── stream: smart2 ── Made for you playlists (Your Mix, Weekly Discoveries …) are local.
    if crate::smart::mfy::is_mfy(&id) {
        return crate::smart::mfy::page(&state.db, &id)
            .ok_or_else(|| "This playlist isn't ready yet".to_string());
    }
    // ── end stream: smart2 ──
    // ── stream: library ── Liked Songs is local (library.rs), signed in or not.
    if id == crate::library::LIKED_ID {
        return Ok(crate::library::liked_page(&state.db.conn()));
    }
    // ── end stream: library ──
    let client = metadata_client(state)?;
    let sort = sort.map(|s| (s, desc.unwrap_or(false)));
    let mut page = state.it.playlist(client, &id, sort).await.map_err(|e| e.to_string())?;
    // Alongside YouTube's own thumbnail, not over it: the dialog offers to drop the custom one.
    page.cover = custom_cover(state, &id);
    Ok(page)
}

/// Store a sort order on a playlist, so YouTube Music and every other client show it the same way.
///
/// Only for a playlist whose `sortMenu.editable` said the options are writes. Everywhere else the
/// order is view-only and this would 400.
pub async fn set_playlist_sort(
    state: &Arc<AppState>,
    playlist_id: String,
    sort: PlaylistSort,
) -> Result<(), String> {
    let client = editable_playlist(state, &playlist_id)?;
    state.it.playlist_set_sort(client, &playlist_id, sort).await.map_err(|e| e.to_string())
}

/// videoId → how many times it was ever played. Feeds the "Your plays" track column and the
/// playlist page's "Most played" sort, which share it so the sort and the numbers beside it agree;
/// a track the map doesn't mention has never been played.
pub fn play_counts(state: &Arc<AppState>) -> std::collections::HashMap<String, i64> {
    state.db.play_counts(0).into_iter().collect()
}

/// The stats page (#396): everything heard since `since` (unix secs, 0 for all time). The page
/// picks the window, because the calendar it is drawn against is the webview's.
pub fn listening_stats(state: &Arc<AppState>, since: i64) -> crate::stats::Stats {
    let now = now_secs();
    let since = since.clamp(0, now);
    let raw = state.db.listening(since, now);
    let all_time = !matches!(raw.first_at, Some(first) if since > first);
    let mut stats = crate::stats::shape(raw, all_time);
    for t in &mut stats.top_songs {
        t.song = shed_queue_context(std::mem::take(&mut t.song));
    }
    stats
}

/// Home's Familiar Artists and the community shelf's seed (#497): the `limit` most played artists
/// of all time, on the stats page's counts.
pub fn top_artists(state: &Arc<AppState>, limit: usize) -> Vec<crate::stats::TopArtist> {
    crate::stats::top_artists(state.db.listened_songs(0, now_secs()), limit)
}

/// "Clear listening history" on the stats page. Your Rotation empties too: it is built from the same
/// plays.
pub fn clear_listening_history(state: &Arc<AppState>) {
    state.db.clear_listens();
}

/// The Your Rotation track list: most-played first, over the trailing window. Rows whose stored JSON
/// no longer parses (a `SongItem` shape change) are dropped rather than failing the whole page.
pub fn on_repeat_songs(state: &Arc<AppState>) -> Vec<SongItem> {
    let since = now_secs() - ON_REPEAT_WINDOW_SECS;
    state
        .db
        .top_plays(since, ON_REPEAT_LIMIT)
        .into_iter()
        .filter_map(|(json, _plays)| serde_json::from_str(&json).ok())
        .map(shed_queue_context)
        .collect()
}

/// A play record is the whole `SongItem` as it sat in the queue, so it carries that slot's queue
/// metadata: `queued`/`queued_by` when the track was "added to queue" (in a Listen Together session,
/// stamped with who added it), `autoplay` when radio appended it, `set_video_id` from whatever
/// playlist it was played from. None of that describes the song, so Your Rotation sheds it: otherwise
/// the row wears a session member's name forever, and playing Your Rotation drops it into "Next in
/// queue" instead of the playlist. Strips on read so rows already stored this way are fixed too.
pub fn shed_queue_context(s: SongItem) -> SongItem {
    SongItem {
        queued: false,
        queued_end: false,
        queued_from: None,
        queued_by: None,
        unavailable: false,
        autoplay: false,
        set_video_id: None,
        added_by: None,
        added_by_avatars: Vec::new(),
        added_at: None,
        ..s
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `total` is the playlist's track count, passed by a walk that wants the whole list: it fetches
/// the pages after this one too, several at a time (`InnerTube::playlist_pages`, #536).
pub async fn get_playlist_more(
    state: &Arc<AppState>,
    token: String,
    total: Option<usize>,
) -> Result<PlaylistContinuation, String> {
    let client = metadata_client(state)?;
    match total {
        Some(total) => state.it.playlist_pages(client, &token, total).await,
        None => state.it.playlist_continuation(client, &token).await,
    }
    .map_err(|e| e.to_string())
}

/// A batch of suggestions for a playlist you own (#395). `token` is the page's `suggestions`, the
/// previous batch's `refresh`, or nothing (see `InnerTube::playlist_suggestions`).
pub async fn get_playlist_suggestions(
    state: &Arc<AppState>,
    playlist_id: String,
    token: Option<String>,
) -> Result<PlaylistSuggestions, String> {
    let client = metadata_client(state)?;
    state
        .it
        .playlist_suggestions(client, &playlist_id, token.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// An album page. `id` is the album browseId (`MPRE…`).
pub async fn get_album(state: &Arc<AppState>, id: String) -> Result<AlbumPage, String> {
    // A local album is built from SQLite, so it opens the same page while offline (local.rs).
    if let Some(key) = id.strip_prefix(crate::local::ALBUM_PREFIX) {
        return Ok(crate::local::album_page(&state.db, key));
    }
    // A local artist rides this route too: same page shape, and none of the artist route's
    // YouTube furniture applies to files on disk (see `local::artist_page`).
    if let Some(name) = id.strip_prefix(crate::local::ARTIST_PREFIX) {
        return Ok(crate::local::artist_page(&state.db, name));
    }
    let client = metadata_client(state)?;
    let mut page = state.it.album(client, &id).await.map_err(|e| e.to_string())?;
    // `album()` hands back the album audio for rows YouTube linked to a music video. With music
    // videos on, those rows play the video, as on YouTube Music, with the song a switch away (#442),
    // unless `videos_first` is off: some videos carry intros and extra scenes the song doesn't (#464).
    let video = crate::state::wants_video(&state.db);
    for item in &mut page.items {
        crate::state::prefer_version(item, video);
    }
    Ok(page)
}

/// An artist page. `id` is the channel browseId (`UC…`).
pub async fn get_artist(state: &Arc<AppState>, id: String) -> Result<ArtistPage, String> {
    let client = metadata_client(state)?;
    let mut page = state.it.artist(client, &id).await.map_err(|e| e.to_string())?;
    // ── stream: smart ── one row per song on Top songs (identity dedup).
    crate::identity::dedup_songs(&mut page.top_songs);
    crate::smart::explicit::remember_rows(&state.db, &page.top_songs, true);
    Ok(page)
}

/// The artist page's "Appears on" shelf (#456). `songs_id` is the page's `topSongsId`. Its own
/// call so the page paints without waiting on the second browse it costs.
pub async fn get_artist_appears_on(
    state: &Arc<AppState>,
    id: String,
    songs_id: String,
) -> Result<Vec<BrowseItem>, String> {
    let client = metadata_client(state)?;
    state.it.artist_appears_on(client, &id, &songs_id).await.map_err(|e| e.to_string())
}

/// Moods & Genres, the tiles the search page opens on.
pub async fn get_moods(state: &Arc<AppState>) -> Result<Vec<MoodSection>, String> {
    let client = metadata_client(state)?;
    state.it.moods(client).await.map_err(|e| e.to_string())
}

/// Where the Moods & Genres covers are cached (`smart_cache`), and for how long.
const MOOD_ART_KEY: &str = "mood_art";
const MOOD_ART_TTL: i64 = 7 * 86_400;

/// The cached Moods & Genres covers (tile `params` → image), no network: Home's moods shelf.
pub fn cached_mood_art(db: &crate::db::Db) -> std::collections::HashMap<String, String> {
    crate::smart::cache_get(db, MOOD_ART_KEY, MOOD_ART_TTL).unwrap_or_default()
}

/// A cover for each Moods & Genres tile, which YouTube draws as a coloured button with no art: the
/// first playlist in that tile's category. Six categories at a time. A tile whose category fails
/// or comes back empty is left out, and the page draws it without art. Covers are cached a week
/// (shared by Search's Browse tiles and Home's moods shelf), so only missing ones are fetched.
pub async fn get_mood_art(
    state: &Arc<AppState>,
    params: Vec<String>,
) -> Result<std::collections::HashMap<String, String>, String> {
    use futures_util::StreamExt;
    let mut known = cached_mood_art(&state.db);
    let missing: Vec<String> = params.iter().filter(|p| !known.contains_key(*p)).cloned().collect();
    if !missing.is_empty() {
        let client = metadata_client(state)?;
        let it = &state.it;
        // ── stream: reliability ── decoration for up to 24 tiles: background priority.
        let got: Vec<(String, String)> = innertube::background(
            futures_util::stream::iter(missing)
                .map(|p| async move {
                    let items = it.browse_grid(client, MOODS_CATEGORY_ID, Some(&p)).await.ok()?;
                    Some((p, items.into_iter().find_map(|i| i.thumbnail)?))
                })
                .buffer_unordered(6)
                .filter_map(|art| async move { art })
                .collect(),
        )
        .await;
        if !got.is_empty() {
            known.extend(got);
            crate::smart::cache_put(&state.db, MOOD_ART_KEY, &known);
        }
    }
    Ok(params.into_iter().filter_map(|p| known.get(&p).map(|a| (p, a.clone()))).collect())
}

/// The browseId every Moods & Genres tile opens with its own `params`.
pub const MOODS_CATEGORY_ID: &str = "FEmusic_moods_and_genres_category";

/// A card grid reached from a carousel's "More" button (e.g. an artist's full albums list).
pub async fn get_browse_grid(
    state: &Arc<AppState>,
    id: String,
    params: Option<String>,
) -> Result<Vec<BrowseItem>, String> {
    let client = metadata_client(state)?;
    state.it.browse_grid(client, &id, params.as_deref()).await.map_err(|e| e.to_string())
}

/// Play a playlist/album: the given items become the queue (no radio). `start` is the clicked
/// track index; `None`/omitted means "just play it" (random opener when shuffle is on).
/// `source_id` (the page's playlist/album playlist id) makes autoplay continue with that
/// context's radio when the queue runs out. `source_name` (the page title) feeds the queue
/// panel's "Next from" header; `shuffle: true` (page Shuffle buttons) turns shuffle on for
/// this queue — pass the items in their real order, the backend shuffles. `continuation` is the
/// page's next-page token when it has one: pass the tracks that are loaded and the backend walks
/// the rest into the queue in the background, so playback starts on page 1.
pub async fn play_playlist(
    state: &Arc<AppState>,
    items: Vec<SongItem>,
    start: Option<usize>,
    source_id: Option<String>,
    source_name: Option<String>,
    shuffle: Option<bool>,
    continuation: Option<String>,
    // ── stream: polish ── the card played from (playlist/album/artist/liked/podcast/daily_mix),
    // recorded as this queue's context for Recents and Home.
    context: Option<crate::polish::context::PlayContext>,
    // ── stream: pod-fixes ── open the start row this many seconds in (a chapter or show-notes
    // timestamp tapped before its episode plays).
    start_secs: Option<f64>,
) -> Result<(), String> {
    let state = state.clone();
    // ── stream: smart ── hidden songs, blocked artists and (filter on) explicit never play.
    let (items, start) = crate::smart::recs::playable(&state.db, items, start);
    // ── stream: podcast-upnext ── an episode's queue is the one before it, it, and its show's
    // next; autoplay predicts the rest (`podcasts::upnext`).
    let picked = items.get(start.unwrap_or(0)).filter(|s| crate::podcasts::is_episode_item(s)).is_some();
    let (items, start, shuffle) = if picked {
        let history = state.current_item().await;
        let (q, at) =
            crate::podcasts::upnext::context_queue(&state.db.podcasts_conn(), &items, start.unwrap_or(0), history);
        (q, Some(at), Some(false))
    } else {
        (items, start, shuffle)
    };
    state.open_at(items.get(start.unwrap_or(0)), start_secs);
    state
        .play_tracks(items, start, source_id, source_name, shuffle.unwrap_or(false), continuation)
        .await;
    // ── stream: polish ──
    let (sid, sname, first) = state.queue_source().await;
    crate::polish::context::started(
        &state.db,
        Some(&*state.host),
        sid.as_deref(),
        sname.as_deref(),
        context,
        first.as_ref(),
    );
    // ── end stream: polish ──
    Ok(())
}

/// Start a radio seeded on a song, artist, album or playlist (context/08). `kind` is
/// `song` | `artist` | `album` | `playlist`; `id` is the videoId (song) or browseId/playlistId
/// (everything else) — the backend resolves it to a radio playlist. `name` titles the queue.
///
/// Starting a song radio on the track that's already playing keeps it playing and replaces only
/// what comes after it; every other case replaces the queue.
pub async fn start_radio(
    state: &Arc<AppState>,
    kind: String,
    id: String,
    name: Option<String>,
    start: Option<f64>,
) -> Result<(), String> {
    let state = state.clone();
    // ── stream: polish ── a radio is a context too (Recents, Home).
    state.start_radio(&kind, &id, name, start).await?;
    let (sid, sname, seed) = state.queue_source().await;
    let ctx = crate::polish::context::PlayContext {
        kind: if kind == "artist" { "mix".into() } else { "radio".into() },
        id: id.clone(),
        title: sname.clone().unwrap_or_default(),
        subtitle: seed.as_ref().map(|s| s.artists.clone()).filter(|a| !a.is_empty()),
        thumbnail: seed.as_ref().and_then(|s| s.thumbnail.clone()),
        params: (kind != "song" && kind != "artist").then(|| kind.clone()),
    };
    crate::polish::context::started(&state.db, Some(&*state.host), sid.as_deref(), sname.as_deref(), Some(ctx), None);
    Ok(())
    // ── end stream: polish ──
}

// --- write actions (context/01 ✎, context/15) ----------------------------------------------

/// stream: magic-d4 — a playlist edit that went through moves the playlist up in Recents
/// (`library::touched`). Passes the result through.
fn touched<T>(state: &AppState, playlist_id: &str, r: Result<T, String>) -> Result<T, String> {
    if r.is_ok() {
        crate::library::touched(state, playlist_id);
    }
    r
}

pub fn require_login(state: &Arc<AppState>) -> Result<&innertube::YouTubeClient, String> {
    if !state.it.is_logged_in() {
        return Err("Sign in first to use this.".into());
    }
    metadata_client(state)
}

/// Like, dislike, or clear a track's rating. One command for all three: YouTube's states are
/// mutually exclusive, so a dislike un-likes in the same call and the UI never has to send two.
pub async fn rate(state: &Arc<AppState>, video_id: String, rating: Rating) -> Result<(), String> {
    let client = require_login(state)?;
    // Before the write, not after: a `refresh_from_next` round trip already in flight was asked
    // before this rating existed, so its answer is stale from here on either way (issue #93).
    state.rate_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.it.rate(client, &video_id, rating).await.map_err(|e| e.to_string())
}

/// Add a song to the library (Library ▸ Songs), or take it out. `token` comes from the row's own
/// menu (`SongItem.library`), which is the only handle YouTube gives on this: it is a feedback
/// action, not a rating, so it leaves Liked Music alone.
pub async fn set_song_saved(state: &Arc<AppState>, token: String) -> Result<(), String> {
    let client = require_login(state)?;
    state.it.feedback(client, &token).await.map_err(|e| e.to_string())
}

/// Save an album to the library, or remove it. `playlist_id` is the album's `OLAK5uy_…`
/// (`AlbumPage.playlistId`).
pub async fn set_album_saved(
    state: &Arc<AppState>,
    playlist_id: String,
    saved: bool,
) -> Result<(), String> {
    let client = require_login(state)?;
    state.it.like_playlist(client, &playlist_id, saved).await.map_err(|e| e.to_string())
}

/// Login, plus the guard every playlist edit needs. Two ids never reach `edit_playlist`: Your Rotation
/// has no YouTube playlist behind it, and Liked Music is an auto-playlist YouTube edits through the
/// rating endpoint instead. Both answer 400 there.
pub fn editable_playlist<'a>(
    state: &'a Arc<AppState>,
    playlist_id: &str,
) -> Result<&'a innertube::YouTubeClient, String> {
    if playlist_id == ON_REPEAT_ID {
        return Err("Your Rotation builds itself from what you play.".into());
    }
    if playlist_id == LIKED_MUSIC_ID {
        return Err("Liked Music follows your likes; like the song instead.".into());
    }
    // The commands that edit one dispatch before they get here. This is the net for any that
    // doesn't, so a playlist on this machine can never reach YouTube as an id it has never seen.
    if is_local_playlist(playlist_id) {
        return Err("This playlist is on this device, so that isn't available for it.".into());
    }
    require_login(state)
}

/// Liked Music. It is indexed like the rest, because a search row is the one place YouTube tells
/// us nothing: `search` responses carry no `likeStatus` at all (live-checked 2026-08-28), so
/// membership of this list is the only way a result can draw its heart filled. Not shown in the
/// "saved in" chip, though: the thumbs-up already says it (the UI filters it out there).
pub const LIKED_MUSIC_ID: &str = "VLLM";
/// When the index was last crawled, as unix seconds. `_v2`: the key changed when Liked Music joined
/// the index, so an install with a fresh stamp re-crawled once instead of showing hearts empty for
/// another six hours.
pub const PLAYLIST_INDEX_STAMP: &str = "playlist_index_synced_at_v2";
/// The library playlists the last crawl found this account can't edit (saved from someone else),
/// as a JSON array of ids. The add-to-playlist picker leaves them out (#651).
pub const PLAYLIST_INDEX_READONLY: &str = "playlist_index_readonly";
/// What `add_to_playlist` rejects with when YouTube refuses the add because the playlist belongs
/// to someone else. A code, not a sentence: the UI words it in the user's language.
pub const NOT_EDITABLE: &str = "not_editable";
/// How long the membership index is trusted before a re-crawl. Adds and removes made in this app
/// patch it as they happen, so this window only ever covers edits made somewhere else.
pub const PLAYLIST_INDEX_TTL_SECS: i64 = 6 * 3600;
/// Continuation pages per playlist. YouTube hands back 100 tracks a page, so this covers 5000 of
/// them. ponytail: a hard stop, not paging state. A playlist past it marks its first 5000 tracks
/// and no more, which beats one pathological list turning a sync into hundreds of requests.
pub const PLAYLIST_INDEX_MAX_PAGES: usize = 50;

/// videoId → the ids of your playlists holding it, straight from SQLite with no network at all,
/// so a track list can draw the "saved" mark on its first paint. Empty until the first sync.
pub fn playlist_index(state: &Arc<AppState>) -> std::collections::HashMap<String, Vec<String>> {
    state.db.playlist_memberships()
}

/// Rebuild that index by walking the playlists you own, then answer with it.
///
/// Nothing else knows playlist membership: the library browse gives cards, a playlist browse gives
/// one list's tracks, and InnerTube's per-video add-to-playlist dialog would be a request per row.
/// So the crawl is the price, and it is paid at most once every `PLAYLIST_INDEX_TTL_SECS`, on a
/// launch or a sign-in. Playlists you merely saved are skipped: they are someone else's, so "you
/// saved this song to it" would be a lie, and their long mixes would double the walk.
pub async fn sync_playlist_index(
    state: &Arc<AppState>,
) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    if !state.it.is_logged_in() {
        // What is left is the playlists on this machine, which no account owns.
        state.db.clear_playlist_index();
        return Ok(state.db.playlist_memberships());
    }
    let fresh_until = state
        .db
        .get_setting(PLAYLIST_INDEX_STAMP)
        // A stamp from before the read-only list existed doesn't cover it: crawl once more.
        .filter(|_| state.db.get_setting(PLAYLIST_INDEX_READONLY).is_some())
        .and_then(|at| at.parse::<i64>().ok())
        .map(|at| at + PLAYLIST_INDEX_TTL_SECS);
    if fresh_until.is_some_and(|until| now_secs() < until) {
        return Ok(state.db.playlist_memberships());
    }
    let client = metadata_client(state)?;
    let library = state.it.library_playlists(client).await.map_err(|e| e.to_string())?;
    // A degraded response that parses as an empty library would otherwise wipe every mark and
    // then call the wipe fresh for six hours. Nothing to index is nothing to trust: keep what is
    // stored and try again on the next launch.
    if library.is_empty() {
        return Ok(state.db.playlist_memberships());
    }
    let mut indexed: Vec<String> = Vec::new();
    let mut readonly: Vec<String> = Vec::new();
    for item in library {
        if item.id == ON_REPEAT_ID {
            continue;
        }
        // One playlist failing (a deleted id, a hiccup) must not abandon the rest of the crawl,
        // and must not drop what is already indexed for it either: leaving it out of `indexed`
        // would have `retain_playlists` forget the tracks we do know about.
        let Ok(page) = state.it.playlist(client, &item.id, None).await else {
            indexed.push(item.id);
            continue;
        };
        // A collaborative playlist reads `owned: false` (YouTube drops the editable header on it)
        // but is one you add to and remove from, so the membership index has to cover it too.
        // Liked Music is exempt: YouTube sends it without the editable header, so it reads
        // `owned: false` even though it is yours.
        if item.id != LIKED_MUSIC_ID && !page.owned && !page.collaborative {
            readonly.push(item.id);
            continue;
        }
        let mut video_ids: Vec<String> = page.items.into_iter().map(|song| song.video_id).collect();
        let mut token = page.continuation;
        for _ in 0..PLAYLIST_INDEX_MAX_PAGES {
            let Some(next) = token.take() else { break };
            let Ok(more) = state.it.playlist_continuation(client, &next).await else { break };
            video_ids.extend(more.items.into_iter().map(|song| song.video_id));
            token = more.continuation;
        }
        state.db.set_playlist_tracks(&item.id, &video_ids);
        indexed.push(item.id);
    }
    state.db.retain_playlists(&indexed);
    state.db.set_setting(
        PLAYLIST_INDEX_READONLY,
        &serde_json::to_string(&readonly).unwrap_or_default(),
    );
    state.db.set_setting(PLAYLIST_INDEX_STAMP, &now_secs().to_string());
    Ok(state.db.playlist_memberships())
}

/// The library playlists the last index crawl found this account can't edit. Empty until the first
/// crawl, and for a playlist saved since: that one is refused at the add instead.
pub fn readonly_playlists(state: &Arc<AppState>) -> Vec<String> {
    state
        .db
        .get_setting(PLAYLIST_INDEX_READONLY)
        .and_then(|ids| serde_json::from_str(&ids).ok())
        .unwrap_or_default()
}

/// `false` means the playlist already had the track and YouTube added nothing — not an error, but
/// the UI must not draw an optimistic row for it (there is no real row to remove later).
/// With `allow_duplicates` set to `true`, `true` is returned for a duplicate that was added on purpose.
pub async fn add_to_playlist(
    state: &Arc<AppState>,
    playlist_id: String,
    video_id: String,
    allow_duplicates: Option<bool>,
) -> Result<bool, String> {
    let client = editable_playlist(state, &playlist_id)?;
    let added = state
        .it
        .playlist_add(client, &playlist_id, &video_id, allow_duplicates.unwrap_or(false))
        .await
        .map_err(|e| match e {
            innertube::Error::PlaylistNotEditable => NOT_EDITABLE.to_owned(),
            e => e.to_string(),
        })?;
    // Also on `false`: YouTube refusing a duplicate means the playlist holds the track, which is
    // exactly what the index should say. A stale index is how it got asked in the first place.
    state.db.add_playlist_track(&playlist_id, &video_id);
    touched(state, &playlist_id, Ok(added))
}

pub async fn remove_from_playlist(
    state: &Arc<AppState>,
    playlist_id: String,
    video_id: String,
    set_video_id: String,
) -> Result<(), String> {
    if is_local_playlist(&playlist_id) {
        let row = set_video_id.parse().map_err(|_| GONE.to_string())?;
        return touched(state, &playlist_id, remove_local_rows(state, &playlist_id, &[row]));
    }
    let client = editable_playlist(state, &playlist_id)?;
    state
        .it
        .playlist_remove(client, &playlist_id, &video_id, &set_video_id)
        .await
        .map_err(|e| e.to_string())?;
    state.db.remove_playlist_track(&playlist_id, &video_id);
    touched(state, &playlist_id, Ok(()))
}

/// Remove several tracks from one playlist in a single request (the bulk bar's Remove).
///
/// All or nothing: YouTube applies the whole action list or rejects it, so the UI can revert its
/// optimistic removal on an error without working out which rows made it.
pub async fn remove_many_from_playlist(
    state: &Arc<AppState>,
    playlist_id: String,
    tracks: Vec<(String, String)>,
) -> Result<(), String> {
    if is_local_playlist(&playlist_id) {
        let rows = tracks
            .iter()
            .map(|(_, row)| row.parse().map_err(|_| GONE.to_string()))
            .collect::<Result<Vec<i64>, _>>()?;
        return touched(state, &playlist_id, remove_local_rows(state, &playlist_id, &rows));
    }
    let client = editable_playlist(state, &playlist_id)?;
    state
        .it
        .playlist_remove_many(client, &playlist_id, &tracks)
        .await
        .map_err(|e| e.to_string())?;
    for (video_id, _) in &tracks {
        state.db.remove_playlist_track(&playlist_id, video_id);
    }
    touched(state, &playlist_id, Ok(()))
}

/// Move a track within a playlist. `before_set_video_id` None = move to the end;
/// local playlists use row ids, while YouTube ones use `setVideoId`.
pub async fn move_in_playlist(
    state: &Arc<AppState>,
    playlist_id: String,
    set_video_id: String,
    before_set_video_id: Option<String>,
) -> Result<(), String> {
    if is_local_playlist(&playlist_id) {
        let row = set_video_id.parse().map_err(|_| GONE.to_string())?;
        let before = match before_set_video_id {
            Some(b) => Some(b.parse().map_err(|_| GONE.to_string())?),
            None => None,
        };
        let key = local_key(&playlist_id)?;
        let r = state
            .db
            .move_local_playlist_track(key, row, before, crate::db::now_secs())
            .map_err(db_err);
        return touched(state, &playlist_id, r);
    }
    let client = editable_playlist(state, &playlist_id)?;
    state
        .it
        .playlist_move(client, &playlist_id, &set_video_id, before_set_video_id.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    touched(state, &playlist_id, Ok(()))
}

/// `local`: keep it on this machine instead of the account (issue #251), which is the only kind
/// there is while signed out. Answers the new playlist's id, a `LOCALPLAYLIST:` browseId for that.
pub async fn create_playlist(
    state: &Arc<AppState>,
    title: String,
    local: Option<bool>,
) -> Result<String, String> {
    if local.unwrap_or(false) {
        let title = title.trim();
        if title.is_empty() {
            return Err("Give the playlist a name.".into());
        }
        let id = state.db.create_local_playlist(title, crate::db::now_secs()).map_err(db_err)?;
        return Ok(format!("{LOCAL_PLAYLIST_PREFIX}{id}"));
    }
    let client = require_login(state)?;
    state.it.create_playlist(client, &title).await.map_err(|e| e.to_string())
}

/// Edit a playlist you own, from the "Edit playlist" dialog: name, description, visibility.
///
/// Each field is `None` when the user left it alone, and only what changed is sent: an edit of
/// the name must not blank a description we failed to read back off the page.
pub async fn edit_playlist_details(
    state: &Arc<AppState>,
    playlist_id: String,
    name: Option<String>,
    description: Option<String>,
    public: Option<bool>,
) -> Result<(), String> {
    // Nobody else can see a playlist on this machine, so there is no visibility to set.
    if is_local_playlist(&playlist_id) {
        let name = name.as_deref().map(str::trim);
        if name == Some("") {
            return Err("Give the playlist a name.".into());
        }
        let key = local_key(&playlist_id)?;
        let r = state
            .db
            .edit_local_playlist(key, name, description.as_deref(), crate::db::now_secs())
            .map_err(db_err);
        return touched(state, &playlist_id, r);
    }
    let client = editable_playlist(state, &playlist_id)?;
    // The switch is two-state; YouTube's third value (UNLISTED) is only ever left as it was.
    let privacy = public.map(|p| if p { "PUBLIC" } else { "PRIVATE" });
    let r = state
        .it
        .playlist_edit_details(
            client,
            &playlist_id,
            name.as_deref(),
            description.as_deref(),
            privacy,
        )
        .await
        .map_err(|e| e.to_string());
    touched(state, &playlist_id, r)
}

/// Custom playlist artwork, in both places it lives.
///
/// Setting one is local-first: the picked image is copied in beside the local-music covers and
/// answered straight back, then pushed to YouTube Music in the background (`sync_cover`), because
/// the upload is three round trips and nobody should watch a spinner for their own file.
///
/// Dropping one waits, and that is deliberate. Once a cover has been up there, YouTube's own
/// thumbnail *is* that cover, so a local-first removal would fall back to the very image being
/// removed and only reach the rebuilt collage a beat later: two swaps, the first of them pointless.
/// The clear is a single small call, so it answers with the thumbnail YouTube rebuilt and the UI
/// changes once.
pub async fn set_playlist_cover(
    state: &Arc<AppState>,
    playlist_id: String,
    path: Option<String>,
) -> Result<CoverResult, String> {
    let key = cover_key(&playlist_id);
    let stored = state.db.get_setting(&key);
    let Some(src) = path else {
        // YouTube first, so the local copy is still on screen while it answers. Its refusal is
        // never fatal though: dropping the cover from this machine is what the user clicked, and
        // an account that was not allowed to set one up there has nothing to clear anyway.
        let thumbnail = match clear_cover_on_youtube(state, &playlist_id).await {
            Ok(t) => {
                state.db.delete_setting(&synced_key(&playlist_id));
                t
            }
            Err(e) => {
                tracing::warn!(playlist_id, error = %e, "custom cover not cleared on YouTube Music");
                // Only worth saying when a cover of ours actually reached the account: otherwise
                // there was nothing up there to keep, and the warning would be a lie.
                if state.db.get_setting(&synced_key(&playlist_id)).is_some() {
                    let _ = state.host.emit(
                        "cover-error",
                        serde_json::json!({
                            "message": "Removed here, but YouTube Music kept its copy.",
                        }),
                    );
                }
                None
            }
        };
        state.db.delete_setting(&key);
        if let Some(old) = stored {
            let _ = std::fs::remove_file(old);
        }
        return Ok(CoverResult { cover: None, thumbnail });
    };
    store_cover(state, &playlist_id, std::path::Path::new(&src))
}

/// Copy an image in as a playlist's artwork, answer the local copy, and send it on to YouTube
/// Music in the background. The picker's half of [`set_playlist_cover`], and how a Spotify import
/// carries the playlist's own cover over (#375).
pub fn store_cover(
    state: &Arc<AppState>,
    playlist_id: &str,
    src: &std::path::Path,
) -> Result<CoverResult, String> {
    // What YouTube's uploader will take. WebP is not on the list: it answers 415 for one, and a
    // cover that only works on this machine is worse than one the picker never offered.
    const IMAGE_EXTS: [&str; 3] = ["jpg", "jpeg", "png"];

    let key = cover_key(playlist_id);
    let stored = state.db.get_setting(&key);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return Err("Pick a JPEG or PNG image: YouTube Music won't take anything else.".into());
    }
    // ponytail: a flat size cap instead of downscaling. It keeps a 40px sidebar thumb from
    // decoding a camera raw in the webview and the upload from swallowing one; reach for the
    // `image` crate and a real resize only if 8 MB turns out to bother anyone.
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    if src.metadata().map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        return Err("That image is over 8 MB. Pick a smaller one.".into());
    }
    let dir = crate::local::covers_dir(&*state.host).join("playlists");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Timestamped, so replacing a cover can't be served out of the webview's cache under the name
    // it already has. The id is filtered to filename characters rather than trusted: it arrives
    // from the UI, and a `..` in it would write outside this directory.
    let stem: String = playlist_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let dest = dir.join(format!("{stem}-{}.{ext}", crate::db::now_secs()));
    std::fs::copy(src, &dest).map_err(|e| e.to_string())?;
    // Only now is the cover it replaces safe to unlink. Dropping it any earlier means a picked
    // file this command goes on to refuse (wrong format, too big, unreadable) takes the artwork
    // already on screen down with it, and the toast talks about the new file while the old one is
    // the thing that just disappeared.
    if let Some(old) = stored {
        let _ = std::fs::remove_file(old);
    }
    let dest = dest.to_string_lossy().to_string();
    // The covers directory is allowed recursively at startup, but the first cover on a fresh
    // install is written after that ran, so name this file explicitly too.
    state.host.allow_asset_file(std::path::Path::new(&dest));
    state.db.set_setting(&key, &dest);
    sync_cover(state, playlist_id, dest.clone());
    Ok(CoverResult { cover: Some(dest), thumbnail: None })
}

/// What the UI needs to draw after a cover changed: where the local copy is, and (on a removal)
/// the thumbnail YouTube rebuilt in its place.
#[derive(serde::Serialize)]
pub struct CoverResult {
    cover: Option<String>,
    thumbnail: Option<String>,
}

/// Send the cover on to YouTube Music behind the picker's back: the local copy is already on
/// screen, and the upload is a three-call round trip nobody should wait through.
///
/// A failure is a toast, not a rollback: the artwork is still right here, and it is still this
/// playlist's cover on this machine. Signed out (or Your Rotation, which YouTube has never heard of),
/// there is nothing to sync and local is all there ever was.
pub fn sync_cover(state: &Arc<AppState>, playlist_id: &str, path: String) {
    if playlist_id == ON_REPEAT_ID || is_local_playlist(playlist_id) || !state.it.is_logged_in() {
        return;
    }
    let state = Arc::clone(state);
    let playlist_id = playlist_id.to_owned();
    crate::rt::spawn(async move {
        let Some(client) = state.clients.get(innertube::METADATA_CLIENT) else {
            return;
        };
        // Read here, not on the command's thread: the file was just written and the caller has its
        // answer already.
        let result = match std::fs::read(&path) {
            Ok(image) => state.it.playlist_set_cover(client, &playlist_id, image).await,
            Err(e) => Err(innertube::Error::Other(e.to_string())),
        };
        match result {
            // Remembered so a later removal knows whether YouTube has anything of ours to drop.
            Ok(()) => state.db.set_setting(&synced_key(&playlist_id), "1"),
            Err(e) => {
                tracing::warn!(playlist_id, error = %e, "playlist cover didn't reach YouTube Music");
                let message = match e {
                    // The one refusal with a known cause and no fix inside this app. Say it once,
                    // plainly, and leave the cover where it already is: on this machine.
                    innertube::Error::CoverRefused => format!("Artwork saved on this device. {e}"),
                    e => format!("Artwork saved here, but the upload to YouTube Music failed: {e}"),
                };
                let _ = state.host.emit("cover-error", serde_json::json!({ "message": message }));
            }
        }
    });
}

/// Drop the custom thumbnail from the account, answering the one YouTube rebuilt from the tracks.
/// Nothing to do (and nothing to answer with) when there is no account behind the playlist.
pub async fn clear_cover_on_youtube(
    state: &Arc<AppState>,
    playlist_id: &str,
) -> Result<Option<String>, String> {
    if playlist_id == ON_REPEAT_ID || is_local_playlist(playlist_id) || !state.it.is_logged_in() {
        return Ok(None);
    }
    let client = metadata_client(state)?;
    state.it.playlist_clear_cover(client, playlist_id).await.map_err(|e| e.to_string())
}

pub fn cover_key(playlist_id: &str) -> String {
    // Browse ids arrive `VL`-prefixed and playlist ids don't; one playlist, one key either way.
    format!("playlist_cover:{}", playlist_id.strip_prefix("VL").unwrap_or(playlist_id))
}

/// Set once a cover of ours has actually landed on the account, so a removal knows whether there
/// is anything up there to warn about failing to clear.
pub fn synced_key(playlist_id: &str) -> String {
    format!("{}:synced", cover_key(playlist_id))
}

/// The custom artwork stored for a playlist, if the file is still there. The user owns that
/// directory and can empty it, and a dead path renders as a broken image.
pub fn custom_cover(state: &Arc<AppState>, playlist_id: &str) -> Option<String> {
    let path = state.db.get_setting(&cover_key(playlist_id))?;
    std::path::Path::new(&path).is_file().then_some(path)
}

pub async fn delete_playlist(state: &Arc<AppState>, playlist_id: String) -> Result<(), String> {
    if is_local_playlist(&playlist_id) {
        // ── stream: library ── a soft delete, undoable with `lib_playlist_restore` for 30 days
        // (Settings › Recover playlists); a launch after that purges the rows and the cover file.
        crate::library::delete_playlist(&state.db.conn(), &playlist_id, now_secs())
            .map_err(|_| GONE.to_string())?;
        let _ = state.host.emit("library-changed", serde_json::json!({ "scope": "playlists" }));
        return Ok(());
        // ── end stream: library ──
    }
    let client = editable_playlist(state, &playlist_id)?;
    state.it.delete_playlist(client, &playlist_id).await.map_err(|e| e.to_string())?;
    state.db.forget_playlist(&playlist_id);
    Ok(())
}

// --- playlists on this machine (issue #251) --------------------------------------------------
// A `LOCALPLAYLIST:<n>` id reaches the same commands a YouTube playlist does (get, add, remove,
// edit, cover, delete), and each one answers it from SQLite before any of the YouTube path runs.
// That is what lets every playlist surface in the UI take both kinds without branching.

pub const GONE: &str = "This playlist is no longer on this device.";

/// `LOCALPLAYLIST:<n>` → n. An id with the prefix and no number is still not YouTube's, so it is
/// an error rather than a fall-through to a browse YouTube would 400.
pub fn local_key(id: &str) -> Result<i64, String> {
    id.strip_prefix(LOCAL_PLAYLIST_PREFIX).and_then(|n| n.parse().ok()).ok_or_else(|| GONE.into())
}

pub fn db_err(e: rusqlite::Error) -> String {
    match e {
        rusqlite::Error::QueryReturnedNoRows => GONE.into(),
        e => e.to_string(),
    }
}

/// The library card. The subtitle is English on purpose: Rust never learns the UI language, so
/// `api.ts` rewords it on the way in, the same as Your Rotation's.
pub fn local_playlist_card(state: &Arc<AppState>, p: &crate::db::LocalPlaylist) -> BrowseItem {
    let id = format!("{LOCAL_PLAYLIST_PREFIX}{}", p.id);
    // A cover the user picked, else the first track's art, which is what YouTube shows for a
    // playlist too short for its four-track collage.
    let thumbnail = custom_cover(state, &id).or_else(|| {
        let first = p.first_song.as_deref()?;
        serde_json::from_str::<SongItem>(first).ok()?.thumbnail
    });
    BrowseItem {
        kind: "playlist",
        id,
        title: p.title.clone(),
        subtitle: Some(format!("{} songs", p.count)),
        thumbnail,
        duration: None,
        album_id: None,
        artist_runs: Vec::new(),
        play_count: None,
        is_video: false,
        is_upload: false,
        explicit: false,
    }
}

pub fn local_playlist_page(state: &Arc<AppState>, id: &str) -> Result<PlaylistPage, String> {
    let key = local_key(id)?;
    let p = state.db.local_playlist(key).ok_or(GONE)?;
    // The row id rides as the `set_video_id`, the handle every removal path already sends back.
    // A row whose JSON no longer parses (a `SongItem` shape change) is skipped, not fatal.
    let items: Vec<SongItem> = state
        .db
        .local_playlist_tracks(key)
        .into_iter()
        .filter_map(|(row, json, added_at)| {
            let song: SongItem = serde_json::from_str(&json).ok()?;
            Some(SongItem { set_video_id: Some(row.to_string()), added_at: Some(added_at), ..song })
        })
        .collect();
    Ok(PlaylistPage {
        title: Some(p.title),
        subtitle: Some(format!("{} songs", items.len())), // the page words its own count
        thumbnail: items.first().and_then(|s| s.thumbnail.clone()),
        description: (!p.description.is_empty()).then_some(p.description),
        privacy: None,
        cover: custom_cover(state, id),
        items,
        continuation: None, // it is all here: nothing to page through
        owned: true,
        collaborative: false,
        sort_menu: None, // no server to keep an order, so every sort is done on the page
        suggestions: None, // YouTube suggests only for its own playlists
    })
}

/// What a track keeps once it is in a playlist: the song, none of the context it was added from
/// (`shed_queue_context`), and no snapshot of account state that would go stale behind it. The
/// rating is read live (the override map, the saved-in index), and Library ▸ Songs tokens are
/// minted per row for one account while these playlists belong to none.
pub fn playlist_row(s: SongItem) -> SongItem {
    SongItem { rating: None, library: None, ..shed_queue_context(s) }
}

pub fn remove_local_rows(state: &Arc<AppState>, playlist_id: &str, rows: &[i64]) -> Result<(), String> {
    let key = local_key(playlist_id)?;
    state.db.remove_local_playlist_tracks(key, rows, crate::db::now_secs()).map_err(db_err)
}

/// Just the playlists on this machine, as library cards. The UI re-reads these after every edit
/// (the count and the artwork follow the tracks), and falls back on them when the account's
/// library can't be fetched, since these need no network.
pub async fn local_playlists(state: &Arc<AppState>) -> Result<Vec<BrowseItem>, String> {
    Ok(state.db.local_playlists().iter().map(|p| local_playlist_card(state, p)).collect())
}

/// Add tracks to a playlist on this machine, answering per track whether it went in (`false`: it
/// was there already, refused the way YouTube refuses one). Whole `SongItem`s rather than the
/// videoIds `add_to_playlist` takes, because nothing will ever fill in a title or artwork later:
/// there is no YouTube playlist to re-read. Files on disk are welcome, unlike in a YouTube one.
pub async fn add_to_local_playlist(
    state: &Arc<AppState>,
    playlist_id: String,
    items: Vec<SongItem>,
) -> Result<Vec<bool>, String> {
    let key = local_key(&playlist_id)?;
    let rows = items
        .into_iter()
        .map(|song| {
            let song = playlist_row(song);
            let json = serde_json::to_string(&song).map_err(|e| e.to_string())?;
            Ok((song.video_id, json))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let r = state.db.add_local_playlist_tracks(key, &rows, crate::db::now_secs()).map_err(db_err);
    touched(state, &playlist_id, r)
}

pub async fn subscribe(state: &Arc<AppState>, channel_id: String, subscribed: bool) -> Result<(), String> {
    let client = require_login(state)?;
    state.it.subscribe(client, &channel_id, subscribed).await.map_err(|e| e.to_string())
}

// --- blocked artists (blocked.rs, plan 046) ----------------------------------------------------

/// The blocked-artist list, for the settings pane.
pub async fn get_blocked_artists(state: &Arc<AppState>) -> Result<Vec<BlockedArtist>, String> {
    Ok(crate::blocked::list(&state.db))
}

/// Block an artist: persist, re-arm the fetch filter, and drop them out of the live queue. Returns
/// the new list. `id` is the channel browseId when the caller has one (a track row often does not).
pub async fn block_artist(
    state: &Arc<AppState>,
    id: Option<String>,
    name: String,
) -> Result<Vec<BlockedArtist>, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("an artist needs a name to be blocked".into());
    }
    let state = state.clone();
    let list =
        crate::blocked::block(&state.db, BlockedArtist { id: id.filter(|i| !i.is_empty()), name });
    let predicate = crate::blocked::block_list(&state.db);
    state.it.set_blocked(predicate.clone());
    // The fetch filter only covers what is fetched from here on, so the queue already on screen
    // has to be cleaned out separately or the block reads as having done nothing.
    state.purge_blocked(&predicate).await;
    Ok(list)
}

pub async fn unblock_artist(state: &Arc<AppState>, key: String) -> Result<Vec<BlockedArtist>, String> {
    let list = crate::blocked::unblock(&state.db, &key);
    state.it.set_blocked(crate::blocked::block_list(&state.db));
    Ok(list)
}

// --- Spotify import (spotify.rs, import.rs, #375) ----------------------------------------------
//
// Errors from these are short codes (`private`, `busy`, `rate_limited`, ...) that the UI words in
// the user's language; anything else is a network error passed through as text.

/// Read a pasted Spotify link, or a file picked from disk (the data export zip, one of its JSON
/// files, or a CSV). Answers what it holds; the tracks stay here until `import_start`.
pub async fn import_read(
    link: Option<String>,
    path: Option<String>,
) -> Result<crate::import::Preview, String> {
    if let Some(link) = link {
        return crate::import::read_link(&link).await;
    }
    let path = path.ok_or("nothing_read")?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    crate::import::read_file(&bytes, &path)
}

/// Start matching the lists picked out of the last read (their indices in its preview).
pub async fn import_start(
    state: &Arc<AppState>,
    lists: Vec<usize>,
) -> Result<crate::import::Snapshot, String> {
    crate::import::start(state, lists)
}

pub async fn import_status() -> Result<Option<crate::import::Snapshot>, String> {
    Ok(crate::import::status())
}

/// The rows of one tier, for the review step.
pub async fn import_rows(
    tier: crate::import::Tier,
) -> Result<Vec<crate::import::ReviewRow>, String> {
    Ok(crate::import::rows(tier))
}

/// The user's pick for one row; `None` leaves the track out.
pub async fn import_pick(
    state: &Arc<AppState>,
    key: String,
    song: Option<SongItem>,
) -> Result<crate::import::Snapshot, String> {
    crate::import::pick(state, &key, song)
}

pub async fn import_create(
    state: &Arc<AppState>,
    options: crate::import::CreateOptions,
) -> Result<(), String> {
    crate::import::create(state, options)
}

/// Stop a running import, or put away a finished one.
pub async fn import_cancel(state: &Arc<AppState>) -> Result<(), String> {
    crate::import::cancel(state);
    Ok(())
}

/// The Spotify link a playlist was imported from, when it was imported from one.
pub async fn import_source(state: &Arc<AppState>, playlist_id: String) -> Result<Option<String>, String> {
    Ok(crate::import::source_url(state, &playlist_id))
}

/// "Update from Spotify": re-read the playlist's link and apply what changed.
pub async fn import_update(
    state: &Arc<AppState>,
    playlist_id: String,
) -> Result<crate::import::Snapshot, String> {
    crate::import::update(state, playlist_id).await
}

/// What a Spotify track, album or artist link is on YouTube Music.
pub async fn import_resolve(
    state: &Arc<AppState>,
    link: String,
) -> Result<crate::import::Resolved, String> {
    crate::import::resolve(state, &link).await
}

// --- local music (local.rs) ------------------------------------------------------------------

/// Rescan the watched folders and return the library. The scan is the deletion check too: its
/// `removed` list is every id that was on screen but is gone from disk, so the UI can drop those
/// tiles without waiting for anyone to click a dead one.
pub async fn get_local_library(state: &Arc<AppState>) -> Result<crate::local::LocalLibrary, String> {
    scan_local(state).await
}

pub async fn add_local_folder(
    state: &Arc<AppState>,
    path: String,
) -> Result<crate::local::LocalLibrary, String> {
    crate::local::add_folder(&state.db, path);
    scan_local(state).await
}

/// Stop watching a folder. Its tracks disappear from the library on the rescan that follows (they
/// come back untouched if the folder is added again — nothing on disk is modified).
pub async fn remove_local_folder(
    state: &Arc<AppState>,
    path: String,
) -> Result<crate::local::LocalLibrary, String> {
    crate::local::remove_folder(&state.db, &path);
    scan_local(state).await
}

/// Disk IO + tag parsing off the async runtime's worker threads.
pub async fn scan_local(state: &Arc<AppState>) -> Result<crate::local::LocalLibrary, String> {
    let host = state.host.clone();
    let state = state.clone();
    let covers = crate::local::covers_dir(&*state.host);
    let lib = crate::rt::spawn_blocking(move || crate::local::scan(&state.db, &covers))
        .await
        .map_err(|e| e.to_string())?;
    // Artwork reaches the page over the asset protocol, which starts out allowing nothing.
    crate::local::allow_covers(&*host, &lib.songs);
    Ok(lib)
}

// --- lyrics ---------------------------------------------------------------------------------

/// Lyrics for a track (cached). The UI passes the metadata it already has from `now-playing`;
/// `duration` is mpv's length in seconds. `None` = no lyrics found anywhere. `source` asks one
/// provider alone, uncached: the source picker's preview.
#[allow(clippy::too_many_arguments)] // the UI's call, flat: `LyricsTrack` + source
pub async fn get_lyrics(
    state: &Arc<AppState>,
    video_id: String,
    title: String,
    artists: String,
    album: Option<String>,
    duration: Option<f64>,
    searched: Option<bool>,
    source: Option<String>,
) -> Result<Option<crate::lyrics::Lyrics>, String> {
    let searched = searched.unwrap_or(false);
    let req = crate::lyrics::LyricsRequest { video_id, title, artists, album, duration, searched };
    crate::lyrics::get_lyrics(state, req, source).await
}

/// Keep one provider's lyrics for this song (`source`), or hand it back to the provider order
/// (`None`). Returns what the song shows now.
#[allow(clippy::too_many_arguments)] // the UI's call, flat: `LyricsTrack` + source
pub async fn choose_lyrics_source(
    state: &Arc<AppState>,
    video_id: String,
    title: String,
    artists: String,
    album: Option<String>,
    duration: Option<f64>,
    searched: Option<bool>,
    source: Option<String>,
) -> Result<Option<crate::lyrics::Lyrics>, String> {
    let searched = searched.unwrap_or(false);
    let req = crate::lyrics::LyricsRequest { video_id, title, artists, album, duration, searched };
    Ok(crate::lyrics::choose_source(state, req, source).await)
}

/// Answers the offset kept (clamped).
pub fn set_lyrics_offset(state: &Arc<AppState>, video_id: String, offset_ms: i64) -> i64 {
    crate::lyrics::set_offset(state, &video_id, offset_ms)
}

/// Every lyrics provider in the user's order, for Settings and the source picker.
pub fn lyrics_providers(state: &Arc<AppState>) -> Vec<crate::lyrics::ProviderInfo> {
    crate::lyrics::providers(state)
}

/// A pasted or imported lyric file (TTML, eLRC, LRC, the JSON shapes) as lines. Empty for plain
/// text, which the editor splits into lines itself.
pub async fn lyrics_parse(text: String) -> Result<Vec<crate::lyrics::LyricLine>, String> {
    crate::lyrics::parse_file(&text)
}


/// The webview's own errors, into the same log file as everything else. Without this a blank
/// screen or a rejected `invoke` leaves no trace at all in the log a user hands over.
pub fn log_ui(level: String, message: String) {
    // Bounded: a throwing `$effect` re-fires every frame, and the file has no size cap.
    let message: String = message.chars().take(2000).collect();
    match level.as_str() {
        "info" => tracing::info!(target: "ui", "{message}"),
        "warn" => tracing::warn!(target: "ui", "{message}"),
        _ => tracing::error!(target: "ui", "{message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Official channels only, one card per stream, most watched first (#424).
    #[test]
    fn live_keeps_official_streams_once_each_by_viewers() {
        let live = |id: &str, title: &str, watching, official| innertube::LiveVideo {
            item: BrowseItem {
                kind: "song",
                id: id.into(),
                title: title.into(),
                subtitle: None,
                thumbnail: None,
                duration: None,
                album_id: None,
                artist_runs: vec![],
                play_count: None,
                is_video: false,
                is_upload: false,
                explicit: false,
            },
            watching,
            official,
        };
        let picked = pick_live(vec![
            live("a", "Lofi Radio", 100, true),
            live("b", "Re-upload", 999, false),
            live("c", "lofi radio", 50, true), // the same title on another channel
            live("a", "Lofi Radio", 100, true), // the same stream from a second query
            live("d", "Jazz Cafe", 200, true),
        ]);
        let ids: Vec<_> = picked.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["d", "a"]);

        // Three per channel at most, the most watched three.
        let one = |id: &str, watching| {
            let mut l = live(id, id, watching, true);
            l.item.subtitle = Some("Lofi Girl".into());
            l
        };
        let picked = pick_live((1..=5).map(|n| one(&n.to_string(), n)).collect());
        let ids: Vec<_> = picked.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["5", "4", "3"]);
    }

    #[test]
    fn on_repeat_rows_shed_the_queue_slot_they_were_played_from() {
        let played = SongItem {
            video_id: "abc".into(),
            title: "Grace".into(),
            queued: true,
            queued_by: Some("simohypers".into()),
            autoplay: true,
            set_video_id: Some("SVI".into()),
            ..Default::default()
        };
        let row = shed_queue_context(played.clone());
        assert_eq!(
            row,
            SongItem { video_id: "abc".into(), title: "Grace".into(), ..Default::default() }
        );
        assert_eq!(row.title, played.title, "the song itself survives");
    }
}
