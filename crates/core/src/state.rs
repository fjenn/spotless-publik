//! App state: transport, player, db, and the queue/playback manager. context/11.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use std::sync::Arc;

use innertube::{
    AccountIdentity, AccountInfo, AudioQuality, BlockList, Clients, InnerTube, SongItem,
    MAIN_CLIENT,
};
use player::Player;
use crate::host::{Host, HostExt, MediaControls};
use tokio::sync::Mutex;

use crate::db::{now_secs, Db, StoredAccount};
use crate::orchestrator::{Orchestrator, PlaybackData, PlaybackPing, ResolveError, REFUSED_MESSAGE};

/// Synthetic browseId for the Your Rotation playlist. Not a YouTube id: `get_playlist` intercepts it
/// and builds the page from local play counts, so it must never collide with a real browseId
/// (`VL…` / `MPRE…` / `RD…`).
pub const ON_REPEAT_ID: &str = "LIMUSIC_ON_REPEAT";
/// How far back Your Rotation looks. A month is long enough to survive a quiet week and short enough
/// that the list still turns over with what you're actually playing.
pub const ON_REPEAT_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;
/// How many songs it holds.
pub const ON_REPEAT_LIMIT: usize = 20;

/// A playlist kept on this machine (issue #251) is the browseId `LOCALPLAYLIST:<row id>`. Like On
/// Repeat, the playlist commands intercept it and answer from SQLite, and YouTube never sees one.
pub const LOCAL_PLAYLIST_PREFIX: &str = "LOCALPLAYLIST:";

/// A radio queue's name: "<name> Radio". No (or an empty) name falls back to the seed song's
/// title, so a radio started from a link or an import still has a title for Recents and Home;
/// a name that already ends in " Radio" (replayed from its own Home card) isn't doubled.
pub fn radio_title(name: Option<&str>, seed_title: Option<&str>) -> Option<String> {
    let n = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or_else(|| seed_title.map(str::trim).filter(|t| !t.is_empty()))?;
    let n = n.strip_suffix(" Radio").unwrap_or(n);
    Some(format!("{n} Radio"))
}

pub fn is_local_playlist(id: &str) -> bool {
    id.starts_with(LOCAL_PLAYLIST_PREFIX)
}

/// One play as it is being heard. See `AppState::hear`.
#[derive(Default)]
struct Listen {
    /// Its `listens` row, from the moment it counts as a play (the latch in `on_position`).
    row: Option<i64>,
    /// The row is a podcast episode's, in `episode_listens` rather than `listens`.
    episode: bool,
    /// Seconds actually heard, and how many of them the row has.
    heard: f64,
    saved: f64,
    /// The previous position tick, which the next one is measured against.
    last_pos: Option<f64>,
}

pub struct AppState {
    pub it: InnerTube,
    pub clients: Clients,
    pub player: Player,
    pub db: Arc<Db>,
    /// The app around the core: events out, paths, platform hooks (host.rs).
    pub host: Arc<dyn Host>,
    pub orchestrator: Arc<Orchestrator>,
    // Serialize effect commands with the live-stream tempo reset, including requests pending.
    playback_effects: Mutex<()>,
    /// Music's effects and the spoken-word speed, kept apart (speeds.rs).
    speeds: std::sync::Mutex<crate::speeds::Speeds>,
    /// mpv's on-disk audio cache dir (context/14) — wiped by the settings "Clear caches" action.
    cache_dir: std::path::PathBuf,
    /// OS media integration (MPRIS/SMTC/NowPlaying). `None` if it failed to init. context/16.
    media: Option<Box<dyn MediaControls>>,
    queue: Mutex<QueueState>,
    /// Bumped on every explicit `play`/jump so superseded async resolves discard their result
    /// (cancellation without JoinHandle bookkeeping). context/06 §6.
    generation: AtomicU64,
    /// Bumped by the `rate` command. [`Self::refresh_from_next`] samples it before its round trip
    /// and throws the answer away if it moved: a like landing in that ~400 ms window is newer
    /// than what YouTube was asked, and applying the stale reply would flip the heart back.
    pub rate_epoch: AtomicU64,
    /// A one-shot resume position `(videoId, secs)` set by `restore_queue` and consumed by the
    /// next `start_current` — applied only when that track is the one being started, so jumping to
    /// a different track first doesn't inherit the old position (context/11).
    pub(crate) pending_seek: std::sync::Mutex<Option<(String, f64)>>,
    /// Whether the watch-history ping has already fired for the current play (latched, so the
    /// frequent position events fire it exactly once; cleared wherever a fresh play starts).
    /// An atomic rather than a `QueueState` field because `on_position` runs on every raw mpv
    /// tick, well above the 4 Hz the UI emit is throttled to, and the flag is latched true for all
    /// but the first ~30 seconds of each track: taking the app's most contended lock to read it
    /// stalled the event pump behind `emit_queue`, `persist_queue`, `prime_lookahead` and the
    /// Listen Together broadcasts for nothing.
    history_pinged: AtomicBool,
    /// The play being heard right now, for the listening stats (#396). See [`Self::hear`].
    listen: std::sync::Mutex<Listen>,
    /// Mirror of mpv's pause flag (set in `media_set_playing`). Position ticks must consult this
    /// instead of assuming "playing" — mpv fires `time-pos` on seeks while paused too.
    is_playing: AtomicBool,
    /// The sleep timer stopped at a track end: the advance opens the next track paused
    /// (`start_current` loads it without playing). See `on_track_ended`.
    hold_next_start: AtomicBool,
    /// Track failures in a row with no playback in between (`on_track_failed`). Reset by any
    /// progress (`on_position`, `on_duration`); at [`MAX_FAILURE_STREAK`] playback stops.
    fail_streak: AtomicU32,
    /// Latest mpv position (f64 bits) + wall-clock secs of the last DB write, for throttled
    /// resume-position persistence.
    latest_position: AtomicU64,
    last_pos_persist: AtomicU64,
    /// Wall-clock secs of the last position push to the OS media controls (throttled ~1s).
    last_media_push: AtomicU64,
    /// Last `(playing, position)` actually handed to the OS media controls, so the steady-state
    /// scrubber tick can be dropped. souvlaki emits a D-Bus `PropertiesChanged` broadcast on every
    /// `set_playback` even when nothing changed, waking every MPRIS client on the desktop; and
    /// MPRIS `Position` is read-on-demand, so the signal does not even carry the one value that
    /// moved. It cannot be dropped entirely: souvlaki answers a `Position` read from the progress
    /// of the last push, so the value still has to be refreshed periodically.
    last_media_state: std::sync::Mutex<Option<(bool, f64)>>,
    /// videoId → (the googlevideo URL the loopback proxy streams from, unix seconds it stops being
    /// treated as usable). Written by the `video_stream` command, read by `videoproxy::handle` and
    /// by `video_stream` itself: an entry that is still live answers a reopen with no network at
    /// all, where re-resolving costs up to two `/player` round trips the user waits through.
    /// ponytail: cleared wholesale at 8 entries rather than LRU-evicted. The player view only ever
    /// looks at one track at a time, so a real LRU would be book-keeping for nothing.
    video_urls: std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>,
    /// Fingerprint of the item list the UI was last sent (see `queue_fingerprint`). When it is
    /// unchanged, `emit_queue` sends the small `queue-index` event instead of megabytes of JSON:
    /// a Tauri event inlines its payload into a JavaScript source string, so a 5,000-track queue
    /// costs ~26 ms of parse in the webview every time it crosses.
    last_queue_fingerprint: AtomicU64,
    /// Dirty key (`persist_fingerprint`) last written to `queue_json`. Same trick as
    /// `last_queue_fingerprint`, for the persistence side: an advance rewrites 40 bytes rather
    /// than the whole blob.
    last_persisted_fingerprint: AtomicU64,
    /// One authentication transition at a time: [`Self::sign_in`], [`Self::switch_account`],
    /// [`Self::sign_out`]. Each reads the cookie and identity, awaits YouTube, then writes both to
    /// the transport and the database, so two of them overlapping lets the loser land last and
    /// win: a failed heal restoring its stale snapshot over a fresh login, or a heal completing
    /// after the user signed out and putting them back in. Async because it is held across those
    /// awaits, which a std `Mutex` cannot be.
    auth: tokio::sync::Mutex<()>,
    /// Serializes concurrent source reloads and lookahead priming.
    reload_mutex: tokio::sync::Mutex<()>,
    /// ── stream: instant ── The load (`generation`) whose track the windows are showing with a
    /// loading state ([`Self::show_loading`]), so a load that gives up can take it down again.
    loading_gen: AtomicU64,
    /// ── stream: instant ── Between `show_loading` and mpv taking the new file: the position ticks
    /// still arriving are the outgoing track's, and must not move the new one's clock.
    loading: AtomicBool,
}

/// Repeat mode for the queue. Serialized lowercase for the UI + `queue_json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

/// Which queue a background playlist walk ([`AppState::fill_playlist`]) is filling: the one that's
/// playing, or an "Add to queue" block (whose pages keep their order and carry the block's label).
enum Fill {
    Playing,
    Queued(Option<String>),
}

/// Canonical persisted account selection. `data_sync_id` drives request delegation; every display
/// field beside it belongs to that same identity. `account_json` and the legacy `data_sync_id`
/// setting are written only as atomic projections of this model.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct SelectedIdentity {
    #[serde(default)]
    data_sync_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    handle: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    has_multiple_identities: bool,
}

pub enum SignInOutcome {
    Complete,
    SelectionRequired,
}

impl SelectedIdentity {
    fn from_account_info(info: AccountInfo, has_multiple_identities: bool) -> Option<Self> {
        Some(Self {
            data_sync_id: info.data_sync_id,
            name: Some(info.name?),
            handle: info.handle,
            email: info.email,
            thumbnail: info.thumbnail,
            channel_id: info.channel_id,
            has_multiple_identities,
        })
    }

    fn from_identity(
        identity: &AccountIdentity,
        refreshed: AccountInfo,
        has_multiple_identities: bool,
    ) -> Self {
        Self {
            data_sync_id: Some(identity.data_sync_id.clone()),
            name: refreshed.name.or_else(|| Some(identity.name.clone())),
            handle: refreshed.handle.or_else(|| identity.handle.clone()),
            email: refreshed.email.or_else(|| identity.email.clone()),
            thumbnail: refreshed.thumbnail.or_else(|| identity.thumbnail.clone()),
            channel_id: refreshed.channel_id.or_else(|| identity.channel_id.clone()),
            has_multiple_identities,
        }
    }

    fn account_json(&self, signed_in: bool) -> serde_json::Value {
        serde_json::json!({
            "signedIn": signed_in,
            "name": self.name,
            "handle": self.handle,
            "email": self.email,
            "thumbnail": self.thumbnail,
            "channelId": self.channel_id,
            "canSwitch": self.has_multiple_identities,
        })
    }

    fn as_account_identity(&self) -> Option<AccountIdentity> {
        Some(AccountIdentity {
            name: self.name.clone().unwrap_or_default(),
            handle: self.handle.clone(),
            email: self.email.clone(),
            thumbnail: self.thumbnail.clone(),
            channel_id: self.channel_id.clone(),
            data_sync_id: self.data_sync_id.clone()?,
            is_selected: false,
        })
    }
}

fn selected_identity_from_db(db: &Db) -> Option<SelectedIdentity> {
    db.get_setting("selected_identity_json")
        .and_then(|json| serde_json::from_str::<SelectedIdentity>(&json).ok())
        .filter(|identity| identity.name.is_some() || identity.data_sync_id.is_some())
        .or_else(|| {
            let account = db
                .get_setting("account_json")
                .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())?;
            let string = |key: &str| {
                account
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            };
            Some(SelectedIdentity {
                data_sync_id: db.get_setting("data_sync_id").filter(|id| !id.is_empty()),
                name: string("name"),
                handle: string("handle"),
                email: string("email"),
                thumbnail: string("thumbnail"),
                channel_id: string("channelId"),
                has_multiple_identities: account
                    .get("canSwitch")
                    .and_then(serde_json::Value::as_bool)
                    // Pre-switcher account_json has no flag. Keep the action discoverable until
                    // the startup identity refresh determines whether this legacy user has one
                    // channel or several.
                    .unwrap_or(true),
            })
        })
}

fn auth_selection_pending(db: &Db) -> bool {
    db.get_setting("account_selection_pending").as_deref() == Some("true")
}

/// Startup compatibility: prefer the canonical model, then fall back to the pre-switcher setting.
pub fn persisted_data_sync_id(db: &Db) -> Option<String> {
    selected_identity_from_db(db)
        .and_then(|identity| identity.data_sync_id)
        .or_else(|| db.get_setting("data_sync_id").filter(|id| !id.is_empty()))
}

fn identity_selection_key(data_sync_id: &str) -> String {
    let mut hasher = DefaultHasher::new();
    "limusic-account-identity-v1".hash(&mut hasher);
    data_sync_id.hash(&mut hasher);
    format!("identity-{:016x}", hasher.finish())
}

fn identity_snapshot(identity: &AccountIdentity, selected: bool) -> serde_json::Value {
    serde_json::json!({
        "selectionKey": identity_selection_key(&identity.data_sync_id),
        "name": identity.name,
        "handle": identity.handle,
        "email": identity.email,
        "thumbnail": identity.thumbnail,
        "channelId": identity.channel_id,
        "selected": selected,
    })
}

#[derive(Default)]
struct QueueState {
    items: Vec<SongItem>,
    current: usize,
    /// Pre-shuffle order snapshot. `Some(..)` ⇔ shuffle is ON; restored on shuffle-off.
    shuffle_orig: Option<Vec<SongItem>>,
    repeat: RepeatMode,
    /// Radio playlist id for autoplay continuation: `RDAMPL<plId>` when the queue came from a
    /// playlist/album, `None` otherwise (autoplay then seeds `RDAMVM<last video>`, so long
    /// sessions drift with what's actually playing, like YTM's).
    radio_seed: Option<String>,
    /// Human name of what seeded the queue (playlist/album title, "<song> Radio") — the queue
    /// panel's "Next from: …" header. Pure display metadata.
    source_name: Option<String>,
    /// The playlist this queue was started from, exactly as the page passed it. What the player's
    /// "Remove from this playlist" writes to, so the action only exists while a playlist is what
    /// is actually playing. `None` for radios, single songs and a mirrored guest queue.
    source_id: Option<String>,
    /// This queue is a radio: YouTube generated every upcoming track, so "Add to queue" goes ahead
    /// of them rather than behind a feed the user never asked to finish ([`enqueue_at`]).
    radio: bool,
    /// `play_song` is fetching this queue's radio right now (the generation it did it for). The
    /// autoplay early trigger fires from `start_current` while such a queue is still its single
    /// seed track, so without this hold both paths request the same `RDAMVM…` page at the same
    /// moment and the queue ends up holding it twice (issue #255).
    hydrating: Option<u64>,
    /// The queue index we've already appended to mpv for gapless lookahead (if any).
    lookahead_loaded: Option<usize>,
    /// Which client served the currently-loaded track (for the WEB_REMIX-403 feedback). context/06.
    current_client: Option<String>,
    /// The client that served the primed lookahead track — promoted to `current_client` on a
    /// gapless advance so the failure feedback still knows the client.
    lookahead_client: Option<String>,
    /// Raw `loudnessDb` of the currently-loaded track, kept so toggling normalization can retune
    /// what is already playing instead of waiting for the next one (#298).
    current_loudness_db: Option<f64>,
    /// Raw `loudnessDb` for the primed lookahead. mpv's `af` is global, so the gain can't ride
    /// along with the appended entry: it's computed and applied when the gapless advance is
    /// observed. Outer `Option` is "a lookahead is primed", inner is "YouTube told us a loudness".
    lookahead_loudness_db: Option<Option<f64>>,
    /// Watch-history ping for the current track + the primed lookahead's (promoted on a gapless
    /// advance, mirroring current/lookahead_client). context/01 §registerPlayback.
    playback_ping: Option<PlaybackPing>,
    lookahead_playback_ping: Option<PlaybackPing>,
    /// Content Playback Nonce for the current play. context/01. (The "already pinged" latch that
    /// used to live beside it is `AppState::history_pinged`.)
    cpn: String,
    /// Latest mpv-reported track duration (secs), for the history-ping threshold.
    duration: f64,
    /// The current track is a broadcast on air (#424). Its deck carries the live file options, so
    /// nothing is primed behind it (seeks into it are the player's to refuse, see `Player::seek`).
    /// Only a `load` sets it; a gapless advance can't land on one.
    live: bool,
    /// Last videoId we re-resolved after a playback failure — guards the one-shot retry in
    /// `on_track_failed` against a retry loop when the retried stream dies too.
    retried: Option<String>,
    /// The queue this one replaced, for Previous at the head of a fresh queue.
    prev_context: Option<PrevContext>,
    /// Which queue this is. Bumped only when a different one replaces it (a playlist, a song, a
    /// restored context, a Listen Together rebuild), never by a skip within it, unlike
    /// `AppState::generation`, which moves on every load. `fill_playlist` checks this one: a skip
    /// during the walk used to stop it for good, leaving a shuffled 4,000-track playlist playing
    /// out of its first page or two (issue #316).
    epoch: u64,
    /// The ids a playlist `/next` has already paired in queue `.0`, so a playlist played in order
    /// asks once per window, not once per track (`refresh_playlist_twins`, #542).
    paired: (u64, HashSet<String>),
}

/// What was playing before a click replaced the whole queue, so Previous can go back to it.
///
/// Clicking a song outside the queue (a search result, a card) throws the queue away and starts a
/// one-track one, which left Previous with nothing behind it to reach: it restarted a track the
/// user had been listening to for two seconds. Keeping the outgoing queue means that press goes
/// where they expect, and the restored queue still has its own order, shuffle and radio behind it.
///
/// ponytail: one level deep. Two presses don't walk back two contexts; make it a bounded Vec if
/// anyone asks, the snapshot is already the whole of what a restore needs.
struct PrevContext {
    items: Vec<SongItem>,
    current: usize,
    shuffle_orig: Option<Vec<SongItem>>,
    radio_seed: Option<String>,
    source_name: Option<String>,
    source_id: Option<String>,
    radio: bool,
    /// Where mpv had got to, so the track resumes instead of starting over.
    position: f64,
}

impl QueueState {
    /// The next row playback moves to: [`next_index`], stepping over rows marked `unavailable`
    /// (Spotify keeps an unplayable track visible and skips it). `None` when nothing playable is
    /// left, which with repeat-all on means every row is unavailable.
    fn next_playable(&self, repeat: RepeatMode) -> Option<usize> {
        next_playable_index(&self.items, self.current, repeat)
    }

    /// Mark the row at `index` unplayable, and its copy in the pre-shuffle snapshot, so a shuffle
    /// toggle doesn't bring it back un-greyed. False when the slot no longer holds `video_id`.
    fn mark_unavailable(&mut self, index: usize, video_id: &str, on: bool) -> bool {
        let Some(item) = self.items.get_mut(index).filter(|i| i.video_id == video_id) else {
            return false;
        };
        if item.unavailable == on {
            return true;
        }
        let before = item.clone();
        item.unavailable = on;
        if let Some(orig) = self.shuffle_orig.as_mut() {
            if let Some(o) = orig.iter_mut().find(|s| **s == before) {
                o.unavailable = on;
            }
        }
        true
    }

    /// Take one row out of the queue, and its copy out of the pre-shuffle snapshot, or turning
    /// shuffle off rebuilds the queue with it back in. Matched whole, so of two copies of a song
    /// (two playlist rows, different `set_video_id`) the right one goes. Never by video id alone:
    /// adds carried into a shuffled `play_tracks` are not in the snapshot, and one that is also in
    /// the playlist would take the playlist's row with it. `current` is the caller's to fix.
    fn remove_row(&mut self, index: usize) -> SongItem {
        let item = self.items.remove(index);
        if let Some(orig) = self.shuffle_orig.as_mut() {
            if let Some(at) = orig.iter().position(|s| *s == item) {
                orig.remove(at);
            }
        }
        item
    }

    /// Move the play pointer within the same queue.
    fn seek_to(&mut self, index: usize) {
        self.current = index;
        // Every advance and every jump routes through here, which makes it the one place the
        // one-retry marker can go stale. Without this it is "retried once, ever": a track that
        // was retried in the morning gets no retry tonight.
        self.retried = None;
        // Past the head, Previous has a track of its own, and coming back to the head later (a
        // repeat-all wrap, a click on the first row) must not swap this queue for a stale one.
        if index > 0 {
            self.prev_context = None;
        }
    }

    /// Set this queue aside so Previous can come back to it. `position` is mpv's.
    ///
    /// Takes `items` and `shuffle_orig` rather than cloning them, so this may only be called from
    /// a caller that replaces both immediately afterwards — which is exactly what a queue
    /// replacement is. Anything that reads either field (`shuffle_orig.is_some()` for sticky
    /// shuffle, `upcoming_queued` for the carried manual adds) has to read it before this.
    fn keep_context(&mut self, position: f64) {
        if self.items.is_empty() {
            return;
        }
        // Clamped, so `restore_context` can index with it: a queue whose tail was trimmed under a
        // pointer sitting on it would otherwise be a panic hours later, in an unrelated press.
        let current = self.current.min(self.items.len() - 1);
        self.prev_context = Some(PrevContext {
            items: std::mem::take(&mut self.items),
            current,
            shuffle_orig: self.shuffle_orig.take(),
            radio_seed: self.radio_seed.take(),
            source_name: self.source_name.take(),
            source_id: self.source_id.take(),
            radio: self.radio,
            position,
        });
    }

    /// Put a kept queue back. Returns the track to start and where to resume it.
    fn restore_context(&mut self, prev: PrevContext) -> (String, f64) {
        let video_id = prev.items[prev.current].video_id.clone();
        self.items = prev.items;
        self.epoch += 1;
        self.current = prev.current;
        self.shuffle_orig = prev.shuffle_orig;
        self.radio_seed = prev.radio_seed;
        self.source_name = prev.source_name;
        self.source_id = prev.source_id;
        self.radio = prev.radio;
        self.lookahead_loaded = None; // mpv's primed next belongs to the queue we just dropped
        self.hydrating = None; // as does any radio still being fetched for it
        self.retried = None;
        (video_id, prev.position)
    }
}

/// How long a resolved googlevideo URL is treated as usable. YouTube's links last about six hours;
/// four is the conservative margin, and the cost of guessing low is one extra resolve.
const VIDEO_URL_TTL_SECS: i64 = 4 * 60 * 60;

/// How many resolved URLs are kept. Small on purpose: the map exists so a reopen and a replay are
/// free, not to be a download cache.
const VIDEO_URL_CAP: usize = 8;

/// How far into a track Previous stops meaning "go back" and starts meaning "start this one over".
const PREV_REWIND_SECS: f64 = 3.0;

type VideoUrls = std::collections::HashMap<String, (String, i64)>;

fn put_url(map: &mut VideoUrls, id: &str, url: String, now: i64) {
    if map.len() >= VIDEO_URL_CAP {
        map.clear();
    }
    map.insert(id.to_owned(), (url, now + VIDEO_URL_TTL_SECS));
}

fn get_url(map: &VideoUrls, id: &str, now: i64) -> Option<String> {
    let (url, expires_at) = map.get(id)?;
    (*expires_at > now).then(|| url.clone())
}

/// Fired once the UI has mounted (the desktop's `show_main`, the iOS app's first screen):
/// `play_on_start` waits for it so the restored track does not start before anything shows it.
pub static UI_MOUNTED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// The running app's state, for the background loops that start before it exists or outlive a
/// borrow of it (Made for you refresh, podcasts, Home). Tauri's `try_state` used to be this.
static GLOBAL: std::sync::RwLock<Option<std::sync::Weak<AppState>>> = std::sync::RwLock::new(None);

/// Register `st` as the app's state (once, right after it is built).
pub fn install(st: &Arc<AppState>) {
    *GLOBAL.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(st));
}

/// The app's state, once [`install`]ed.
pub fn global() -> Option<Arc<AppState>> {
    GLOBAL.read().unwrap_or_else(|e| e.into_inner()).as_ref()?.upgrade()
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        it: InnerTube,
        clients: Clients,
        player: Player,
        db: Arc<Db>,
        host: Arc<dyn Host>,
        orchestrator: Arc<Orchestrator>,
        cache_dir: std::path::PathBuf,
        media: Option<Box<dyn MediaControls>>,
    ) -> Self {
        let speeds =
            crate::speeds::Speeds::with_spoken(db.get_setting(crate::speeds::EPISODE_SPEED_KEY).as_deref());
        AppState {
            it,
            clients,
            player,
            db,
            host,
            orchestrator,
            cache_dir,
            media,
            queue: Mutex::new(QueueState::default()),
            playback_effects: Mutex::new(()),
            speeds: std::sync::Mutex::new(speeds),
            auth: tokio::sync::Mutex::default(),
            reload_mutex: tokio::sync::Mutex::default(),
            loading_gen: AtomicU64::new(0),
            loading: AtomicBool::new(false),
            history_pinged: AtomicBool::new(false),
            listen: std::sync::Mutex::default(),
            is_playing: AtomicBool::new(false),
            hold_next_start: AtomicBool::new(false),
            fail_streak: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            rate_epoch: AtomicU64::new(0),
            pending_seek: std::sync::Mutex::new(None),
            video_urls: std::sync::Mutex::new(std::collections::HashMap::new()),
            latest_position: AtomicU64::new(0),
            last_pos_persist: AtomicU64::new(0),
            last_media_push: AtomicU64::new(0),
            last_media_state: std::sync::Mutex::new(None),
            last_queue_fingerprint: AtomicU64::new(0),
            last_persisted_fingerprint: AtomicU64::new(0),
        }
    }

    /// Remember where the loopback video proxy should fetch `id` from.
    pub fn put_video_url(&self, id: &str, url: String) {
        let Ok(mut map) = self.video_urls.lock() else { return };
        put_url(&mut map, id, url, crate::db::now_secs());
    }

    pub fn video_url(&self, id: &str) -> Option<String> {
        let map = self.video_urls.lock().ok()?;
        get_url(&map, id, crate::db::now_secs())
    }

    /// Drop a remembered URL the `<video>` element could not load. A googlevideo link that has
    /// expired early, or been revoked, looks exactly like this from the webview, and without this
    /// the cache would keep handing back the dead one until its TTL ran out.
    pub fn forget_video_url(&self, id: &str) {
        if let Ok(mut map) = self.video_urls.lock() {
            map.remove(id);
        }
    }

    /// Give the player `video_id`'s music video for the audio file it was handed as `audio_url`,
    /// when mpv draws videos itself and the user has music videos on. Never on macOS, where the
    /// player view's `<video>` element shows the picture (videoproxy.rs), so this is a no-op there.
    /// Detached: the resolve is a `/player` round trip, and the audio never waits on a picture.
    fn attach_video(self: &Arc<Self>, item: &SongItem, is_video: bool, audio_url: &str) {
        let video_id = item.video_id.as_str();
        // ── stream: video-pods ── an episode's picture follows the podcast rules
        // (`podcasts::video`), not the music-video setting, and may be another video (an RSS
        // episode's YouTube twin) or no extra track at all (a video enclosure carries its own).
        if crate::podcasts::is_episode_item(item) {
            if cfg!(target_os = "macos") {
                return;
            }
            match crate::podcasts::video::for_item(&self.db, item) {
                Some(crate::podcasts::video::EpisodeVideo { video_id: Some(id), .. }) => {
                    self.attach_picture(&id, audio_url);
                }
                Some(_) => {
                    let _ = self.host.emit("video-ready", video_id);
                }
                None => {}
            }
            return;
        }
        // ── end stream: video-pods ──
        if !is_video
            || cfg!(target_os = "macos")
            || crate::local::is_local_song(video_id)
            || self.db.get_setting("music_videos").as_deref() != Some("true")
        {
            return;
        }
        self.attach_picture(video_id, audio_url);
    }

    /// Resolve `video_id`'s picture and hand it to mpv as one more track of `audio_url`.
    fn attach_picture(self: &Arc<Self>, video_id: &str, audio_url: &str) {
        let (st, id, audio) = (self.clone(), video_id.to_owned(), audio_url.to_owned());
        crate::rt::spawn(async move {
            let url = match st.video_url(&id) {
                Some(u) => u,
                None => {
                    // ponytail: 720p, the ceiling the <video> path settled on for the player view.
                    // mpv scales whatever it gets; raise it if theater mode ever shows the video.
                    let disabled = st.disabled_clients();
                    let Some(u) = st.orchestrator.resolve_video(&id, 720, &disabled).await else {
                        return;
                    };
                    st.put_video_url(&id, u.clone());
                    u
                }
            };
            // Through the same chunked loopback proxy as the audio, for the same reason: mpv's
            // open-ended range request is throttled to ~2x realtime (audioproxy.rs). Direct when
            // the user has a proxy, as `mpv_stream_url` does.
            let proxied = if crate::http::has_proxy() {
                None
            } else {
                crate::audioproxy::register(&url, &std::collections::HashMap::new())
            };
            st.player.set_video_for(&audio, proxied.as_deref().unwrap_or(&url));
            let _ = st.host.emit("video-ready", &id);
        });
    }

    /// mpv could not open the picture it was handed for `audio` (only the playing deck ever opens
    /// one). Drop that URL and try once more, which now goes to VISIONOS first
    /// (`Orchestrator::mark_video_failed`); a second failure within the TTL leaves the artwork.
    ///
    /// Ignored once `audio` is no longer the file playing: the open is a round trip, and a skip or
    /// a crossfade landing during it would otherwise pin the failure on the next track. A crossfade
    /// swaps decks before it sends `TrackEnded`, and events arrive in order, so by the time the
    /// next track's own failure gets here the queue has moved to it too. The old track keeps its
    /// dead URL until it next plays, fails again, and is handled then.
    pub async fn on_video_failed(self: &Arc<Self>, audio: &str) {
        if self.player.current_path().as_deref() != Some(audio) {
            tracing::debug!("video: a failed picture for a track no longer playing, ignored");
            return;
        }
        let Some(item) = self.current_item().await else { return };
        // ── stream: video-pods ── an episode's picture can be another video (its YouTube twin).
        let picture = crate::podcasts::video::for_item(&self.db, &item)
            .and_then(|v| v.video_id)
            .unwrap_or_else(|| item.video_id.clone());
        self.forget_video_url(&picture);
        if self.orchestrator.mark_video_failed(&picture) {
            self.attach_current_video().await;
        }
    }

    /// The picture stalled playback and the player dropped it; the sound plays on. A toast says
    /// why the picture went away. The iOS app reads it off `video_stats` and switches to audio.
    pub fn on_video_dropped(&self, audio: &str) {
        if self.player.current_path().as_deref() != Some(audio) {
            return;
        }
        self.emit_notice("Video unavailable right now. Playing the audio.");
    }

    /// [`Self::attach_video`] for the track already playing, when music videos were just turned on.
    pub async fn attach_current_video(self: &Arc<Self>) {
        if let (Some(item), Some(path)) = (self.current_item().await, self.player.current_path()) {
            self.attach_video(&item, item.is_video, &path);
        }
    }

    /// The `quality` setting moved. Every cached stream URL was chosen at the old quality and the
    /// cache is keyed by video id only, so empty it, and re-prime the next track at the new one.
    /// The playing track keeps its stream until it ends (no rebuffer mid-song).
    pub async fn quality_changed(self: &Arc<Self>) {
        self.db.clear_stream_urls();
        {
            let mut q = self.queue.lock().await;
            if q.lookahead_loaded.take().is_some() {
                let _ = self.player.clear_playlist();
            }
        }
        self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
    }

    pub fn quality(&self) -> AudioQuality {
        streaming_quality(self.db.get_setting("quality").as_deref())
    }

    /// User-disabled stream clients — comma-separated setting. Also the force-fail lever for the
    /// rustypipe-solo acceptance test; `LIMUSIC_DISABLED_CLIENTS` env overrides for quick testing.
    pub fn disabled_clients(&self) -> HashSet<String> {
        let raw = std::env::var("LIMUSIC_DISABLED_CLIENTS")
            .ok()
            .or_else(|| self.db.get_setting("disabled_stream_clients"))
            .unwrap_or_default();
        raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }

    // --- auth (context/15) ------------------------------------------------------------------

    /// Sign in with the Cookie header captured from the login webview (context/15 Path A).
    ///
    /// `account_menu` validates the cookie and provides the active header. `accounts_list` then
    /// discovers every server-selectable identity. A persisted matching identity wins over
    /// Google's current default; a new multi-channel login pauses before finalization and asks the
    /// UI to choose.
    /// `expect_account` is the SAPISID the caller read before it went off to fetch `cookie`. The
    /// session heal needs it: the user can switch accounts while it exports the jar, and signing
    /// in with what it found would drag them back to the account they just left.
    pub async fn sign_in(
        &self,
        cookie: String,
        expect_account: Option<&str>,
    ) -> Result<SignInOutcome, String> {
        // Validating a cookie already in hand: a 401 here is the answer, not something to wait on
        // a heal for. `refresh_session` also reaches `sign_in` while healing, and a wait there
        // would park the healing task on the heal it is performing, holding `auth` for the whole
        // timeout.
        innertube::without_healing(self.sign_in_inner(cookie, expect_account)).await
    }

    async fn sign_in_inner(
        &self,
        cookie: String,
        expect_account: Option<&str>,
    ) -> Result<SignInOutcome, String> {
        let _turn = self.auth.lock().await;
        if let Some(expect) = expect_account {
            let live = self.it.cookie().unwrap_or_default();
            if innertube::cookie_sapisid(&live) != Some(expect) {
                return Err("The active account changed — skipping this sign-in.".into());
            }
        }
        let cookie = cookie.trim().to_owned();
        if innertube::cookie_sapisid(&cookie).is_none() {
            return Err("Sign-in didn't complete — try signing in again.".into());
        }
        let previous_cookie = self.it.cookie();
        let previous_data_sync_id = self.it.data_sync_id();
        let persisted_identity = selected_identity_from_db(&self.db);
        let persisted_id = persisted_identity
            .as_ref()
            .and_then(|identity| identity.data_sync_id.clone())
            .or_else(|| self.db.get_setting("data_sync_id").filter(|id| !id.is_empty()));

        self.it.set_cookie(Some(cookie.clone()));
        // Discovery must not inherit a stale/default identity. The persisted id is restored only
        // after the fresh accounts list proves that this cookie can act as it.
        self.it.set_data_sync_id(None);
        let client =
            self.clients.get(innertube::METADATA_CLIENT).ok_or("metadata client missing")?;
        let active = match self.it.account_menu(client).await {
            // A valid, authenticating cookie returns the account header (name). No name means the
            // session didn't actually authenticate — reject it up front so we don't "succeed" into
            // a silently-empty library.
            Ok(i) if i.name.is_some() => i,
            Ok(_) => {
                self.restore_auth_transport(previous_cookie, previous_data_sync_id);
                return Err("That session didn't authenticate — sign in again.".into());
            }
            // Auth didn't take (network) — roll back so we're not half-logged-in.
            Err(e) => {
                self.restore_auth_transport(previous_cookie, previous_data_sync_id);
                return Err(format!("Sign-in failed: {e}"));
            }
        };

        let active_visitor_data = active.visitor_data.clone();

        // Discovery must go out with the login-bound visitorData account_menu just handed us, not
        // the anonymous bootstrap id still sitting in the session: pairing that id with login
        // cookies is what gets accounts_list rejected, and a rejected list reads below as "this
        // Google account has one channel". Every success path persists this same value; hand the
        // old one back until then so a rolled-back sign-in isn't left holding it.
        let previous_visitor_data = self.it.visitor_data();
        if active_visitor_data.is_some() {
            self.it.set_visitor_data(active_visitor_data.clone());
        }
        // Losing the list endpoint must not regress ordinary one-channel sign-in. It only removes
        // the optional switcher for this login attempt; account_menu still supplies a valid active
        // identity exactly as older releases used it.
        let identities = match self.it.account_identities(client).await {
            Ok(identities) => identities,
            Err(error) => {
                tracing::warn!(%error, "could not discover alternate YouTube identities");
                Vec::new()
            }
        };
        self.it.set_visitor_data(previous_visitor_data);

        let chosen = persisted_id
            .as_deref()
            .and_then(|id| identities.iter().find(|identity| identity.data_sync_id == id));
        if let Some(identity) = chosen {
            if identities.len() == 1 {
                let selected = SelectedIdentity::from_identity(identity, active, false);
                self.persist_selected_identity(selected, active_visitor_data.as_deref())
                    .inspect_err(|_| {
                        self.restore_auth_transport(
                            previous_cookie.clone(),
                            previous_data_sync_id.clone(),
                        );
                        self.restore_session_cookie_setting(previous_cookie.as_deref());
                    })?;
                return Ok(SignInOutcome::Complete);
            }
            self.activate_identity(identity, identities.len() > 1, client).await.inspect_err(
                |_| {
                    self.restore_auth_transport(
                        previous_cookie.clone(),
                        previous_data_sync_id.clone(),
                    );
                    self.restore_session_cookie_setting(previous_cookie.as_deref());
                },
            )?;
            return Ok(SignInOutcome::Complete);
        }

        // A missing/unreadable list must not replace a previously selected channel with Google's
        // current default. Revalidate the stored server-issued id directly; account_menu either
        // confirms it and refreshes metadata, or the login fails closed.
        //
        // Only when the list is empty. A list that came back and does *not* contain the persisted
        // id means these cookies belong to a different Google account, so the stored id is theirs
        // to drop: forcing it here would delegate account A's channel onto account B's cookie and
        // fail every sign-in until the user signs out first.
        if identities.is_empty() {
            if let Some(data_sync_id) = persisted_id.as_deref() {
                let identity = persisted_identity
                    .as_ref()
                    .and_then(SelectedIdentity::as_account_identity)
                    .unwrap_or_else(|| AccountIdentity {
                        name: String::new(),
                        handle: None,
                        email: None,
                        thumbnail: None,
                        channel_id: None,
                        data_sync_id: data_sync_id.to_owned(),
                        is_selected: false,
                    });
                self.activate_identity(
                    &identity,
                    persisted_identity.as_ref().is_some_and(|saved| saved.has_multiple_identities),
                    client,
                )
                .await
                .inspect_err(|_| {
                    self.restore_auth_transport(
                        previous_cookie.clone(),
                        previous_data_sync_id.clone(),
                    );
                    self.restore_session_cookie_setting(previous_cookie.as_deref());
                })?;
                return Ok(SignInOutcome::Complete);
            }
        }

        if identities.len() == 1 {
            // One channel stays seamless. Reuse the already-fetched active header instead of
            // introducing another network request or a new delegated id into the historical
            // single-account path.
            let selected = SelectedIdentity::from_account_info(active, false)
                .ok_or("That session didn't return an account identity")?;
            self.persist_selected_identity(selected, active_visitor_data.as_deref()).inspect_err(
                |_| {
                    self.restore_auth_transport(
                        previous_cookie.clone(),
                        previous_data_sync_id.clone(),
                    );
                    self.restore_session_cookie_setting(previous_cookie.as_deref());
                },
            )?;
            return Ok(SignInOutcome::Complete);
        }

        if identities.len() > 1 {
            // No persisted choice for these cookies: atomically keep the authenticated cookie,
            // mark the login unfinished, and remove stale projections. A restart will reopen the
            // required picker instead of silently acting as YouTube's default channel.
            self.it.set_data_sync_id(None);
            self.db.set_pending_auth_selection(&cookie, None).map_err(|error| {
                self.restore_auth_transport(previous_cookie.clone(), previous_data_sync_id.clone());
                self.restore_session_cookie_setting(previous_cookie.as_deref());
                error.to_string()
            })?;
            self.persist_visitor_data(active_visitor_data.as_deref());
            self.sync_active_account();
            let _ = self.host.emit("account-selection-required", ());
            return Ok(SignInOutcome::SelectionRequired);
        }

        // No selectable token was exposed (the normal historical single-channel response). Keep
        // the seamless path and persist the active header + response-context dataSyncId together.
        let selected = SelectedIdentity::from_account_info(active, false)
            .ok_or("That session didn't return an account identity")?;
        self.persist_selected_identity(selected, active_visitor_data.as_deref()).inspect_err(
            |_| {
                self.restore_auth_transport(previous_cookie.clone(), previous_data_sync_id.clone());
                self.restore_session_cookie_setting(previous_cookie.as_deref());
            },
        )?;
        Ok(SignInOutcome::Complete)
    }

    /// Fresh switcher rows for the account picker. Raw delegated ids never cross the Tauri
    /// boundary; the UI receives a process-local opaque selector and display metadata only.
    pub async fn account_identities(&self) -> Result<Vec<serde_json::Value>, String> {
        if !self.it.is_logged_in() {
            return Err("Sign in before switching channels.".into());
        }
        let client =
            self.clients.get(innertube::METADATA_CLIENT).ok_or("metadata client missing")?;
        let identities = self.it.account_identities(client).await.map_err(|e| e.to_string())?;
        let selected_id = self.it.data_sync_id();
        // Nothing is selected yet during a forced first pick, so YouTube's own default marker
        // would be a lie: the user hasn't chosen it, and picking it is still a required step.
        let pending = auth_selection_pending(&self.db);
        Ok(identities
            .iter()
            .map(|identity| {
                let selected = !pending
                    && selected_id
                        .as_deref()
                        .map(|id| id == identity.data_sync_id)
                        .unwrap_or(identity.is_selected);
                identity_snapshot(identity, selected)
            })
            .collect())
    }

    /// Switch without a Google re-login. The selector is resolved against a fresh server list and
    /// account_menu must succeed under a one-off context for that exact identity before the shared
    /// transport, persistence, or UI is updated.
    pub async fn switch_account(&self, selection_key: &str) -> Result<serde_json::Value, String> {
        // Same as `sign_in`: this validates an identity it was handed, so let it hear the answer.
        innertube::without_healing(self.switch_account_inner(selection_key)).await
    }

    async fn switch_account_inner(&self, selection_key: &str) -> Result<serde_json::Value, String> {
        let _turn = self.auth.lock().await;
        if !self.it.is_logged_in() {
            return Err("Sign in before switching channels.".into());
        }
        let client =
            self.clients.get(innertube::METADATA_CLIENT).ok_or("metadata client missing")?;
        let identities = self.it.account_identities(client).await.map_err(|e| e.to_string())?;
        let identity = identities
            .iter()
            .find(|identity| identity_selection_key(&identity.data_sync_id) == selection_key)
            .ok_or(
                "That YouTube channel is no longer available. Refresh the list and try again.",
            )?;
        self.activate_identity(identity, identities.len() > 1, client).await
    }

    async fn activate_identity(
        &self,
        identity: &AccountIdentity,
        has_multiple_identities: bool,
        client: &innertube::YouTubeClient,
    ) -> Result<serde_json::Value, String> {
        let refreshed =
            match self.it.account_menu_for_identity(client, &identity.data_sync_id).await {
                Ok(info) if info.name.is_some() => info,
                Ok(_) => {
                    return Err(
                        "YouTube did not confirm that channel. Try signing in again.".into()
                    );
                }
                Err(error) => {
                    return Err(format!("Couldn't switch YouTube channel: {error}"));
                }
            };
        let visitor_data = refreshed.visitor_data.clone();
        let selected =
            SelectedIdentity::from_identity(identity, refreshed, has_multiple_identities);
        self.persist_selected_identity(selected, visitor_data.as_deref())
    }

    /// Persist a freshly picked channel: the canonical selected identity, its projections, and the
    /// visitorData, then refresh the saved account row and the transport to match. context/15.
    fn persist_selected_identity(
        &self,
        selected: SelectedIdentity,
        visitor_data: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let account = selected.account_json(true);
        let selected_json = serde_json::to_string(&selected).map_err(|e| e.to_string())?;
        let account_json = account.to_string();
        let session_cookie = self
            .it
            .cookie()
            .ok_or("The YouTube session expired before the channel could be saved.")?;
        self.db
            .set_auth_identity(
                &session_cookie,
                &selected_json,
                selected.data_sync_id.as_deref(),
                &account_json,
            )
            .map_err(|e| format!("Couldn't save the selected YouTube channel: {e}"))?;
        self.persist_visitor_data(visitor_data);
        self.sync_active_account();
        self.forget_playlist_index();
        self.it.set_data_sync_id(selected.data_sync_id.clone());
        let _ = self.host.emit("auth-changed", &account);
        Ok(account)
    }

    /// The playlist membership index belongs to one account, so a sign-out or a channel switch
    /// empties it. Its timestamp goes too, or the next sync would think it was still fresh.
    fn forget_playlist_index(&self) {
        self.db.clear_playlist_index();
        self.db.delete_setting(crate::api::PLAYLIST_INDEX_STAMP);
        self.db.delete_setting(crate::api::PLAYLIST_INDEX_READONLY);
    }

    fn restore_auth_transport(&self, cookie: Option<String>, data_sync_id: Option<String>) {
        self.it.set_cookie(cookie);
        self.it.set_data_sync_id(data_sync_id);
    }

    fn restore_session_cookie_setting(&self, cookie: Option<&str>) {
        if let Some(cookie) = cookie {
            self.db.set_setting("session_cookie", cookie);
        } else {
            self.db.delete_setting("session_cookie");
        }
    }

    fn persist_visitor_data(&self, visitor_data: Option<&str>) {
        if let Some(visitor_data) = visitor_data {
            self.it.set_visitor_data(Some(visitor_data.to_owned()));
            self.db.set_setting("visitor_data", visitor_data);
        }
    }

    /// Sign out: drop to guest. The saved row stays, so the account is still listed and one click
    /// from coming back. Deleting the credentials is `remove_google_account`, which the menu offers
    /// as its own action behind a confirm.
    pub async fn sign_out(&self) {
        let _turn = self.auth.lock().await;
        self.clear_session();
    }

    /// Saved Google accounts for the account menu. Display fields only: cookies, delegated ids and
    /// visitorData never cross the Tauri boundary (context/15).
    pub fn google_accounts(&self) -> Vec<serde_json::Value> {
        let active = self.db.get_setting("active_account");
        self.db
            .list_accounts()
            .into_iter()
            .map(|account| {
                let info = account
                    .account_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                serde_json::json!({
                    "id": account.id,
                    "name": info.get("name").cloned(),
                    "handle": info.get("handle").cloned(),
                    "email": info.get("email").cloned(),
                    "thumbnail": info.get("thumbnail").cloned(),
                    "active": active.as_deref() == Some(account.id.as_str()),
                })
            })
            .collect()
    }

    /// Activate a saved Google account without a webview round trip. The stored cookie is
    /// validated with `account_menu` first: Google rotates its short-lived tokens, and activating
    /// a dead session would leave the app half-logged-in. On success the transport, the
    /// projections and the UI all move together; on failure the previous session is left untouched.
    pub async fn switch_google_account(&self, id: &str) -> Result<serde_json::Value, String> {
        // The stored cookie being stale is the expected failure here, and the caller rolls back
        // on it. Waiting on a heal would only delay that by up to `HEAL_TIMEOUT`.
        innertube::without_healing(self.switch_google_account_inner(id)).await
    }

    async fn switch_google_account_inner(&self, id: &str) -> Result<serde_json::Value, String> {
        let _turn = self.auth.lock().await;
        let account = self.db.get_account(id).ok_or("That account is no longer saved.")?;
        let previous_cookie = self.it.cookie();
        let previous_data_sync_id = self.it.data_sync_id();
        let previous_visitor = self.it.visitor_data();

        self.it.set_cookie(Some(account.session_cookie.clone()));
        self.it.set_data_sync_id(account.data_sync_id.clone());
        self.it.set_visitor_data(account.visitor_data.clone());

        // An account saved mid multi-channel sign-in (cookie authenticated, no channel picked
        // yet) returns to the required picker instead of silently acting as YouTube's default.
        if account.selected_identity_json.is_none() {
            // Same rollback as the `account_menu` failure below: the transport already carries the
            // selected account, so returning without undoing it would leave the app requesting as
            // one account while the database and the UI still describe the other.
            if let Err(e) = self.db.set_pending_auth_selection(&account.session_cookie, Some(id)) {
                self.restore_auth_transport(previous_cookie, previous_data_sync_id);
                self.it.set_visitor_data(previous_visitor);
                return Err(format!("Couldn't activate the account: {e}"));
            }
            self.persist_visitor_data(account.visitor_data.as_deref());
            self.forget_playlist_index();
            let snapshot = self.account_snapshot();
            let _ = self.host.emit("account-selection-required", ());
            let _ = self.host.emit("auth-changed", &snapshot);
            return Ok(snapshot);
        }

        let client =
            self.clients.get(innertube::METADATA_CLIENT).ok_or("metadata client missing")?;
        let outcome = match self.it.account_menu(client).await {
            Ok(info) if info.name.is_some() => Ok(info.visitor_data),
            Ok(_) => Err("That account's session has expired. Sign in with it again.".to_owned()),
            Err(error) => Err(format!("Couldn't switch accounts: {error}")),
        };
        let refreshed_visitor = match outcome {
            Ok(visitor_data) => visitor_data,
            Err(message) => {
                self.restore_auth_transport(previous_cookie, previous_data_sync_id);
                self.it.set_visitor_data(previous_visitor);
                return Err(message);
            }
        };

        self.db.restore_account(&account).map_err(|e| format!("Couldn't switch accounts: {e}"))?;
        // `account_menu` hands back a login-bound visitorData, and it may already have rotated the
        // jar out from under the row `restore_account` just wrote back. Re-sync both from what the
        // transport actually holds now.
        self.persist_visitor_data(refreshed_visitor.as_deref());
        self.sync_active_account();
        self.forget_playlist_index();
        let snapshot = self.account_snapshot();
        let _ = self.host.emit("auth-changed", &snapshot);
        Ok(snapshot)
    }

    /// Remove a saved Google account from the menu. Removing the active one signs out too.
    pub async fn remove_google_account(&self, id: &str) {
        let _turn = self.auth.lock().await;
        self.forget_account(id);
    }

    /// Delete a saved row, dropping to guest when it is the one driving requests. Active-ness
    /// comes from the live session cookie first, with `active_account` as the fallback for an
    /// already signed-out transport, so removing an account can never leave the transport holding
    /// its credentials.
    fn forget_account(&self, id: &str) {
        let live = self.it.cookie().and_then(|cookie| crate::db::account_key(&cookie));
        let active = live.as_deref() == Some(id)
            || self.db.get_setting("active_account").as_deref() == Some(id);
        self.db.remove_account(id);
        if active {
            self.clear_session();
        }
    }

    /// Drop the live session back to guest, leaving the saved rows alone.
    fn clear_session(&self) {
        self.it.set_cookie(None);
        self.it.set_data_sync_id(None);
        self.db.delete_setting("session_cookie");
        self.db.delete_setting("active_account");
        self.forget_playlist_index();
        let _ = self.db.clear_auth_identity();
        let _ = self.host.emit("auth-changed", serde_json::json!({ "signedIn": false }));
    }

    /// Mirror the live session + identity projections into the `accounts` table. Called at the end
    /// of every persist path (single-channel sign-in, channel pick, pending multi-channel sign-in,
    /// account switch), so the saved row for the active session is always current. Best-effort: a
    /// failure leaves the projections consistent with the transport and the next persist retries.
    fn sync_active_account(&self) {
        let Some(cookie) = self.it.cookie() else { return };
        let Some(id) = crate::db::account_key(&cookie) else { return };
        let account = StoredAccount {
            id,
            session_cookie: cookie.clone(),
            data_sync_id: self.db.get_setting("data_sync_id").filter(|id| !id.is_empty()),
            selected_identity_json: self.db.get_setting("selected_identity_json"),
            account_json: self.db.get_setting("account_json"),
            visitor_data: self.db.get_setting("visitor_data"),
            added_at: now_secs(),
        };
        if let Err(error) = self.db.upsert_account(&account) {
            tracing::warn!(%error, "could not persist the signed-in account row");
            return;
        }
        self.db.set_setting("active_account", &account.id);
        // The startup bootstrap reads this projection; keep it in step with the jar.
        self.db.set_setting("session_cookie", &cookie);
    }

    /// Write a jar the transport rotated (`cookie_changed`, #165 / KI-2) back where the next
    /// launch and the next account switch read it: the `session_cookie` projection and the
    /// account's own row. Keyed on the jar's own SAPISID, so a switch racing this write cannot
    /// file one account's cookies under another account's row.
    pub fn persist_rotated_cookie(&self) {
        let Some(cookie) = self.it.cookie() else { return };
        let Some(id) = crate::db::account_key(&cookie) else { return };
        self.db.set_setting("session_cookie", &cookie);
        self.db.update_account_cookie(&id, &cookie);
    }

    /// Keep-alive ping: rolls the session tokens and returns a fresh login-bound visitorData.
    /// The transport's own rotation only fires on requests the app makes, so an idle night leaves
    /// the jar to go stale, and `refresh_session` can only re-mint whichever account the login
    /// webview happens to hold. This is what keeps a switched-to account alive.
    ///
    /// Gated on the live cookie still being the active account, and re-checked after the await so
    /// a switch landing mid-request cannot be overwritten.
    pub async fn keep_session_alive(&self) {
        let Some(cookie) = self.it.cookie() else { return };
        let Some(active) = self.db.get_setting("active_account") else { return };
        if crate::db::account_key(&cookie).as_deref() != Some(active.as_str()) {
            return;
        }
        let Some(client) = self.clients.get(innertube::METADATA_CLIENT) else { return };
        match self.it.account_menu(client).await {
            Ok(info) if info.name.is_some() => {
                let live = self.it.cookie().and_then(|c| crate::db::account_key(&c));
                if live.as_deref() != Some(active.as_str())
                    || self.db.get_setting("active_account").as_deref() != Some(active.as_str())
                {
                    return;
                }
                self.persist_visitor_data(info.visitor_data.as_deref());
                self.sync_active_account();
                tracing::debug!("session keep-alive successful");
            }
            Ok(_) => tracing::warn!("session keep-alive: the session did not authenticate"),
            Err(error) => tracing::warn!(%error, "session keep-alive failed"),
        }
    }

    /// Current account for the UI. New installs derive it from the canonical selected identity;
    /// legacy `account_json` remains a read fallback so existing databases migrate without a wipe.
    pub fn account_snapshot(&self) -> serde_json::Value {
        if auth_selection_pending(&self.db) && self.it.is_logged_in() {
            return serde_json::json!({
                "signedIn": true,
                "selectionRequired": true,
                "canSwitch": true,
            });
        }
        if let Some(selected) = selected_identity_from_db(&self.db) {
            return selected.account_json(self.it.is_logged_in());
        }
        let mut v = self
            .db
            .get_setting("account_json")
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        v["signedIn"] = serde_json::json!(self.it.is_logged_in());
        v
    }

    /// `is_upload` comes off the queue row, never off `/player`: the response that has to be
    /// handled here (LOGIN_REQUIRED) carries no `videoDetails` to read it from. Issue #71.
    async fn resolve(
        &self,
        video_id: &str,
        is_upload: bool,
        duration_secs: i64,
    ) -> Result<PlaybackData, ResolveError> {
        // ── stream: downloads ── a downloaded track plays its file: no resolve, no network.
        if let Some(data) = crate::downloads::playback_data(video_id) {
            return Ok(data);
        }
        // ── end stream: downloads ──
        // ── stream: polish ── an RSS episode streams its ad-free YouTube twin when one is found.
        let twin = crate::polish::podmatch::twin_for(self, video_id, duration_secs).await;
        let video_id = twin.as_deref().unwrap_or(video_id);
        // ── end stream: polish ──
        // ── stream: podcasts ── an RSS episode's enclosure goes to mpv as is.
        if let Some(data) = crate::podcasts::playback_data(video_id) {
            return Ok(data);
        }
        // ── end stream: podcasts ──
        // A local file is its own "stream": no network, no cache, no extraction (local.rs).
        if let Some(path) = crate::local::song_path(video_id) {
            return crate::local::playback_data(video_id, path).map_err(|_| {
                // Gone since the last scan. Forget it here rather than at the next scan, and say
                // so — the UI drops the row (and any Shortcuts tile) on the spot instead of
                // leaving something that can only fail again.
                let removed = crate::local::forget_missing(&self.db, path);
                let _ = self.host.emit("local-changed", serde_json::json!({ "removed": removed }));
                ResolveError::LocalMissing(path.to_owned())
            });
        }
        // Latency cache first (context/11) — honor expiry, never a source of truth.
        //
        // The URL has to outlive the *track*, not just the load. googlevideo keeps serving a
        // response it already started past `expire`, so a stale URL plays happily from the top;
        // but every seek past the demuxer cache is a *new* request, and that one gets a 403. A
        // flat 60s margin is plenty for a three-minute song and useless for an hour-long mix,
        // which is why seeking in long videos died while short ones were fine. Issue #188.
        // The cost of asking for more life than a row has is one resolve.
        // ── stream: firstaudio ── a resolve of this track already in flight (a hint, the
        // lookahead) fills the cache below; wait for it rather than racing it.
        let _gate = crate::firstaudio::resolve_gate(video_id).await;
        let now = now_secs();
        if let Some(c) = self.db.get_stream(video_id, cache_horizon(now, duration_secs)) {
            tracing::debug!(video_id, "stream url cache hit");
            // Cached URL carries no fresh metadata; the UI already has it from the queue item.
            //
            // The URL was validated under one client's User-Agent, so the replay sends the same
            // one. An empty map (what this used to return) sent the audio proxy upstream as
            // reqwest's default, and left mpv on the previous track's UA. `is_upload` is false by
            // construction: upload URLs are never cached (see the write below).
            let headers = match c.client.as_deref() {
                Some(k) => self.orchestrator.headers_for(k, false),
                None => Default::default(),
            };
            return Ok(PlaybackData {
                video_id: video_id.to_owned(),
                stream_url: c.url,
                itag: c.itag,
                headers,
                expires_in_seconds: c.expires_at - now,
                loudness_db: c.loudness_db,
                // Cached alongside the URL: a hit skips `/player`, so without it a replay never
                // registered in watch history — and neither did a first listen whose stream the
                // gapless lookahead had already cached (issue #83). `None` on rows written before
                // the column, and on whatever YouTube declines a stale `ei` for; the ping is
                // best-effort either way.
                playback_ping: c
                    .ping_url
                    .zip(c.ping_client)
                    .map(|(url, client)| PlaybackPing { url, client }),
                title: None,
                artists: None,
                duration: None,
                thumbnail: None,
                // Cached alongside the URL: a hit skips `/player`, so without it a replay inside
                // the cache window (hours, and every track you just listened to) would fall back
                // to the queue row's flag, which is exactly the thing that can't be trusted.
                is_video: c.is_video,
                is_episode: c.is_episode,
                live: false, // never written to the cache, see below
                stream_client: c.client.unwrap_or_else(|| "cache".to_owned()),
            });
        }
        // ── stream: firstaudio ── this stream is about to play: warm its head while validating.
        let resolved = crate::audioproxy::WANT_HEAD
            .scope(
                true,
                self.orchestrator.resolve(video_id, is_upload, self.quality(), &self.disabled_clients()),
            )
            .await;
        // A refusal holds the hints and the lookahead back; a working resolve lets them go.
        crate::firstaudio::note_resolve(resolved.as_ref().err().map(ResolveError::is_transient));
        let data = resolved?;
        // Never cache rustypipe URLs: googlevideo serves them only for bounded-Range requests,
        // which mpv doesn't send → LOADING_FAILED(-13). Caching one poisons the videoId for ~6h.
        //
        // Nor upload URLs: they expire in minutes (32 seconds without the MLPT context), and the
        // read above only accepts a row with 60s of life left, so the write is dead on arrival.
        //
        // Nor a live playlist (#424): the broadcast it describes can end and the recording take
        // its videoId within the cache window, and a replay then needs a real `/player` answer.
        if data.stream_client != "rustypipe" && !is_upload && !data.live {
            self.db.put_stream(
                video_id,
                &crate::db::CachedStream {
                    url: data.stream_url.clone(),
                    itag: data.itag,
                    expires_at: now + data.expires_in_seconds.max(0),
                    loudness_db: data.loudness_db,
                    is_video: data.is_video,
                    is_episode: data.is_episode,
                    ping_url: data.playback_ping.as_ref().map(|p| p.url.clone()),
                    ping_client: data.playback_ping.as_ref().map(|p| p.client.clone()),
                    client: Some(data.stream_client.clone()),
                },
                now,
            );
        }
        Ok(data)
    }

    // ── stream: firstaudio ──
    /// A hint that `item` is about to be played (its row hovered): resolve it now, at
    /// background priority, and warm the stream's first bytes, so the click starts from the cached
    /// path. Rationed by [`crate::firstaudio::Hints`]; anything that would not resolve over the
    /// network anyway (local, podcast, downloaded, uploads, offline) is left alone.
    pub async fn hint_play(self: &std::sync::Arc<Self>, item: SongItem) {
        let id = item.video_id.as_str();
        if item.is_upload
            || crate::local::is_local_song(id)
            // An RSS episode plays its enclosure as is; a YouTube episode resolves like a song.
            || crate::podcasts::is_rss_id(id)
            || crate::downloads::is_downloaded(id)
            || crate::downloads::offline_blocks(id)
            || crate::polish::explicit::blocks(&item)
        {
            return;
        }
        if !crate::firstaudio::HINTS.lock().map(|mut h| h.admit(id, std::time::Instant::now())).unwrap_or(false) {
            return;
        }
        let secs = parse_duration_ms(item.duration.as_deref()) / 1000;
        let cached = self.db.get_stream(id, cache_horizon(now_secs(), secs)).is_some();
        // ── stream: reliability ── the speculative lane: ahead of background work, behind clicks.
        let resolved = innertube::speculative(self.resolve(id, false, secs)).await;
        if let Ok(data) = &resolved {
            if !data.live {
                crate::audioproxy::warm(&data.stream_url, &data.headers);
            }
        }
        tracing::debug!(video_id = id, cached, ok = resolved.is_ok(), "play hint");
        if let Ok(mut h) = crate::firstaudio::HINTS.lock() {
            h.done(!cached, std::time::Instant::now());
        }
    }
    // ── end stream: firstaudio ──

    /// A single pick (a search result, a Home or recommendation card, a song link): play that
    /// song only, then hand over to Autoplay (stream: autoplay, `crate::autoplay`). The list the
    /// song was picked from never follows it. `from` is what the pick came from for the
    /// "Playing from" line (the search query); there is no "Next from" context.
    ///
    /// Autoplay's first batch is fetched right away (skipped when the setting is off): the seed's
    /// radio, run on its studio recording, filtered and taste-reranked, every row marked
    /// `autoplay` so the queue panel draws it under "Next: Autoplay" and the switch can take it
    /// away again.
    pub async fn play_song(self: &std::sync::Arc<Self>, seed: SongItem) {
        self.play_single(seed, None).await
    }

    /// [`Self::play_song`] with the "Playing from" label.
    pub async fn play_single(self: &std::sync::Arc<Self>, seed: SongItem, from: Option<String>) {
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let video_id = seed.video_id.clone();
        crate::firstaudio::start(&video_id); // stream: firstaudio
        let canon_seed = seed.clone(); // ── stream: canonical ──
        let sticky = self.sticky_shuffle();

        // A local file isn't a videoId YouTube has ever heard of: it has no radio, and asking for
        // one offline (where local music earns its keep) is a guaranteed-failing request.
        // Autoplay off means the song plays and the queue ends there (#238).
        let no_radio = crate::local::is_local_song(&video_id)
            || crate::podcasts::is_episode_item(&seed) // ── stream: podcasts ──
            || !self.autoplay_enabled();
        let position = self.current_position();

        {
            let mut q = self.queue.lock().await;
            // Unplayed manual adds survive a context switch (Spotify semantics): they follow the
            // pick, ahead of autoplay. Both of these read what `keep_context` is about to take.
            let carried = upcoming_queued(&q.items, q.current);
            let shuffled = q.shuffle_orig.is_some();
            q.keep_context(position);
            install_single(&mut q, seed, carried, from);
            q.epoch += 1;
            q.lookahead_loaded = None;
            // Hold off the autoplay early trigger `start_current` is about to spawn: this queue is
            // one track long, so it would fetch the very radio being hydrated below (#255).
            q.hydrating = (!no_radio).then_some(gen);
            // Shuffle carries into the new queue only when it's sticky; otherwise a new context
            // starts unshuffled. (Autoplay rows are never shuffled into the pick.)
            q.shuffle_orig = (sticky && shuffled).then(|| q.items.clone());
        }

        if !self.start_current(gen).await {
            self.end_hydration(gen).await;
            return;
        }

        if no_radio {
            self.prime_lookahead(gen).await;
            return;
        }

        // ── stream: autoplay ── the radio runs on the studio song: a music video's audio track,
        // a live or sped-up cut's studio recording (`canonical::studio_seed`).
        let studio = crate::canonical::studio_seed(self, &canon_seed).await;
        let anchor = studio.clone().unwrap_or_else(|| canon_seed.clone());
        let radio_on = anchor.video_id.clone();
        let radio_id = format!("RDAMVM{radio_on}");
        let hydrated = self.fetch_radio(Some(&radio_on), &radio_id).await;
        self.end_hydration(gen).await; // autoplay may top this queue up from here on
        let added = match hydrated {
            Ok((mut page, seed_id)) => {
                // Search and home cards carry no album name; the radio's own row for the same
                // track does (#309). Nor its video twin, signed in (#442).
                self.adopt_seed_row(gen, &video_id, &page).await;
                page.retain(|i| i.video_id != video_id);
                tracing::info!(seed = %video_id, on = %radio_on, rows = page.len(), "autoplay: single pick");
                self.absorb_radio(page, gen, &seed_id, SINGLE_BATCH, &[anchor, canon_seed]).await
            }
            Err(e) => {
                tracing::warn!(error = %e, "autoplay: radio for the pick failed");
                0
            }
        };
        if added == 0 {
            self.prime_lookahead(gen).await;
        }
    }

    /// The radio page's row for the playing pick lends it what a search or home card lacks: the
    /// album name and the video twin.
    async fn adopt_seed_row(&self, gen: u64, video_id: &str, page: &[SongItem]) {
        let Some(item) = page.iter().find(|i| i.video_id == video_id) else { return };
        let mut q = self.queue.lock().await;
        if self.generation.load(Ordering::SeqCst) != gen {
            return;
        }
        let cur = q.current;
        let Some(have) = q.items.get_mut(cur).filter(|i| i.video_id == video_id) else { return };
        let had_album = have.album.is_some();
        adopt_twin(have, item);
        have.album = have.album.take().or(item.album.clone());
        // The seed is already playing and Last.fm, Discord and the OS media widget were told it
        // has no album. Sent under the queue lock, so it can't overtake the next track's
        // `set_track` / `set_metadata`.
        if let (false, Some(album), Some(m)) = (had_album, have.album.as_deref(), &self.media) {
            m.set_album(album);
        }
    }

    /// Play a finite list of tracks (a playlist/album). `start` is the clicked track; `None`
    /// means "just play the playlist" (the header Play button) — first track, or any track when
    /// shuffle is on. Unlike `play_song` this seeds NO radio — the given items *are* the queue
    /// (context/08: playlist playback). `source_id` (the page's playlist/album playlist id) makes
    /// autoplay continue with that context's radio when the queue runs out. `source_name` is the
    /// page title for the queue panel's "Next from" header; `shuffle` (the page Shuffle buttons)
    /// turns shuffle ON for this queue — the backend owns the randomization, so un-shuffle can
    /// restore the true order and every re-shuffle is fresh.
    ///
    /// `continuation` is the playlist page's next-page token, if it has one: the queue is seeded
    /// from the tracks the page had loaded and the rest is walked in the background
    /// ([`Self::fill_playlist`]) so a long playlist starts playing now instead of after ~50
    /// chained round trips.
    #[allow(clippy::too_many_arguments)]
    pub async fn play_tracks(
        self: &std::sync::Arc<Self>,
        items: Vec<SongItem>,
        start: Option<usize>,
        source_id: Option<String>,
        source_name: Option<String>,
        shuffle: bool,
        continuation: Option<String>,
    ) {
        if items.is_empty() {
            return;
        }
        // A mix has no "rest of the playlist" worth walking (see `is_mix`) — drop the token.
        let continuation = continuation.filter(|_| !is_mix(source_id.as_deref()));
        let sticky = self.sticky_shuffle();
        let position = self.current_position();
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let epoch = {
            let mut q = self.queue.lock().await;
            // Explicitly requested by a page Shuffle button, or already on and set to stick
            // across queues (issue #117: off by default, so an album opened while shuffle is on
            // plays in order).
            let keep_shuffled = shuffle || (sticky && q.shuffle_orig.is_some());
            // ── stream: podcast-upnext ── an episode's queue is in listening order, never shuffled.
            let keep_shuffled = keep_shuffled
                && !start.and_then(|i| items.get(i)).is_some_and(crate::podcasts::is_episode_item);
            let start = match start {
                Some(i) => i.min(items.len() - 1),
                None if keep_shuffled => {
                    rand::Rng::gen_range(&mut rand::thread_rng(), 0..items.len())
                }
                None => 0,
            };
            // Unplayed manual adds survive a context switch (Spotify semantics) — spliced back in
            // right after the new current track, ahead of the new context.
            let carried = upcoming_queued(&q.items, q.current);
            // After `keep_shuffled` and `carried`, both of which read what this takes.
            q.keep_context(position);
            q.items = items;
            q.epoch += 1;
            q.current = start;
            q.lookahead_loaded = None;
            q.source_id = source_id.clone();
            q.radio_seed = radio_seed_for(source_id);
            q.source_name = source_name;
            q.radio = false; // a chosen playlist/album; `start_radio` sets it back on for its own
            q.hydrating = None; // a `play_song` radio still in flight is for a queue that's gone
            let whole = self.shuffle_whole_queue();
            arrange_new_context(&mut q, keep_shuffled, whole, carried);
            q.epoch
        };
        // start_current emits now-playing + queue + persists; prime the gapless lookahead after.
        if self.start_current(gen).await {
            self.prime_lookahead(gen).await;
        }
        if let Some(token) = continuation {
            let me = self.clone();
            tokio::spawn(async move { me.fill_playlist(epoch, gen, token, Fill::Playing).await });
        }
    }

    /// Start a radio (context/08): an endless YouTube-generated queue seeded on a song, artist,
    /// album or playlist. `kind` is one of `song` / `artist` / `album` / `playlist` and `id` the
    /// matching videoId or browseId; `name` is what the queue header calls it.
    ///
    /// A radio *is* a playlist id — the whole feature is a prefix convention plus one `/next`
    /// call. Songs and playlists build theirs client-side (`RDAMVM…` / `RDAMPL…`); artists can't,
    /// so theirs comes off the artist page (`radio_playlist_id`). Once started it continues
    /// through the same autoplay path as any other queue ([`Self::extend_queue_radio`]), which is
    /// why this only has to install a first page and a seed.
    pub async fn start_radio(
        self: &std::sync::Arc<Self>,
        kind: &str,
        id: &str,
        name: Option<String>,
        start: Option<f64>,
    ) -> Result<(), String> {
        // Guarded here rather than in each menu: everything without a YouTube item behind it
        // (files on disk, the locally-built Your Rotation) would otherwise reach `/next` as an id
        // YouTube has never heard of.
        if crate::local::is_local_song(id)
            || crate::podcasts::is_rss_id(id) // ── stream: podcasts ──
            || id.starts_with(crate::local::ALBUM_PREFIX)
            || id.starts_with(crate::local::ARTIST_PREFIX)
            || id == ON_REPEAT_ID
            || is_local_playlist(id)
        {
            return Err("This has no radio behind it.".into());
        }
        let client = self.clients.get(innertube::METADATA_CLIENT).ok_or("no metadata client")?;
        // Resolve the seed to (videoId?, radio playlist id). Album and artist need a page fetch:
        // an album's radio keys off its audio playlist, not its `MPRE…` browseId, and an artist
        // radio id is server-supplied.
        let (video_id, playlist_id) = match kind {
            "song" => (Some(id.to_owned()), format!("RDAMVM{id}")),
            "playlist" => (None, radio_seed_for(Some(id.to_owned())).ok_or("This playlist has no radio.")?),
            "album" => {
                let page = self.it.album(client, id).await.map_err(|e| e.to_string())?;
                let pl = page.playlist_id.ok_or("This album has no radio.")?;
                (None, radio_seed_for(Some(pl)).ok_or("This album has no radio.")?)
            }
            "artist" => {
                let page = self.it.artist(client, id).await.map_err(|e| e.to_string())?;
                match page.radio_playlist_id {
                    Some(pl) => (None, pl),
                    // No radio button on the header: fall back to a radio on this artist's most
                    // played track, which is roughly what that button seeds anyway.
                    None => {
                        let top = page.top_songs.first().ok_or("This artist has no radio.")?;
                        (Some(top.video_id.clone()), format!("RDAMVM{}", top.video_id))
                    }
                }
            }
            other => return Err(format!("unknown radio kind: {other}")),
        };

        let (mut items, mut seed) = self.fetch_radio(video_id.as_deref(), &playlist_id).await?;
        // ── stream: canonical ── a radio on a music video runs on its song: YouTube builds a
        // video's radio out of videos. Unless that video is playing, the radio opens on the song
        // (the video stays its Song/Video twin). Then cache-only cleanup; the rest maps lazily.
        let asked_id = video_id.clone();
        let mut video_id = video_id;
        let seed_row = asked_id.as_ref().and_then(|v| items.iter().find(|i| &i.video_id == v).cloned());
        if let Some(row) = seed_row.as_ref() {
            if let Some(atv) = crate::canonical::audio_seed(self, row).await {
                if let Ok((page, s)) = self.fetch_radio(Some(&atv), &format!("RDAMVM{atv}")).await {
                    items = page;
                    seed = s;
                    if let Some(open) = items.iter_mut().find(|i| i.video_id == atv) {
                        open.counterpart.get_or_insert_with(|| row.video_id.clone());
                        if open.counterpart_thumbnail.is_none() {
                            open.counterpart_thumbnail = row.thumbnail.clone();
                        }
                    }
                    video_id = Some(atv);
                }
            }
        }
        let canon_seed = video_id.as_ref().and_then(|v| items.iter().find(|i| &i.video_id == v).cloned());
        let canon_pending = crate::canonical::prepare_db(&self.db, &mut items, canon_seed.as_ref());
        // ── end stream: canonical ──
        // ── stream: smart ── never a blocked/explicit-filtered song (the seed itself stays).
        crate::smart::recs::filter_radio(&self.db, &mut items, video_id.as_deref()).await;
        // The seed song comes back inside the first page (usually first, but the panel decides):
        // start there so a radio on song X actually opens on X. Only what follows it is reordered,
        // and a radio with no seed song (artist, album, playlist) is reordered whole.
        let seed_at = video_id.as_ref().and_then(|v| items.iter().position(|i| &i.video_id == v));
        self.freshen_radio(&mut items[seed_at.map_or(0, |i| i + 1)..]);
        let title = radio_title(name.as_deref(), seed_at.and_then(|i| items.get(i)).map(|s| s.title.as_str()));

        // Radio on the song that's already playing: splice instead of replacing, so the track
        // keeps playing without a re-buffer (Metrolist's `startRadioSeamlessly`).
        let playing_seed = {
            let q = self.queue.lock().await;
            asked_id.is_some() && q.items.get(q.current).map(|i| &i.video_id) == asked_id.as_ref()
        };
        // A shared link's `t=`: only a song radio has a seed track to start partway into.
        let start = start.filter(|s| kind == "song" && s.is_finite() && *s > 0.0);
        // ── stream: canonical ── a video's `t=` is not a moment of its song.
        let start = start.filter(|_| video_id == asked_id || playing_seed);
        if playing_seed && !self.player.is_idle() {
            // ── stream: canonical ── the playing video stays; its song doesn't follow it.
            if video_id != asked_id {
                items.retain(|i| Some(&i.video_id) != video_id.as_ref());
            }
            self.splice_radio(items, seed, title).await;
            self.canonical_spawn(canon_pending); // ── stream: canonical ──
            if let Some(pos) = start {
                self.user_seek(pos).await?;
            }
            return Ok(());
        }
        // Handed to `loadfile` as `start=` (see `start_current`); mpv drops a seek sent before the
        // file is loaded. Keyed on the seed's id, so it can't land on a different track.
        // `latest_position` is left alone on purpose: `play_tracks` below reads it as the position
        // the replaced queue is resumed at (`keep_context`), and that is not the link's offset.
        if let (Some(pos), Some(vid), Some(_)) = (start, video_id.as_ref(), seed_at) {
            *self.pending_seek.lock().unwrap() = Some((vid.clone(), pos));
        }
        self.play_tracks(items, Some(seed_at.unwrap_or(0)), Some(seed), title, false, None).await;
        self.queue.lock().await.radio = true;
        self.canonical_spawn(canon_pending); // ── stream: canonical ──
        Ok(())
    }

    /// Fetch a radio's first page, escalating when YouTube hands back a dead one. Returns the
    /// tracks plus the playlist id that actually produced them (autoplay's seed, which is not
    /// necessarily the one asked for).
    ///
    /// A `RDAMVM…` radio for an obscure or region-locked track routinely answers with the seed
    /// song and nothing else, and "start radio" then looks like it did nothing. So: ask the song
    /// what mix it belongs to (`automixPreviewVideoRenderer`) and take that instead.
    ///
    /// ponytail: two rungs, not Metrolist's three — the third scrapes the Related tab, which needs
    /// a whole endpoint + an ATV-only filter to salvage the cases these two already miss. Add it
    /// if songs turn up that reach here and still come back empty.
    pub(crate) async fn fetch_radio(
        &self,
        video_id: Option<&str>,
        playlist_id: &str,
    ) -> Result<(Vec<SongItem>, String), String> {
        const NO_RADIO: &str = "YouTube has no radio for this.";
        let client = self.clients.get(innertube::METADATA_CLIENT).ok_or("no metadata client")?;
        let first =
            self.it.next(client, video_id, Some(playlist_id)).await.map_err(|e| e.to_string())?;
        if first.items.len() > 1 {
            return Ok((first.items, playlist_id.to_owned()));
        }
        let Some(video_id) = video_id else { return Err(NO_RADIO.into()) };
        let bare = self.it.next(client, Some(video_id), None).await.map_err(|e| e.to_string())?;
        if let Some(mix) = bare.automix_playlist_id {
            let page = self
                .it
                .next(client, Some(video_id), Some(&mix))
                .await
                .map_err(|e| e.to_string())?;
            if page.items.len() > 1 {
                return Ok((page.items, mix));
            }
        }
        if bare.items.len() > 1 {
            return Ok((bare.items, format!("RDAMVM{video_id}")));
        }
        Err(NO_RADIO.into())
    }

    /// Install a radio behind the track that's already playing: history and the current song stay,
    /// everything after them becomes the radio. Manual "add to queue" items are carried rather
    /// than destroyed (same rule as a context switch in `play_tracks`) — Metrolist silently drops
    /// them, which on a desktop app is just losing the user's work.
    async fn splice_radio(
        self: &std::sync::Arc<Self>,
        items: Vec<SongItem>,
        seed: String,
        title: Option<String>,
    ) {
        {
            let mut q = self.queue.lock().await;
            if !self.sticky_shuffle() {
                q.shuffle_orig = None; // a radio is a new context, so a non-sticky shuffle ends here
            }
            splice_radio_into(&mut q, items, seed, title);
            // Whatever mpv had primed as the gapless next belongs to the old queue.
            if q.lookahead_loaded.take().is_some() {
                let _ = self.player.clear_playlist();
            }
        }
        self.emit_queue().await;
        self.persist_queue().await;
        self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
    }

    /// Walk the rest of a playlist in the background and append it to the playing queue, page by
    /// page. The alternative (what the UI used to do) is loading every page *before* starting
    /// playback: continuation tokens are chained, so that's ~50 sequential round trips on a
    /// 5000-track playlist, all of them before the first note.
    ///
    /// Shuffle stays a real shuffle. Each arriving page is mixed through the *unplayed* tail
    /// ([`append_page`]), so the only track not drawn from the full playlist is the one already
    /// playing — and the walk is long finished before it ends.
    ///
    /// Guarded by `epoch` ([`QueueState::epoch`]): if the user starts something else mid-walk, the
    /// pages are dropped rather than appended to a queue they don't belong to. Skipping around
    /// inside this queue doesn't stop it. `gen` is the load the walk started under, only for
    /// priming. [`Fill`] says which queue the pages join.
    async fn fill_playlist(
        self: &std::sync::Arc<Self>,
        epoch: u64,
        gen: u64,
        mut token: String,
        fill: Fill,
    ) {
        // ponytail: ~5k tracks at 100/page. A bound so a playlist that keeps handing out tokens
        // can't walk forever; raise it if a real playlist ever hits the cap.
        const MAX_PAGES: usize = 50;
        let Some(client) = self.clients.get(innertube::METADATA_CLIENT) else { return };
        let mut pages = 0;
        // The first page goes out at once so the panel visibly starts filling; after that the
        // walk emits at most once a second. Every page is a *full* payload (the rows changed),
        // and a 50-page walk of a 5,000-track playlist sends ~67 MB of JSON through
        // `webview.eval` if each one is announced. The final state is emitted after the loop.
        let mut last_emit: Option<std::time::Instant> = None;
        for _ in 0..MAX_PAGES {
            let page = match self.it.playlist_continuation(client, &token).await {
                Ok(page) => page,
                // A page that fails ends the walk — better a short queue than a stalled one.
                Err(e) => {
                    tracing::warn!(error = %e, "playlist fill stopped early");
                    break;
                }
            };
            if page.items.is_empty() {
                break; // an empty page is the end, token or not
            }
            let mut items = page.items;
            // The rest of an added playlist belongs to the same block: same markers, same heading,
            // and its order left alone (shuffle is about the playlist that's playing).
            if let Fill::Queued(from) = &fill {
                for item in &mut items {
                    item.queued_end = true;
                    item.queued_from = from.clone();
                }
            }
            {
                let mut q = self.queue.lock().await;
                if q.epoch != epoch {
                    return; // another queue owns the state now: don't touch it, don't persist
                }
                let before = q.items.get(q.current + 1).map(|i| i.video_id.clone());
                let whole = self.shuffle_whole_queue();
                append_page(&mut q, items, matches!(fill, Fill::Playing), whole);
                // An append can retarget a primed repeat-all wrap (index 0 → the new tail), and a
                // page landing in front of the tail adds can take the primed slot; drop the
                // lookahead when it stops pointing at what plays next, same check as
                // `insert_queued`. `append_page` leaves a still-valid slot alone, so the common
                // case re-primes to a no-op instead of re-resolving on every page.
                let expected = q.next_playable(q.repeat);
                let after = q.items.get(q.current + 1).map(|i| i.video_id.clone());
                if q.lookahead_loaded.is_some()
                    && (q.lookahead_loaded != expected || before != after)
                {
                    q.lookahead_loaded = None;
                    let _ = self.player.clear_playlist();
                }
            }
            pages += 1;
            if last_emit.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1)) {
                last_emit = Some(std::time::Instant::now());
                self.emit_queue().await;
            }
            // Only while nothing has been skipped since the walk began. A skip loads its own track
            // and primes behind it; priming while that load is still resolving hands mpv a next
            // entry for a playlist `loadfile` is about to replace.
            // ponytail: after a skip the walk stops re-priming, so a page that lands behind a
            // current track that was the last one gets no gapless handoff (the track-end fallback
            // loads it instead). Re-prime once the skip's load has settled if that gap matters.
            if self.generation.load(Ordering::SeqCst) == gen {
                self.prime_lookahead(gen).await;
            }
            match page.continuation {
                Some(next) => token = next,
                None => break,
            }
        }
        // Persist/broadcast once at the end, not per page: the queue JSON grows with every page and
        // nothing else reads it mid-walk.
        if pages > 0 {
            tracing::info!(pages, "playlist fill appended pages");
            self.emit_queue().await; // whatever the throttle above held back
            self.persist_queue().await;
        }
    }

    /// Play/pause toggle that also starts a *restored* (or exhausted) queue: if mpv has nothing
    /// loaded but the queue is non-empty, load the current track (applying any resume position);
    /// otherwise just toggle mpv. Keeps the UI's single play/pause button working after a restart.
    pub async fn resume_or_toggle(self: &std::sync::Arc<Self>) {
        if self.player.is_idle() {
            let (idx, has_items) = {
                let q = self.queue.lock().await;
                (q.current, !q.items.is_empty())
            };
            if has_items {
                self.play_index(idx).await;
                return;
            }
        }
        let _ = self.player.toggle();
    }

    /// ── stream: pod-fixes ── The next start of `item` opens `start` seconds in. Rides on
    /// `pending_seek`, handed to mpv's `loadfile` as `start=` and read by `show_loading`, so it
    /// holds while the stream is still resolving; a seek after the play would be dropped by mpv.
    /// Keyed on the row, so a shuffled or skipped start can't inherit it.
    pub(crate) fn open_at(&self, item: Option<&SongItem>, start: Option<f64>) {
        if let (Some(item), Some(pos)) = (item, start.filter(|s| s.is_finite() && *s > 0.0)) {
            *self.pending_seek.lock().unwrap() = Some((item.video_id.clone(), pos));
        }
    }

    /// Jump to a specific queue index.
    pub async fn play_index(self: &std::sync::Arc<Self>, index: usize) {
        self.play_index_at(index, None).await;
    }

    /// [`Self::play_index`], opening the track `start` seconds in (a comment's timecode). Goes
    /// through `pending_seek` like every other start offset: mpv drops a seek sent before the file
    /// is loaded, so it can't be issued after this returns.
    pub async fn play_index_at(self: &std::sync::Arc<Self>, index: usize, start: Option<f64>) {
        // ponytail: the remote's PlayIndex carries no start, so a timecode on a linked phone plays
        // from the top. Add `start` to the action if that ever matters.
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let video_id = {
            let mut q = self.queue.lock().await;
            if index >= q.items.len() {
                return;
            }
            q.seek_to(index);
            q.lookahead_loaded = None;
            q.items[index].video_id.clone()
        };
        if let Some(pos) = start.filter(|s| s.is_finite() && *s > 0.0) {
            *self.pending_seek.lock().unwrap() = Some((video_id.clone(), pos));
        }
        // The next mpv tick is seconds away because the track must be resolved first. Until then
        // `latest_position` is still the value that pause/exit persistence will write, so seed it
        // from `pending_seek` when that saved position belongs to this exact video. A different
        // track must start at zero instead of inheriting the restored track's position.
        let resume = self
            .pending_seek
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(pending_id, _)| pending_id == &video_id)
            .map_or(0.0, |(_, position)| *position);
        self.latest_position.store(resume.to_bits(), Ordering::SeqCst);
        if self.start_current(gen).await {
            self.prime_lookahead(gen).await;
        }
    }

    /// The Song/Video switch (#442): play the current track's other version, from where it is. The
    /// row trades its id for its `counterpart` and keeps the old one there, so the switch goes back
    /// the same way. The reload is the failed-track retry's (`on_track_failed`).
    pub async fn switch_version(self: &std::sync::Arc<Self>) {
        {
            let mut q = self.queue.lock().await;
            let cur = q.current;
            let pos = self.current_position();
            // `q.duration` doubles as the proof mpv is on this file, as in `on_track_failed`.
            let resume = q.duration > 0.0 && pos > 1.0 && pos < q.duration;
            let Some(item) = q.items.get_mut(cur) else { return };
            let old = item.video_id.clone();
            let pos = twin_position(pos, &item.counterpart_segments);
            if !swap_version(item) {
                return;
            }
            // A video's cut runs its own length; `backfill_metadata` reads it off `/player`.
            item.duration = None;
            if resume {
                *self.pending_seek.lock().unwrap() = Some((item.video_id.clone(), pos));
            }
            // Un-shuffling rebuilds the queue from this copy, which would bring the old one back.
            if let Some(orig) = q.shuffle_orig.as_mut() {
                if let Some(o) = orig.iter_mut().find(|i| i.video_id == old) {
                    swap_version(o);
                }
            }
            q.lookahead_loaded = None;
        }
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if self.start_current(gen).await {
            self.prime_lookahead(gen).await;
        }
    }

    /// The music video settings changed: move the queue's rows onto the version they now pick, so
    /// the next song follows the switch, not the setting its album was opened under (#464). The
    /// playing row is left alone, it has its own Song/Video switch. Also run when a playlist's
    /// twins arrive (#542). True when a row moved, the queue then emitted and persisted.
    pub async fn apply_version_preference(self: &std::sync::Arc<Self>) -> bool {
        let video = wants_video(&self.db);
        let stale_lookahead = {
            let mut q = self.queue.lock().await;
            let q = &mut *q;
            let cur = q.current;
            let primed_id = |q: &QueueState| {
                q.lookahead_loaded.and_then(|i| q.items.get(i)).map(|i| i.video_id.clone())
            };
            let primed = primed_id(q);
            let playing = q.items.get(cur).map(|i| i.video_id.clone());
            let mut changed = false;
            for (i, item) in q.items.iter_mut().enumerate() {
                changed |= i != cur && prefer_version(item, video);
            }
            // Un-shuffling rebuilds the queue from this copy, which would bring the old ones back.
            for item in q.shuffle_orig.iter_mut().flatten() {
                changed |= Some(&item.video_id) != playing.as_ref() && prefer_version(item, video);
            }
            if !changed {
                return false;
            }
            // mpv holds the old version as the gapless next.
            let stale = primed.is_some() && primed != primed_id(q);
            if stale {
                q.lookahead_loaded = None;
                let _ = self.player.clear_playlist();
            }
            stale
        };
        self.emit_queue().await;
        self.persist_queue().await;
        if stale_lookahead {
            self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
        }
        true
    }

    /// Advance the queue after a track ended (EOF) or died (load error). Don't assume mpv
    /// gaplessly transitioned — ask it: if a lookahead was primed mpv is already playing the
    /// next entry (just sync pointer + UI); if mpv went idle (lookahead absent/failed, or the
    /// track errored on a single-entry playlist) load the next track explicitly, otherwise
    /// playback silently stalls while the UI shows a phantom "now playing".
    pub async fn on_track_ended(self: &std::sync::Arc<Self>) {
        // Sleep timer "end of track": the player paused at the end. A next track loaded from here
        // must stay paused, or the queue's advance would start it and undo the timer. (A primed
        // lookahead needs nothing: mpv already moved into it paused.)
        let hold = self.player.take_held_at_end();
        let (has_next, primed) = {
            let mut q = self.queue.lock().await;
            match q.next_playable(q.repeat) {
                Some(next) => {
                    let primed = q.lookahead_loaded == Some(next);
                    q.seek_to(next);
                    (true, primed)
                }
                None => (false, false),
            }
        };
        if !has_next {
            // Try autoplay before declaring the queue dead (the early trigger usually already
            // extended it; this is the fallback when that didn't land in time). Off the pump:
            // spawn, and let the task either continue playback or do the pause bookkeeping —
            // pausing here and un-pausing a second later would flicker every consumer (UI,
            // MPRIS, Discord).
            let me = self.clone();
            crate::rt::spawn(async move {
                let gen = me.generation.fetch_add(1, Ordering::SeqCst) + 1;
                if me.extend_queue_radio(gen).await > 0 {
                    {
                        let mut q = me.queue.lock().await;
                        let next = q.current + 1; // the first appended track
                        q.seek_to(next);
                        q.lookahead_loaded = None; // start_current's loadfile replaces mpv's playlist
                    }
                    me.hold_next_start.store(hold, Ordering::SeqCst);
                    if me.start_current(gen).await {
                        me.prime_lookahead(gen).await;
                    }
                } else {
                    tracing::info!("queue exhausted");
                    me.emit_own("playback-state", "paused");
                    // mpv goes idle without flipping its pause flag, so no Paused event will fire
                    // — tell the OS widget + Discord ourselves or they show "playing" forever
                    // past the last song.
                    me.media_set_playing(false);
                }
            });
            return;
        }
        if !primed || self.player.is_idle() {
            // No gapless handoff happened. `is_idle` alone is not enough: right at EOF mpv can
            // still report not-idle before it settles, and with nothing primed that would send us
            // into the gapless branch below — a phantom "now playing" over a silent player. If we
            // never primed this index, mpv cannot have advanced into it; load explicitly. Bump the
            // generation so any in-flight lookahead resolve discards itself (double-enqueue).
            let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            tracing::info!("no primed lookahead at track end — loading next explicitly");
            // start_current's loadfile *replaces* mpv's playlist, so whatever was recorded here is
            // gone either way — clear it like every other load path (play_index, on_track_failed,
            // play_tracks) does. Left stale, the re-prime below can no-op as "already primed"
            // against an entry mpv no longer holds, and the next track end finds nothing to
            // advance into.
            self.queue.lock().await.lookahead_loaded = None;
            self.hold_next_start.store(hold, Ordering::SeqCst);
            if self.start_current(gen).await {
                self.prime_lookahead(gen).await;
            }
            return;
        }
        // mpv already advanced into the primed lookahead. Sync pointer + UI, prime the next.
        let gen = self.generation.load(Ordering::SeqCst);
        {
            let mut q = self.queue.lock().await;
            // `af` is global in mpv and the appended entry couldn't carry its own, so the new track
            // is playing at the *previous* one's gain until this lands.
            if let Some(loudness) = q.lookahead_loudness_db.take() {
                q.current_loudness_db = loudness;
                if let Err(e) = self.player.set_gain(self.track_gain(loudness)) {
                    tracing::warn!(error = %e, "applying lookahead loudness gain failed");
                }
            }
            // The primed entry is what's playing now — nothing is primed beyond it. (Also what
            // lets a single-item repeat-all queue re-prime itself instead of "already primed".)
            q.lookahead_loaded = None;
            q.current_client = q.lookahead_client.take();
            // New track is now playing → fresh history state (mirrors start_current).
            q.playback_ping = q.lookahead_playback_ping.take();
            q.cpn = innertube::generate_cpn();
            self.history_pinged.store(false, Ordering::Relaxed);
            self.end_listen();
            q.duration = 0.0;
            q.live = false;
        }
        if let Some(item) = self.current_item().await {
            self.apply_item_effects(&item).await; // speeds.rs: an episode into a song, or back
            self.emit_now_playing(&item, "gapless", false);
            if hold {
                // The sleep timer stopped at the boundary: mpv sits paused at the next track's
                // start, so the UI and the OS widget must say paused, not "playing".
                self.emit_own("playback-state", "paused");
                self.media_set_playing(false);
            }
            // Same as `start_current`: an autoplay-appended track carries no rating of its own.
            self.refresh_from_next(&item.video_id, gen);
        }
        self.emit_queue().await;
        self.persist_queue().await; // index advanced without an explicit load → persist it
        tracing::info!("advanced to next track (gapless)");
        // Prime off the pump, not on it. `prime_lookahead` resolves the next stream over the
        // network, and this fn is awaited by the mpv event pump — blocking here stops mpv's events
        // being drained for the length of a round-trip, which delays the new track's `duration`
        // (its progress bar) and, worse, the *next* track-end. The generation guard inside already
        // makes a superseded resolve discard itself, so it's safe to detach.
        let me = self.clone();
        crate::rt::spawn(async move {
            me.prime_lookahead(gen).await;
            me.extend_queue_radio(gen).await; // autoplay early trigger (no-op unless tail is near)
        });
    }

    /// Which client's URL the playing track was resolved from ("WEB_REMIX", "VISIONOS", ...).
    /// Names the culprit in a playback-error toast, which on Windows is the only diagnostic a
    /// reporter can actually produce.
    pub async fn current_stream_client(&self) -> Option<String> {
        self.queue.lock().await.current_client.clone()
    }

    /// A track died at the player layer (dead/403 URL, or our own loopback proxy refusing).
    ///
    /// First, whose fault: the audio proxy is checked (and rebuilt) before anything else. If it was
    /// dead, nothing is wrong with the track or its URL (iOS took the socket from the suspended
    /// app), so the same track is reloaded through the rebuilt proxy, its cached URL kept and its
    /// one retry not spent. That case used to evict, retry on the same dead port, and advance:
    /// 187 tracks skipped in a row on the owner's phone (docs/streams/proxyfix.md).
    ///
    /// Otherwise as before: a WEB_REMIX failure is recorded so the next resolve bypasses it
    /// (context/06 §2), the URL is evicted, and the SAME track is retried once via the fallback
    /// clients before giving up and advancing. And never a skip storm: after
    /// [`MAX_FAILURE_STREAK`] failures with no playback in between, playback stops on the track
    /// that failed, with one message, instead of walking the queue.
    pub async fn on_track_failed(self: &std::sync::Arc<Self>) -> FailOutcome {
        let proxy = crate::audioproxy::ensure_alive();
        let (video_id, client, already_retried) = {
            let q = self.queue.lock().await;
            let vid = q.items.get(q.current).map(|i| i.video_id.clone());
            let retried = vid.is_some() && vid == q.retried;
            (vid, q.current_client.clone(), retried)
        };
        let streak = self.fail_streak.load(Ordering::Relaxed);
        let action = failure_action(proxy, video_id.is_some(), already_retried, streak);
        tracing::info!(?proxy, already_retried, streak, ?action, "track failure");
        match action {
            FailAction::Halt => {
                self.halt_after_failures().await;
                return FailOutcome::Halted;
            }
            FailAction::Retry { infra } => {
                let Some(vid) = video_id else { return FailOutcome::Halted };
                if infra {
                    self.fail_streak.fetch_add(1, Ordering::Relaxed);
                } else {
                    // Whatever served it, the URL is dead — evict so the retry can't replay it.
                    self.db.evict_stream(&vid);
                    if client.as_deref() == Some(MAIN_CLIENT) {
                        tracing::warn!(video_id = %vid, "WEB_REMIX stream failed on GET — marking + evicting");
                        self.orchestrator.mark_web_remix_failed(&vid).await;
                    }
                }
                // Retry once, whatever served it. This used to be WEB_REMIX and cache only, on the
                // reasoning that a fallback client had already had its turn. That holds for a URL
                // that was dead on arrival and not for one that dies an hour in: a direct client's
                // URL can go bad mid-track (expiry, a seek's fresh request refused), and skipping
                // straight to the next song is the "long mixes just drop" report. Issue #188.
                // The `retried` marker is cleared on every queue pointer move, so this is once per
                // play of a track, not once ever. A proxy rebuild does not spend it.
                {
                    let mut q = self.queue.lock().await;
                    if !infra {
                        q.retried = Some(vid.clone());
                    }
                    // start_current's loadfile replaces mpv's playlist
                    q.lookahead_loaded = None;
                    // Come back where the user was, not at 0. `q.duration` is mpv's own, reset to
                    // 0 on every load, so it is also the proof that mpv really was playing *this*
                    // file: on a gapless advance `latest_position` still holds the previous
                    // track's until the next tick. `start_current` consumes this.
                    let pos = self.current_position();
                    if q.duration > 0.0 && pos > 1.0 && pos < q.duration {
                        *self.pending_seek.lock().unwrap() = Some((vid.clone(), pos));
                    }
                }
                if infra {
                    tracing::info!(video_id = %vid, "retrying the track through the rebuilt proxy");
                } else {
                    tracing::info!(video_id = %vid, "retrying failed track via fallback clients");
                }
                let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
                if self.start_current(gen).await {
                    self.prime_lookahead(gen).await;
                }
                FailOutcome::Retried
            }
            FailAction::Advance => {
                if let (Some(vid), Some(c)) = (&video_id, &client) {
                    self.db.evict_stream(vid);
                    if c == MAIN_CLIENT {
                        self.orchestrator.mark_web_remix_failed(vid).await;
                    }
                }
                self.fail_streak.fetch_add(1, Ordering::Relaxed);
                self.on_track_ended().await;
                FailOutcome::Advanced
            }
        }
    }

    /// Stop on the track that failed, paused, ready to try again on Play: what Spotify does when
    /// nothing will play, instead of skipping through the queue. Mirrors `on_audio_device_lost`.
    async fn halt_after_failures(&self) {
        tracing::warn!("too many track failures in a row (or no proxy): stopping");
        self.fail_streak.store(0, Ordering::Relaxed); // a press of Play gets a fresh run
        self.generation.fetch_add(1, Ordering::SeqCst); // cancel any start in flight
        // mpv may already have moved into the primed next entry; stop empties its playlist.
        let _ = self.player.stop();
        self.queue.lock().await.lookahead_loaded = None;
        self.emit_own("playback-state", "paused");
        self.media_set_playing(false);
    }

    /// A resolve failure the queue must not move past ([`resolve_failure_action`]): the failed
    /// track stays current, nothing plays, the UIs show paused and the error banner. Play (or a
    /// tap) tries again.
    async fn stop_on_resolve_failure(&self, video_id: &str, message: &str) {
        self.fail_streak.store(0, Ordering::Relaxed); // a press of Play gets a fresh run
        // The outgoing track may still be loaded (paused by `show_loading`): Play must not
        // resume it under this one's title.
        let _ = self.player.stop();
        self.queue.lock().await.lookahead_loaded = None;
        self.emit_own("playback-state", "paused");
        self.media_set_playing(false);
        self.emit_error(video_id, message);
    }

    /// The audio device went away under a loaded file (a PC waking from sleep re-enumerates its
    /// devices, or one was unplugged). mpv ends the file with an error exactly like a dead URL
    /// does, and routing it through `on_track_failed` is what made a sleeping PC wake up playing
    /// music by itself: the retry evicted a perfectly good stream URL, `start_current` ends with an
    /// unconditional `play()` so the user's pause was thrown away, the device was still missing so
    /// the next track failed the same way, and the app walked the queue (scrobbling each track it
    /// touched) until the device came back and one of those `play()` calls stuck. Issue #267.
    ///
    /// Nothing is wrong with the stream, so stay exactly where we are. mpv is idle now, so the
    /// remembered position is what `resume_or_toggle` reloads at when the user presses play.
    pub async fn on_audio_device_lost(&self) {
        let pos = self.current_position();
        // Read the position first: this empties mpv's playlist, which resets `time-pos`.
        //
        // The file died, the *playlist* didn't. mpv runs with no `keep-open` and with
        // `prefetch-playlist`, so it moves straight on to the appended gapless lookahead and
        // tries it too. If the device came back in between (a PipeWire restart, a Bluetooth
        // blip) that track starts playing, unpaused, while the queue still points at this one:
        // the same "woke up playing music by itself" the retry path used to cause.
        let _ = self.player.stop();
        let mut q = self.queue.lock().await;
        q.lookahead_loaded = None;
        if let Some(item) = q.items.get(q.current) {
            if q.duration > 0.0 && pos > 1.0 && pos < q.duration {
                *self.pending_seek.lock().unwrap() = Some((item.video_id.clone(), pos));
            }
        }
    }

    /// The crossfade lookahead's track failed to open. The current track is unaffected, so the queue
    /// and transport stay put; clearing `lookahead_loaded` makes `on_track_ended` load the next
    /// track explicitly, and evicting its URL stops the cache serving it again (for hours) to fail
    /// in front of the user.
    pub async fn on_lookahead_failed(&self) {
        let video_id = {
            let mut q = self.queue.lock().await;
            let idx = q.lookahead_loaded.take();
            idx.and_then(|i| q.items.get(i)).map(|item| item.video_id.clone())
        };
        let Some(video_id) = video_id else { return };
        tracing::warn!(video_id = %video_id, "lookahead stream died, evicting its cached URL");
        self.db.evict_stream(&video_id);
    }

    /// Resolve + load the current track into mpv (replace). Returns false if resolve failed or the
    /// request was superseded.
    ///
    /// ── stream: instant ── The windows show the track the moment this starts, with a loading
    /// state, rather than when its audio does ([`Self::show_loading`]). A load that gives up
    /// without another one taking over clears that state again.
    async fn start_current(self: &std::sync::Arc<Self>, gen: u64) -> bool {
        let started = self.load_current(gen).await;
        // Whatever ended this load (a newer tap included, whose own audio is already paused) must
        // never leave the clock frozen.
        self.loading.store(false, Ordering::SeqCst);
        if !started
            && self.generation.load(Ordering::SeqCst) == gen
            && self.loading_gen.load(Ordering::SeqCst) == gen
        {
            let (item, live) = {
                let q = self.queue.lock().await;
                (q.items.get(q.current).cloned(), q.live)
            };
            let now = item.map(|i| Self::now_playing_json(&i, "current", live));
            self.emit_own("now-playing", now); // `null`: the queue emptied under it
        }
        started
    }

    /// ── stream: instant ── Tell every window what is about to play, before the seconds its
    /// stream takes to resolve (Spotify answers a tap at once): the player bar, the mini player
    /// and Now Playing switch to it, its row turns green, and `loading` puts the play button in
    /// its loading state. The outgoing track stops here so it isn't heard under the new title.
    /// The full `now-playing` follows from [`Self::load_current`] once the audio starts.
    async fn show_loading(&self, gen: u64, item: &SongItem) {
        {
            // Under the reload lock: a load already past its last generation check finishes its
            // `load` + `play` first, and is paused here rather than playing over this one.
            let _guard = self.reload_mutex.lock().await;
            if self.generation.load(Ordering::SeqCst) != gen {
                return;
            }
            // The bar's clock restarts too. Where this track opens, as `play_index_at` seeds it.
            let resume = self
                .pending_seek
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(id, _)| *id == item.video_id)
                .map_or(0.0, |(_, p)| *p);
            self.loading.store(true, Ordering::SeqCst);
            self.latest_position.store(resume.to_bits(), Ordering::SeqCst);
            // Only for a load that has to wait: one that resolves at once replaces the file within
            // milliseconds, and a paused output costs ~30 ms to start again (measured, instant.md).
            let secs = parse_duration_ms(item.duration.as_deref()) / 1000;
            let id = item.video_id.as_str();
            let quick = crate::local::is_local_song(id)
                || crate::downloads::is_downloaded(id)
                || self.db.get_stream(id, cache_horizon(now_secs(), secs)).is_some();
            if !quick && !self.player.is_idle() {
                let _ = self.player.pause();
            }
        }
        self.loading_gen.store(gen, Ordering::SeqCst);
        let mut now = Self::now_playing_json(item, "loading", false);
        now["loading"] = true.into();
        self.emit_own("now-playing", now);
        self.emit_own("position", serde_json::json!({ "position": self.current_position() }));
        self.emit_own("duration", serde_json::json!({ "duration": row_secs(item) }));
        self.emit_queue().await;
        crate::firstaudio::mark("shown"); // stream: firstaudio
    }

    async fn load_current(self: &std::sync::Arc<Self>, gen: u64) -> bool {
        // Taken up front: only the start this flag was set for may honour it.
        let hold = self.hold_next_start.swap(false, Ordering::SeqCst);
        // Resolve the current track, auto-skipping any that no client can play (dead / region-locked
        // videos — context/06 "no client could resolve") instead of stalling the queue on them.
        // Bounded: each failure advances current by one, so the loop terminates at the queue tail.
        let (mut item, data) = loop {
            if self.generation.load(Ordering::SeqCst) != gen {
                return false; // user moved on
            }
            let Some(item) = self.current_item().await else { return false };
            // ── stream: polish ── explicit filter on: never play an explicit row, however queued.
            if crate::polish::explicit::blocks(&item) {
                // ── stream: downloads ── offline, everything not downloaded is stepped over too.
                let why = if crate::downloads::offline_blocks(&item.video_id) {
                    "offline"
                } else {
                    crate::polish::explicit::reason(&item).unwrap_or("explicit")
                };
                let mut q = self.queue.lock().await;
                let next = next_playable_index(&q.items, q.current, RepeatMode::Off);
                let Some(next) = next.filter(|n| *n != q.current) else {
                    drop(q);
                    self.emit_skip(&item.title, why);
                    return false;
                };
                q.seek_to(next);
                q.lookahead_loaded = None;
                drop(q);
                self.emit_skip(&item.title, why);
                self.emit_queue().await;
                continue;
            }
            // ── end stream: polish ──
            self.show_loading(gen, &item).await; // ── stream: instant ──
            let resolved = self
                .resolve(
                    &item.video_id,
                    item.is_upload,
                    parse_duration_ms(item.duration.as_deref()) / 1000,
                )
                .await;
            // A resolve takes seconds; a skip during it bumps the generation. Re-check before
            // acting on the result: an abandoned failure would otherwise move `current` under the
            // track that's already playing and leave a stale error banner (nothing clears it, the
            // new track's now-playing fired before the banner appeared).
            if self.generation.load(Ordering::SeqCst) != gen {
                return false;
            }
            match resolved {
                Ok(d) => {
                    crate::firstaudio::mark("resolved"); // stream: firstaudio
                    if item.unavailable {
                        // It plays after all (a retry from a click, or the network came back).
                        let mut q = self.queue.lock().await;
                        let cur = q.current;
                        q.mark_unavailable(cur, &item.video_id, false);
                    }
                    break (item, d);
                }
                // A deleted local file is gone for good, so it leaves the queue outright rather
                // than being skipped over — a row that can only ever fail again is noise. Every
                // other failure is transient in principle (network, region, a cipher self-heal),
                // so those keep their place.
                Err(ResolveError::LocalMissing(_)) => {
                    let exhausted = {
                        let mut q = self.queue.lock().await;
                        let cur = q.current;
                        if cur < q.items.len() {
                            q.remove_row(cur);
                        }
                        q.lookahead_loaded = None;
                        // `current` now points at what followed it. When it was the tail, step
                        // back onto the last surviving track: an index past the end leaves the
                        // transport pointing at nothing, and Play does nothing at all.
                        let exhausted = q.current >= q.items.len();
                        if exhausted {
                            let last = q.items.len().saturating_sub(1);
                            q.seek_to(last);
                        }
                        exhausted
                    };
                    self.emit_queue().await;
                    self.persist_queue().await;
                    self.emit_notice(&format!("{} is no longer on your disk", item.title));
                    if exhausted {
                        return false;
                    }
                }
                Err(e) => {
                    let streak = self.fail_streak.load(Ordering::Relaxed);
                    let action = resolve_failure_action(&e, streak);
                    if action == ResolveFail::Skip {
                        self.fail_streak.fetch_add(1, Ordering::Relaxed);
                    }
                    if action == ResolveFail::Halt {
                        // The third track in a row YouTube said no to: stop on it, as Spotify
                        // does, rather than walk the queue. Each one was a verdict on its video,
                        // so this one is greyed like the others.
                        let mut q = self.queue.lock().await;
                        let cur = q.current;
                        q.mark_unavailable(cur, &item.video_id, true);
                        drop(q);
                        self.emit_queue().await;
                        self.persist_queue().await;
                    }
                    if action != ResolveFail::Skip {
                        // Stay: nothing is wrong with this track. The network is down, or YouTube
                        // is refusing us right now. Skipping would walk the whole queue in a
                        // couple of seconds, toast once per track and persist the new position
                        // (the owner's iPhone, build 13: ~70 tracks), so stay exactly where we
                        // are, paused, and say what happened. A tap tries again, once.
                        let message =
                            if action == ResolveFail::Halt { REFUSED_MESSAGE.to_owned() } else { e.to_string() };
                        self.stop_on_resolve_failure(&item.video_id, &message).await;
                        return false;
                    }
                    let mut q = self.queue.lock().await;
                    // Kept and greyed out, not deleted (Spotify semantics).
                    let cur = q.current;
                    q.mark_unavailable(cur, &item.video_id, true);
                    // Deliberately ignores repeat-all: wrapping the unplayable-skip would spin
                    // forever on a queue where nothing resolves. Skips stop at the tail, and step
                    // over rows already known to be unavailable.
                    let next = next_playable_index(&q.items, q.current, RepeatMode::Off);
                    let Some(next) = next else {
                        drop(q);
                        self.emit_queue().await;
                        self.persist_queue().await;
                        self.emit_error(&item.video_id, &e.to_string()); // nothing left to skip to
                        return false;
                    };
                    q.seek_to(next);
                    q.lookahead_loaded = None;
                    drop(q);
                    self.emit_skip(&item.title, skip_reason(&e));
                    self.emit_queue().await;
                }
            }
        };
        // ── stream: instant ── Held through `load` + `play`, so a newer tap's `show_loading` either
        // lands first (and this load sees the new generation) or pauses what this one started.
        self.apply_item_effects(&item).await; // speeds.rs: before a byte plays
        let reload = self.reload_mutex.lock().await;
        if self.generation.load(Ordering::SeqCst) != gen {
            return false; // user moved on
        }
        // Resume a restored position, but only for the exact track it was saved against (any first
        // play consumes it, so jumping elsewhere doesn't inherit it). This is handed to `loadfile`
        // as `start=`, not issued as a seek afterwards: mpv does *not* queue a seek that arrives
        // before the file is loaded. It fails with MPV_ERROR_COMMAND and playback starts at 0, so
        // a restored position and a failed-track retry both lost their offset (issue #188).
        let seek = self
            .pending_seek
            .lock()
            .unwrap()
            .take()
            .filter(|(vid, _)| *vid == item.video_id)
            .map(|(_, pos)| pos);
        // ── stream: podcasts ── an episode resumes where it was left.
        let seek = seek.or_else(|| crate::podcasts::progress::resume_position(&item.video_id));
        // ── end stream: podcasts ──
        let play_u = mpv_stream_url(&data);
        if let Err(e) = self.player.load(
            &play_u,
            &data.headers,
            self.track_gain(data.loudness_db),
            seek,
            data.live,
        ) {
            self.emit_error(&item.video_id, &e.to_string());
            return false;
        }
        crate::firstaudio::loaded(); // stream: firstaudio
        self.loading.store(false, Ordering::SeqCst); // ── stream: instant ── ticks are this file's now
        // Where mpv starts, from now on: its first tick comes after the unpause, and that pause
        // flag's Play (`lt_on_play_state`) reads this. Left at the last track's position, a Continue
        // here told the device link the music was back where this desktop last stopped, and the
        // phone picked it up there later.
        self.latest_position.store(seek.unwrap_or(0.0).to_bits(), Ordering::SeqCst);
        if hold {
            // Opened after the sleep timer's end-of-track stop: ready, but not playing.
            let _ = self.player.pause();
            self.emit_own("playback-state", "paused");
        } else {
            let _ = self.player.play();
        }
        drop(reload);
        // Items played from cards/radio can arrive without a duration; the player response knows
        // the exact length of the cut we stream. Backfill before emitting — lyrics matching keys
        // on it (a wrong-cut LRCLIB match plays lyrics seconds off the audio).
        backfill_metadata(
            &mut item,
            data.duration.as_deref(),
            data.artists.as_deref(),
            data.is_video,
            data.is_episode,
        );
        // A broadcast's picture is segment bases too (#424), so there is no 720p file to pair.
        self.attach_video(&item, item.is_video && !data.live, &play_u);
        {
            let mut q = self.queue.lock().await;
            q.current_client = Some(data.stream_client.clone());
            q.current_loudness_db = data.loudness_db;
            // Fresh play → fresh history state (context/01 §registerPlayback).
            q.playback_ping = data.playback_ping.clone();
            q.cpn = innertube::generate_cpn();
            self.history_pinged.store(false, Ordering::Relaxed);
            self.end_listen();
            q.duration = 0.0;
            // Across a restart too: the restored track is announced before anything resolves it,
            // and a broadcast restored as an ordinary track came back with a seek bar (#424).
            if q.live || data.live {
                let id = if data.live { item.video_id.as_str() } else { "" };
                self.db.set_setting("live_video", id);
            }
            q.live = data.live;
            let cur = q.current;
            if let Some(qi) = q.items.get_mut(cur) {
                // Same repair on the queue's own copy: it's what the queue panel renders, what
                // `persist_queue` saves, and what `record_play` writes into Your Rotation.
                if qi.video_id == item.video_id {
                    backfill_metadata(
                        qi,
                        data.duration.as_deref(),
                        data.artists.as_deref(),
                        data.is_video,
                        data.is_episode,
                    );
                }
            }
        }
        if data.live {
            if let Err(error) = self.reset_tempo(true).await {
                tracing::warn!(%error, "live: tempo stays as it was");
            }
        }
        // ── stream: instant ── A newer tap is already showing its own track; this one must not
        // come back up over it. Its `load` replaces this audio.
        if self.generation.load(Ordering::SeqCst) != gen {
            return false;
        }
        self.emit_now_playing(&item, &data.stream_client, data.live);
        // The rating just emitted is whatever the row was parsed with, which for a search result
        // or a radio track is nothing at all (issue #93). Ask.
        self.refresh_from_next(&item.video_id, gen);
        // We just told mpv to play, but its `pause` flag was already `false`, so no property event
        // will announce it (see `Player::is_playing`). Say so ourselves — otherwise MPRIS and
        // Discord never learn the track started. After `emit_now_playing`, so the new track is the
        // current one before anything renders it as playing.
        self.media_set_playing(true);
        self.emit_queue().await;
        self.persist_queue().await;
        // Autoplay early trigger: extend the queue while the tail still plays, so the gapless
        // lookahead can prime into the continuation. The near-tail guard inside makes this a
        // no-op for almost every track start. Detached — never on the caller's path.
        let me = self.clone();
        crate::rt::spawn(async move { me.extend_queue_radio(gen).await });
        true
    }

    /// Resolve the next queue item and append it to mpv for a gapless transition. context/14.
    ///
    /// An upcoming track no client can resolve is skipped *here*, while the current track is
    /// still playing — deferring it to the track boundary pauses playback while the whole client
    /// waterfall fails again on the event pump (and used to wedge until a manual skip).
    /// ponytail: at most 3 removals per prime so a network outage can't eat the whole queue.
    async fn prime_lookahead(self: &std::sync::Arc<Self>, gen: u64) {
        // A guest plays whatever the host announces next, and a primed entry would let mpv move on
        // by itself into a track the room may not be playing.
        for _ in 0..3 {
            // YouTube refused a resolve a moment ago: the lookahead waits out the backoff, and the
            // track boundary resolves the next row itself (once, stopping if it is refused too).
            if crate::firstaudio::backing_off() {
                return;
            }
            let next_idx = {
                let q = self.queue.lock().await;
                // Repeat-one primes nothing: mpv loops the file itself (next_index → None).
                let Some(next) = q.next_playable(q.repeat) else { return };
                if q.lookahead_loaded == Some(next) {
                    return; // already primed
                }
                // `enqueue` appends with the deck's options as they are, and a live load left them
                // at the live set (no reconnect). When the broadcast ends the next track comes in
                // through `start_current`, which sets its own.
                if q.live {
                    return;
                }
                next
            };
            // ── stream: polish ── never crossfade into or out of a podcast episode: the transition
            // next to one is gapless. The user's setting comes back once both sides are music.
            let speech = {
                let q = self.queue.lock().await;
                let speech = [q.items.get(q.current), q.items.get(next_idx)]
                    .into_iter()
                    .flatten()
                    .any(crate::podcasts::is_episode_item);
                speech
            };
            if crate::podcasts::progress::hold_crossfade(speech) {
                self.apply_crossfade().await;
            }
            // ── end stream: polish ──
            let (next_video, next_title, next_upload, next_secs) = {
                let q = self.queue.lock().await;
                match q.items.get(next_idx) {
                    Some(item) => (
                        item.video_id.clone(),
                        item.title.clone(),
                        item.is_upload,
                        parse_duration_ms(item.duration.as_deref()) / 1000,
                    ),
                    None => return,
                }
            };
            match self.resolve(&next_video, next_upload, next_secs).await {
                Ok(d) => {
                    self.enqueue_lookahead(gen, next_idx, &next_video, d).await;
                    return;
                }
                Err(e) => {
                    let streak = self.fail_streak.load(Ordering::Relaxed);
                    match resolve_failure_action(&e, streak) {
                        ResolveFail::Stay => {
                            // The next track is fine; YouTube is refusing us or unreachable.
                            // Marking it would grey a good row in the persisted queue, and the
                            // loop would do it again to the row that moved up (the iPhone's skip
                            // storm). Leave the queue alone; the backoff holds the next prime.
                            tracing::warn!(video_id = %next_video, error = %e, "lookahead resolve failed, leaving the queue alone");
                            return;
                        }
                        // Enough in a row: stop marking. The track boundary meets this row and
                        // stops on it there.
                        ResolveFail::Halt => return,
                        ResolveFail::Skip => {
                            self.fail_streak.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    tracing::warn!(video_id = %next_video, error = %e, "lookahead resolve failed, marking unavailable");
                    if self.generation.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    // Spotify semantics: the row stays (greyed out in the queue, and in the saved
                    // queue) and playback steps over it; `next_playable` skips it on the next
                    // pass of this loop. Only if the slot still holds the track that failed: the
                    // queue can shift under a resolve.
                    let marked = {
                        let mut q = self.queue.lock().await;
                        next_idx != q.current && q.mark_unavailable(next_idx, &next_video, true)
                    };
                    if !marked {
                        return;
                    }
                    self.emit_queue().await;
                    self.persist_queue().await;
                    self.emit_skip(&next_title, skip_reason(&e));
                }
            }
        }
    }

    /// Second half of [`Self::prime_lookahead`]: hand the resolved stream to mpv and record it.
    async fn enqueue_lookahead(
        self: &std::sync::Arc<Self>,
        gen: u64,
        next_idx: usize,
        next_video: &str,
        data: PlaybackData,
    ) {
        if self.generation.load(Ordering::SeqCst) != gen {
            return;
        }

        // Headers are global in mpv; the direct-URL clients need none beyond UA, which the
        // current track already set. Just append the URL.
        let stream_url = mpv_stream_url(&data);
        let _guard = self.reload_mutex.lock().await;
        if self.generation.load(Ordering::SeqCst) != gen {
            return;
        }
        let mut q = self.queue.lock().await;
        // The queue can change under a resolve (a guest add inserts at current+1) — enqueueing
        // then would gaplessly play the wrong song. Verify the slot still holds the same track.
        if q.items.get(next_idx).map(|i| i.video_id != next_video).unwrap_or(true) {
            tracing::debug!(index = next_idx, "queue changed during lookahead resolve — dropped");
            return;
        }
        // Another prime already claimed this slot while we resolved. Two run concurrently at the
        // autoplay trigger point — `start_current` spawns `extend_queue_radio` (which primes after
        // extending) and its caller primes right after, both on the same generation, so the
        // pre-resolve "already primed" check in `prime_lookahead` can't see the other one. Appending
        // again leaves a *duplicate* entry in mpv's playlist, and since the queue advances one index
        // per end-file, that offsets mpv from `current` for the rest of the session: the dup replays
        // while the UI shows the track after it.
        if q.lookahead_loaded == Some(next_idx) {
            tracing::debug!(
                index = next_idx,
                "lookahead already primed by a concurrent resolve — dropped"
            );
            return;
        }
        // A broadcast is not appended (#424): the player's `load` is what knows a URL is live, and
        // an appended entry inherits the file options and duration handling of the track before
        // it. Left unprimed, it is reached through `start_current` like a first play.
        if data.live {
            tracing::debug!(index = next_idx, "next track is live, not primed");
            return;
        }
        let url = stream_url;
        if self.player.current_path().as_deref() == Some(&url) {
            return;
        }
        // ── stream: audio ── the next track's gain rides with its gapless entry (no boundary click).
        if let Err(e) = self.player.enqueue_with_gain(&url, self.track_gain(data.loudness_db)) {
            tracing::warn!(error = %e, "enqueue lookahead failed");
            return;
        }
        q.lookahead_loaded = Some(next_idx);
        q.lookahead_client = Some(data.stream_client.clone());
        q.lookahead_loudness_db = Some(data.loudness_db);
        q.lookahead_playback_ping = data.playback_ping.clone();
        // Same backfill as start_current: a gapless advance emits this item straight from the
        // queue, so the repair has to land before it becomes the current track.
        if let Some(qi) = q.items.get_mut(next_idx) {
            backfill_metadata(
                qi,
                data.duration.as_deref(),
                data.artists.as_deref(),
                data.is_video,
                data.is_episode,
            );
            // Resolved now, minutes early, so the picture is there the moment the track starts.
            self.attach_video(qi, qi.is_video, &url);
        }
        tracing::debug!(index = next_idx, "gapless lookahead primed");
    }

    pub(crate) async fn current_item(&self) -> Option<SongItem> {
        let q = self.queue.lock().await;
        q.items.get(q.current).cloned()
    }

    /// [`loudness_gain`] for one track, honouring the `normalize_volume` setting. Off means mpv
    /// gets no `af` at all, so the master plays exactly as mastered: no attenuation, no boost and
    /// no limiter (#298). Every load path routes through here, so the switch cannot half-apply.
    fn track_gain(&self, loudness_db: Option<f64>) -> Option<f64> {
        (self.db.get_setting("normalize_volume").as_deref() != Some("false"))
            // ── stream: audio ── Spotify's Quiet/Normal/Loud targets (was: fixed -7 LUFS).
            .then(|| crate::audio::track_loudness_gain(&self.db, loudness_db))
            .flatten()
    }

    /// Retune what is already playing after the `normalize_volume` switch moved. The point of the
    /// setting is hearing the same track both ways, so it cannot wait for the next track change.
    pub async fn reapply_gain(&self) {
        let loudness = self.queue.lock().await.current_loudness_db;
        if let Err(e) = self.player.set_gain(self.track_gain(loudness)) {
            tracing::warn!(error = %e, "reapplying loudness gain failed");
        }
    }

    // --- events (context/11 UI contract) ----------------------------------------------------

    /// Everything the `now-playing` event carries. Shared with [`Self::playback_snapshot`] so a
    /// window that asks for the current track can't be told a different shape than one that
    /// listened for it.
    fn now_playing_json(item: &SongItem, stream_client: &str, live: bool) -> serde_json::Value {
        serde_json::json!({
            "videoId": item.video_id,
            "title": item.title,
            "artists": item.artists,
            "artistId": item.artist_id,
            // Per-artist links, so a collab in the player bar navigates like it does in a row.
            "artistRuns": item.artist_runs,
            "thumbnail": item.thumbnail,
            "duration": item.duration,
            // The lyrics fetch needs it on a gapless advance, where the queue event that would
            // carry it lands after this one.
            "album": item.album,
            "streamClient": stream_client,
            "rating": item.rating,
            // YouTube's own `musicVideoType` says this track is a video upload, not the generated
            // audio track, which is what the player view's music-video mode gates on (plan 031).
            "isVideo": item.is_video,
            // Podcast controls in the transport (#500).
            "isEpisode": item.is_episode,
            // ── stream: pod-fixes ── false hides every lyrics surface (`lyrics::offered`).
            "showsLyrics": crate::lyrics::offered(item),
            // A broadcast on air: no length, nothing to seek, a Live badge for the bar (#424).
            "live": live,
            // The "E" badge on the system media controls (iOS lock screen, Dynamic Island).
            "explicit": crate::polish::explicit::is_explicit(item),
            // ── stream: instant ── shown ahead of its audio (`show_loading`): the play button's
            // loading state. Every other `now-playing` is a track that is playing.
            "loading": false,
        })
    }

    /// What a window that opened mid-playback missed. Events are fire-and-forget, so the mini
    /// player (a second webview, created long after the track started) and the main window on a
    /// cold start both have to ask once instead of guessing.
    pub async fn playback_snapshot(&self) -> serde_json::Value {
        let (speed, semitones, reverb) = self.player.playback_params();
        let (duration, item, live) = {
            let q = self.queue.lock().await;
            (q.duration, q.items.get(q.current).cloned(), q.live)
        };
        serde_json::json!({
            "now": item.as_ref().map(|i| Self::now_playing_json(i, "current", live)),
            "paused": !self.is_playing.load(Ordering::Relaxed),
            "position": self.current_position(),
            // 0 until mpv loads the file, which a restored track waits for until play (#502).
            "duration": if duration > 0.0 { duration } else { item.as_ref().map_or(0.0, row_secs) },
            "volume": saved_volume(&self.db),
            "speed": speed,
            "semitones": semitones,
            "reverb": reverb,
        })
    }

    /// Ask YouTube what the current track's rating actually is, and tell every window if it
    /// differs from the snapshot they were just handed. Issue #93.
    ///
    /// A `SongItem`'s `rating` is only ever a snapshot of the response that produced the row, and
    /// most rows state nothing at all: search rows carry no `likeStatus` (0 of 29) and `/next`
    /// panel rows carry none either (0 of 50), both live-checked 2026-08-24, so a liked song
    /// played from search or reached by autoplay renders exactly like an unrated one. A restored
    /// queue's rating is worse than absent: it is however old the database is, so a like made on
    /// the phone since never shows up. The authority is a bare `next(videoId)`'s
    /// `playerOverlays…likeButtonRenderer` (~4 KB gzipped), which is what YouTube Music's own
    /// player bar draws from.
    ///
    /// Detached and doubly guarded: nothing waits on it, a skip during the round trip discards the
    /// answer (`generation`), and so does a rating the user set meanwhile (`rate_epoch`).
    ///
    /// The same reply carries the track's Song/Video twin on accounts YouTube pairs them for, so
    /// it fills that in too, at no extra request (#520). A queue played from a playlist also asks
    /// the playlist, which can pair the track differently ([`Self::refresh_playlist_twins`]).
    fn refresh_from_next(self: &std::sync::Arc<Self>, video_id: &str, gen: u64) {
        // Signed out there is no rating to be wrong about and no twin to learn (signed-out `/next`
        // never wraps a row), and a local file is not a videoId YouTube has ever heard of.
        if !self.it.is_logged_in()
            || crate::local::is_local_song(video_id)
            || crate::podcasts::is_rss_id(video_id) // ── stream: podcasts ──
        {
            return;
        }
        self.refresh_playlist_twins(video_id);
        let me = self.clone();
        let video_id = video_id.to_owned();
        crate::rt::spawn(async move {
            let Some(client) = me.clients.get(innertube::METADATA_CLIENT) else { return };
            let epoch = me.rate_epoch.load(Ordering::SeqCst);
            // No playlist id: the bare form is ~25x smaller than the radio one (4 KB vs 100 KB
            // gzipped), and it is the request the web client sends for the track itself.
            let next = match me.it.next(client, Some(&video_id), None).await {
                Ok(next) => next,
                Err(e) => {
                    tracing::debug!(error = %e, "next refresh failed; keeping the snapshot");
                    return;
                }
            };
            // The track's other version, on accounts YouTube pairs them for (#520). The radio
            // started from a music video wraps nothing, so this is the only place a video played
            // on its own learns its song. A fact about the id, so no generation check.
            if let Some(seed) = next.items.iter().find(|i| i.video_id == video_id) {
                let learned = {
                    let mut q = me.queue.lock().await;
                    let q = &mut *q;
                    let mut learned = false;
                    let rows = q.items.iter_mut().chain(q.shuffle_orig.iter_mut().flatten());
                    for item in rows.filter(|i| i.video_id == video_id) {
                        learned |= adopt_twin(item, seed);
                    }
                    learned
                };
                if learned {
                    me.emit_queue().await;
                }
            }
            let Some(rating) = next.rating else { return };
            if me.generation.load(Ordering::SeqCst) != gen
                || me.rate_epoch.load(Ordering::SeqCst) != epoch
            {
                return;
            }
            // Write it back to every copy in the queue as well: that is what the queue panel
            // renders, what `persist_queue` saves, and what the next `now-playing` emits.
            let changed = {
                let mut q = me.queue.lock().await;
                let mut changed = false;
                for item in q.items.iter_mut().filter(|i| i.video_id == video_id) {
                    changed |= item.rating != Some(rating);
                    item.rating = Some(rating);
                }
                changed
            };
            if !changed {
                return;
            }
            // Not persisted on purpose: `persist_fingerprint` excludes `rating`, and this runs on
            // every track start anyway, so a stored copy would only ever be the stale one again.
            let _ =
                me.host.emit("rating", serde_json::json!({ "videoId": video_id, "rating": rating }));
        });
    }

    /// The Song/Video twins the queue's own playlist gives the playing track and the rows after it
    /// (#542). On its own, `/next` pairs a song with its main video or with nothing, so a playlist
    /// holding another video of the song, or a video of a song with no main one, lost that video:
    /// only a request naming the playlist pairs each entry with what the playlist holds. Those
    /// pairs win over the bare reply's ([`adopt_playlist_twins`]).
    ///
    /// One reply covers the playlist from the top to ~49 rows past the track asked about
    /// (live-checked 2026-11-07: 50 rows for the first, 99 for the 50th), so a playlist played in
    /// order asks once per window. Detached like the rating: nothing waits on it.
    fn refresh_playlist_twins(self: &std::sync::Arc<Self>, video_id: &str) {
        let me = self.clone();
        let video_id = video_id.to_owned();
        crate::rt::spawn(async move {
            let (playlist, epoch) = {
                let mut q = me.queue.lock().await;
                let q = &mut *q;
                let Some(playlist) = playlist_context(q.source_id.as_deref()) else { return };
                // A manual add or an autoplay track isn't in the playlist to ask about.
                let from_playlist = q.items.get(q.current).is_some_and(|i| {
                    i.video_id == video_id && !i.queued && !i.queued_end && !i.autoplay
                });
                if !from_playlist {
                    return;
                }
                if q.paired.0 != q.epoch {
                    q.paired = (q.epoch, HashSet::new());
                }
                // Marked before the request, so a failed one isn't retried on every replay.
                if !q.paired.1.insert(video_id.clone()) {
                    return;
                }
                (playlist, q.epoch)
            };
            let Some(client) = me.clients.get(innertube::METADATA_CLIENT) else { return };
            let next = match me.it.next(client, Some(&video_id), Some(&playlist)).await {
                Ok(next) => next,
                Err(e) => {
                    tracing::debug!(error = %e, "playlist next failed; keeping the twins we have");
                    return;
                }
            };
            let learned = {
                let mut q = me.queue.lock().await;
                let q = &mut *q;
                if q.epoch != epoch {
                    return; // another queue now
                }
                for item in &next.items {
                    q.paired.1.insert(item.video_id.clone());
                    q.paired.1.extend(item.counterpart.clone());
                }
                let rows = q.items.iter_mut().chain(q.shuffle_orig.iter_mut().flatten());
                adopt_playlist_twins(rows, &next.items)
            };
            if !learned {
                return;
            }
            // Rows still to come take the version "Play the video first" picks, as album rows do.
            // Only with music videos on: off, a video the user put in a playlist stays what it was.
            let moved = me.db.get_setting("music_videos").as_deref() == Some("true")
                && me.apply_version_preference().await;
            if !moved {
                me.emit_queue().await;
            }
        });
    }

    fn emit_now_playing(&self, item: &SongItem, stream_client: &str, live: bool) {
        crate::smart::taste::track_started(item); // ── stream: smart ── taste events
        // ── stream: podcasts ── track the episode; a gapless advance can't take `start=`.
        if let Some(t) = crate::podcasts::progress::track_started(item) {
            if stream_client == "gapless" {
                let _ = self.player.seek(t);
            }
        }
        // ── end stream: podcasts ──
        self.emit_own("now-playing", Self::now_playing_json(item, stream_client, live));
        self.emit_own("playback-state", "playing");
        // Push the same metadata to the OS media widget (context/16) and Discord.
        self.media_show(item);
        // The tray's "now playing" row follows every track, whatever the notification setting
        // (it read "Nothing playing" through a whole podcast when notifications were off).
        self.host.now_playing_changed(&item.title, &item.artists);
        // A restored queue is paused at launch: nothing started, so nothing to announce.
        if stream_client != "restored"
            && self.db.get_setting("track_notifications").as_deref() == Some("true")
        {
            // A gapless advance or a Listen Together host's change started on its own, so with the
            // opt-in it notifies even with Limusic in front (#94). Ceiling: the rare non-primed
            // advance at track end comes through `start_current` like a user's pick, and stays
            // background-only.
            let even_focused = matches!(stream_client, "gapless" | "listen-together")
                && self.db.get_setting("track_notifications_focused").as_deref() == Some("true");
            self.host.track_changed(&item.title, &item.artists, even_focused);
        }
        // New track ⇒ let the next position tick through immediately instead of waiting out the
        // ~1s throttle, so a restored seek position (and the play-state self-heal) lands at once.
        self.last_media_push.store(0, Ordering::Relaxed);
    }

    /// The OS media widget's track (context/16).
    fn media_show(&self, item: &SongItem) {
        if let Some(m) = &self.media {
            // MPRIS/SMTC want a URL; a local track's artwork is a path, so hand it a file:// one.
            // Scheme, not leading slash: a Windows cover path is `C:\...`, and SMTC drops the whole
            // metadata update (cover included) when the thumbnail URI won't parse.
            let cover = item.thumbnail.as_ref().map(|t| {
                if t.contains("://") {
                    crate::host::cover_url(t)
                } else {
                    format!("file://{t}")
                }
            });
            m.set_metadata(&item.title, &item.artists, item.album.as_deref(), cover.as_deref());
        }
    }

    /// Push the volume to MPRIS (#220). Called after every change, wherever it came from.
    pub fn media_set_volume(&self, volume: i64) {
        if let Some(m) = &self.media {
            m.set_volume(volume);
        }
    }

    /// A Speed & pitch change, for music or spoken word (`target`; `None` is whatever is playing).
    /// It is kept in its own bucket (speeds.rs) and applied only if that is what is playing now:
    /// the music sheet used while an episode plays changes the next song, not the episode.
    pub async fn set_playback_params(
        &self,
        speed: f64,
        semitones: i32,
        reverb: player::Reverb,
        target: Option<crate::speeds::Target>,
    ) -> Result<(), String> {
        use crate::speeds::Target;
        if !speed.is_finite() || !player::SPEED_RANGE.contains(&speed) {
            return Err("Invalid playback effect values".into());
        }
        let _effects = self.playback_effects.lock().await;
        let spoken_now = self.current_item().await.is_some_and(|i| crate::podcasts::is_episode_item(&i));
        let target = target.unwrap_or(if spoken_now { Target::Spoken } else { Target::Music });
        let mut next = *self.speeds.lock().unwrap();
        next.set(target, (speed, semitones, reverb));
        if (target == Target::Spoken) == spoken_now {
            let (speed, semitones, reverb) = next.for_item(spoken_now);
            self.apply_playback_params(speed, semitones, reverb).await?;
        }
        *self.speeds.lock().unwrap() = next;
        if target == Target::Spoken {
            self.db.set_setting(crate::speeds::EPISODE_SPEED_KEY, &speed.to_string());
        }
        Ok(())
    }

    /// Put the playing item's own speed on mpv (speeds.rs): called on every track start, so an
    /// episode never hands its 2x to the song after it, nor a song its pitch to an episode.
    async fn apply_item_effects(&self, item: &SongItem) {
        let _effects = self.playback_effects.lock().await;
        let want = self.speeds.lock().unwrap().for_item(crate::podcasts::is_episode_item(item));
        if want == self.player.playback_params() {
            return;
        }
        if let Err(error) = self.apply_playback_params(want.0, want.1, want.2).await {
            tracing::warn!(%error, "speeds: the item's own speed did not apply");
        }
    }

    // Caller holds playback_effects through the update and notification so two windows cannot
    // emit confirmations out of order. The room reset uses this same path before shared playback.
    async fn apply_playback_params(
        &self,
        speed: f64,
        semitones: i32,
        reverb: player::Reverb,
    ) -> Result<(), String> {
        self.player.set_playback_params(speed, semitones, reverb).map_err(|e| e.to_string())?;
        self.media_refresh_timing().await;
        let _ = self.host.emit(
            "playback-params",
            serde_json::json!({ "speed": speed, "semitones": semitones, "reverb": reverb }),
        );
        Ok(())
    }

    /// Back to 1x in a listening room, or for a broadcast (`live`, #424): played faster, a live
    /// stream catches up with its own live edge within seconds and stalls on every segment.
    async fn reset_tempo(&self, live: bool) -> Result<(), String> {
        let _effects = self.playback_effects.lock().await;
        let (speed, semitones, reverb) = self.player.playback_params();
        if speed != 1.0 && live {
            self.apply_playback_params(1.0, semitones, reverb).await?;
        }
        Ok(())
    }

    /// Tempo changes do not emit a new mpv duration; refresh OS and Discord timelines immediately.
    pub async fn media_refresh_timing(&self) {
        let secs = self.queue.lock().await.duration;
        let speed = self.player.speed();
        if let Some(m) = &self.media {
            if secs > 0.0 {
                m.set_duration(secs, speed);
            }
            self.media_set_playing(self.is_playing.load(Ordering::Relaxed));
        }
    }

    /// Push play/pause state + the current position to the OS media controls (context/16) and
    /// Discord. The single choke point for play/pause, so both stay in step with mpv. Discord gets
    /// the flag only — its position flows exclusively through the ticks, so a stale
    /// `current_position()` here (the last tick can predate a track change) can't poison its
    /// timeline.
    pub fn media_set_playing(&self, playing: bool) {
        self.is_playing.store(playing, Ordering::Relaxed);
        if let Some(m) = &self.media {
            let pos = self.current_position();
            // Record what the controls now hold, or the next tick would repeat this very push.
            *self.last_media_state.lock().unwrap() = Some((playing, pos));
            m.set_playback(playing, pos, self.player.speed());
        }
    }

    /// The position of the audio being heard, read from mpv this instant, with the wall-clock time
    /// of the read (Unix ms): what the lyric editor stamps a tap with. The UI holds a position
    /// that is up to 250 ms old and arrived an unknown time ago; with this pair it works out where
    /// the song was at the keypress, `position - (atMs - keypress) × speed`, however long the
    /// command took to get here. `None` with nothing loaded, or while the bar shows another device.
    pub fn playback_clock(&self) -> Option<serde_json::Value> {
        let before = std::time::SystemTime::now();
        let position = self.player.clock()?;
        // The middle of the read, which takes microseconds unless mpv's core is busy.
        let at = before + before.elapsed().unwrap_or_default() / 2;
        let at_ms = at.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64() * 1000.0;
        Some(serde_json::json!({
            "position": position,
            "atMs": at_ms,
            "speed": self.player.speed(),
            "paused": !self.is_playing.load(Ordering::Relaxed),
        }))
    }

    /// ── stream: instant ── A picked track is waiting for its stream ([`Self::show_loading`]).
    pub fn is_loading(&self) -> bool {
        self.loading.load(Ordering::SeqCst)
    }

    /// Latest mpv position (secs) — for OS scrubber updates + relative media-key seeks.
    pub fn current_position(&self) -> f64 {
        f64::from_bits(self.latest_position.load(Ordering::SeqCst))
    }

    /// Advance/rewind the queue (OS "next"/"previous" keys + the UI's skip buttons). `play_index`
    /// itself no-ops for guests.
    pub async fn next_in_queue(self: &std::sync::Arc<Self>) {
        self.next_in_queue_local().await;
    }

    /// [`Self::next_in_queue`] on this desktop's own queue, never sent to a linked device: for
    /// callers that mean the queue here, not whatever the player bar shows.
    async fn next_in_queue_local(self: &std::sync::Arc<Self>) {
        let i = {
            let q = self.queue.lock().await;
            // Manual next escapes a repeat-one loop (the next track then loops too), so treat One
            // as All here: with any repeat engaged the queue wraps instead of dead-ending.
            let repeat = if q.repeat == RepeatMode::One { RepeatMode::All } else { q.repeat };
            next_playable_index(&q.items, q.current, repeat)
        };
        match i {
            Some(i) => self.play_index(i).await,
            // Repeat off at the tail. Autoplay tops the queue up asynchronously (on track start /
            // track end), so the continuation may simply not have landed yet — fetch it now rather
            // than leaving Skip a dead button. Still a no-op when autoplay is off, when the user is
            // a guest, or when the radio returns nothing: there genuinely is no next track then.
            None => {
                let gen = self.generation.load(Ordering::SeqCst);
                if self.extend_queue_radio(gen).await > 0 {
                    let next = self.queue.lock().await.current + 1;
                    self.play_index(next).await;
                }
            }
        }
    }

    /// Previous: rewind first, step back second (issue #187). Past the first few seconds of a
    /// track, Previous restarts it, the convention every other transport follows, so reaching the
    /// earlier track means pressing it again from the top. Guests fall through to `play_index`,
    /// which is where they get the host-only hint.
    pub async fn prev_in_queue(self: &std::sync::Arc<Self>) {
        if self.current_position() > PREV_REWIND_SECS {
            let _ = self.user_seek(0.0).await;
            return;
        }
        // Nothing in front of the head of the queue, so the last thing actually played is in
        // whatever this queue replaced. Falls through to the restart below when there is none.
        if self.restore_prev_context().await {
            return;
        }
        let i = self.queue.lock().await.current.saturating_sub(1);
        self.play_index(i).await;
    }

    /// Previous at the head of a queue that replaced another one: put that one back, at the track
    /// and position it was left at. Returns whether it happened.
    ///
    /// Only from the head. Anywhere else Previous has a track of its own to step back to, and the
    /// queue panel shows that order, so stepping somewhere it doesn't show would be a surprise.
    ///
    /// Also the queue panel's "Back to …" line (`prev_track_title`), which is why this is public:
    /// that button isn't Previous, so it works however long the new track has been playing.
    pub async fn restore_prev_context(self: &std::sync::Arc<Self>) -> bool {
        let prev = {
            let mut q = self.queue.lock().await;
            if q.current != 0 {
                return false;
            }
            match q.prev_context.take() {
                Some(prev) => prev,
                None => return false,
            }
        };
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (video_id, position) = self.queue.lock().await.restore_context(prev);
        // `start_current` consumes this, and only for this exact track.
        if position > 1.0 {
            *self.pending_seek.lock().unwrap() = Some((video_id, position));
        }
        // Until mpv ticks, `current_position` would still report the track we just left.
        self.latest_position.store(0f64.to_bits(), Ordering::SeqCst);
        if self.start_current(gen).await {
            self.prime_lookahead(gen).await;
        }
        true
    }

    /// Tell the UI the queue changed.
    ///
    /// A Tauri event is delivered by inlining its JSON into a JavaScript source string and
    /// `eval`ing it in every listening webview, so the payload is *parsed as source*: 2.6 MB and
    /// ~26 ms for a 5,000-track queue, twice over with the mini player open. The play pointer
    /// moving is by far the most common reason this is called and it does not need the rows at
    /// all, so when the track list is unchanged this sends a few hundred bytes instead. The UI
    /// keeps the array it already holds (`player.svelte.ts`, `onQueueIndex`).
    async fn emit_queue(&self) {
        let q = self.queue.lock().await;
        let fingerprint = queue_fingerprint(&q.items);
        let unchanged =
            self.last_queue_fingerprint.swap(fingerprint, Ordering::Relaxed) == fingerprint;
        let payload = if unchanged {
            serde_json::json!({
                "currentIndex": q.current,
                "shuffle": q.shuffle_orig.is_some(),
                "repeat": q.repeat,
                "sourceName": &q.source_name,
                "sourceId": &q.source_id,
                "prevTrack": prev_track_title(&q),
                // The playing row, so `start_current`'s duration/artists backfill still reaches
                // the panel without shipping the other 4,999 rows to carry it.
                "current": q.items.get(q.current),
                "autoplay": self.autoplay_enabled(),
            })
        } else {
            serde_json::json!({
                "items": &q.items,
                "currentIndex": q.current,
                "shuffle": q.shuffle_orig.is_some(),
                "repeat": q.repeat,
                "sourceName": &q.source_name,
                "sourceId": &q.source_id,
                "prevTrack": prev_track_title(&q),
                "autoplay": self.autoplay_enabled(),
                // ── stream: polish ── videoId → "explicit" | "unavailable" (grey these rows).
                "skipReasons": crate::polish::explicit::reasons(&self.db, &q.items),
            })
        };
        self.emit_own(if unchanged { "queue-index" } else { "queue-changed" }, payload);
    }

    /// A pure tail append: ship the added rows, not the whole list.
    ///
    /// `emit_queue` sends every row whenever the fingerprint changed, and an autoplay top-up
    /// changes it every time. Since the list only grew at the end, the panel can push the new rows
    /// onto the array it already holds. `len` is what the queue is now, so a UI that missed an
    /// event can tell (its own length will not match) and re-sync with `get_queue` instead of
    /// drifting silently.
    async fn emit_queue_appended(&self, added: usize) {
        let q = self.queue.lock().await;
        let start = q.items.len().saturating_sub(added);
        let payload = serde_json::json!({
            "items": &q.items[start..],
            "len": q.items.len(),
            "currentIndex": q.current,
        });
        // Load-bearing: without it the next `emit_queue` thinks the rows are unchanged and sends a
        // `queue-index` for a list that grew, leaving the panel stale.
        self.last_queue_fingerprint.store(queue_fingerprint(&q.items), Ordering::Relaxed);
        self.emit_own("queue-appended", payload);
    }

    fn emit_error(&self, video_id: &str, message: &str) {
        tracing::error!(video_id, message, "playback error");
        let _ = self
            .host
            .emit("playback-error", serde_json::json!({ "videoId": video_id, "message": message }));
    }

    /// A transient toast. Same channel as [`Self::emit_skip`], for messages that phrase themselves.
    fn emit_notice(&self, message: &str) {
        tracing::info!(message, "playback notice");
        let _ = self.host.emit("playback-notice", serde_json::json!({ "message": message }));
    }

    /// A track was auto-skipped because no client could resolve it — a transient toast, not the
    /// persistent error banner: the queue keeps playing, so this shouldn't read as a failure.
    /// `reason` is the parenthetical the toast shows. "unavailable" for anything YouTube simply
    /// would not serve; an upload gets its own, because the song is not gone, the session is.
    fn emit_skip(&self, title: &str, reason: &str) {
        tracing::warn!(title, reason, "skipping unplayable track");
        let _ = self.host.emit(
            "playback-notice",
            serde_json::json!({ "message": format!("Skipped ({reason}): {title}") }),
        );
    }

    pub async fn queue_snapshot(&self) -> serde_json::Value {
        let q = self.queue.lock().await;
        serde_json::json!({
            "items": &q.items,
            "currentIndex": q.current,
            "shuffle": q.shuffle_orig.is_some(),
            "repeat": q.repeat,
            "sourceName": &q.source_name,
            "sourceId": &q.source_id,
            "prevTrack": prev_track_title(&q),
            "autoplay": self.autoplay_enabled(),
            // ── stream: polish ── videoId → "explicit" | "unavailable": rows playback skips.
            "skipReasons": crate::polish::explicit::reasons(&self.db, &q.items),
        })
    }

    // ── stream: polish ── the queue's source fields and playing row, for the context registry.
    pub async fn queue_source(&self) -> (Option<String>, Option<String>, Option<SongItem>) {
        let q = self.queue.lock().await;
        (q.source_id.clone(), q.source_name.clone(), q.items.get(q.current).cloned())
    }
    // ── end stream: polish ──

    // ── stream: canonical ── music videos in generated rows become their songs, off the hot path.
    /// Look `pending` (generated video rows, from `canonical::prepare_db`) up in the background
    /// and put the answers into the queue ahead of the play position. Playback never waits.
    pub fn canonical_spawn(self: &std::sync::Arc<Self>, pending: Vec<SongItem>) {
        if pending.is_empty() {
            return;
        }
        // ── stream: reliability ── deferred a few seconds and coalesced (see
        // `canonical::DEFERRED`), then only the rows still upcoming are looked up.
        let start = crate::canonical::DEFERRED.lock().map(|mut d| d.push(pending)).unwrap_or(false);
        if !start {
            return;
        }
        let me = self.clone();
        crate::rt::spawn(async move {
            loop {
                tokio::time::sleep(crate::canonical::POST_PLAY_DEFER).await;
                let batch = crate::canonical::DEFERRED.lock().map(|mut d| d.take()).unwrap_or_default();
                if batch.is_empty() {
                    return;
                }
                let upcoming: std::collections::HashSet<String> = {
                    let q = me.queue.lock().await;
                    let after = q.items.iter().skip(q.current + 1);
                    after.chain(q.shuffle_orig.iter().flatten()).map(|s| s.video_id.clone()).collect()
                };
                let batch: Vec<SongItem> =
                    batch.into_iter().filter(|s| upcoming.contains(&s.video_id)).collect();
                if batch.is_empty() {
                    continue;
                }
                let Some(r) = crate::canonical::Resolver::of(&me) else { continue };
                let found = r.lookup_many(&batch).await;
                me.canonical_apply(&found).await;
            }
        });
        // ── end stream: reliability ──
    }

    /// Apply lookup answers to the upcoming rows (and the pre-shuffle copy). The playing row and
    /// hand-queued rows stay as they are. Re-primes the gapless next when it moved.
    pub async fn canonical_apply(
        self: &std::sync::Arc<Self>,
        found: &std::collections::HashMap<String, Option<SongItem>>,
    ) {
        let stale = {
            let mut q = self.queue.lock().await;
            let q = &mut *q;
            let cur = q.current;
            let playing = q.items.get(cur).map(|i| i.video_id.clone());
            let first = crate::canonical::apply(&mut q.items, cur + 1, playing.as_deref(), found);
            if let Some(orig) = q.shuffle_orig.as_mut() {
                crate::canonical::apply(orig, 0, playing.as_deref(), found);
            }
            let Some(first) = first else { return };
            let stale = q.lookahead_loaded.is_some_and(|i| i >= first);
            if stale {
                q.lookahead_loaded = None;
                let _ = self.player.clear_playlist();
            }
            stale
        };
        self.emit_queue().await;
        self.persist_queue().await;
        if stale {
            self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
        }
    }
    // ── end stream: canonical ──

    // ── stream: smart2 ── Smart Shuffle: recommended rows in the upcoming playlist part.
    pub async fn shuffle_is_on(&self) -> bool {
        self.queue.lock().await.shuffle_orig.is_some()
    }

    /// Upcoming rows of the playlist itself (not queued by hand, not autoplay).
    pub async fn upcoming_playlist_len(&self) -> usize {
        let q = self.queue.lock().await;
        q.items.iter().skip(q.current + 1).filter(|i| !i.queued && !i.queued_end && !i.autoplay).count()
    }

    /// Insert `recs` into the upcoming playlist rows, one after every `every` of them. They are
    /// not in the pre-shuffle snapshot, so turning shuffle off drops them. Returns how many went in.
    pub async fn smart_insert(self: &std::sync::Arc<Self>, recs: Vec<SongItem>, every: usize) -> usize {
        let every = every.max(1);
        let n = {
            let mut q = self.queue.lock().await;
            let mut at = (q.current + 1).min(q.items.len());
            while q.items.get(at).is_some_and(|i| i.queued || i.queued_end) {
                at += 1;
            }
            let mut recs = recs.into_iter();
            let mut n = 0;
            let mut run = 0;
            while at < q.items.len() && !q.items[at].autoplay {
                run += 1;
                at += 1;
                if run == every {
                    match recs.next() {
                        Some(r) => {
                            q.items.insert(at, r);
                            at += 1;
                            n += 1;
                            run = 0;
                        }
                        None => break,
                    }
                }
            }
            if n > 0 && q.lookahead_loaded.take().is_some() {
                let _ = self.player.clear_playlist();
            }
            n
        };
        if n > 0 {
            self.emit_queue().await;
            self.persist_queue().await;
            self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
        }
        n
    }

    /// Drop upcoming rows whose id is in `ids` (Smart Shuffle off, or one suggestion removed).
    pub async fn smart_remove(self: &std::sync::Arc<Self>, ids: &std::collections::HashSet<String>) -> usize {
        let rows: Vec<usize> = {
            let q = self.queue.lock().await;
            (q.current + 1..q.items.len()).filter(|&i| ids.contains(&q.items[i].video_id)).collect()
        };
        for &i in rows.iter().rev() {
            self.remove_from_queue_local(i).await;
        }
        rows.len()
    }
    // ── end stream: smart2 ──

    /// A position tick from mpv. Once the current track passes the play threshold (context/01
    /// §registerPlayback) this counts the play locally (the Your Rotation playlist) and fires the
    /// watch-history ping, latched to happen exactly once per play. The ping is additionally
    /// gated on the `enable_history` setting + being logged in. Best-effort (errors logged).
    pub async fn on_position(&self, pos: f64) {
        if pos > 0.0 {
            self.fail_streak.store(0, Ordering::Relaxed); // it plays: the failure run is over
        }
        self.record_position(pos);
        self.hear(pos);
        // ── stream: podcasts ── episode resume/played state; SponsorBlock asks for a seek.
        if let Some(to) = crate::podcasts::progress::on_tick(pos) {
            let _ = self.player.seek(to);
        }
        // ── end stream: podcasts ──
        // Latched: nothing below can fire again for this play, so don't queue up behind the
        // queue mutex on every tick just to be told so. See `AppState::history_pinged`.
        if self.history_pinged.load(Ordering::Relaxed) {
            return;
        }
        let crossed = {
            let q = self.queue.lock().await;
            // `swap` and not a store: the early-return above reads the flag outside the lock,
            // so the latch has to be the thing that decides, not the read. Latches even if the
            // ping below is missing or fails, so it is never retried.
            if pos >= history_threshold(q.duration)
                && !self.history_pinged.swap(true, Ordering::Relaxed)
            {
                Some((q.playback_ping.clone(), q.cpn.clone(), q.items.get(q.current).cloned()))
            } else {
                None
            }
        };
        let Some((ping, cpn, played)) = crossed else { return };
        // stream: library: a private session records nothing, here or on YouTube (library.rs).
        if crate::library::is_private(&self.db) {
            return;
        }

        // Local play count (Your Rotation, the stats page), on the same threshold. Deliberately not
        // gated on `enable_history` or sign-in: that setting is about registering plays with
        // YouTube, while this never leaves the machine and has to work signed out. Local files
        // count too; Your Rotation leaves them out when it reads.
        // ponytail: resuming a restored track from past the threshold counts it a second time (the
        // watch-history ping has always done the same). One extra count per launch, for one song.
        // If it ever skews the stats, pass the `pending_seek` offset into the latch and skip the
        // record when the play didn't start near zero.
        // ── stream: polish ── stamp the context this listen came from (Recents, Home shortcuts).
        let ctx = match played.as_ref() {
            Some(item) => {
                let (sid, sname) = {
                    let q = self.queue.lock().await;
                    (q.source_id.clone(), q.source_name.clone())
                };
                crate::polish::context::on_listen(&self.db, Some(&*self.host), item, sid.as_deref(), sname.as_deref())
            }
            None => None,
        };
        // ── end stream: polish ──
        if let Some(item) = played {
            self.count_listen(&item, ctx.as_ref());
        }

        if !self.history_enabled() || !self.it.is_logged_in() {
            return;
        }
        // Say so when there's nothing to ping. This used to be a silent `return`, which is why
        // issue #83 could only be diagnosed by reading the source: the play was dropped with no
        // trace anywhere. Anything above `debug` is noise on a working install, but the line
        // exists now.
        let Some(ping) = ping else {
            tracing::info!(
                "no watch-history ping for this play (no client returned a tracking URL)"
            );
            return;
        };
        // Ping as the client that was issued the URL, not always the metadata one.
        let Some(client) = self.clients.get(&ping.client).cloned() else {
            tracing::warn!(client = %ping.client, "watch-history ping: unknown client");
            return;
        };
        let it = self.it.clone();
        crate::rt::spawn(async move {
            match it.register_playback(&client, &ping.url, &cpn, None).await {
                Ok(()) => tracing::debug!(client = %ping.client, "watch-history ping sent"),
                Err(e) => tracing::warn!(error = %e, "watch-history ping failed"),
            }
        });
    }

    /// Latest mpv-reported track duration (secs), feeding the history-ping threshold + OS scrubber.
    pub async fn on_duration(&self, secs: f64) {
        crate::podcasts::progress::on_duration(secs); // ── stream: podcasts ──
        if secs.is_finite() && secs > 0.0 {
            self.fail_streak.store(0, Ordering::Relaxed); // the file opened
            self.queue.lock().await.duration = secs;
            let speed = self.player.speed();
            if let Some(m) = &self.media {
                m.set_duration(secs, speed);
            }
        }
    }

    /// Watch-history ping enabled? Default on; only an explicit `"false"` disables it.
    fn history_enabled(&self) -> bool {
        self.db.get_setting("enable_history").map(|v| v != "false").unwrap_or(true)
    }

    /// Shuffle sticky across queues? Default off: turning shuffle on applies to the queue that's
    /// playing, and opening an album/playlist afterwards plays it in order (issue #117).
    fn sticky_shuffle(&self) -> bool {
        self.db.get_setting("sticky_shuffle").as_deref() == Some("true")
    }

    /// Shuffle mixes what was added with "Add to queue" into the playlist, instead of keeping it
    /// behind the playlist in its own order. Default off.
    fn shuffle_whole_queue(&self) -> bool {
        self.db.get_setting("shuffle_whole_queue").as_deref() == Some("true")
    }

    /// Autoplay enabled? Default on; only an explicit `"false"` disables it (mirrors
    /// `history_enabled`).
    fn autoplay_enabled(&self) -> bool {
        self.db.get_setting("autoplay").map(|v| v != "false").unwrap_or(true)
    }

    /// Reorder a radio page so the songs you heard lately come last. YouTube ranks a radio the same
    /// way every time it is asked for the same seed, so every radio on a song, and every autoplay
    /// after the same playlist, used to open on the same handful of tracks.
    pub(crate) fn freshen_radio(&self, items: &mut [SongItem]) {
        const RECENT: i64 = 30 * 24 * 3600;
        let heard = self.db.last_played(now_secs() - RECENT).into_iter().collect();
        freshen(items, &heard);
        crate::smart::recs::rerank_radio(&self.db, items, &heard); // ── stream: smart ── taste
    }

    /// [`Self::freshen_radio`] for autoplay: the same unheard-first order, then taste without the
    /// co-vote term (bench5, docs/streams/autoplay.md).
    fn freshen_autoplay(&self, items: &mut [SongItem]) {
        const RECENT: i64 = 30 * 24 * 3600;
        let heard = self.db.last_played(now_secs() - RECENT).into_iter().collect();
        freshen(items, &heard);
        crate::smart::recs::rerank_autoplay(&self.db, items, &heard);
    }

    /// Lift the hold `play_song` put on the autoplay trigger while it hydrated a radio, unless a
    /// newer queue has taken it over in the meantime (its own `play_song` clears it).
    async fn end_hydration(&self, gen: u64) {
        let mut q = self.queue.lock().await;
        if q.hydrating == Some(gen) {
            q.hydrating = None;
        }
    }

    /// Extend the queue with radio continuation when it's nearly out (autoplay). Returns how many
    /// tracks were appended. Guards: setting on, repeat Off, not a guest, tail near (last two
    /// tracks), generation unchanged across the network call. Continuation matches where the queue
    /// came from: playlist/album radio (`radio_seed`) or song radio seeded from the last track.
    /// Dedupes against the entire current queue; caps at `AUTOPLAY_BATCH` per hop. When the radio
    /// returns nothing new, playback later stops exactly as pre-autoplay (no retry loop).
    async fn extend_queue_radio(self: &std::sync::Arc<Self>, gen: u64) -> usize {
        const AUTOPLAY_BATCH: usize = 20;
        // ── stream: podcast-upnext ── an episode at the tail continues with episodes, never music.
        let podcast = self.queue.lock().await.items.last().is_some_and(crate::podcasts::is_episode_item);
        if podcast {
            return self.extend_podcasts(gen).await;
        }
        let plan = {
            let q = self.queue.lock().await;
            if q.hydrating == Some(gen) {
                // This queue's own `play_song` is already fetching its radio (#255). Keyed to the
                // generation: a marker left by a queue that has since been replaced (`play_index`
                // and the other load paths bump the generation without clearing it) must not stop
                // the new queue from topping itself up.
                return 0;
            }
            if q.repeat != RepeatMode::Off {
                return 0; // the queue never exhausts under repeat
            }
            if q.items.len().saturating_sub(q.current) > 2 {
                return 0; // tail not near yet
            }
            // ── stream: autoplay ── setting off, a podcast at the end, nothing to seed from:
            // the queue ends (`autoplay::seeds`).
            crate::autoplay::seeds(&q.items, self.autoplay_enabled(), q.radio_seed.as_deref())
        };
        let Some(plan) = plan else { return 0 };
        // ── stream: canonical ── every seed runs on its studio song, not a video or live cut.
        let mut anchors: Vec<SongItem> = Vec::with_capacity(plan.songs.len());
        for s in &plan.songs {
            anchors.push(crate::canonical::studio_seed(self, s).await.unwrap_or_else(|| s.clone()));
        }
        let Some(client) = self.clients.get(innertube::METADATA_CLIENT) else { return 0 };
        // The context's own radio (anchored on its last song) and the radios of its last few
        // songs, fetched together and fused (`autoplay::fuse`, bench5 task C).
        let last_id = anchors.first().map(|a| a.video_id.clone());
        let mut asks: Vec<(Option<String>, String)> = Vec::new();
        if let Some(pl) = &plan.pinned {
            asks.push((last_id.clone(), pl.clone()));
        }
        for a in &anchors {
            asks.push((Some(a.video_id.clone()), format!("RDAMVM{}", a.video_id)));
        }
        // Snapshot → network → re-lock, same discipline as `prime_lookahead`; the generation check
        // in `absorb_radio` is what makes it safe, and it dedupes against the queue as it is after
        // the fetch, so whatever grew it meanwhile can't come back as a duplicate.
        let pages = futures_util::future::join_all(
            asks.iter().map(|(v, pl)| self.it.next(client, v.as_deref(), Some(pl))),
        )
        .await;
        let mut ok: Vec<Vec<SongItem>> = Vec::new();
        for (r, (_, pl)) in pages.into_iter().zip(&asks) {
            match r {
                Ok(next) if !next.items.is_empty() => ok.push(next.items),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, seed = %pl, "autoplay radio fetch failed"),
            }
        }
        let label = asks.first().map(|(_, pl)| pl.clone()).unwrap_or_default();
        if !ok.is_empty() {
            let fresh = crate::autoplay::fuse(&ok);
            tracing::info!(
                seeds = asks.len(),
                pages = ok.len(),
                rows = fresh.len(),
                last = ?last_id,
                "autoplay: continuing from the queue's last songs"
            );
            let added = self.absorb_radio(fresh, gen, &label, AUTOPLAY_BATCH, &anchors).await;
            if added > 0 {
                return added;
            }
        }
        // A radio window can come back entirely blocked, or not at all. Re-requesting the same
        // seed from the same anchor returns the same page, so escalate the way `start_radio` does:
        // a song radio on the last track, which `fetch_radio` bounces through
        // `automixPreviewVideoRenderer` into a different mix. Once.
        if self.generation.load(Ordering::SeqCst) != gen {
            return 0; // user moved on; a second fetch would be thrown away too
        }
        if !ok.is_empty() && crate::blocked::list(&self.db).is_empty() {
            return 0;
        }
        let Some(last_video) = last_id else { return 0 };
        let song_seed = format!("RDAMVM{last_video}");
        let Ok((fresh, seed)) = self.fetch_radio(Some(&last_video), &song_seed).await else {
            return 0;
        };
        self.absorb_radio(fresh, gen, &seed, AUTOPLAY_BATCH, &anchors).await
    }

    // ── stream: podcast-upnext ──
    /// Autoplay for a queue ending on an episode (`podcasts::upnext`): your followed shows' new
    /// episodes, the shows you listen to most first, then one popular episode from a show like the
    /// one playing. The followed block lands at once (local); the discovery pick follows when the
    /// (cached) charts answer, in the background unless nothing else came.
    async fn extend_podcasts(self: &std::sync::Arc<Self>, gen: u64) -> usize {
        use crate::podcasts::upnext;
        if !self.autoplay_enabled() {
            return 0;
        }
        let (items, current) = {
            let q = self.queue.lock().await;
            if q.hydrating == Some(gen)
                || q.repeat != RepeatMode::Off
                || q.items.len().saturating_sub(q.current) > 2
            {
                return 0;
            }
            (q.items.clone(), q.current)
        };
        let seed = items.get(current).map(|s| s.artists.clone()).unwrap_or_default();
        let (mut taken, picks) = {
            let conn = self.db.podcasts_conn();
            let taken = upnext::taken(&conn, &items, current);
            let picks = upnext::followed(&conn, &taken);
            (taken, picks)
        };
        let added = self.append_upnext(gen, picks.iter().map(upnext::Pick::song).collect()).await;
        for p in &picks {
            taken.add(&p.episode);
        }
        upnext::add_follows(&self.db.podcasts_conn(), &mut taken);
        let me = self.clone();
        let discovery = async move {
            let Some(pick) = upnext::discover(&me.db, &seed, &taken).await else { return 0 };
            me.append_upnext(gen, vec![pick.song()]).await
        };
        if added > 0 {
            crate::rt::spawn(discovery);
            return added;
        }
        discovery.await
    }

    /// Append predicted episodes to the tail as autoplay rows (the panels' "Next up"), unless the
    /// queue moved on or Autoplay was switched off meanwhile.
    async fn append_upnext(self: &std::sync::Arc<Self>, gen: u64, rows: Vec<SongItem>) -> usize {
        if rows.is_empty() || self.generation.load(Ordering::SeqCst) != gen || !self.autoplay_enabled() {
            return 0;
        }
        let added = {
            let mut q = self.queue.lock().await;
            let existing: HashSet<String> = q.items.iter().map(|i| i.video_id.clone()).collect();
            merge_radio(&mut q.items, rows, existing, usize::MAX)
        };
        if added > 0 {
            tracing::info!(added, "podcast up next extended the queue");
            self.emit_queue_appended(added).await;
            self.persist_queue().await;
            self.prime_lookahead(gen).await;
        }
        added
    }
    // ── end stream: podcast-upnext ──

    /// Append a fetched radio page to the tail of the queue and run the bookkeeping that follows.
    /// Returns how many tracks landed. The generation check lives here because every caller was on
    /// the network right before it.
    async fn absorb_radio(
        self: &std::sync::Arc<Self>,
        mut fresh: Vec<SongItem>,
        gen: u64,
        seed: &str,
        cap: usize,
        anchors: &[SongItem],
    ) -> usize {
        if self.generation.load(Ordering::SeqCst) != gen {
            return 0; // user moved on while we fetched
        }
        // Switched off while we fetched: `autoplay_changed` has already cleared the tail, and
        // appending now would put back the tracks the switch just took away.
        if !self.autoplay_enabled() {
            return 0;
        }
        // ── stream: canonical ── songs only (no music videos, no versions the queue's last track
        // isn't), cache-only here; what needs a search is mapped once the batch has landed.
        // ── stream: autoplay ── the version filter runs against the studio seed, so a live cut
        // that started it doesn't let other live cuts in.
        let canon_last = match anchors.first() {
            Some(a) => Some(a.clone()),
            None => self.queue.lock().await.items.last().cloned(),
        };
        let mut canon_pending = crate::canonical::prepare_db(&self.db, &mut fresh, canon_last.as_ref());
        // ── end stream: canonical ──
        // ── stream: smart ── blocked songs/artists and (filter on) explicit never land.
        crate::smart::recs::filter_radio(&self.db, &mut fresh, None).await;
        // ── stream: autoplay ── never another song that only shares the seed's title (or is
        // named like its artist), and nothing heard in the last hour.
        crate::autoplay::drop_namesakes(&mut fresh, anchors);
        let recent: HashSet<String> = self
            .db
            .last_played(now_secs() - crate::autoplay::RECENT_SECS)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        crate::autoplay::drop_recent(&mut fresh, &recent);
        // Before the cap: the batch that lands is the songs you haven't heard, when there are any,
        // then taste (`recs::rerank_autoplay`).
        self.freshen_autoplay(&mut fresh);
        let (added, trimmed) = {
            let mut q = self.queue.lock().await;
            // ── stream: canonical ── not a song the queue already holds in another version.
            crate::canonical::drop_known(&mut fresh, &q.items);
            // Against the queue as it is *now*, not a snapshot from before the fetch: a playlist
            // fill, a guest add or `play_song`'s own hydration can all have appended something
            // while we were on the network, and dropping a stale set on top of it duplicates.
            let existing: HashSet<String> = q.items.iter().map(|i| i.video_id.clone()).collect();
            let added = merge_radio(&mut q.items, fresh, existing, cap);
            // ── stream: canonical ── only the videos that landed are worth a search.
            canon_pending.retain(|p| q.items.iter().any(|i| i.video_id == p.video_id));
            // Not while shuffle is on: `shuffle_orig` is a parallel clone of the list and rebasing
            // both consistently is a bigger change than this one. Deliberate, see plan 039.
            let trimmed = if added > 0 && q.shuffle_orig.is_none() {
                let cur = q.current;
                let dropped = trim_played(&mut q.items, cur, KEEP_PLAYED);
                if dropped > 0 {
                    q.current -= dropped;
                    // Only ever set to `current` or `current + 1`, so this never underflows.
                    q.lookahead_loaded = q.lookahead_loaded.map(|i| i.saturating_sub(dropped));
                }
                dropped
            } else {
                0
            };
            (added, trimmed)
        };
        if added > 0 {
            tracing::info!(added, trimmed, seed = %seed, "autoplay extended the queue");
            if trimmed == 0 {
                self.emit_queue_appended(added).await;
            } else {
                self.emit_queue().await; // the front moved, so indices shifted: send the truth
            }
            self.persist_queue().await;
            // Appending at the tail never touches a primed lookahead slot; this covers the
            // "current was last, nothing was primed" case.
            self.prime_lookahead(gen).await;
            self.canonical_spawn(canon_pending); // ── stream: canonical ──
        }
        added
    }

    /// Persist the queue so a restart can restore it losslessly (context/11 §state). Called
    /// whenever the queue changes or advances.
    ///
    /// The blob is 526 bytes per track (measured), so a 5,000-track queue is a ~2.6 MB
    /// synchronous write, and the advance path calls this on every single track. On an advance
    /// the rows are identical and only the pointer moved, so that case writes ~40 bytes to
    /// `queue_index` instead. `queue_index` is written every time, blob or not, and carries the
    /// fingerprint of the rows it was true for, so `restore_queue` can tell a fresher pointer from
    /// one left over from a different item list.
    async fn persist_queue(&self) {
        let (index_json, blob) = {
            let q = self.queue.lock().await;
            let fingerprint = queue_fingerprint(&q.items);
            let index_json = serde_json::json!({
                "current": q.current,
                "repeat": q.repeat,
                // Which item list these numbers were true for. `queue_json` and `queue_index` are
                // two separate writes, so a kill between them, or two `persist_queue` calls
                // landing out of order, can pair a pointer with rows it was never valid for.
                // `restore_queue` checks this before trusting any of it.
                "fp": fingerprint.to_string(),
            })
            .to_string();
            let persist_fp = persist_fingerprint(&q);
            let changed =
                self.last_persisted_fingerprint.swap(persist_fp, Ordering::Relaxed) != persist_fp;
            let blob = changed.then(|| {
                serde_json::json!({
                    "items": &q.items,
                    "current": q.current,
                    "repeat": q.repeat,
                    "shuffleOrig": &q.shuffle_orig,
                    "radioSeed": &q.radio_seed,
                    "sourceName": &q.source_name,
                    "sourceId": &q.source_id,
                    "radio": q.radio,
                })
                .to_string()
            });
            (index_json, blob)
        };
        if let Some(blob) = blob {
            self.db.set_setting("queue_json", &blob);
        }
        self.db.set_setting("queue_index", &index_json);
    }

    /// Restore the last session's queue on startup — paused, not autoplaying (context/11). The
    /// saved position is applied when the user first hits play (see `start_current`). Emits
    /// `queue-changed` + `now-playing` so the UI shows the restored track.
    pub async fn restore_queue(self: &std::sync::Arc<Self>) {
        let Some(json) = self.db.get_setting("queue_json") else { return };
        let Ok(saved) = serde_json::from_str::<serde_json::Value>(&json) else { return };
        let items: Vec<SongItem> = saved
            .get("items")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if items.is_empty() {
            return;
        }
        let mut current = (saved.get("current").and_then(|v| v.as_u64()).unwrap_or(0) as usize)
            .min(items.len() - 1);
        // Shuffle/repeat ride the same blob; read tolerantly — old blobs lack them.
        let mut repeat: RepeatMode = saved
            .get("repeat")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let shuffle_orig: Option<Vec<SongItem>> =
            saved.get("shuffleOrig").and_then(|v| serde_json::from_value(v.clone()).ok()).flatten();
        let radio_seed: Option<String> =
            saved.get("radioSeed").and_then(|v| v.as_str()).map(str::to_owned);
        let source_name: Option<String> =
            saved.get("sourceName").and_then(|v| v.as_str()).map(str::to_owned);
        let source_id: Option<String> =
            saved.get("sourceId").and_then(|v| v.as_str()).map(str::to_owned);
        let radio = saved.get("radio").and_then(|v| v.as_bool()).unwrap_or(false);
        // `queue_index` is rewritten on every persist while `queue_json` is only rewritten when
        // the rows change, so on an advance it is the fresher of the two. But the two are separate
        // writes: a kill between them, or two `persist_queue` calls landing out of order (a
        // background playlist fill racing a track advance), can leave a `queue_index` on disk that
        // was computed against a different item list than the one in `queue_json` now. Trusting it
        // blindly would pair a pointer with rows it was never valid for and `.min(items.len() - 1)`
        // would turn that into a plausible-looking wrong track instead of anything visible. The
        // `fp` stamp is what the index was true for; only apply it when it still matches what we
        // just parsed. A mismatch (or a database written before the stamp existed) falls back to
        // the blob's own current/repeat, which is exactly today's behaviour.
        let items_fp = queue_fingerprint(&items).to_string();
        if let Some(idx) = self
            .db
            .get_setting("queue_index")
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .filter(|idx| idx.get("fp").and_then(|v| v.as_str()) == Some(items_fp.as_str()))
        {
            if let Some(c) = idx.get("current").and_then(|v| v.as_u64()) {
                current = (c as usize).min(items.len() - 1);
            }
            if let Some(r) = idx.get("repeat").and_then(|v| serde_json::from_value(v.clone()).ok())
            {
                repeat = r;
            }
        }
        let live = self.db.get_setting("live_video").as_deref() == Some(&items[current].video_id);
        let pos = self.db.get_setting("queue_position").and_then(|s| s.parse::<f64>().ok());
        // A broadcast resumes at its live edge, so its saved position means nothing.
        if let Some(p) = pos.filter(|p| *p > 0.0 && !live) {
            *self.pending_seek.lock().unwrap() = Some((items[current].video_id.clone(), p));
            // Also what the seekbar shows, and what an exit before the first play saves back.
            // Left at 0, quitting without pressing play overwrote the saved position (#502).
            self.latest_position.store(p.to_bits(), Ordering::SeqCst);
        }
        {
            let mut q = self.queue.lock().await;
            q.current = current;
            q.items = items;
            q.repeat = repeat;
            q.shuffle_orig = shuffle_orig;
            q.radio_seed = radio_seed;
            q.source_name = source_name;
            q.source_id = source_id;
            q.radio = radio;
            q.live = live;
        }
        if repeat == RepeatMode::One {
            let _ = self.player.set_loop_file(true);
        }
        if let Some(item) = self.current_item().await {
            // Restored, not playing — announce the track but leave playback paused. Declare the
            // paused state *first*: mpv reports `pause: false` while idle at boot, so a track
            // announced before this would briefly look like it was playing (and put a presence card
            // up for a song nobody started).
            self.media_set_playing(false);
            self.emit_now_playing(&item, "restored", live);
            // The stored rating is as old as the database: a like made on another device since
            // has never been seen here (issue #93).
            self.refresh_from_next(&item.video_id, self.generation.load(Ordering::SeqCst));
            self.emit_own("playback-state", "paused");
            // mpv has nothing loaded until play, so it reports neither: the bar gets the saved
            // position and the row's own length from here.
            self.emit_own("position", serde_json::json!({ "position": self.current_position() }));
            self.emit_own("duration", serde_json::json!({ "duration": row_secs(&item) }));
            // ── stream: firstaudio ── most sessions start with Play on this very track: resolve
            // it now, in the background lane, so the press starts from a cached URL and warm first
            // bytes instead of a cold `/player` round (2-3 s measured on an iPhone).
            if !live {
                let st = self.clone();
                crate::rt::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    if st.player.is_idle() {
                        st.hint_play(item).await;
                    }
                });
            }
        }
        self.emit_queue().await;
    }

    /// Starts the restored queue when playback on startup is enabled.
    ///
    /// Called after [`Self::restore_queue`]. Guests in a Listen Together room leave playback to the
    /// host, and an already active player must not be started a second time.
    pub async fn play_on_start(self: &std::sync::Arc<Self>) {
        let enabled = || self.db.get_setting("play_on_start").as_deref() == Some("true");
        if !enabled() {
            return;
        }
        let Some(item) = self.current_item().await else { return };
        UI_MOUNTED.notified().await;
        // Login launches can run before Wi-Fi or DNS is ready (#483); a local file needs no
        // network. The other checks come after the wait: in that minute the user can start
        // something, join a room as a guest or turn this off.
        if !crate::local::is_local_song(&item.video_id) && !crate::http::wait_for_network().await {
            return;
        }
        // A linked phone already playing keeps the music; routed, this index would land on its queue.
        if !enabled() || !self.player.is_idle() {
            return;
        }
        let current = self.queue.lock().await.current;
        self.play_index(current).await;
    }

    /// Throttled position persistence for resume-on-restart. Records the latest position always
    /// (for a precise flush on pause) and writes it to the DB at most every 5s.
    fn record_position(&self, pos: f64) {
        self.latest_position.store(pos.to_bits(), Ordering::SeqCst);
        let now = now_secs() as u64;
        if now.saturating_sub(self.last_pos_persist.load(Ordering::Relaxed)) >= 5 {
            self.last_pos_persist.store(now, Ordering::Relaxed);
            self.db.set_setting("queue_position", &pos.to_string());
        }
        // Update the OS scrubber (~1s), throttled separately from the DB write. Discord rides the
        // same tick — not to redraw its bar (it runs its own clock off the timestamps we pushed)
        // but so it can notice a seek and re-push. A tick is NOT proof of playback (mpv also fires
        // `time-pos` on seeks while paused), so ask mpv for the play state rather than assuming it.
        if now.saturating_sub(self.last_media_push.load(Ordering::Relaxed)) >= 1 {
            self.last_media_push.store(now, Ordering::Relaxed);
            // Never ask mpv anything here — this runs on the event pump, and `mpv_get_property` is
            // synchronous on mpv's core lock. `is_playing` is kept current by `PlayerEvent::Playing`,
            // which mpv now pushes (it derives from `idle-active`, not just `pause`).
            let playing = self.is_playing.load(Ordering::Relaxed);
            let speed = self.player.speed();
            if let Some(m) = &self.media {
                // Push when the play state flipped, or when the position moved further than one
                // tick of ordinary playback explains (a seek, a track change). Not every second:
                // see `last_media_state`. At a 1 s gate this settles into a push every 2-3
                // seconds, which keeps a polled scrubber within a couple of seconds of the truth.
                let mut last = self.last_media_state.lock().unwrap();
                let send = match *last {
                    None => true,
                    Some((p, at)) => p != playing || (pos - at).abs() > 2.0,
                };
                if send {
                    *last = Some((playing, pos));
                    drop(last);
                    m.set_playback(playing, pos, speed);
                }
            }
        }
    }

    /// Repeat-one started the track over (#518): a new play, so the local play count, the
    /// watch-history ping and the scrobble each fire again. mpv's `loop-file` sends no end-file,
    /// so nothing else resets them, and the whole loop used to count once. The duration is kept:
    /// it is the same file, and mpv won't report it again.
    pub async fn on_looped(&self) {
        let mut q = self.queue.lock().await;
        q.cpn = innertube::generate_cpn();
        self.history_pinged.store(false, Ordering::Relaxed);
        self.end_listen();
    }

    /// Flush the latest known position to the DB immediately (e.g. on pause), and the time heard
    /// of the play it belongs to.
    pub fn flush_position(&self) {
        crate::podcasts::progress::flush(); // ── stream: podcasts ──
        let pos = f64::from_bits(self.latest_position.load(Ordering::SeqCst));
        self.db.set_setting("queue_position", &pos.to_string());
        self.save_listen(&mut self.listen.lock().unwrap());
    }

    /// Credit the time since the last position tick to the play being heard (#396). mpv ticks
    /// several times a second while audio plays, so any other move is not listening: a jump back
    /// (a seek, the next file starting), a jump forward of seconds (a seek), or a tick while paused
    /// (a seek while paused). Written to the play's row every 30 s heard, so a crash loses at most
    /// that; pause and exit write it at once (`flush_position`).
    fn hear(&self, pos: f64) {
        let mut l = self.listen.lock().unwrap();
        if let Some(last) = l.last_pos {
            let step = pos - last;
            if step > 0.0 && step < 2.0 && self.is_playing.load(Ordering::Relaxed) {
                l.heard += step;
            }
        }
        l.last_pos = Some(pos);
        if l.heard - l.saved >= 30.0 {
            self.save_listen(&mut l);
        }
    }

    fn save_listen(&self, l: &mut Listen) {
        if let Some(row) = l.row.filter(|_| l.heard > l.saved) {
            if l.episode {
                self.db.set_episode_listen_secs(row, l.heard as i64);
            } else {
                self.db.set_listen_secs(row, l.heard as i64);
            }
            l.saved = l.heard;
        }
    }

    /// The play crossed the threshold (`on_position`): write its history row. Songs go into
    /// `listens` (stats, Your Rotation, taste); podcast episodes into `episode_listens`, which only
    /// Recents and the stats page's listening time read. Build 11 left episodes out entirely, so
    /// someone listening to podcasts saw an empty Recents and stats page.
    fn count_listen(&self, item: &SongItem, ctx: Option<&crate::polish::context::PlayContext>) {
        let Ok(json) = serde_json::to_string(item) else { return };
        let episode = crate::podcasts::is_episode_item(item); // ── stream: podcasts ──
        // An episode's show: the one it was played from, else the one it was listed under.
        let show = match ctx.filter(|c| c.kind == "podcast") {
            _ if !episode => None,
            Some(c) => Some(c.id.clone()),
            None => self.db.with_conn(|c| {
                c.query_row(
                    "SELECT show_id FROM podcast_episodes
                     WHERE (id = ?1 OR video_id = ?1) AND show_id IS NOT NULL LIMIT 1",
                    [&item.video_id],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            }),
        };
        let mut l = self.listen.lock().unwrap();
        let (now, heard) = (now_secs(), l.heard as i64);
        l.row = if episode {
            self.db.record_episode_listen(&item.video_id, &json, show.as_deref(), now, heard)
        } else {
            self.db.record_listen(&item.video_id, &json, now, heard)
        };
        l.episode = episode;
        l.saved = l.heard;
    }

    /// A new track starts: settle the play before it and count this one from zero.
    fn end_listen(&self) {
        let mut l = self.listen.lock().unwrap();
        self.save_listen(&mut l);
        let row = l.row.filter(|_| !l.episode); // taste reads `listens` rows only
        crate::smart::taste::track_ended(&self.db, l.heard, row); // ── stream: smart ──
        *l = Listen::default();
    }

    /// Clear both cache tiers (settings "Clear caches"): the SQLite URL cache + mpv's on-disk
    /// audio bytes. Best-effort on the files — the current track may re-buffer. context/14.
    pub fn clear_caches(&self) {
        self.db.clear_stream_cache();
        // The stored PoToken is a cache too, and "clear caches" is where someone goes when
        // playback has started behaving oddly. Dropping it costs one BotGuard bootstrap.
        self.db.delete_setting("potoken_session");
        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for e in entries.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    // --- Playback controls ----------------------------------------------------------------------

    /// Put the user's crossfade setting on the player. Per `Player::set_crossfade`, this takes
    /// effect from the next transition.
    pub async fn apply_crossfade(&self) {
        let secs = saved_crossfade(&self.db);
        // ── stream: polish ── held off next to a podcast episode (podcasts::progress).
        let secs = secs.filter(|_| !crate::podcasts::progress::crossfade_held());
        self.player.set_crossfade(secs);
    }

    /// User seek from the UI.
    pub async fn user_seek(&self, position: f64) -> Result<(), String> {
        if self.player.is_idle() {
            // Nothing loaded to seek, as with a track restored at launch: move where it will
            // start instead, which an exit before play also saves (#502).
            if let Some(item) = self.current_item().await {
                *self.pending_seek.lock().unwrap() = Some((item.video_id, position));
                self.latest_position.store(position.to_bits(), Ordering::SeqCst);
            }
            return Ok(());
        }
        self.player.seek(position).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Toggle shuffle on the current queue. ON: snapshot the order, then Fisher–Yates only the
    /// *upcoming* items. OFF: restore the snapshot, keeping the playing track, minus anything the
    /// shuffle already played (see [`unshuffled`]). Guests: host-only hint.
    /// ponytail: OFF restores from the snapshot — tracks added while shuffled are dropped from the
    /// restored order (still playing/played fine; snapshot semantics).
    pub async fn toggle_shuffle(self: &std::sync::Arc<Self>) {
        {
            let mut q = self.queue.lock().await;
            if q.items.is_empty() {
                return;
            }
            if q.shuffle_orig.is_some() {
                unshuffle_queue(&mut q);
            } else {
                q.shuffle_orig = Some(q.items.clone());
                let current = q.current;
                shuffle_upcoming(&mut q.items, current, self.shuffle_whole_queue());
            }
            // The primed lookahead almost certainly points at the wrong song now — drop it
            // unconditionally (cheap; re-primed below).
            if q.lookahead_loaded.take().is_some() {
                let _ = self.player.clear_playlist();
            }
        }
        self.emit_queue().await;
        self.persist_queue().await;
        let gen = self.generation.load(Ordering::SeqCst);
        self.prime_lookahead(gen).await;
        // Un-shuffling can leave nothing upcoming — a full shuffled pass through an album drops
        // every heard track from the restored order. Autoplay's own triggers only fire on track
        // start / track end, so without this the queue panel shows no continuation (and the gapless
        // lookahead has nothing to prime) until the current song runs out. No-op unless the tail is
        // actually near.
        self.extend_queue_radio(gen).await;
    }

    /// Set the repeat mode. Repeat-one is enforced by mpv's `loop-file` (seamless, no end-file
    /// event); all/off live in the queue-advance logic (`next_index`). Guests: host-only hint.
    pub async fn set_repeat(self: &std::sync::Arc<Self>, mode: RepeatMode) {
        self.update_repeat(|_| mode).await;
    }

    /// Advance repeat mode Off -> All -> One -> Off (the global hotkey; the UI cycles itself).
    pub async fn cycle_repeat(self: &std::sync::Arc<Self>) {
        self.update_repeat(|mode| match mode {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        })
        .await;
    }

    /// The new mode and mpv's `loop-file` are written under one queue lock: a held hotkey fires
    /// several of these at once, and splitting the read from the write lost presses or left mpv
    /// looping a mode the queue no longer had.
    async fn update_repeat(
        self: &std::sync::Arc<Self>,
        next: impl FnOnce(RepeatMode) -> RepeatMode,
    ) {
        {
            let mut q = self.queue.lock().await;
            q.repeat = next(q.repeat);
            let _ = self.player.set_loop_file(q.repeat == RepeatMode::One);
        }
        self.emit_queue().await; // carries the new repeat state to the UI
        self.persist_queue().await;
        // Repeat-all newly on while playing the last track: the wrap target needs priming.
        self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
    }

    /// "Play next" from a ⋯ menu: the tracks land at the "up next" boundary — right after the
    /// current song, behind any earlier manual adds (FIFO) — never buried at the end. `from` is
    /// the album/playlist they came from, for the queue panel's block heading.
    pub async fn play_next(
        self: &std::sync::Arc<Self>,
        items: Vec<SongItem>,
        from: Option<String>,
    ) {
        self.enqueue(items, true, from, None).await;
    }

    /// "Add to queue": the tracks go at the tail of the queue, behind the playing album or playlist
    /// and ahead of autoplay (see [`enqueue_at`]). `continuation` is the next-page token: the rest
    /// of a long playlist is walked in the background instead of adding only page one.
    pub async fn add_to_queue(
        self: &std::sync::Arc<Self>,
        items: Vec<SongItem>,
        from: Option<String>,
        continuation: Option<String>,
    ) {
        self.enqueue(items, false, from, continuation).await;
    }

    /// Shared body of "Play next" / "Add to queue". In a session, guests route through the host
    /// (suggest → auto-approve) and the host tags the add with their own name so the room sees who
    /// added it.
    async fn enqueue(
        self: &std::sync::Arc<Self>,
        items: Vec<SongItem>,
        next: bool,
        from: Option<String>,
        continuation: Option<String>,
    ) {
        if items.is_empty() {
            return;
        }
        self.insert_queued(items, next, from.clone()).await;
        if let Some(token) = continuation {
            // After `insert_queued`: an add to an empty queue starts playback, which bumps the
            // generation the walk primes under.
            let gen = self.generation.load(Ordering::SeqCst);
            let epoch = self.queue.lock().await.epoch;
            let me = self.clone();
            tokio::spawn(
                async move { me.fill_playlist(epoch, gen, token, Fill::Queued(from)).await },
            );
        }
    }

    /// Splice a block of manually-queued tracks into the queue, then emit/persist/re-prime and (as
    /// host) broadcast. Shared by "Play next", "Add to queue" and approved guest suggestions.
    ///
    /// `next` ("Play next") marks them `queued` and puts them right behind the playing track,
    /// behind any earlier Play next ([`guest_insert_index`]). "Add to queue" marks them
    /// `queued_end` and puts them at the tail ([`enqueue_at`]). `from` names the album/playlist
    /// they came from, which keeps a walked playlist's later pages with their block.
    async fn insert_queued(
        self: &std::sync::Arc<Self>,
        mut items: Vec<SongItem>,
        next: bool,
        from: Option<String>,
    ) {
        if items.is_empty() {
            return;
        }
        for item in &mut items {
            item.queued = next;
            item.queued_end = !next;
            item.queued_from = from.clone();
            // Queued by hand, so it is the user's: switching Autoplay off leaves it be.
            item.autoplay = false;
        }
        let dedupe = self.db.get_setting("prevent_duplicates").as_deref() == Some("true");
        let was_empty = {
            let mut q = self.queue.lock().await;
            let was_empty = q.items.is_empty();
            let before = q.items.get(q.current + 1).map(|i| i.video_id.clone());
            // "Prevent duplicates" is a *move*, not a reject: every existing copy goes, then the
            // tracks are spliced in at the target. `at` is computed after, so a copy removed from
            // before the playing track doesn't push the insert one slot too far.
            let ids: HashSet<&str> = items.iter().map(|i| i.video_id.as_str()).collect();
            let qm = &mut *q; // the guard hands out one borrow; a struct ref splits per field
            let mut removed = dedupe && drop_duplicates(&mut qm.items, &mut qm.current, &ids);
            // Setting or not: a copy already waiting in the manual block is *moved*, never doubled.
            // "Play next" on a row you can see in the queue means move it up, and a second identical
            // row is no answer to that. Context/playlist rows are left alone — queueing one of those
            // is a deliberate copy.
            let cur = qm.current;
            let n = qm.items.len();
            let mut i = 0;
            qm.items.retain(|it| {
                let keep = i <= cur
                    || !((it.queued || it.queued_end) && ids.contains(it.video_id.as_str()));
                i += 1;
                keep
            });
            removed |= qm.items.len() != n;
            if removed {
                if let Some(orig) = qm.shuffle_orig.as_mut() {
                    // Off the snapshot too, or turning shuffle off rebuilds the queue with them.
                    let kept: HashSet<String> =
                        qm.items.iter().map(|i| i.video_id.clone()).collect();
                    orig.retain(|i| kept.contains(&i.video_id));
                }
            }
            let at = if next { guest_insert_index(&q.items, q.current) } else { enqueue_at(&q) };
            // The snapshot takes the new tracks too, or turning shuffle off would delete them. It
            // takes them in their real order: that's what un-shuffle restores.
            if let Some(orig) = q.shuffle_orig.as_mut() {
                orig.extend(items.iter().cloned());
                // Shuffle is on, so a block added from an album/playlist joins it right away
                // rather than waiting for the next toggle (same rule as `shuffle_upcoming`: the
                // block keeps its place, its own tracks get randomized).
                if from.is_some() {
                    use rand::seq::SliceRandom;
                    items.shuffle(&mut rand::thread_rng());
                }
            }
            if !next && q.shuffle_orig.is_some() && self.shuffle_whole_queue() {
                // Shuffling the whole queue: an add made while shuffled lands somewhere random in
                // the mix, between the Play next tracks and autoplay, like the rest of it.
                use rand::Rng;
                let lo = guest_insert_index(&q.items, q.current);
                for item in items {
                    let hi = enqueue_at(&q).max(lo);
                    let to = rand::thread_rng().gen_range(lo..=hi);
                    q.items.insert(to, item);
                }
            } else {
                q.items.splice(at..at, items);
            }
            // Drop the primed lookahead when what plays next moved (an append past the tail
            // retargets a primed repeat-all wrap from index 0 to the new item) or when a different
            // song now sits in the primed slot — otherwise the gapless advance plays the wrong one.
            let expected = q.next_playable(q.repeat);
            let after = q.items.get(q.current + 1).map(|i| i.video_id.clone());
            // `removed`: the removals renumbered the queue, so the recorded index no longer means
            // what it did. Cheaper to drop it and let the re-prime below sort it out.
            if q.lookahead_loaded.is_some()
                && (removed || q.lookahead_loaded != expected || before != after)
            {
                q.lookahead_loaded = None;
                let _ = self.player.clear_playlist();
            }
            was_empty
        };
        // Nothing was playing and nothing was queued: an add is the whole queue, so start it —
        // otherwise the tracks sit there paused and the click looks like it did nothing.
        if was_empty {
            self.play_index(0).await; // emits + persists + primes on its way
            return;
        }
        self.emit_queue().await;
        self.persist_queue().await;
        // Re-prime: replaces a dropped stale lookahead, and covers the insert-after-last case
        // (no lookahead existed because nothing was next). No-op when still primed correctly.
        self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
    }

    /// Remove an upcoming track from the queue (host's ✕ on guest adds; also plain local queue
    /// editing outside a session). The currently playing index can't be removed; guests can't
    /// remove anything (add-only).
    pub async fn remove_from_queue(self: &std::sync::Arc<Self>, index: usize) {
        self.remove_from_queue_local(index).await;
    }

    /// [`Self::remove_from_queue`] on this desktop's own queue, never sent to a linked device. A
    /// failed lookahead and a blocked artist name a row here; routed, the index would land on
    /// whatever row of the phone's queue has it.
    async fn remove_from_queue_local(self: &std::sync::Arc<Self>, index: usize) {
        let stale_lookahead = {
            let mut q = self.queue.lock().await;
            if index >= q.items.len() || index == q.current {
                return;
            }
            q.remove_row(index);
            if index < q.current {
                q.current -= 1;
            }
            match q.lookahead_loaded {
                // mpv holds the removed song as the gapless next — drop it. (Compared against the
                // recorded index, not `current + 1`, so a primed repeat-all wrap target is caught.)
                Some(i) if i == index => {
                    q.lookahead_loaded = None;
                    let _ = self.player.clear_playlist();
                    true
                }
                // The primed entry is the same song at a shifted index.
                Some(i) if i > index => {
                    q.lookahead_loaded = Some(i - 1);
                    false
                }
                _ => false,
            }
        };
        self.emit_queue().await;
        self.persist_queue().await;
        if stale_lookahead {
            self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
        }
    }

    /// Drop a freshly blocked artist out of the live queue, and skip off them if they are what is
    /// playing. Only upcoming tracks: history stays, because rewriting what you already heard is a
    /// lie, and `remove_from_queue` refuses `current` anyway.
    ///
    /// ponytail: reuses `remove_from_queue` per index instead of one bulk retain, so it emits and
    /// persists once per removed track. Blocking is a once-in-a-while click on a queue of tens, and
    /// that function is the only place the index rebase (`current`,
    /// `lookahead_loaded`, the mpv gapless entry, the LT broadcast) is written correctly. Write a
    /// bulk version only if a real queue makes this visibly slow.
    pub async fn purge_blocked(self: &std::sync::Arc<Self>, bl: &BlockList) -> usize {
        if bl.is_empty() {
            return 0;
        }
        let (upcoming, playing_blocked) = {
            let q = self.queue.lock().await;
            let upcoming: Vec<usize> = q.items[q.current.min(q.items.len())..]
                .iter()
                .enumerate()
                .filter(|(off, item)| *off > 0 && bl.blocks_song(item))
                .map(|(off, _)| q.current + off)
                .collect();
            (upcoming, q.items.get(q.current).is_some_and(|i| bl.blocks_song(i)))
        };
        // Descending: every removal shifts everything after it.
        for i in upcoming.iter().rev() {
            self.remove_from_queue_local(*i).await;
        }
        // After the purge, so the skip cannot land on another blocked track.
        if playing_blocked && !self.player.is_idle() {
            self.next_in_queue_local().await;
            // The purge can empty the tail: with repeat off and autoplay dead there is nothing to
            // advance into, and `next_in_queue` leaves the blocked track playing. Pausing is the
            // honest end of a queue that now holds nobody the user still wants to hear.
            let stuck = {
                let q = self.queue.lock().await;
                q.items.get(q.current).is_some_and(|i| bl.blocks_song(i))
            };
            if stuck {
                let _ = self.player.pause();
            }
        }
        if !upcoming.is_empty() || playing_blocked {
            tracing::info!(
                removed = upcoming.len(),
                skipped = playing_blocked,
                "purged a blocked artist from the queue"
            );
        }
        upcoming.len()
    }

    /// Drag-to-reorder in the queue panel: take the track at `from` and drop it at `to`. Only
    /// upcoming tracks move, and only among themselves — the playing track and the history stay
    /// where they are (both indices are clamped past `current`). Guests own no queue.
    ///
    /// ponytail: the other markers stay. A playlist track dragged in among the manual adds is still
    /// a playlist track, so Clear queue leaves it. Adopt-the-block-you-land-in only if that reads
    /// wrong in practice.
    pub async fn move_in_queue(self: &std::sync::Arc<Self>, from: usize, to: usize) {
        let stale_lookahead = {
            let mut q = self.queue.lock().await;
            let first = q.current + 1;
            if from < first || from >= q.items.len() || to < first || to >= q.items.len() {
                return;
            }
            if from == to {
                return;
            }
            let before = q.items.get(first).map(|i| i.video_id.clone());
            let mut item = q.items.remove(from);
            // A track you put somewhere yourself is yours: switching Autoplay off no longer takes
            // it, and the panel stops drawing it under the Autoplay divider.
            item.autoplay = false;
            q.items.insert(to, item);
            // Only what plays *next* can invalidate the primed gapless slot; a move deeper in the
            // queue leaves it alone rather than paying for a re-resolve on every drag.
            let expected = q.next_playable(q.repeat);
            let stale = q.lookahead_loaded.is_some()
                && (q.lookahead_loaded != expected
                    || before != q.items.get(first).map(|i| i.video_id.clone()));
            if stale {
                q.lookahead_loaded = None;
                let _ = self.player.clear_playlist();
            }
            stale
        };
        self.emit_queue().await;
        self.persist_queue().await;
        if stale_lookahead {
            self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
        }
    }

    /// Remove every upcoming track the user queued by hand, both "Play next" and "Add to queue"
    /// (the panel's Clear queue). Played/playing items and the playlist context stay.
    pub async fn clear_queued(self: &std::sync::Arc<Self>) {
        self.drop_upcoming(|item| item.queued || item.queued_end).await;
    }

    // ── lane magic-d2 ── drag and drop into the queue, and Undo for Clear queue.

    /// Songs dropped on the queue panel: they land at the drop line, `at` being the queue index the
    /// line sits in front of. The drop line only lives in "Next in queue" (Spotify), so `at` is
    /// clamped to the manual block: right after the playing track up to its end. Like any manual
    /// add, a copy already waiting in the manual block moves rather than doubles. An empty queue
    /// starts playing what was dropped, the way Add to queue does.
    pub async fn add_to_queue_at(
        self: &std::sync::Arc<Self>,
        items: Vec<SongItem>,
        at: usize,
        from: Option<String>,
    ) {
        if items.is_empty() {
            return;
        }
        if self.queue.lock().await.items.is_empty() {
            return self.insert_queued(items, false, from).await;
        }
        let dedupe = self.db.get_setting("prevent_duplicates").as_deref() == Some("true");
        {
            let mut q = self.queue.lock().await;
            let before = q.items.get(q.current + 1).map(|i| i.video_id.clone());
            let removed = splice_queued_at(&mut q, items, at, from, dedupe);
            let expected = q.next_playable(q.repeat);
            let after = q.items.get(q.current + 1).map(|i| i.video_id.clone());
            if q.lookahead_loaded.is_some()
                && (removed || q.lookahead_loaded != expected || before != after)
            {
                q.lookahead_loaded = None;
                let _ = self.player.clear_playlist();
            }
        }
        self.emit_queue().await;
        self.persist_queue().await;
        self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
    }

    /// Undo of Clear queue: the hand-queued rows the panel snapshotted come back right after the
    /// playing track, in their order and with their own markers (Play next / Add to queue / which
    /// album they came from). Rows queued again since are not doubled. Returns how many came back.
    pub async fn restore_queued(self: &std::sync::Arc<Self>, items: Vec<SongItem>) -> usize {
        let n = {
            let mut q = self.queue.lock().await;
            let n = restore_queued_rows(&mut q, items);
            if n > 0 && q.lookahead_loaded.take().is_some() {
                let _ = self.player.clear_playlist();
            }
            n
        };
        if n > 0 {
            self.emit_queue().await;
            self.persist_queue().await;
            self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
        }
        n
    }

    /// The Autoplay switch flipped. Off: the upcoming tracks autoplay brought in go, so the queue
    /// shows what will actually play. On: a tail that has already run low is topped up now, rather
    /// than when the next track starts and notices.
    pub async fn autoplay_changed(self: &std::sync::Arc<Self>, on: bool) {
        if on {
            let gen = self.generation.load(Ordering::SeqCst);
            self.extend_queue_radio(gen).await;
        } else {
            self.drop_upcoming(|item| item.autoplay).await;
        }
        // The switch's own state, even when no row came or went: the queue panels and a linked
        // phone show it.
        self.emit_queue().await;
    }

    /// Remove every upcoming track `pick` matches. Guests: add-only, no clearing.
    async fn drop_upcoming(self: &std::sync::Arc<Self>, pick: impl Fn(&SongItem) -> bool) {
        {
            let mut q = self.queue.lock().await;
            if !retain_upcoming(&mut q, |item| !pick(item)) {
                return; // nothing matched, don't touch the lookahead
            }
            // Indices shifted, so a primed lookahead may point at the wrong slot. Drop it
            // unconditionally (cheap; re-primed below), same as toggle_shuffle.
            if q.lookahead_loaded.take().is_some() {
                let _ = self.player.clear_playlist();
            }
        }
        self.emit_queue().await;
        self.persist_queue().await;
        self.prime_lookahead(self.generation.load(Ordering::SeqCst)).await;
    }

    /// The windows' playback events from this desktop's own player.
    fn emit_own<S: serde::Serialize + Clone>(&self, event: &str, payload: S) {
        let _ = self.host.emit(event, payload);
    }

}

/// Where an "Add to queue" lands (Spotify semantics, Encore): the end of the manual queue ("Next
/// in queue"), which sits right after the playing track and ahead of the rest of the album or
/// playlist that is playing ("Next from …") and of autoplay. Limusic used to put it behind the
/// playlist (#369); Spotify plays what you queued before it returns to the context.
fn enqueue_at(q: &QueueState) -> usize {
    manual_block_end(&q.items, q.current)
}

/// One past the manual block: the run of `queued` / `queued_end` rows right after `current`.
fn manual_block_end(items: &[SongItem], current: usize) -> usize {
    let mut at = (current + 1).min(items.len());
    while items.get(at).is_some_and(|i| i.queued || i.queued_end) {
        at += 1;
    }
    at
}

/// ── lane magic-d2 ── the queue side of a drop on the queue panel ([`AppState::add_to_queue_at`]).
/// `at` is the queue index the drop line is in front of, as the panel saw it. Copies already in
/// the manual block move (and with "prevent duplicates", every copy), so `at` is rebased past what
/// was removed in front of it, then clamped into "Next in queue". A row dropped among Play next
/// rows is Play next; anywhere else it is Add to queue. Returns whether rows were removed.
fn splice_queued_at(
    q: &mut QueueState,
    mut items: Vec<SongItem>,
    at: usize,
    from: Option<String>,
    dedupe: bool,
) -> bool {
    let ids: HashSet<String> = items.iter().map(|i| i.video_id.clone()).collect();
    let cur = q.current;
    let goes = |i: usize, it: &SongItem| {
        i != cur
            && ids.contains(&it.video_id)
            && (dedupe || (i > cur && (it.queued || it.queued_end)))
    };
    let gone_before = |end: usize| {
        q.items.iter().take(end).enumerate().filter(|(i, it)| goes(*i, it)).count()
    };
    let shift = gone_before(at);
    // `current` follows its own track down past anything removed in front of it.
    let shift_cur = gone_before(cur);
    let before = q.items.len();
    let mut i = 0;
    q.items.retain(|it| {
        let keep = !goes(i, it);
        i += 1;
        keep
    });
    let removed = q.items.len() != before;
    q.current = cur - shift_cur;
    let lo = (q.current + 1).min(q.items.len());
    let hi = manual_block_end(&q.items, q.current);
    let at = at.saturating_sub(shift).clamp(lo, hi);
    let next = at == lo || q.items.get(at - 1).is_some_and(|i| i.queued);
    for item in &mut items {
        item.queued = next;
        item.queued_end = !next;
        item.queued_from = from.clone();
        item.autoplay = false;
    }
    if let Some(orig) = q.shuffle_orig.as_mut() {
        if removed {
            let kept: HashSet<&str> = q.items.iter().map(|i| i.video_id.as_str()).collect();
            orig.retain(|i| kept.contains(i.video_id.as_str()));
        }
        orig.extend(items.iter().cloned());
    }
    q.items.splice(at..at, items);
    removed
}

/// ── lane magic-d2 ── Undo of Clear queue ([`AppState::restore_queued`]): the cleared rows go back
/// right after the playing track, markers intact. A row that is somehow not marked as hand-queued
/// comes back as Add to queue, so the next Clear queue takes it again. Rows already waiting in
/// the manual block (queued again before the Undo) are skipped. Returns how many came back.
fn restore_queued_rows(q: &mut QueueState, items: Vec<SongItem>) -> usize {
    let waiting: HashSet<String> = upcoming_queued(&q.items, q.current)
        .into_iter()
        .map(|i| i.video_id)
        .collect();
    let back: Vec<SongItem> = items
        .into_iter()
        .filter(|i| !waiting.contains(&i.video_id))
        .map(|mut i| {
            if !i.queued && !i.queued_end {
                i.queued_end = true;
            }
            i.autoplay = false;
            i
        })
        .collect();
    let n = back.len();
    // Nothing playing: the queue was replaced and stopped since; there is no "after" to go back to.
    if n == 0 || q.items.is_empty() {
        return 0;
    }
    if let Some(orig) = q.shuffle_orig.as_mut() {
        orig.extend(back.iter().cloned());
    }
    let at = (q.current + 1).min(q.items.len());
    q.items.splice(at..at, back);
    n
}

/// Where the rest of the playing album or playlist goes as its pages arrive: behind the manual
/// queue and what is already queued of the playlist, in front of autoplay.
fn context_end(q: &QueueState) -> usize {
    let at = manual_block_end(&q.items, q.current);
    q.items[at..].iter().position(|i| i.autoplay).map_or(q.items.len(), |p| at + p)
}

/// Drop every copy of `ids` already in the queue, so a manual add moves the track instead of
/// duplicating it (the "prevent duplicates" setting). The playing track is never dropped, and
/// `current` follows its own track down. Returns whether anything went.
fn drop_duplicates(items: &mut Vec<SongItem>, current: &mut usize, ids: &HashSet<&str>) -> bool {
    let mut removed = false;
    for i in (0..items.len()).rev() {
        if i != *current && ids.contains(items[i].video_id.as_str()) {
            items.remove(i);
            if i < *current {
                *current -= 1;
            }
            removed = true;
        }
    }
    removed
}

/// Keep only the upcoming tracks `keep` accepts; played and playing ones always stay. Returns
/// whether anything went. The pre-shuffle snapshot loses the same tracks, or turning shuffle off
/// would bring them back.
fn retain_upcoming(q: &mut QueueState, keep: impl Fn(&SongItem) -> bool) -> bool {
    let cur = q.current;
    let before = q.items.len();
    let mut i = 0;
    q.items.retain(|item| {
        let k = i <= cur || keep(item);
        i += 1;
        k
    });
    if q.items.len() == before {
        return false;
    }
    if let Some(orig) = q.shuffle_orig.as_mut() {
        let left: HashSet<&str> = q.items.iter().map(|i| i.video_id.as_str()).collect();
        orig.retain(|item| keep(item) || left.contains(item.video_id.as_str()));
    }
    true
}

/// Where a "Play next" track goes: right after the current song, behind any earlier "Play next"
/// adds (`queued`, FIFO — includes solo adds, guest suggestions, and host adds), ahead of anything
/// added with "Add to queue" (`queued_end`) and of the upcoming playlist.
fn guest_insert_index(items: &[SongItem], current: usize) -> usize {
    let mut at = (current + 1).min(items.len());
    while items.get(at).map(|i| i.queued).unwrap_or(false) {
        at += 1;
    }
    at
}

/// The autoplay radio seed for a queue source: playlist/album pages pass their playlist id
/// (`VL…` browseId or bare `OLAK5uy_…`/`PL…`) → `RDAMPL<id>` playlist radio. `None` (single
/// song / artist top-songs) → no pinned seed; autoplay seeds `RDAMVM<last video>` at extension
/// time instead. A playlist on this machine has no YouTube radio, so it gets no seed either, while
/// keeping its `source_id` (that is what the player's "Remove from this playlist" writes to).
fn radio_seed_for(source_id: Option<String>) -> Option<String> {
    // ── stream: autoplay ── only ids YouTube has a radio for: Liked Songs (`VLLM`), an artist's
    // Popular (`UC…`) and Made for you used to get an `RDAMPL…` YouTube has never heard of, and
    // autoplay died at their end. They now continue from their last songs instead.
    source_id.filter(|id| !is_local_playlist(id)).and_then(|id| crate::autoplay::context_radio(&id))
}

/// Replace everything after the playing track with a radio, keeping the history, the current song
/// and any unplayed manual adds. Deduped against what survives, so the seed can't come round again
/// two tracks later. The tracks aren't marked `autoplay`: this queue is what the user asked for,
/// not filler appended behind one.
fn splice_radio_into(
    q: &mut QueueState,
    items: Vec<SongItem>,
    seed: String,
    title: Option<String>,
) {
    let carried = upcoming_queued(&q.items, q.current);
    q.items.truncate(q.current + 1);
    q.items.extend(carried);
    let mut seen: HashSet<String> = q.items.iter().map(|i| i.video_id.clone()).collect();
    for item in items {
        if seen.insert(item.video_id.clone()) {
            q.items.push(item);
        }
    }
    q.radio_seed = Some(seed);
    q.source_name = title;
    q.source_id = None; // a radio is nobody's playlist to edit
    q.radio = true;
    // Shuffle is sticky across queues: re-snapshot the new order as the "original", then shuffle
    // what's upcoming (same handling as a radio hydration in `play_song`).
    if q.shuffle_orig.is_some() {
        q.shuffle_orig = Some(q.items.clone());
        let cur = q.current;
        // A radio's adds already sit right behind the playing track, pinned either way.
        shuffle_upcoming(&mut q.items, cur, false);
    }
}

/// How many already-played rows an endlessly-extending queue keeps behind the play pointer.
///
/// Autoplay appends forever and nothing used to drop anything, so a long session grew the item list
/// without bound (measured 336,776 bytes of `queue_json` on a real install). Every append re-emits
/// the whole list as a JavaScript source string and rewrites the whole blob, so the per-track cost
/// rose with how long you had been listening. 200 is past any plausible scroll-back in the panel.
const KEEP_PLAYED: usize = 200;

/// The track the panel's "Back to …" offers, which is the one `restore_prev_context` would start.
/// `None` unless the restore is actually reachable: a kept queue, and the pointer still at the head
/// of the one that replaced it. So the line appears with the new queue and goes away by itself the
/// moment playback moves on from it.
fn prev_track_title(q: &QueueState) -> Option<&str> {
    if q.current != 0 {
        return None;
    }
    let prev = q.prev_context.as_ref()?;
    Some(prev.items[prev.current].title.as_str())
}

/// Drop played rows beyond `keep` from the front, returning how many were removed so the caller can
/// rebase every index that pointed into `items`.
fn trim_played(items: &mut Vec<innertube::SongItem>, current: usize, keep: usize) -> usize {
    let drop = current.saturating_sub(keep);
    if drop == 0 {
        return 0;
    }
    items.drain(..drop);
    drop
}

/// [`AppState::freshen_radio`]'s order: songs not in `heard` keep YouTube's order and go first,
/// then the heard ones, least recently heard first, so a page heard end to end still rotates.
fn freshen(items: &mut [SongItem], heard: &std::collections::HashMap<String, i64>) {
    // Stable, and `None` sorts before any `Some`.
    items.sort_by_key(|i| heard.get(&i.video_id).copied());
}

/// Autoplay's first batch behind a single pick: a full radio page.
const SINGLE_BATCH: usize = 50;

/// The queue a single pick makes (`crate::autoplay`): the song, then whatever the user had queued
/// by hand, and nothing else. No context: no "Next from", no playlist to edit, no pinned radio.
/// `from` labels "Playing from" (a search query).
fn install_single(q: &mut QueueState, seed: SongItem, carried: Vec<SongItem>, from: Option<String>) {
    q.items = std::iter::once(seed).chain(carried).collect();
    q.current = 0;
    q.radio_seed = None;
    q.source_name = from.filter(|f| !f.trim().is_empty());
    q.source_id = None;
    q.radio = false;
}

/// Append radio-continuation tracks to the queue: dedupe against `existing` (the whole current
/// queue + everything appended this hop), cap at `cap`, and mark each as `autoplay` so the UI can
/// show where the chosen queue ends and autoplay begins. Returns how many were appended.
fn merge_radio(
    items: &mut Vec<SongItem>,
    fresh: Vec<SongItem>,
    mut existing: HashSet<String>,
    cap: usize,
) -> usize {
    let mut added = 0;
    for mut item in fresh {
        if added >= cap {
            break;
        }
        if !existing.insert(item.video_id.clone()) {
            continue; // already in the queue (or appended this hop)
        }
        item.autoplay = true;
        items.push(item);
        added += 1;
    }
    added
}

/// What the skip toast blames. "unavailable" is right for a video YouTube would not serve, and
/// wrong for an upload: the file is still there, the session is what failed. Issue #71.
///
/// `Unreachable` and `SignInRequired` no longer reach here: `ResolveError::affects_every_track`
/// stops both call sites before they skip anything, because neither says anything about the track.
fn skip_reason(e: &ResolveError) -> &'static str {
    match e {
        ResolveError::UploadUnavailable(_) | ResolveError::SignInRequired(_) => "sign-in needed",
        _ => "unavailable",
    }
}

/// The queue index that plays after `current`, honoring repeat-all wrap. `None` at the tail
/// when repeat is off (exhausted) or one (mpv loops the file itself — the queue never advances).
fn next_index(len: usize, current: usize, repeat: RepeatMode) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let next = current + 1;
    if next < len {
        Some(next)
    } else if repeat == RepeatMode::All {
        Some(0)
    } else {
        None
    }
}

/// [`next_index`], stepping over rows marked `unavailable`. Bounded by the queue length, so a
/// repeat-all queue where nothing is playable ends instead of spinning.
fn next_playable_index(items: &[SongItem], current: usize, repeat: RepeatMode) -> Option<usize> {
    let mut at = current;
    for _ in 0..items.len() {
        let next = next_index(items.len(), at, repeat)?;
        // ── stream: polish ── explicit rows (filter on) are stepped over like unavailable ones.
        if !items[next].unavailable && !crate::polish::explicit::blocks(&items[next]) {
            return Some(next);
        }
        if next == current {
            return None;
        }
        at = next;
    }
    None
}

/// The tail of `play_playlist` once the new context's rows are in `q.items` with `q.current` on
/// the clicked track: shuffle it if asked, then put the carried manual queue (Spotify semantics:
/// Play next first, then Add to queue) right after the playing track, ahead of the new context.
/// Carried rows are not in the snapshot; un-shuffle takes them from the live queue
/// ([`unshuffle_queue`]), so they survive turning shuffle off.
fn arrange_new_context(
    q: &mut QueueState,
    keep_shuffled: bool,
    whole: bool,
    carried: Vec<SongItem>,
) {
    if keep_shuffled {
        // Snapshot the real playlist order (for un-shuffle), then play the clicked track first
        // with everything else shuffled behind it.
        q.shuffle_orig = Some(q.items.clone());
        let start = q.current;
        q.current = shuffle_new_queue(&mut q.items, start);
    } else {
        q.shuffle_orig = None; // non-sticky shuffle ends with the queue it was turned on for
    }
    let (next, added): (Vec<_>, Vec<_>) = carried.into_iter().partition(|i| i.queued);
    let mix = keep_shuffled && !added.is_empty() && whole;
    let at = q.current + 1;
    q.items.splice(at..at, next.into_iter().chain(added));
    if mix {
        let cur = q.current;
        shuffle_upcoming(&mut q.items, cur, true);
    }
}

/// Shuffle off: restore the snapshot around the playing track, keep the live manual queue right
/// after it, and drop from what's upcoming anything the shuffle already played.
fn unshuffle_queue(q: &mut QueueState) {
    let Some(orig) = q.shuffle_orig.take() else { return };
    let playing = q.items[q.current].video_id.clone();
    let fallback = q.current;
    // The shuffled prefix (through the playing track) is what's already been played — the
    // restored order must not offer any of it again.
    let heard: HashSet<String> = q.items[..=q.current].iter().map(|i| i.video_id.clone()).collect();
    let manual = upcoming_queued(&q.items, q.current);
    let (items, idx) = unshuffled(orig, &heard, &playing, fallback, manual);
    q.items = items;
    q.current = idx;
}

/// Un-shuffle: restore the snapshot, re-locate the playing track in it, then drop from what's
/// still *upcoming* anything the shuffle already played (`heard`). Restoring verbatim re-queues
/// those: shuffle through a whole album and the tracks that happen to sit after the last shuffled
/// one in album order come back as "up next" the moment shuffle goes off, even though they just
/// played. Heard tracks *behind* the playing one stay — they're the hidden history, and removing
/// them would only move the playing track's index for no visible gain.
/// ponytail: dupes match by videoId — the same song twice in one queue keeps only its first copy
/// past the playing track; harmless (identical items).
fn unshuffled(
    orig: Vec<SongItem>,
    heard: &HashSet<String>,
    playing_id: &str,
    fallback: usize,
    manual: Vec<SongItem>,
) -> (Vec<SongItem>, usize) {
    let idx = orig
        .iter()
        .position(|i| i.video_id == playing_id)
        .unwrap_or_else(|| fallback.min(orig.len().saturating_sub(1)));
    let mut items = orig;
    let mut i = 0;
    items.retain(|it| {
        let keep = i <= idx || !heard.contains(&it.video_id);
        i += 1;
        keep
    });
    // The manual queue is queue state, not playlist order, and the live queue is its truth: the
    // snapshot may not hold adds carried in from an earlier context or made while shuffled. So
    // whatever manual rows the snapshot has are dropped and the live block (`manual`, in its
    // order: Play next, then Add to queue) goes right after the playing track, ahead of the
    // playlist, as in Spotify.
    let mut tail = items.split_off((idx + 1).min(items.len()));
    tail.retain(|it| !(it.queued || it.queued_end));
    let (next, added): (Vec<_>, Vec<_>) = manual.into_iter().partition(|it| it.queued);
    items.extend(next);
    items.extend(added);
    items.extend(tail);
    (items, idx)
}

/// The unplayed manual adds (after `current`, either marker) — carried into a new queue on a
/// context switch so what the user lined up survives them playing something else.
fn upcoming_queued(items: &[SongItem], current: usize) -> Vec<SongItem> {
    items.iter().skip(current + 1).filter(|i| i.queued || i.queued_end).cloned().collect()
}

/// Fisher–Yates over the *upcoming* items only — the playing track and the already-played prefix
/// stay put, and the leading run of manually-queued tracks keeps its spot: "Next in queue" plays
/// next regardless of shuffle (Spotify semantics). Autoplay tracks stay *behind* the remaining
/// queue tracks (each section shuffled within itself) — shuffle never promotes radio filler ahead
/// of the playlist.
fn shuffle_upcoming(items: &mut [SongItem], current: usize, whole: bool) {
    let first = (current + 1).min(items.len());
    let mut start = first;
    // The manual queue ("Next in queue") is pinned. With `whole` an Add to queue row joins the mix
    // (the opt-in "shuffle the whole queue" setting); otherwise it stays queued, as in Spotify.
    while items.get(start).is_some_and(|i| i.queued || (!whole && i.queued_end)) {
        start += 1;
    }
    shuffle_added_runs(&mut items[first..start]);
    if whole && start < items.len() {
        // "Shuffle the whole queue": the playlist and what was added to it are one mix. Play next
        // stays next (pinned above), autoplay stays last: it is filler that tops up from the end.
        crate::smart::shuffle::shuffle_slice(&mut items[start..]); // ── stream: smart ──
        items[start..].sort_by_key(|i| i.autoplay);
    } else if start < items.len() {
        // The playlist, then autoplay; each shuffled within itself. A stray manual row outside
        // the pinned block (an older saved queue) is pulled up to the end of it first, stable.
        let rest = &mut items[start..];
        rest.sort_by_key(|i| (!i.queued_end, i.autoplay));
        let added = rest.iter().take_while(|i| i.queued_end).count();
        let playlist = added + rest[added..].iter().take_while(|i| !i.autoplay).count();
        shuffle_added_runs(&mut rest[..added]);
        crate::smart::shuffle::shuffle_slice(&mut rest[added..playlist]); // ── stream: smart ──
        crate::smart::shuffle::shuffle_slice(&mut rest[playlist..]); // ── stream: smart ──
    }
}

/// Inside a run of manual adds: a whole album/playlist added is a set of tracks like any other, so
/// shuffle each such run in place (un-shuffle restores the real order from the snapshot). Songs
/// queued one at a time carry no `queued_from` and keep their FIFO order: "play this next, then
/// that" is an order the user stated, not one to randomize.
fn shuffle_added_runs(items: &mut [SongItem]) {
    use rand::seq::SliceRandom;
    let mut i = 0;
    while i < items.len() {
        let Some(from) = items[i].queued_from.clone() else {
            i += 1;
            continue;
        };
        let mut end = i + 1;
        while end < items.len() && items[end].queued_from.as_deref() == Some(from.as_str()) {
            end += 1;
        }
        items[i..end].shuffle(&mut rand::thread_rng());
        i = end;
    }
}

/// Append a background-walked playlist page ([`AppState::fill_playlist`]) to a queue that's
/// already playing. The page goes onto `shuffle_orig` in its true order too, so un-shuffle still
/// restores the *whole* playlist and not just the part that had loaded when playback started.
///
/// With shuffle on and `playing`, the unplayed tail is re-shuffled so the new page is mixed through
/// it instead of sitting at the end. The pivot is the primed gapless slot when there is one: that
/// track is already loaded into mpv, so moving it would desync what mpv plays next from what the
/// queue says is next. It was drawn from the same random tail anyway.
///
/// The playing playlist's pages go in front of what the user added at the tail ([`context_end`]),
/// which plays when the playlist is done. A page walked in for "Add to queue" passes
/// `playing: false` and goes right behind its own block, so a second add made while the first is
/// still loading stays behind all of it.
fn append_page(q: &mut QueueState, page: Vec<SongItem>, playing: bool, whole: bool) {
    if let Some(orig) = q.shuffle_orig.as_mut() {
        orig.extend(page.iter().cloned());
    }
    if playing {
        let at = context_end(q);
        q.items.splice(at..at, page);
        if q.shuffle_orig.is_some() {
            let pivot = q.lookahead_loaded.filter(|&i| i > q.current).unwrap_or(q.current);
            shuffle_upcoming(&mut q.items, pivot, whole);
        }
    } else {
        let from = page.first().and_then(|i| i.queued_from.clone());
        let cur = q.current;
        let at = q
            .items
            .iter()
            .rposition(|i| i.queued_end && i.queued_from == from)
            .filter(|&p| p > cur)
            .map_or_else(|| enqueue_at(q), |p| p + 1);
        q.items.splice(at..at, page);
    }
}

/// A mix (`RD…`, reached as the `VLRD…` browseId) is a generated, effectively endless feed rather
/// than a playlist with an end, and `extend_queue_radio` already tops the queue up from its tail.
/// Walking its continuations would fetch page after page for no gain.
fn is_mix(source_id: Option<&str>) -> bool {
    source_id.is_some_and(|id| id.strip_prefix("VL").unwrap_or(id).starts_with("RD"))
}

/// The YouTube playlist a queue's `source_id` names, to ask `/next` about (#542). Not a playlist
/// on this machine, nor a mix (YouTube generated its rows), nor an album, whose page already
/// pairs its rows (`InnerTube::album`).
fn playlist_context(source_id: Option<&str>) -> Option<String> {
    let id = source_id.filter(|id| !is_local_playlist(id))?;
    let id = id.strip_prefix("VL").unwrap_or(id);
    (!id.starts_with("RD") && !id.starts_with("OLAK5uy_")).then(|| id.to_owned())
}

/// Shuffle a *fresh* queue around the clicked track (shuffle was already on when it started,
/// Spotify semantics): the clicked track plays first, everything else follows in random order.
/// Returns the new current index (always 0). The swap is fine — everything past 0 is shuffled.
fn shuffle_new_queue(items: &mut [SongItem], start: usize) -> usize {
    if !items.is_empty() {
        items.swap(0, start.min(items.len() - 1));
        shuffle_upcoming(items, 0, false); // a fresh queue holds no adds yet
    }
    0
}

/// How much life a cached stream URL needs left before replaying it is worth it: the whole track,
/// plus a minute for the load. Sizing this off the load alone is what broke seeking in hour-long
/// videos (see the call site, issue #188). `duration_secs` is 0 when the queue row has no length,
/// which degrades to the old behavior rather than refusing every cache hit.
fn cache_horizon(now: i64, duration_secs: i64) -> i64 {
    now + 60 + duration_secs.max(0)
}

/// Parse a `"m:ss"` / `"h:mm:ss"` duration string to ms (0 if absent/unparseable).
pub fn parse_duration_ms(s: Option<&str>) -> i64 {
    let Some(s) = s else { return 0 };
    let parts: Vec<i64> = s.split(':').filter_map(|p| p.trim().parse().ok()).collect();
    let secs = match parts.as_slice() {
        [s] => *s,
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => 0,
    };
    secs * 1000
}

/// A queue row's own length in seconds (0 when it states none).
fn row_secs(item: &SongItem) -> f64 {
    parse_duration_ms(item.duration.as_deref()) as f64 / 1000.0
}

fn format_duration(ms: i64) -> String {
    let total = ms / 1000;
    match total / 3600 {
        0 => format!("{}:{:02}", total / 60, total % 60),
        h => format!("{}:{:02}:{:02}", h, (total % 3600) / 60, total % 60),
    }
}

/// Trade a row's id for its `counterpart` (#442), keeping the old one as the way back. False when
/// there is none.
pub fn swap_version(item: &mut SongItem) -> bool {
    let Some(other) = item.counterpart.take() else { return false };
    item.counterpart = Some(std::mem::replace(&mut item.video_id, other));
    item.is_video = !item.is_video;
    // Only a pair trades: an album row has art of its own but none for the video it swapped out.
    if let (Some(a), Some(b)) = (item.thumbnail.as_mut(), item.counterpart_thumbnail.as_mut()) {
        std::mem::swap(a, b);
    }
    for seg in &mut item.counterpart_segments {
        std::mem::swap(&mut seg.start_ms, &mut seg.twin_start_ms);
    }
    // `twin_position` walks them in this version's order.
    item.counterpart_segments.sort_by_key(|s| s.start_ms);
    true
}

/// Give each row the twin a playlist's own `/next` pairs it with (#542), over the one it had. A
/// row matches either side of a pair: a playlist entry reaches the queue as the video it holds or
/// as the song YouTube serves it as. True when anything changed.
fn adopt_playlist_twins<'a>(
    rows: impl Iterator<Item = &'a mut SongItem>,
    pairs: &[SongItem],
) -> bool {
    let mut twins = std::collections::HashMap::new();
    for pair in pairs {
        let Some(other) = &pair.counterpart else { continue };
        let mut flipped = pair.clone();
        swap_version(&mut flipped);
        twins.insert(other.as_str(), flipped);
        twins.insert(pair.video_id.as_str(), pair.clone());
    }
    let mut changed = false;
    for row in rows {
        let Some(twin) = twins.get(row.video_id.as_str()) else { continue };
        if row.counterpart != twin.counterpart {
            row.counterpart = None; // the playlist's twin replaces it, art and segments too
        }
        changed |= adopt_twin(row, twin);
    }
    changed
}

/// Take `from`'s twin, with its art and segments, when `into` has none, or fill in what `into`
/// lacks for the same twin. True when anything changed.
fn adopt_twin(into: &mut SongItem, from: &SongItem) -> bool {
    let Some(twin) = &from.counterpart else { return false };
    match &into.counterpart {
        None => {
            into.counterpart = Some(twin.clone());
            into.counterpart_thumbnail = from.counterpart_thumbnail.clone();
            into.counterpart_segments = from.counterpart_segments.clone();
            true
        }
        // An album page names the video but carries neither its art nor the segments (#535).
        Some(have) if have == twin => {
            let mut learned = false;
            if into.counterpart_thumbnail.is_none() && from.counterpart_thumbnail.is_some() {
                into.counterpart_thumbnail = from.counterpart_thumbnail.clone();
                learned = true;
            }
            if into.counterpart_segments.is_empty() && !from.counterpart_segments.is_empty() {
                into.counterpart_segments = from.counterpart_segments.clone();
                learned = true;
            }
            learned
        }
        Some(_) => false,
    }
}

/// Where `pos` (seconds into this version) falls in its counterpart, through the segment playing
/// (#520, #535). An intro or a break in the video has no moment in the song, so it lands where the
/// song picks up again; past the last segment, at its end. No segments: the same timestamp.
fn twin_position(pos: f64, segments: &[innertube::Segment]) -> f64 {
    let ms = (pos * 1000.0).round() as i64;
    let twin_ms = match segments.iter().find(|s| ms < s.start_ms + s.duration_ms) {
        Some(s) => s.twin_start_ms + (ms - s.start_ms).max(0),
        None => match segments.last() {
            Some(s) => s.twin_start_ms + s.duration_ms,
            None => return pos,
        },
    };
    twin_ms as f64 / 1000.0
}

/// Whether a row YouTube pairs with its other version should play the music video (#464).
pub fn wants_video(db: &Db) -> bool {
    db.get_setting("music_videos").as_deref() == Some("true")
        && db.get_setting("videos_first").as_deref() != Some("false")
}

/// Put a row on the version [`wants_video`] picked. False when it is already there or has no other.
pub fn prefer_version(item: &mut SongItem, video: bool) -> bool {
    item.is_video != video && swap_version(item)
}

/// Fill a missing item duration from the player response's `lengthSeconds` (e.g. "167") — the
/// exact length of the cut we stream. Never overwrites an existing duration.
/// Repair a queue item from the `/player` response, which is the one thing every entry path (search,
/// album, card, radio, a restored queue, a row replayed out of Your Rotation) goes through.
///
/// `length_seconds` fills a duration items from cards/radio arrive without. `author` is
/// `videoDetails.author` from the MAIN client, i.e. YouTube's own artist for the track: it repairs
/// an artist string that is missing (album rows ship the artist column empty) or that is a whole
/// display subtitle rather than a name ("Miley Cyrus • Plastic Hearts • 2020"). A "•" never appears
/// in a real artist line, collabs use "&" and ",". Both shapes reach the player bar, the OS widget
/// and Last.fm, and a wrong artist there is worse than a missing one: it scrobbles as another
/// artist entirely. Rows persisted before this existed are healed the next time they play, because
/// the caller writes the repaired item back into the queue.
fn backfill_metadata(
    item: &mut SongItem,
    length_seconds: Option<&str>,
    author: Option<&str>,
    is_video: Option<bool>,
    is_episode: Option<bool>,
) {
    if item.duration.is_none() {
        if let Some(secs) = length_seconds.and_then(|s| s.trim().parse::<i64>().ok()) {
            item.duration = Some(format_duration(secs * 1000));
        }
    }
    // YouTube's own `musicVideoType` outranks whatever the row was parsed with: a card played
    // from a shelf never carried the flag, an album row keeps it after being swapped to its audio
    // id, and a mirrored or restored row has no flag at all. Overwrite, don't OR: a stale `true`
    // on an audio track would put the still album-art "video" on screen (plan 031).
    if let Some(is_video) = is_video {
        item.is_video = is_video;
    }
    if let Some(is_episode) = is_episode {
        item.is_episode = is_episode;
    }
    if let Some(author) = author.map(str::trim).filter(|a| !a.is_empty()) {
        if item.artists.trim().is_empty() || item.artists.contains('•') {
            item.artists = author.to_owned();
            // The per-artist links belong to the string we just replaced; the UI renders them in
            // place of `artists` when they're there, so a stale set would put the bad line back.
            item.artist_runs.clear();
        }
    }
}

/// The level to come up at: what the user left the slider on last run. Written by the UI on
/// commit rather than by `set_volume`, which a drag calls every frame (and every settings write
/// is an fsync). mpv would start at 100 otherwise.
pub fn saved_volume(db: &Db) -> i64 {
    let v = db.get_setting("volume").and_then(|s| s.parse().ok());
    v.filter(|v| (0..=100).contains(v)).unwrap_or(100)
}

/// Crossfade length in seconds, or `None` when it's off. Experimental, so off unless asked for,
/// and clamped to the range the settings slider offers in case the stored value predates it.
pub fn saved_crossfade(db: &Db) -> Option<f64> {
    if db.get_setting("crossfade").as_deref() != Some("true") {
        return None;
    }
    let secs = db.get_setting("crossfade_secs").and_then(|s| s.parse::<f64>().ok());
    // stream: audio: Spotify's 12 s ceiling (was 10).
    Some(secs.filter(|s| s.is_finite()).unwrap_or(5.0).clamp(1.0, 12.0))
}

/// How far into a track a play counts (context/01 §registerPlayback): halfway, capped at 30s.
/// `duration` is mpv's, which is 0.0 until it reports one, so an unknown length means the full 30s.
fn history_threshold(duration: f64) -> f64 {
    if duration > 1.0 {
        (duration / 2.0).min(30.0)
    } else {
        30.0
    }
}

/// Per-track loudness gain (dB) from YouTube's `loudnessDb` (context/03, context/14), toward the
/// -7 LUFS target: loud masters get pulled down, quiet ones get lifted (#237). The player adds a
/// limiter whenever the gain is positive, so a lifted track can't clip.
///
/// `loudnessDb` is measured against **-14 LUFS** (YouTube's own response proves it:
/// `perceptualLoudnessDb == loudnessDb - 14`, i.e. the track's absolute LUFS). Applying it raw
/// normalizes to -14 like the *video* site, but YouTube **Music** only pulls down what exceeds
/// **-7 LUFS**, so raw made us ~6 dB quieter than YTM web on a typical modern master. Normalize to
/// the same -7 target.
///
/// YTM itself never boosts, which left older, quieter masters ~6 dB under their neighbours. Here
/// they are lifted by at most [`MAX_BOOST_DB`].
///
/// `None` means "no filter": on target (or no metadata), mpv gets its `af` cleared rather than a
/// `volume=0dB` no-op in the chain. Callers go through [`AppState::track_gain`], which is where
/// the user's `normalize_volume` switch turns the whole thing off.
// ponytail: no peak data from YouTube, so the boost cap is a guess at how much limiting a dynamic
// master tolerates. Lower MAX_BOOST_DB if classical/jazz sounds squashed.
#[cfg_attr(not(test), allow(dead_code))] // stream: audio: superseded by audio::track_loudness_gain
fn loudness_gain(loudness_db: Option<f64>) -> Option<f64> {
    let gain = (TARGET_LUFS - (loudness_db? - 14.0)).clamp(-24.0, MAX_BOOST_DB);
    (gain.abs() >= 0.05).then_some(gain)
}

/// The URL to hand mpv for one resolved track: the loopback chunked proxy when it is up, otherwise
/// the googlevideo URL itself. Local files (no scheme) and non-http schemes pass straight through.
///
/// ffmpeg opens a stream with an open-ended `Range: bytes=X-`, which googlevideo throttles to ~2×
/// realtime; the proxy re-issues it as bounded ranges, which are served at full speed, so both the
/// first play and every seek start sooner. audioproxy.rs has the numbers and the kill-switch.
fn mpv_stream_url(data: &PlaybackData) -> String {
    // A user proxy reaches mpv as `http-proxy` (#241), and ffmpeg would send the loopback request
    // through it, and it cannot reach our 127.0.0.1, so audio would go silent. Hand mpv the
    // googlevideo URL instead and let the user's proxy carry it: no chunked proxy, but working
    // playback. Nothing else is lost, because the proxy's own upstream fetch already goes through
    // `http::client()`, which is proxied.
    //
    // A live playlist (#424) goes direct too: the chunking proxy serves one byte-range file per
    // registration, and a playlist is text that names other URLs, which mpv has to fetch itself.
    if crate::http::has_proxy() || data.live {
        return data.stream_url.clone();
    }
    let http = data.stream_url.starts_with("http://") || data.stream_url.starts_with("https://");
    if http {
        if let Some(url) = crate::audioproxy::register(&data.stream_url, &data.headers) {
            return url;
        }
    }
    data.stream_url.clone()
}

/// Loudness target, matching YouTube Music's own player rather than the video site's -14.
const TARGET_LUFS: f64 = -7.0;

/// Most a quiet track is lifted toward [`TARGET_LUFS`]. Every dB of boost is a dB the limiter may
/// have to take back off the peaks, so this is a dynamics budget, not a loudness one: +6 flattened
/// dynamic masters audibly (#298, #300), while +3 still closes most of the gap #237 reported.
const MAX_BOOST_DB: f64 = 3.0;

/// Fingerprint of a queue's track list: what `emit_queue` compares to decide whether the UI
/// already has these rows. Hashes length, `video_id`, and every field the panel renders
/// differently per row (`set_video_id`, `queued`, `queued_end`, `queued_from`, `queued_by`,
/// `autoplay`): two rows identical in all of those are interchangeable in the panel, so staying
/// quiet about a swap between them is correct. Deliberately excludes `duration`, `artists`,
/// `title`, `thumbnail` and `rating`: those are exactly the fields `backfill_metadata` repairs on
/// the playing row, and that repair rides along on the `queue-index` event's `current` field on
/// purpose. Hashing them would send the whole queue on every track start and undo the point of
/// that split.
fn queue_fingerprint(items: &[SongItem]) -> u64 {
    let mut hasher = DefaultHasher::new();
    items.len().hash(&mut hasher);
    for item in items {
        item.video_id.hash(&mut hasher);
        item.set_video_id.hash(&mut hasher);
        item.queued.hash(&mut hasher);
        item.queued_end.hash(&mut hasher);
        item.queued_from.hash(&mut hasher);
        item.queued_by.hash(&mut hasher);
        item.autoplay.hash(&mut hasher);
    }
    hasher.finish()
}

/// Dirty key for `queue_json`, which stores more than the rows. `queue_index` carries
/// current/repeat, so anything else the blob holds has to force a blob rewrite by
/// itself. `shuffle_upcoming` only touches rows after `current`, so toggling shuffle while the
/// last track plays leaves `items` byte-identical and the row fingerprint alone would skip the
/// write, losing the shuffle state across a restart.
fn persist_fingerprint(q: &QueueState) -> u64 {
    let mut hasher = DefaultHasher::new();
    queue_fingerprint(&q.items).hash(&mut hasher);
    q.shuffle_orig.is_some().hash(&mut hasher);
    q.radio_seed.hash(&mut hasher);
    q.source_name.hash(&mut hasher);
    q.source_id.hash(&mut hasher);
    q.radio.hash(&mut hasher);
    hasher.finish()
}

/// Track failures in a row, with no playback in between, after which playback stops instead of
/// moving on to the next track (Spotify stops too; it never cycles the whole queue).
pub const MAX_FAILURE_STREAK: u32 = 3;

/// The one message shown when playback stops for [`MAX_FAILURE_STREAK`].
pub const CANT_PLAY_MESSAGE: &str = "Can't play right now. Check your connection.";

/// What `on_track_failed` did, for the event pump's toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    /// The track is being loaded again: no toast.
    Retried,
    /// Gave up on it and moved on: the track's own error.
    Advanced,
    /// Stopped, paused, on the failed track: [`CANT_PLAY_MESSAGE`].
    Halted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailAction {
    /// Load the same track again. `infra`: our proxy was at fault, not the track.
    Retry { infra: bool },
    Advance,
    Halt,
}

/// What an auto-advancing caller (the play loop, the gapless lookahead) does with a track whose
/// resolve failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolveFail {
    /// Mark it unavailable and move on to the next row.
    Skip,
    /// Don't move: YouTube refused us or is unreachable, which says nothing about this track.
    Stay,
    /// Stop: the [`MAX_FAILURE_STREAK`]th failure in a row with nothing played in between.
    Halt,
}

/// Kept pure so both callers' policy can be tested without a player or a network. `streak`:
/// failures already in a row before this one ([`AppState::fail_streak`], reset when a track
/// really plays). Never a skip storm: a refusal never skips, and verdicts stop at the third.
fn resolve_failure_action(e: &ResolveError, streak: u32) -> ResolveFail {
    if e.affects_every_track() {
        ResolveFail::Stay
    } else if streak + 1 >= MAX_FAILURE_STREAK {
        ResolveFail::Halt
    } else {
        ResolveFail::Skip
    }
}

/// The failure policy, kept pure so it can be tested without a player.
/// `proxy`: the audio proxy's health, checked just now (`None`: not running, streams go direct).
/// `streak`: failures already in a row before this one.
fn failure_action(
    proxy: Option<crate::loopback::Health>,
    has_track: bool,
    already_retried: bool,
    streak: u32,
) -> FailAction {
    use crate::loopback::Health;
    let gave_up = streak + 1 >= MAX_FAILURE_STREAK;
    match proxy {
        // Local infrastructure, not the track: never a reason to move on.
        Some(Health::Down) => FailAction::Halt,
        Some(Health::Rebound { .. }) if has_track && !gave_up => FailAction::Retry { infra: true },
        Some(Health::Rebound { .. }) => FailAction::Halt,
        _ if !has_track => FailAction::Advance,
        _ if !already_retried => FailAction::Retry { infra: false },
        _ if gave_up => FailAction::Halt,
        _ => FailAction::Advance,
    }
}

/// The `quality` setting, the one key both apps write (the Mac's Select; the iPhone resolves its
/// per-network choices into it). Five choices, two real YouTube tiers: Low and Normal cap at about
/// 128 kbit/s, High and Very high take the best stream. Unset is Automatic, the default on both;
/// it plays as High (the core does not know the network; the iPhone resolves it before writing).
pub fn streaming_quality(setting: Option<&str>) -> AudioQuality {
    match setting {
        Some("LOW" | "NORMAL") => AudioQuality::Low,
        Some("HIGH" | "VERY_HIGH") => AudioQuality::High,
        _ => AudioQuality::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        adopt_playlist_twins, adopt_twin, append_page, arrange_new_context, backfill_metadata,
        cache_horizon, next_playable_index, unshuffle_queue,
        drop_duplicates, enqueue_at, format_duration, freshen, get_url, guest_insert_index,
        history_threshold, is_mix, loudness_gain, merge_radio, next_index, parse_duration_ms,
        persist_fingerprint, playlist_context, prefer_version, prev_track_title, put_url,
        queue_fingerprint, radio_seed_for, radio_title, retain_upcoming, shuffle_new_queue,
        shuffle_upcoming, splice_radio_into, swap_version, trim_played, twin_position, unshuffled,
        upcoming_queued, QueueState, RepeatMode, VideoUrls, KEEP_PLAYED, LOCAL_PLAYLIST_PREFIX,
        MAX_BOOST_DB,
    };
    use super::{failure_action, resolve_failure_action, FailAction, ResolveFail, MAX_FAILURE_STREAK}; // stream: proxyfix
    use super::{install_single, SINGLE_BATCH};
    use super::streaming_quality;

    #[test]
    fn streaming_quality_defaults_to_automatic() {
        use super::AudioQuality;
        assert_eq!(streaming_quality(None), AudioQuality::Auto);
        assert_eq!(streaming_quality(Some("AUTO")), AudioQuality::Auto);
        assert_eq!(streaming_quality(Some("LOW")), AudioQuality::Low);
        assert_eq!(streaming_quality(Some("NORMAL")), AudioQuality::Low);
        assert_eq!(streaming_quality(Some("HIGH")), AudioQuality::High);
        assert_eq!(streaming_quality(Some("VERY_HIGH")), AudioQuality::High);
    }
    use super::{restore_queued_rows, splice_queued_at}; // lane magic-d2

    // ── stream: autoplay ── Spotify's context rules at the queue level.
    #[test]
    fn a_search_pick_plays_only_that_song_then_autoplay() {
        // A playlist was playing, with one song queued by hand.
        let mut q = QueueState {
            items: vec![song("p1", None), song("p2", None), song("mine", Some("me")), song("p3", None)],
            current: 0,
            source_id: Some("VLPL1".into()),
            source_name: Some("Road Trip".into()),
            radio_seed: Some("RDAMPLPL1".into()),
            ..Default::default()
        };
        // Search "Queen", click the first song: the other results never follow it.
        let carried = upcoming_queued(&q.items, q.current);
        q.keep_context(0.0);
        install_single(&mut q, song("bo", None), carried, Some("Queen".into()));
        let ids: Vec<&str> = q.items.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, ["bo", "mine"], "the pick, then the user's own queue, nothing else");
        assert_eq!(q.source_id, None, "no context: nothing to edit, no Next from");
        assert_eq!(q.radio_seed, None, "autoplay seeds from the song, not the old playlist");
        assert_eq!(q.source_name.as_deref(), Some("Queen"), "Playing from: the search");
        assert!(!q.radio);
        // Autoplay's batch lands behind the user's queue, every row marked.
        let existing = q.items.iter().map(|i| i.video_id.clone()).collect();
        let page = vec![song("dm", None), song("bo", None), song("ab", None)];
        let added = merge_radio(&mut q.items, page, existing, SINGLE_BATCH);
        assert_eq!(added, 2);
        let ids: Vec<&str> = q.items.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, ["bo", "mine", "dm", "ab"]);
        assert!(q.items[2..].iter().all(|i| i.autoplay));
        assert!(!q.items[1].autoplay);
    }

    #[test]
    fn an_artist_or_liked_context_has_no_made_up_radio() {
        // Their end continues from their last songs (`autoplay::seeds`), not from an `RDAMPL…`
        // YouTube can't answer.
        assert_eq!(radio_seed_for(Some("VLLM".into())), None);
        assert_eq!(radio_seed_for(Some("UCiMhD4jzUqG-IgPzUmmytRQ".into())), None);
        assert_eq!(radio_seed_for(Some("MFY_DAILY_1".into())), None);
    }

    #[test]
    fn radio_title_falls_back_to_the_seed_and_is_not_doubled() {
        assert_eq!(radio_title(Some("Get Lucky"), None).as_deref(), Some("Get Lucky Radio"));
        assert_eq!(radio_title(Some("Get Lucky Radio"), None).as_deref(), Some("Get Lucky Radio"));
        assert_eq!(radio_title(None, Some("Never Gonna Give You Up")).as_deref(), Some("Never Gonna Give You Up Radio"));
        assert_eq!(radio_title(Some(""), Some("Seed")).as_deref(), Some("Seed Radio"));
        assert_eq!(radio_title(None, None), None);
    }

    /// The whole point of the video-URL map is answering a reopen without a round trip, so a live
    /// entry has to hit and a stale one has to miss. Getting the second one wrong hands the proxy a
    /// dead googlevideo link for hours.
    #[test]
    fn swap_version_goes_both_ways() {
        let mut it = innertube::SongItem {
            video_id: "audio".into(),
            counterpart: Some("video".into()),
            thumbnail: Some("cover".into()),
            counterpart_thumbnail: Some("frame".into()),
            counterpart_segments: vec![seg(0, 5477, 484451)],
            ..Default::default()
        };
        assert!(swap_version(&mut it));
        assert_eq!((it.video_id.as_str(), it.counterpart.as_deref()), ("video", Some("audio")));
        assert!(it.is_video);
        assert_eq!(it.thumbnail.as_deref(), Some("frame"));
        assert_eq!(it.counterpart_segments, [seg(5477, 0, 484451)]);
        assert!(swap_version(&mut it));
        assert_eq!((it.video_id.as_str(), it.counterpart.as_deref()), ("audio", Some("video")));
        assert!(!it.is_video);
        assert_eq!(it.thumbnail.as_deref(), Some("cover"));
        assert_eq!(it.counterpart_segments, [seg(0, 5477, 484451)]);
        // An album row knows no art for its twin: it keeps its own rather than losing it.
        let mut album = innertube::SongItem {
            video_id: "audio".into(),
            counterpart: Some("video".into()),
            thumbnail: Some("cover".into()),
            ..Default::default()
        };
        assert!(swap_version(&mut album));
        assert_eq!(album.thumbnail.as_deref(), Some("cover"));
        // Nothing to switch to: the row is left exactly as it was.
        let mut lone = innertube::SongItem { video_id: "a".into(), ..Default::default() };
        assert!(!swap_version(&mut lone));
        assert_eq!(lone.video_id, "a");
    }

    fn seg(start_ms: i64, twin_start_ms: i64, duration_ms: i64) -> innertube::Segment {
        innertube::Segment { start_ms, twin_start_ms, duration_ms }
    }

    // The Mirrors pair from #520: the song starts 5.477 s into the video.
    #[test]
    fn a_switch_lands_on_the_same_moment_of_the_other_version() {
        let video = [seg(5477, 0, 484451)];
        let song = [seg(0, 5477, 484451)];
        assert_eq!(twin_position(100.0, &video), 94.523);
        assert_eq!(twin_position(94.523, &song), 100.0); // and back
        assert_eq!(twin_position(3.0, &video), 0.0); // still in the intro: the song's start
        assert_eq!(twin_position(42.0, &[]), 42.0); // no segmentMap: the same timestamp
    }

    // "Locked Away" from #535: the video breaks between 1:49.1 and 2:03.6, so a switch after the
    // break is 14.5 s further along than the intro alone says.
    #[test]
    fn a_switch_maps_through_every_segment() {
        let song = [seg(0, 22698, 86438), seg(86439, 123644, 141061)];
        let video = [seg(22698, 0, 86438), seg(123644, 86439, 141061)];
        assert_eq!(twin_position(60.0, &song), 82.698);
        assert_eq!(twin_position(150.0, &song), 187.205); // the first segment alone: 172.698
        assert_eq!(twin_position(187.205, &video), 150.0);
        assert_eq!(twin_position(10.0, &video), 0.0); // the intro: the song's start
        assert_eq!(twin_position(115.0, &video), 86.439); // the break: where the song picks up
        assert_eq!(twin_position(265.5, &video), 227.5); // the outro: the song's end
    }

    #[test]
    fn a_twin_is_adopted_only_by_a_row_without_one() {
        let twin = innertube::SongItem {
            video_id: "v".into(),
            counterpart: Some("s".into()),
            counterpart_thumbnail: Some("cover".into()),
            counterpart_segments: vec![seg(5477, 0, 484451)],
            ..Default::default()
        };
        let mut row = innertube::SongItem { video_id: "v".into(), ..Default::default() };
        assert!(adopt_twin(&mut row, &twin));
        assert_eq!(row.counterpart_thumbnail.as_deref(), Some("cover"));
        assert_eq!(row.counterpart_segments, [seg(5477, 0, 484451)]);
        // A row that already knows its twin (an album swap, an earlier answer) keeps it.
        assert!(!adopt_twin(&mut row, &innertube::SongItem::default()));
        let other = innertube::SongItem { counterpart: Some("x".into()), ..Default::default() };
        assert!(!adopt_twin(&mut row, &other));
        assert_eq!(row.counterpart.as_deref(), Some("s"));
        // An album row knows its twin from the album page, without art or segments: the same
        // twin fills them in, once.
        let mut album = innertube::SongItem {
            video_id: "v".into(),
            counterpart: Some("s".into()),
            ..Default::default()
        };
        assert!(adopt_twin(&mut album, &twin));
        assert_eq!(album.counterpart_thumbnail.as_deref(), Some("cover"));
        assert_eq!(album.counterpart_segments, [seg(5477, 0, 484451)]);
        assert!(!adopt_twin(&mut album, &twin));
    }

    // #542: the reporter's two-entry playlist. Zoo's entry reached the queue as the song, which the
    // bare `/next` paired with its main video; The Fox's as the video the playlist holds, which
    // nothing paired. The playlist's pairs win, whichever side the row is on.
    #[test]
    fn a_playlist_twin_replaces_the_bare_one_from_either_side() {
        let pair = |song: &str, video: &str| innertube::SongItem {
            video_id: song.into(),
            thumbnail: Some(format!("{song}-cover")),
            counterpart: Some(video.into()),
            counterpart_thumbnail: Some(format!("{video}-frame")),
            ..Default::default()
        };
        let pairs = [pair("zoo", "zoo-playlist-video"), pair("fox", "fox-video")];
        let mut rows = [
            innertube::SongItem {
                counterpart: Some("zoo-main-video".into()),
                counterpart_thumbnail: Some("main-frame".into()),
                counterpart_segments: vec![seg(0, 5000, 1000)],
                ..pair("zoo", "zoo-main-video")
            },
            innertube::SongItem {
                video_id: "fox-video".into(),
                is_video: true,
                ..Default::default()
            },
            innertube::SongItem { video_id: "elsewhere".into(), ..Default::default() },
        ];
        assert!(adopt_playlist_twins(rows.iter_mut(), &pairs));
        assert_eq!(rows[0].counterpart.as_deref(), Some("zoo-playlist-video"));
        assert_eq!(rows[0].counterpart_thumbnail.as_deref(), Some("zoo-playlist-video-frame"));
        assert!(rows[0].counterpart_segments.is_empty(), "the old video's segments went with it");
        assert_eq!(rows[1].counterpart.as_deref(), Some("fox"));
        assert_eq!(rows[1].counterpart_thumbnail.as_deref(), Some("fox-cover"));
        assert_eq!(rows[2].counterpart, None);
        // A second answer with the same pairs changes nothing.
        assert!(!adopt_playlist_twins(rows.iter_mut(), &pairs));
    }

    #[test]
    fn only_a_youtube_playlist_is_asked_for_its_twins() {
        assert_eq!(playlist_context(Some("VLPLCruHskPLraU")).as_deref(), Some("PLCruHskPLraU"));
        assert_eq!(playlist_context(Some("LM")).as_deref(), Some("LM"));
        assert_eq!(playlist_context(Some("VLRDCLAK5uy_x")), None);
        assert_eq!(playlist_context(Some("OLAK5uy_album")), None);
        assert_eq!(playlist_context(Some(&format!("{LOCAL_PLAYLIST_PREFIX}1"))), None);
        assert_eq!(playlist_context(None), None);
    }

    // A video that reorders the song's parts would swap into segments out of order.
    #[test]
    fn swapped_segments_stay_in_order() {
        let mut it = innertube::SongItem {
            video_id: "v".into(),
            counterpart: Some("s".into()),
            counterpart_segments: vec![seg(0, 50000, 10000), seg(20000, 0, 10000)],
            ..Default::default()
        };
        assert!(swap_version(&mut it));
        assert_eq!(it.counterpart_segments, [seg(0, 20000, 10000), seg(50000, 0, 10000)]);
    }

    #[test]
    fn prefer_version_only_moves_rows_off_the_wanted_side() {
        let mut it = innertube::SongItem {
            video_id: "audio".into(),
            counterpart: Some("video".into()),
            ..Default::default()
        };
        assert!(!prefer_version(&mut it, false));
        assert!(prefer_version(&mut it, true));
        assert_eq!(it.video_id, "video");
        assert!(!prefer_version(&mut it, true));
        assert!(prefer_version(&mut it, false));
        assert_eq!(it.video_id, "audio");
        // A music video with nothing to switch to stays a video.
        let mut lone =
            innertube::SongItem { video_id: "v".into(), is_video: true, ..Default::default() };
        assert!(!prefer_version(&mut lone, false));
    }

    #[test]
    fn a_cached_video_url_lives_then_expires() {
        let mut map = VideoUrls::new();
        let t0 = 1_700_000_000;
        put_url(&mut map, "abc", "https://example.invalid/v".into(), t0);
        assert_eq!(get_url(&map, "abc", t0 + 60).as_deref(), Some("https://example.invalid/v"));
        assert_eq!(get_url(&map, "abc", t0 + 5 * 60 * 60), None, "past the TTL, must miss");
        assert_eq!(get_url(&map, "nope", t0).as_deref(), None);
    }

    /// The play threshold decides whether a play ever reaches YouTube's history or Your Rotation, and
    /// the two ends are the ones that bite: a track whose duration mpv hasn't reported yet must not
    /// count instantly, and a long track must not need half an hour.
    #[test]
    fn a_play_counts_halfway_in_or_at_thirty_seconds() {
        assert_eq!(history_threshold(0.0), 30.0, "unknown duration falls back to the full 30s");
        assert_eq!(history_threshold(40.0), 20.0, "short track: halfway");
        assert_eq!(history_threshold(600.0), 30.0, "long track: capped, not five minutes");
    }

    /// The cap is a clear, not an eviction, so the ninth insert leaves exactly one entry.
    #[test]
    fn the_video_url_map_clears_at_its_cap() {
        let mut map = VideoUrls::new();
        for i in 0..9 {
            put_url(&mut map, &format!("id{i}"), "u".into(), 0);
        }
        assert_eq!(map.len(), 1);
        assert!(get_url(&map, "id8", 0).is_some(), "the newest insert survives the clear");
    }

    #[test]
    fn queue_fingerprint_tracks_the_rows_not_their_metadata() {
        let row = |id: &str| innertube::SongItem { video_id: id.into(), ..Default::default() };
        let a = vec![row("a"), row("b"), row("c")];
        // Same rows in the same order: the UI already has them.
        assert_eq!(queue_fingerprint(&a), queue_fingerprint(&[row("a"), row("b"), row("c")]));
        // A reorder, an append and a removal all have to be visible.
        assert_ne!(queue_fingerprint(&a), queue_fingerprint(&[row("a"), row("c"), row("b")]));
        assert_ne!(queue_fingerprint(&a), queue_fingerprint(&[row("a"), row("b")]));
        assert_ne!(
            queue_fingerprint(&a),
            queue_fingerprint(&[row("a"), row("b"), row("c"), row("d")])
        );
        // A metadata repair on an existing row does not: it rides on the index event instead.
        let mut patched = a.clone();
        patched[0].duration = Some("3:11".into());
        assert_eq!(queue_fingerprint(&a), queue_fingerprint(&patched));
        // `restore_queue` compares these as strings off disk, not as numbers, so the string form
        // has to discriminate as well as the hash does: a stamp that matched everything would
        // silently re-open the wrong-track gap the stamp exists to close.
        assert_ne!(
            queue_fingerprint(&a).to_string(),
            queue_fingerprint(&[row("a"), row("b")]).to_string()
        );
    }

    // `move_in_queue` can drag one occurrence of a repeated videoId past the other (a track queued
    // twice from two different playlists, say). The panel tells the two rows apart by the fields
    // that differ between them, not by videoId alone, so the fingerprint has to as well: otherwise
    // the swap is invisible, `emit_queue` stays on the small `queue-index` event, and the drag
    // looks like it did nothing.
    #[test]
    fn queue_fingerprint_distinguishes_same_video_id_rows_by_queue_metadata() {
        let row = |queued_from: Option<&str>| innertube::SongItem {
            video_id: "x".into(),
            queued_from: queued_from.map(Into::into),
            ..Default::default()
        };
        let a = vec![row(Some("Chill Mix")), row(None)];
        let b = vec![row(None), row(Some("Chill Mix"))];
        assert_ne!(queue_fingerprint(&a), queue_fingerprint(&b));

        // Same story for `set_video_id`, the other field that tells apart two occurrences of the
        // same videoId (one came from a playlist page, so it carries a playlistSetVideoId; a
        // second copy queued manually doesn't).
        let row2 = |set_video_id: Option<&str>| innertube::SongItem {
            video_id: "x".into(),
            set_video_id: set_video_id.map(Into::into),
            ..Default::default()
        };
        let c = vec![row2(Some("1")), row2(Some("2"))];
        let d = vec![row2(Some("2")), row2(Some("1"))];
        assert_ne!(queue_fingerprint(&c), queue_fingerprint(&d));
    }

    // `shuffle_upcoming` only reorders rows after `current`, so toggling shuffle while the last
    // track is playing leaves `items` byte-identical: the rows-only `queue_fingerprint` can't see
    // it, and `persist_queue`'s dirty check has to cover the rest of what `queue_json` stores or
    // the toggle is lost across a restart.
    #[test]
    fn persist_fingerprint_changes_when_shuffle_toggles_with_identical_items() {
        let mut q = QueueState {
            items: vec![innertube::SongItem { video_id: "a".into(), ..Default::default() }],
            ..QueueState::default()
        };
        let off = persist_fingerprint(&q);
        q.shuffle_orig = Some(q.items.clone());
        let on = persist_fingerprint(&q);
        assert_ne!(off, on);
    }

    #[test]
    fn durations_round_trip_past_an_hour() {
        assert_eq!(format_duration(191_000), "3:11");
        assert_eq!(format_duration(5_468_000), "1:31:08");
        // The parser is the inverse for both shapes (queue rows persist the string).
        assert_eq!(parse_duration_ms(Some("1:31:08")), 5_468_000);
        assert_eq!(parse_duration_ms(Some("3:11")), 191_000);
    }
    use std::collections::HashSet;

    // `by.is_some()` (a named guest/host add) and a nameless solo add are both manual adds:
    // the `queued` marker is what forms the FIFO block, not the name.
    fn song(id: &str, by: Option<&str>) -> innertube::SongItem {
        innertube::SongItem {
            video_id: id.into(),
            title: id.into(),
            queued: by.is_some(),
            queued_by: by.map(Into::into),
            ..Default::default()
        }
    }

    /// The repair every entry path goes through. Covers the two shapes that reached Last.fm as an
    /// artist name: nothing at all (album rows), and a whole display subtitle (song cards, and rows
    /// replayed out of Your Rotation that were recorded back when the card parser leaked one).
    #[test]
    fn player_response_repairs_a_missing_or_bogus_artist() {
        let with = |artists: &str, runs: Vec<&str>| innertube::SongItem {
            artists: artists.into(),
            artist_runs: runs
                .into_iter()
                .map(|t| innertube::models::metadata::ArtistRun {
                    text: t.into(),
                    id: Some("UCstale".into()),
                })
                .collect(),
            ..song("v", None)
        };

        // No artist (an album track) → YouTube's own author.
        let mut it = with("", vec![]);
        backfill_metadata(&mut it, None, Some("Delara"), None, None);
        assert_eq!(it.artists, "Delara");

        // A display subtitle that was parsed as the artist → replaced, and the links that pointed
        // at the old text go with it (the UI renders runs instead of `artists` when they exist).
        for bogus in [
            "Miley Cyrus • Plastic Hearts • 2020",
            "late night slow • 29M views",
            "Song • Dua Lipa",
        ] {
            let mut it = with(bogus, vec![bogus]);
            backfill_metadata(&mut it, None, Some("Dua Lipa"), None, None);
            assert_eq!(it.artists, "Dua Lipa", "{bogus} should have been replaced");
            assert!(it.artist_runs.is_empty(), "{bogus}: stale links must not survive the swap");
        }

        // A real artist line is never second-guessed, links included. Collabs use "&" and ",".
        let mut it = with("Nicki Minaj, Ice Spice & Aqua", vec!["Nicki Minaj", " & ", "Aqua"]);
        backfill_metadata(&mut it, None, Some("Nicki Minaj"), None, None);
        assert_eq!(it.artists, "Nicki Minaj, Ice Spice & Aqua");
        assert_eq!(it.artist_runs.len(), 3);

        // Nothing to repair with (rustypipe served the stream, or /player had no author) → as-is.
        for author in [None, Some(""), Some("   ")] {
            let mut it = with("", vec![]);
            backfill_metadata(&mut it, None, author, None, None);
            assert_eq!(it.artists, "");
        }
    }

    #[test]
    fn player_response_fills_only_a_missing_duration() {
        let mut it = song("v", None);
        backfill_metadata(&mut it, Some("191"), None, None, None);
        assert_eq!(it.duration.as_deref(), Some("3:11"));
        // A duration the row already carried wins: it's the length of the cut YouTube listed.
        let mut it = innertube::SongItem { duration: Some("3:02".into()), ..song("v", None) };
        backfill_metadata(&mut it, Some("191"), None, None, None);
        assert_eq!(it.duration.as_deref(), Some("3:02"));
        // Junk from the player response can't blank an existing one.
        let mut it = song("v", None);
        backfill_metadata(&mut it, Some("not-a-number"), None, None, None);
        assert_eq!(it.duration, None);
    }

    /// The player view's music-video mode reads this flag, and the rows that reach it are only as
    /// good as the parse that built them: a card played from a shelf carries no flag at all, and an
    /// album row keeps a `true` after being swapped to its audio id. `/player` settles both ways.
    #[test]
    fn player_response_settles_whether_the_track_is_a_video() {
        let mut it = song("v", None);
        backfill_metadata(&mut it, None, None, Some(true), None);
        assert!(it.is_video);
        // A stale `true` on what YouTube calls an audio track is cleared, not kept: video mode on
        // an art track shows a still image, which is worse than the artwork it replaced.
        backfill_metadata(&mut it, None, None, Some(false), None);
        assert!(!it.is_video);
        // Nothing said (rustypipe, or a cached URL) → whatever the row already claimed stands.
        it.is_video = true;
        backfill_metadata(&mut it, None, None, None, None);
        assert!(it.is_video);
        // The podcast flag (#500) on the same terms: an episode row stays one when nothing was said.
        it.is_episode = true;
        backfill_metadata(&mut it, None, None, None, None);
        assert!(it.is_episode);
        backfill_metadata(&mut it, None, None, None, Some(false));
        assert!(!it.is_episode);
    }

    /// The trim is what keeps an endless radio session from growing the queue (and every emit and
    /// every persisted blob with it) forever. Indices are what it can break, so assert on video ids.
    #[test]
    fn trim_played_keeps_the_history_bounded() {
        let mut items: Vec<_> = (0..300).map(|i| song(&format!("s{i}"), None)).collect();

        // Nothing to drop while the played run is still within the cap.
        let mut short = items[..150].to_vec();
        assert_eq!(trim_played(&mut short, 150, KEEP_PLAYED), 0);
        assert_eq!(short.len(), 150);
        assert_eq!(trim_played(&mut Vec::new(), 0, KEEP_PLAYED), 0);

        // Past the cap: exactly the excess goes, and the playing row is still the playing row.
        let current = 250;
        let playing = items[current].video_id.clone();
        let dropped = trim_played(&mut items, current, KEEP_PLAYED);
        assert_eq!(dropped, 50);
        assert_eq!(items.len(), 250);
        assert_eq!(items[current - dropped].video_id, playing);

        // An endless radio: append a batch, advance through it, trim. Bounded, not 600-and-rising.
        let mut items = vec![song("seed", None)];
        let mut current = 0;
        for hop in 0..30 {
            items.extend((0..20).map(|i| song(&format!("h{hop}-{i}"), None)));
            current += 20;
            let playing = items[current].video_id.clone();
            current -= trim_played(&mut items, current, KEEP_PLAYED);
            assert_eq!(items[current].video_id, playing);
            assert!(items.len() <= KEEP_PLAYED + 21, "grew to {}", items.len());
        }
    }

    // Autoplay switched off (or Clear queue): only what is still to come goes, and the pre-shuffle
    // snapshot loses the same tracks, or turning shuffle off would put them back.
    #[test]
    fn dropping_upcoming_keeps_history_and_the_shuffle_snapshot_in_step() {
        let auto = |id: &str| innertube::SongItem { autoplay: true, ..song(id, None) };
        let ids = |v: &[innertube::SongItem]| {
            v.iter().map(|i| i.video_id.as_str()).collect::<Vec<_>>().join(",")
        };
        let mut q = QueueState {
            items: vec![
                auto("played"),
                song("now", None),
                auto("r1"),
                song("a1", None),
                auto("r2"),
            ],
            current: 1,
            shuffle_orig: Some(vec![
                auto("played"),
                song("now", None),
                song("a1", None),
                auto("r1"),
                auto("r2"),
            ]),
            ..Default::default()
        };
        assert!(retain_upcoming(&mut q, |i| !i.autoplay));
        assert_eq!(ids(&q.items), "played,now,a1");
        assert_eq!(ids(q.shuffle_orig.as_deref().unwrap()), "played,now,a1");
        assert_eq!(q.current, 1);
        assert!(!retain_upcoming(&mut q, |i| !i.autoplay), "nothing left to drop");
    }

    #[test]
    fn guest_adds_stack_fifo_after_current() {
        let solo = |id: &str| innertube::SongItem { queued: true, ..song(id, None) };
        // Host playlist [A*, B, C] (playing A): manual add goes right after current, not the end.
        let items = vec![song("a", None), song("b", None), song("c", None)];
        assert_eq!(guest_insert_index(&items, 0), 1);
        // A guest track already up next → the new one queues behind it (FIFO), before B.
        let items = vec![song("a", None), song("g1", Some("kim")), song("b", None)];
        assert_eq!(guest_insert_index(&items, 0), 2);
        // Solo adds (no name, `queued`) form the same FIFO block: the next add lands behind them.
        let items = vec![song("a", None), solo("s1"), song("b", None)];
        assert_eq!(guest_insert_index(&items, 0), 2);
        // Current is the last item → append.
        let items = vec![song("a", None)];
        assert_eq!(guest_insert_index(&items, 0), 1);
        // Empty queue (nothing playing yet) → index 0… clamped, no panic.
        assert_eq!(guest_insert_index(&[], 0), 0);
    }

    // "Start radio" on the song that's already playing: the track keeps playing, the tail is
    // replaced, and the manual adds the user made are not collateral damage (Metrolist drops them).
    #[test]
    fn radio_replaces_the_tail_but_keeps_history_and_manual_adds() {
        let ids = |items: &[innertube::SongItem]| {
            items.iter().map(|i| i.video_id.clone()).collect::<Vec<_>>()
        };
        let mut q = QueueState {
            // played, playing, a manual add, then two tracks of the old context
            items: vec![
                song("done", None),
                song("now", None),
                song("mine", Some("me")),
                song("old1", None),
                song("old2", None),
            ],
            current: 1,
            ..QueueState::default()
        };
        // The radio's first page leads with the seed song — it must not be queued a second time.
        // Nor may it re-offer what's still in the queue (history, or the user's own adds). A track
        // from the replaced tail (`old1`) is fair game: it isn't in the queue any more.
        let page = vec![
            song("now", None),
            song("r1", None),
            song("done", None),
            song("mine", None),
            song("old1", None),
        ];
        splice_radio_into(&mut q, page, "RDAMVMnow".into(), Some("Now Radio".into()));

        assert_eq!(ids(&q.items), ["done", "now", "mine", "r1", "old1"]);
        assert_eq!(q.current, 1); // nothing moved under the playing track
        assert_eq!(q.radio_seed.as_deref(), Some("RDAMVMnow"));
        assert_eq!(q.source_name.as_deref(), Some("Now Radio"));
        // Radio tracks are the queue, not autoplay filler — no "Autoplay" divider under them.
        assert!(q.items.iter().all(|i| !i.autoplay));
    }

    #[test]
    fn radio_started_under_shuffle_stays_shuffled_and_restorable() {
        let mut items = vec![song("now", None)];
        let mut q = QueueState {
            shuffle_orig: Some(items.clone()),
            items: std::mem::take(&mut items),
            current: 0,
            ..QueueState::default()
        };
        let page: Vec<_> = (0..50).map(|i| song(&format!("r{i}"), None)).collect();
        splice_radio_into(&mut q, page, "RDAMVMnow".into(), None);

        assert_eq!(q.items[0].video_id, "now"); // the playing track never moves
                                                // Un-shuffle has the whole radio to restore, not just what was there before it started.
        let orig = q.shuffle_orig.as_ref().unwrap();
        assert_eq!(orig.len(), 51);
        let mut a: Vec<_> = orig.iter().map(|i| i.video_id.clone()).collect();
        let mut b: Vec<_> = q.items.iter().map(|i| i.video_id.clone()).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b); // nothing lost, nothing duplicated
    }

    #[test]
    fn next_index_wraps_only_on_repeat_all() {
        assert_eq!(next_index(3, 2, RepeatMode::Off), None);
        assert_eq!(next_index(3, 2, RepeatMode::All), Some(0));
        assert_eq!(next_index(3, 2, RepeatMode::One), None); // mpv loops the file itself
        assert_eq!(next_index(3, 0, RepeatMode::Off), Some(1));
        assert_eq!(next_index(0, 0, RepeatMode::All), None);
        assert_eq!(next_index(1, 0, RepeatMode::All), Some(0)); // single-item wrap
    }

    // The background playlist walk: a page that lands mid-playback has to reach the *unplayed*
    // tail (otherwise "shuffle" only ever shuffles page 1), without moving the track already
    // handed to mpv for the gapless advance.
    #[test]
    fn appended_page_mixes_into_the_tail_but_leaves_the_primed_slot() {
        let ids = |items: &[innertube::SongItem]| {
            items.iter().map(|i| i.video_id.clone()).collect::<Vec<_>>()
        };
        let page = || (0..100).map(|i| song(&format!("p{i}"), None)).collect::<Vec<_>>();
        let mut tail_positions = Vec::new();

        for _ in 0..20 {
            let items = vec![song("a", None), song("b", None), song("c", None)];
            let mut q = QueueState {
                shuffle_orig: Some(items.clone()),
                items,
                current: 0,
                lookahead_loaded: Some(1), // "b" is already loaded into mpv
                ..QueueState::default()
            };
            append_page(&mut q, page(), true, false);

            assert_eq!(q.items.len(), 103);
            assert_eq!(q.items[0].video_id, "a"); // playing
            assert_eq!(q.items[1].video_id, "b"); // primed gapless slot, pinned
                                                  // Un-shuffle restores the whole playlist, including pages that arrived after playback
                                                  // started.
            assert_eq!(ids(q.shuffle_orig.as_ref().unwrap()).len(), 103);
            assert_eq!(ids(q.shuffle_orig.as_ref().unwrap())[..3], ["a", "b", "c"]);
            let mut sorted = ids(&q.items);
            sorted.sort();
            let mut expected = ids(q.shuffle_orig.as_ref().unwrap());
            expected.sort();
            assert_eq!(sorted, expected); // nothing lost, nothing duplicated

            tail_positions.push(q.items.iter().position(|i| i.video_id == "c").unwrap());
        }
        // Without the re-shuffle the page would sit behind "c" and it would land at index 2 every
        // time. (Staying at 2 in all 20 runs by chance is (1/101)^20.)
        assert!(tail_positions.iter().any(|&p| p != 2), "page was appended, not mixed in");
    }

    #[test]
    fn appended_page_keeps_order_when_shuffle_is_off() {
        let mut q = QueueState {
            items: vec![song("a", None), song("b", None)],
            current: 0,
            ..QueueState::default()
        };
        append_page(&mut q, vec![song("c", None), song("d", None)], true, false);
        let ids: Vec<_> = q.items.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        assert!(q.shuffle_orig.is_none());
    }

    // The rest of an added playlist joins the block its first page went into — not the tail of the
    // queue, which is a different place entirely now that "Add to queue" inserts up front. Order
    // kept even under shuffle: the user queued that album, shuffle belongs to what's playing.
    #[test]
    fn appended_page_joins_the_add_to_queue_block_it_belongs_to() {
        // Two playlists added back to back at the tail, the first still being walked: its next
        // page goes behind its own rows, not behind the second one.
        let from = |id: &str, pl: &str| innertube::SongItem {
            queued_end: true,
            queued_from: Some(pl.into()),
            ..song(id, None)
        };
        let items = vec![song("a", None), song("b", None), from("x1", "X"), from("y1", "Y")];
        let mut q = QueueState {
            shuffle_orig: Some(items.clone()),
            items,
            current: 0,
            ..QueueState::default()
        };
        append_page(&mut q, vec![from("x2", "X"), from("x3", "X")], false, false);
        let ids: Vec<_> = q.items.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "x1", "x2", "x3", "y1"]);
        // Still on the snapshot, so un-shuffle keeps them.
        assert_eq!(q.shuffle_orig.as_ref().unwrap().len(), 6);
    }

    // The owner's phone: iOS took the proxy's socket, every track was "refused", and the queue
    // walked itself 187 tracks deep. A dead proxy is not a bad track: reload the same one through
    // the rebuilt proxy, without spending its retry.
    #[test]
    fn a_dead_proxy_retries_the_same_track_instead_of_skipping() {
        use crate::loopback::Health;
        let rebuilt = Some(Health::Rebound { port: 53050, moved: false });
        assert_eq!(failure_action(rebuilt, true, false, 0), FailAction::Retry { infra: true });
        // Even after the track's own retry was used up.
        assert_eq!(failure_action(rebuilt, true, true, 0), FailAction::Retry { infra: true });
        let moved = Some(Health::Rebound { port: 60001, moved: true });
        assert_eq!(failure_action(moved, true, true, 1), FailAction::Retry { infra: true });
        // A proxy that cannot be brought back: stop, never advance.
        assert_eq!(failure_action(Some(Health::Down), true, false, 0), FailAction::Halt);
        // One that keeps dying under every retry is bounded too.
        assert_eq!(failure_action(rebuilt, true, false, MAX_FAILURE_STREAK - 1), FailAction::Halt);
    }

    /// Drives a queue whose every resolve fails with `e` the way `load_current` does (skip bumps
    /// the streak, anything else stops) and returns (tracks attempted, how it stopped).
    fn walk_failing_queue(e: &crate::orchestrator::ResolveError, len: usize) -> (usize, ResolveFail) {
        let mut streak = 0;
        for attempted in 1..=len {
            match resolve_failure_action(e, streak) {
                ResolveFail::Skip => streak += 1,
                stop => return (attempted, stop),
            }
        }
        (len, ResolveFail::Skip)
    }

    // The owner's iPhone, TestFlight 1.0.1 build 13: Next in full-screen lyrics, then a new track
    // every 1.7 s for ~70 tracks, none of them played. The play loop's path.
    #[test]
    fn the_play_loop_never_skip_storms() {
        use crate::orchestrator::ResolveError as E;
        let v = || "v".to_owned();
        // A refusal (bot check, 403, consent page) stops on the first track, unmarked.
        assert_eq!(walk_failing_queue(&E::Refused(v()), 70), (1, ResolveFail::Stay));
        assert_eq!(walk_failing_queue(&E::Unreachable(v()), 70), (1, ResolveFail::Stay));
        // Real per-video verdicts are skipped, but only twice: the third in a row stops.
        assert_eq!(
            walk_failing_queue(&E::AllClientsFailed(v()), 70),
            (MAX_FAILURE_STREAK as usize, ResolveFail::Halt)
        );
        assert_eq!(walk_failing_queue(&E::UploadUnavailable(v()), 70).1, ResolveFail::Halt);
        // A track that plays resets the streak (on_position/on_duration), so a dead track
        // between good ones is still skipped.
        assert_eq!(resolve_failure_action(&E::AllClientsFailed(v()), 0), ResolveFail::Skip);
    }

    // The same night's first symptom: "lookahead resolve failed, marking unavailable" for tracks
    // that were fine. The lookahead's path.
    #[test]
    fn the_lookahead_marks_only_verdicts_and_only_until_the_streak_runs_out() {
        use crate::orchestrator::ResolveError as E;
        let v = || "v".to_owned();
        for streak in 0..5 {
            assert_eq!(resolve_failure_action(&E::Refused(v()), streak), ResolveFail::Stay);
            assert_eq!(resolve_failure_action(&E::SignInRequired(v()), streak), ResolveFail::Stay);
        }
        assert_eq!(resolve_failure_action(&E::AllClientsFailed(v()), 0), ResolveFail::Skip);
        assert_eq!(resolve_failure_action(&E::AllClientsFailed(v()), 1), ResolveFail::Skip);
        assert_eq!(
            resolve_failure_action(&E::AllClientsFailed(v()), MAX_FAILURE_STREAK - 1),
            ResolveFail::Halt
        );
    }

    // A healthy proxy: the track itself failed. Retry once, then advance, as before.
    #[test]
    fn a_bad_track_is_retried_once_then_skipped() {
        use crate::loopback::Health;
        for proxy in [Some(Health::Alive), None] {
            assert_eq!(failure_action(proxy, true, false, 0), FailAction::Retry { infra: false });
            assert_eq!(failure_action(proxy, true, true, 0), FailAction::Advance);
        }
    }

    // Never a skip storm, whatever the cause: three tracks in a row failing stops playback on the
    // third (paused, one message) instead of cycling the queue. Simulates the event sequence the
    // pump produces, with the streak bookkeeping `on_track_failed` does.
    #[test]
    fn three_failed_tracks_in_a_row_stop_playback() {
        use crate::loopback::Health;
        let mut streak = 0;
        let mut advanced = 0;
        let mut retried: Option<usize> = None;
        let mut track = 0;
        let outcome = loop {
            let already = retried == Some(track);
            match failure_action(Some(Health::Alive), true, already, streak) {
                FailAction::Retry { infra } => {
                    assert!(!infra);
                    retried = Some(track);
                }
                FailAction::Advance => {
                    streak += 1;
                    advanced += 1;
                    track += 1;
                }
                FailAction::Halt => break track,
            }
            assert!(track < 10, "skip storm");
        };
        assert_eq!(advanced, 2);
        assert_eq!(outcome, 2, "stops on the third track, having tried it");
    }

    // `on_track_failed` retries a track once and marks it. Without a reset that reads as "once,
    // ever": the same song, played again tomorrow, would get no retry at all.
    #[test]
    fn moving_the_pointer_clears_the_one_retry_marker() {
        let mut q = QueueState {
            items: vec![song("a", None), song("b", None)],
            current: 0,
            retried: Some("a".into()),
            ..QueueState::default()
        };
        q.seek_to(1);
        assert_eq!(q.retried, None);
    }

    // A cached URL has to outlive the track, not just the load. googlevideo keeps serving a
    // response it already started, so a stale URL plays from the top and dies on the first seek.
    #[test]
    fn a_cached_url_must_outlive_the_track_it_is_replayed_for() {
        let now = 1_000;
        let three_minutes = 180;
        let an_hour = 3_847; // the mix from issue #188

        // A row with ten minutes left: fine for a song, not for the mix.
        let ten_minutes_left = now + 600;
        assert!(ten_minutes_left > cache_horizon(now, three_minutes));
        assert!(ten_minutes_left < cache_horizon(now, an_hour));

        // Unknown length falls back to the old load-only margin rather than refusing everything.
        assert_eq!(cache_horizon(now, 0), now + 60);
    }

    // The one that silently breaks: a duplicate sitting *before* the playing track. Removing it
    // renumbers the queue, so `current` has to follow its own song or the insert lands one slot off.
    #[test]
    fn dropping_a_duplicate_before_the_current_track_moves_the_current_index() {
        let mut items =
            vec![song("dup", None), song("a", None), song("b", None), song("dup", None)];
        let mut current = 1; // playing "a"
        let ids = HashSet::from(["dup"]);
        assert!(drop_duplicates(&mut items, &mut current, &ids));

        let left: Vec<_> = items.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(left, ["a", "b"]);
        assert_eq!(current, 0); // still playing "a"
        assert_eq!(guest_insert_index(&items, current), 1); // the add lands right after it

        // The playing track is exempt: "play next" on the current song is a repeat gesture.
        let mut items = vec![song("a", None), song("b", None)];
        let mut current = 0;
        assert!(!drop_duplicates(&mut items, &mut current, &HashSet::from(["a"])));
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn mixes_are_not_walked() {
        assert!(is_mix(Some("VLRDCLAK5uy_abc"))); // a mix as the playlist page sees it
        assert!(is_mix(Some("RDAMVMxyz")));
        assert!(!is_mix(Some("VLPL123")));
        assert!(!is_mix(Some("VLOLAK5uy_abc")));
        assert!(!is_mix(None));
    }

    /// A row removed while shuffled stays gone when shuffle goes off, and of two copies of a song
    /// only the one removed leaves the snapshot.
    #[test]
    fn removed_row_stays_out_after_unshuffle() {
        let row = |id: &str, set: &str| innertube::SongItem {
            set_video_id: Some(set.into()),
            ..song(id, None)
        };
        let orig = vec![row("a", "1"), row("b", "2"), row("c", "3"), row("b", "4")];
        let mut q = QueueState {
            items: vec![row("a", "1"), row("b", "4"), row("c", "3"), row("b", "2")],
            shuffle_orig: Some(orig),
            ..Default::default()
        };
        assert_eq!(q.remove_row(3).set_video_id.as_deref(), Some("2"));
        let orig = q.shuffle_orig.take().unwrap();
        let sets = orig.iter().map(|s| s.set_video_id.clone().unwrap()).collect::<Vec<_>>();
        assert_eq!(sets, ["1", "3", "4"]);
        let (items, _) = unshuffled(orig, &HashSet::from(["a".to_owned()]), "a", 0, vec![]);
        assert_eq!(items.iter().filter(|s| s.video_id == "b").count(), 1);
        // A carried "Play next" of a song the playlist also holds has no snapshot row of its own.
        let mut q = QueueState {
            items: vec![row("a", "1"), song("c", Some("me")), row("c", "3")],
            shuffle_orig: Some(vec![row("a", "1"), row("c", "3")]),
            ..Default::default()
        };
        q.remove_row(1);
        assert_eq!(q.shuffle_orig.unwrap().len(), 2, "the playlist's own copy stays");
    }

    #[test]
    fn unshuffle_restores_order_and_current() {
        let orig = vec![song("a", None), song("b", None), song("c", None), song("d", None)];
        let heard = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<HashSet<String>>();
        let ids = |items: &[innertube::SongItem]| {
            items.iter().map(|i| i.video_id.clone()).collect::<Vec<_>>()
        };
        // Shuffle played "c" first → nothing upcoming has been heard, plain restore.
        let (items, idx) = unshuffled(orig.clone(), &heard(&["c"]), "c", 9, vec![]);
        assert_eq!(ids(&items), ["a", "b", "c", "d"]);
        assert_eq!(idx, 2);
        // A full shuffled pass ending on "c": "d" already played, so it must not come back as
        // up-next; "a"/"b" stay behind the playing track as hidden history.
        let (items, idx) = unshuffled(orig.clone(), &heard(&["a", "b", "c", "d"]), "c", 9, vec![]);
        assert_eq!(ids(&items), ["a", "b", "c"]);
        assert_eq!(idx, 2);
        // Partial pass: only the heard ones are dropped from upcoming, order otherwise intact.
        let (items, idx) = unshuffled(orig.clone(), &heard(&["a", "c"]), "a", 9, vec![]);
        assert_eq!(ids(&items), ["a", "b", "d"]);
        assert_eq!(idx, 0);
        // Playing id absent from the snapshot → fallback index, clamped to len-1.
        let (_, idx) = unshuffled(orig, &HashSet::new(), "zz", 9, vec![]);
        assert_eq!(idx, 3);

        // Manual adds come from the live queue, not the snapshot (which may hold stale copies or
        // none at all): Play next, then Add to queue, right after the playing track (Spotify).
        let next = |id: &str| innertube::SongItem { queued: true, ..song(id, None) };
        let end = |id: &str| innertube::SongItem { queued_end: true, ..song(id, None) };
        let with_adds = vec![song("a", None), song("b", None), end("stale"), song("c", None)];
        let (items, idx) =
            unshuffled(with_adds, &heard(&["a"]), "a", 9, vec![next("mine"), end("tail")]);
        assert_eq!(ids(&items), ["a", "mine", "tail", "b", "c"]);
        assert_eq!(idx, 0);
    }

    #[test]
    fn shuffle_preserves_prefix_and_multiset() {
        let ids: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let mut items: Vec<_> = ids.iter().map(|id| song(id, None)).collect();
        shuffle_upcoming(&mut items, 2, false);
        // The playing track and the already-played prefix stay put…
        for (i, id) in ids.iter().take(3).enumerate() {
            assert_eq!(&items[i].video_id, id);
        }
        // …and the whole queue is a permutation (nothing lost, nothing duplicated).
        let mut got: Vec<_> = items.iter().map(|i| i.video_id.clone()).collect();
        got.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn new_queue_shuffle_plays_clicked_track_first() {
        let ids: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let mut items: Vec<_> = ids.iter().map(|id| song(id, None)).collect();
        let cur = shuffle_new_queue(&mut items, 7);
        assert_eq!(cur, 0);
        assert_eq!(items[0].video_id, "t7"); // the clicked track plays first…
        let mut got: Vec<_> = items.iter().map(|i| i.video_id.clone()).collect();
        got.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(got, want); // …and the rest is a permutation of everything else
                               // Degenerate cases: empty and single-item queues don't panic.
        let mut empty: Vec<innertube::SongItem> = vec![];
        assert_eq!(shuffle_new_queue(&mut empty, 3), 0);
        let mut one = vec![song("only", None)];
        assert_eq!(shuffle_new_queue(&mut one, 5), 0);
        assert_eq!(one[0].video_id, "only");
    }

    #[test]
    fn manual_adds_survive_context_switch() {
        let solo = |id: &str| innertube::SongItem { queued: true, ..song(id, None) };
        // Playing B (index 1); Q1/Q2 are unplayed manual adds, Q0 already played — only the
        // unplayed ones carry into a new queue.
        let items = vec![solo("q0"), song("b", None), solo("q1"), song("c", None), solo("q2")];
        let carried = upcoming_queued(&items, 1);
        let ids: Vec<_> = carried.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, ["q1", "q2"]);
        // Nothing queued upcoming → nothing carried.
        assert!(upcoming_queued(&[song("a", None)], 0).is_empty());
    }

    #[test]
    fn shuffle_leaves_manual_queue_block_in_place() {
        let solo = |id: &str| innertube::SongItem { queued: true, ..song(id, None) };
        let mut items = vec![song("now", None), solo("q1"), solo("q2")];
        items.extend((0..8).map(|i| song(&format!("t{i}"), None)));
        shuffle_upcoming(&mut items, 0, false);
        // The manual "Next in queue" block still plays next, in order.
        assert_eq!(items[1].video_id, "q1");
        assert_eq!(items[2].video_id, "q2");
        // Everything is still a permutation (nothing lost).
        assert_eq!(items.len(), 11);
    }

    // A whole album queued with "Play next" is a set of tracks the user shuffles like any other:
    // the block keeps its place at the front, its own tracks get randomized. Songs queued one at a
    // time keep the order they were queued in.
    #[test]
    fn shuffle_randomizes_a_play_next_album_but_not_loose_adds() {
        let solo = |id: &str| innertube::SongItem { queued: true, ..song(id, None) };
        let from = |id: &str, name: &str| innertube::SongItem {
            queued: true,
            queued_from: Some(name.into()),
            ..song(id, None)
        };
        let mut moved = false;
        for _ in 0..20 {
            let mut items = vec![song("now", None), solo("q1"), solo("q2")];
            items.extend((0..12).map(|i| from(&format!("a{i}"), "Album")));
            items.extend((0..4).map(|i| song(&format!("t{i}"), None)));
            shuffle_upcoming(&mut items, 0, false);
            // The loose adds stay where they were, in order, ahead of the album block.
            assert_eq!(items[1].video_id, "q1");
            assert_eq!(items[2].video_id, "q2");
            // The album block stays a block — it just plays in a different order inside it.
            let block: Vec<_> = items[3..15].iter().map(|i| i.video_id.clone()).collect();
            assert!(block.iter().all(|id| id.starts_with('a')), "the block kept its place");
            moved |= block != (0..12).map(|i| format!("a{i}")).collect::<Vec<_>>();
            let mut sorted = block.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), 12, "nothing lost or duplicated");
        }
        assert!(moved, "the album block was never shuffled");
    }

    #[test]
    fn shuffle_keeps_autoplay_after_queue_tracks() {
        let auto = |id: &str| innertube::SongItem { autoplay: true, ..song(id, None) };
        // Playing index 0; upcoming = 4 playlist tracks + 4 autoplay tracks.
        let mut items = vec![song("now", None)];
        items.extend((0..4).map(|i| song(&format!("p{i}"), None)));
        items.extend((0..4).map(|i| auto(&format!("a{i}"))));
        shuffle_upcoming(&mut items, 0, false);
        // Every playlist track still comes before every autoplay track.
        let flags: Vec<bool> = items[1..].iter().map(|i| i.autoplay).collect();
        assert_eq!(flags, [false, false, false, false, true, true, true, true]);
        assert_eq!(items.len(), 9);
    }

    // "Shuffle the whole queue": adds are mixed in with the playlist. Play next still plays next
    // and autoplay still comes last.
    #[test]
    fn shuffling_the_whole_queue_mixes_the_adds_in() {
        let queued = |id: &str| innertube::SongItem { queued: true, ..song(id, None) };
        let added = |id: &str| innertube::SongItem { queued_end: true, ..song(id, None) };
        let auto = |id: &str| innertube::SongItem { autoplay: true, ..song(id, None) };
        let mut items = vec![song("now", None), queued("next")];
        items.extend((0..20).map(|i| song(&format!("p{i}"), None)));
        items.extend((0..20).map(|i| added(&format!("x{i}"))));
        items.extend((0..2).map(|i| auto(&format!("a{i}"))));
        let before: HashSet<String> = items.iter().map(|i| i.video_id.clone()).collect();
        shuffle_upcoming(&mut items, 0, true);
        assert_eq!(items[1].video_id, "next");
        assert!(items[42..].iter().all(|i| i.autoplay));
        assert_eq!(items.iter().map(|i| i.video_id.clone()).collect::<HashSet<_>>(), before);
        // The odds of all twenty adds still sitting behind the playlist are 1 in 40 choose 20.
        assert!(items[2..22].iter().any(|i| i.queued_end), "adds are mixed into the playlist");
    }

    #[test]
    fn radio_seed_from_source() {
        // Playlist browseIds are VL-prefixed — stripped before building the radio id.
        // A mix is already a radio playlist — it seeds autoplay as itself, not wrapped again.
        assert_eq!(radio_seed_for(Some("VLRDCLAK5uy_x".into())).as_deref(), Some("RDCLAK5uy_x"));
        assert_eq!(radio_seed_for(Some("VLPL123".into())).as_deref(), Some("RDAMPLPL123"));
        // Album audio playlist ids come bare.
        assert_eq!(radio_seed_for(Some("OLAK5uy_x".into())).as_deref(), Some("RDAMPLOLAK5uy_x"));
        // No source (single song / artist top-songs) → no pinned seed.
        assert_eq!(radio_seed_for(None), None);
        // A playlist on this machine has no YouTube radio: autoplay seeds off the last track.
        assert_eq!(radio_seed_for(Some(format!("{LOCAL_PLAYLIST_PREFIX}7"))), None);
    }

    #[test]
    fn autoplay_merge_dedupes_and_caps() {
        let mut items = vec![song("a", None), song("b", None)];
        let existing: std::collections::HashSet<String> =
            items.iter().map(|i| i.video_id.clone()).collect();
        // "a" is already queued, "c" appears twice in the radio result — one copy survives.
        let fresh = vec![
            song("a", None),
            song("c", None),
            song("c", None),
            song("d", None),
            song("e", None),
        ];
        let added = merge_radio(&mut items, fresh, existing.clone(), 2);
        assert_eq!(added, 2); // cap honored: c + d, e cut off
        let ids: Vec<_> = items.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        // Appended tracks are marked autoplay (the UI's divider/badge key); originals aren't.
        assert!(items[2].autoplay && items[3].autoplay);
        assert!(!items[0].autoplay && !items[1].autoplay);
        // Nothing new in the radio result → 0, queue untouched (playback then stops as before).
        assert_eq!(merge_radio(&mut items.clone(), vec![song("a", None)], existing, 20), 0);
    }

    #[test]
    fn radio_puts_unheard_songs_first() {
        let mut items: Vec<_> = ["a", "b", "c", "d", "e"].map(|id| song(id, None)).into();
        let heard = [("a".to_owned(), 300), ("c".to_owned(), 100)].into();
        freshen(&mut items, &heard);
        let ids: Vec<_> = items.iter().map(|i| i.video_id.as_str()).collect();
        // Unheard keep YouTube's order; heard follow, the one heard longest ago first.
        assert_eq!(ids, ["b", "d", "e", "c", "a"]);
    }

    #[test]
    fn loudness_gain_moves_toward_the_target() {
        // "As It Was" measures loudnessDb 7.77 ⇒ -6.23 LUFS, 0.77 dB over the -7 target.
        assert_eq!(loudness_gain(Some(7.77)).map(|g| (g * 100.0).round()), Some(-77.0));
        // Exactly on target gets no filter at all.
        assert_eq!(loudness_gain(Some(7.0)), None);
        // Quieter masters are lifted: "Shape of You" 6.35 ⇒ +0.65 dB.
        assert_eq!(loudness_gain(Some(6.35)).map(|g| (g * 100.0).round()), Some(65.0));
        // ...but by no more than the cap ("bad guy" 0.11 would want +6.89).
        assert_eq!(loudness_gain(Some(0.11)), Some(MAX_BOOST_DB));
        assert_eq!(loudness_gain(Some(-5.0)), Some(MAX_BOOST_DB));
        // Extreme loudness clamps at −24 dB.
        assert_eq!(loudness_gain(Some(40.0)), Some(-24.0));
        // No metadata → no filter.
        assert_eq!(loudness_gain(None), None);
    }

    /// Clicking a song outside the queue replaces it, and Previous then has to reach the track
    /// that was playing, where it was left, with its own order and shuffle behind it.
    #[test]
    fn a_replaced_queue_comes_back_where_it_was_left() {
        let mut q = QueueState {
            items: vec![song("a", None), song("b", None), song("c", None)],
            current: 1,
            shuffle_orig: Some(vec![song("c", None), song("b", None), song("a", None)]),
            source_name: Some("Deep cuts".into()),
            source_id: Some("PL1".into()),
            radio_seed: Some("RDAMPLPL1".into()),
            ..QueueState::default()
        };

        q.keep_context(42.5);
        q.items = vec![song("clicked", None)]; // what `play_song` does next
        q.current = 0;
        assert!(q.shuffle_orig.is_none(), "the new queue starts on its own terms");

        let prev = q.prev_context.take().expect("the replaced queue is kept");
        assert_eq!(q.restore_context(prev), ("b".into(), 42.5));
        assert_eq!(
            q.items.iter().map(|i| i.video_id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(q.current, 1);
        assert_eq!(q.source_id.as_deref(), Some("PL1"));
        assert_eq!(q.radio_seed.as_deref(), Some("RDAMPLPL1"));
        assert!(q.shuffle_orig.is_some(), "shuffle came back with it");
        assert!(q.prev_context.is_none(), "one level deep: the restore is the end of it");
    }

    /// The panel's "Back to …" line is drawn from this, so it has to name the track Previous would
    /// actually start, and it has to disappear once the restore is out of reach.
    #[test]
    fn the_back_line_lasts_as_long_as_the_restore_does() {
        let mut q = QueueState {
            items: vec![song("a", None), song("b", None)],
            current: 1,
            ..QueueState::default()
        };
        assert_eq!(prev_track_title(&q), None, "nothing was replaced");

        q.keep_context(12.0);
        q.items = vec![song("clicked", None)];
        q.current = 0;
        assert_eq!(prev_track_title(&q), Some("b"));

        // The radio hydrated behind the clicked song and it played on: Previous has a track of its
        // own to reach now, so the kept queue and the line go.
        q.items.push(song("radio", None));
        q.seek_to(1);
        assert_eq!(prev_track_title(&q), None);
        // And back at the head (repeat-all wrapped, or the first row was clicked) it stays gone.
        q.seek_to(0);
        assert_eq!(prev_track_title(&q), None);
    }

    /// An empty queue is not a context worth coming back to — Previous would land on nothing.
    #[test]
    fn nothing_is_kept_from_an_empty_queue() {
        let mut q = QueueState::default();
        q.keep_context(0.0);
        assert!(q.prev_context.is_none());
    }

    // ── Encore: Spotify queue semantics ────────────────────────────────────────────────────

    fn ids_of(items: &[innertube::SongItem]) -> Vec<String> {
        items.iter().map(|i| i.video_id.clone()).collect()
    }
    fn next_row(id: &str) -> innertube::SongItem {
        innertube::SongItem { queued: true, ..song(id, None) }
    }
    fn added_row(id: &str) -> innertube::SongItem {
        innertube::SongItem { queued_end: true, ..song(id, None) }
    }
    fn auto_row(id: &str) -> innertube::SongItem {
        innertube::SongItem { autoplay: true, ..song(id, None) }
    }

    /// "Play next" goes right after the playing track, behind earlier Play next rows and ahead of
    /// Add to queue rows and the context.
    #[test]
    fn play_next_goes_after_earlier_play_next_ahead_of_adds() {
        let items = vec![song("now", None), next_row("n1"), added_row("x1"), song("p1", None)];
        assert_eq!(guest_insert_index(&items, 0), 2);
        let empty = vec![song("now", None), song("p1", None)];
        assert_eq!(guest_insert_index(&empty, 0), 1);
    }

    /// "Add to queue" appends to the manual queue ("Next in queue"), ahead of the rest of the
    /// playing playlist and of autoplay, in a playlist or a radio alike.
    #[test]
    fn add_to_queue_appends_to_the_manual_queue_ahead_of_the_context() {
        let q = QueueState {
            items: vec![song("a", None), song("b", None), song("c", None)],
            ..QueueState::default()
        };
        assert_eq!(enqueue_at(&q), 1, "nothing queued: right after the playing track");
        let q = QueueState {
            items: vec![
                song("a", None),
                next_row("mine"),
                added_row("x1"),
                song("b", None),
                auto_row("r1"),
            ],
            ..QueueState::default()
        };
        assert_eq!(enqueue_at(&q), 3, "behind earlier adds, ahead of the playlist");
        let q = QueueState {
            items: vec![song("r1", None), added_row("x1"), song("r2", None)],
            radio: true,
            ..QueueState::default()
        };
        assert_eq!(enqueue_at(&q), 2);
        let q = QueueState { items: vec![song("a", None)], ..QueueState::default() };
        assert_eq!(enqueue_at(&q), 1, "appended, not out of bounds");
    }

    /// The rest of a long playing playlist arrives page by page: it goes behind the manual queue
    /// and the playlist rows already there, ahead of autoplay.
    #[test]
    fn the_playing_playlists_next_page_goes_behind_the_manual_queue() {
        let mut q = QueueState {
            items: vec![
                song("a1", None),
                next_row("p"),
                added_row("x1"),
                song("a2", None),
                auto_row("r1"),
            ],
            ..QueueState::default()
        };
        append_page(&mut q, vec![song("a3", None), song("a4", None)], true, false);
        assert_eq!(ids_of(&q.items), ["a1", "p", "x1", "a2", "a3", "a4", "r1"]);
    }

    /// Shuffle never moves the manual queue: Play next and Add to queue rows stay right after the
    /// playing track in their order; the playlist is shuffled behind them, autoplay last.
    #[test]
    fn shuffle_keeps_the_manual_queue_in_front() {
        let mut items = vec![song("now", None), next_row("n1"), added_row("x0"), added_row("x1")];
        items.extend((0..6).map(|i| song(&format!("p{i}"), None)));
        items.extend((0..2).map(|i| auto_row(&format!("a{i}"))));
        shuffle_upcoming(&mut items, 0, false);
        let ids = ids_of(&items);
        assert_eq!(ids[..4], ["now", "n1", "x0", "x1"]);
        let mut playlist = ids[4..10].to_vec();
        playlist.sort_unstable();
        assert_eq!(playlist, ["p0", "p1", "p2", "p3", "p4", "p5"]);
        assert!(items[10..].iter().all(|i| i.autoplay));
    }

    /// Bug 1 (LIMUSIC-MAP §5): a new context started shuffled used to leave the carried manual
    /// adds out of the snapshot, so turning shuffle off deleted them. They must play first in the
    /// new context, and survive the shuffle toggle.
    #[test]
    fn new_context_keeps_the_manual_queue_shuffled_or_not() {
        let carried = vec![next_row("n1"), added_row("x1"), added_row("x2")];
        let album: Vec<_> = (0..5).map(|i| song(&format!("t{i}"), None)).collect();

        // In order: clicked track, then the manual queue, then the rest of the album.
        let mut q = QueueState { items: album.clone(), current: 2, ..QueueState::default() };
        arrange_new_context(&mut q, false, false, carried.clone());
        assert_eq!(ids_of(&q.items), ["t0", "t1", "t2", "n1", "x1", "x2", "t3", "t4"]);
        assert_eq!(q.current, 2);
        assert!(q.shuffle_orig.is_none());

        // Shuffled: clicked track first, the manual queue right behind it.
        let mut q = QueueState { items: album.clone(), current: 2, ..QueueState::default() };
        arrange_new_context(&mut q, true, false, carried.clone());
        assert_eq!(q.current, 0);
        assert_eq!(ids_of(&q.items)[..4], ["t2", "n1", "x1", "x2"]);
        // Shuffle off keeps them, still right after the playing track.
        unshuffle_queue(&mut q);
        assert!(q.shuffle_orig.is_none());
        assert_eq!(q.items[q.current].video_id, "t2");
        let ids = ids_of(&q.items);
        assert_eq!(ids[q.current + 1..q.current + 4], ["n1", "x1", "x2"]);
        assert_eq!(ids.iter().filter(|i| i.starts_with('t')).count(), 5, "no album row lost");
    }

    /// Adds made while shuffled survive shuffle off, in front of the restored playlist.
    #[test]
    fn manual_adds_made_while_shuffled_survive_unshuffle() {
        let album: Vec<_> = (0..4).map(|i| song(&format!("t{i}"), None)).collect();
        let mut q = QueueState { items: album.clone(), ..QueueState::default() };
        q.shuffle_orig = Some(q.items.clone());
        shuffle_upcoming(&mut q.items, 0, false);
        // What insert_queued does for an add while shuffled (it also appends to the snapshot).
        let at = enqueue_at(&q);
        q.items.insert(at, added_row("x1"));
        q.shuffle_orig.as_mut().unwrap().push(added_row("x1"));
        unshuffle_queue(&mut q);
        assert_eq!(ids_of(&q.items), ["t0", "x1", "t1", "t2", "t3"]);
    }

    /// Bug 2 (LIMUSIC-MAP §5): a track no client can resolve is marked unavailable and skipped,
    /// never deleted from the queue.
    #[test]
    fn failed_track_is_skipped_not_removed() {
        let mut q = QueueState {
            items: vec![song("a", None), song("dead", None), song("c", None)],
            ..QueueState::default()
        };
        assert_eq!(q.next_playable(RepeatMode::Off), Some(1));
        assert!(q.mark_unavailable(1, "dead", true));
        assert_eq!(q.items.len(), 3, "the row stays");
        assert!(q.items[1].unavailable);
        assert_eq!(q.next_playable(RepeatMode::Off), Some(2), "skipped");
        // The slot moved under the resolve: nothing is marked.
        assert!(!q.mark_unavailable(2, "dead", true));
        // A later success clears it.
        assert!(q.mark_unavailable(1, "dead", false));
        assert_eq!(q.next_playable(RepeatMode::Off), Some(1));
    }

    #[test]
    fn next_playable_steps_over_unavailable_rows_and_ends() {
        let dead = |id: &str| innertube::SongItem { unavailable: true, ..song(id, None) };
        let items = vec![song("a", None), dead("b"), dead("c")];
        assert_eq!(next_playable_index(&items, 0, RepeatMode::Off), None);
        // Repeat-all with one playable row repeats it, like a one-track queue.
        assert_eq!(next_playable_index(&items, 0, RepeatMode::All), Some(0));
        let items = vec![dead("x"), song("a", None), dead("b")];
        assert_eq!(next_playable_index(&items, 1, RepeatMode::All), Some(1));
        let all_dead = vec![dead("x"), dead("y")];
        assert_eq!(next_playable_index(&all_dead, 0, RepeatMode::All), None, "never spins");
        let items = vec![song("z", None), song("a", None), dead("b")];
        assert_eq!(next_playable_index(&items, 1, RepeatMode::All), Some(0));
        assert_eq!(next_playable_index(&items, 1, RepeatMode::One), None, "mpv loops the file");
        // Marking a row in a shuffled queue marks its snapshot copy too.
        let mut q = QueueState { items: items.clone(), ..QueueState::default() };
        q.shuffle_orig = Some(items);
        q.mark_unavailable(1, "a", true);
        assert!(q.shuffle_orig.unwrap()[1].unavailable);
    }

    // ── lane magic-d2 ── drops on the queue panel and Undo of Clear queue.

    /// A drop lands at the line inside "Next in queue": between two Play next rows it is Play
    /// next, behind them it is Add to queue, and a line over the context clamps to the block's end.
    #[test]
    fn a_drop_on_the_queue_lands_at_the_line_inside_next_in_queue() {
        let base = || QueueState {
            items: vec![
                song("now", None),
                next_row("n1"),
                next_row("n2"),
                added_row("x1"),
                song("p1", None),
                auto_row("r1"),
            ],
            ..QueueState::default()
        };
        let mut q = base();
        assert!(!splice_queued_at(&mut q, vec![song("d", None)], 2, Some("Album".into()), false));
        assert_eq!(ids_of(&q.items), ["now", "n1", "d", "n2", "x1", "p1", "r1"]);
        assert!(q.items[2].queued && !q.items[2].queued_end, "among Play next rows: Play next");
        assert_eq!(q.items[2].queued_from.as_deref(), Some("Album"));

        let mut q = base();
        splice_queued_at(&mut q, vec![song("d", None)], 4, None, false);
        assert_eq!(ids_of(&q.items), ["now", "n1", "n2", "x1", "d", "p1", "r1"]);
        assert!(q.items[4].queued_end && !q.items[4].queued, "behind Add to queue rows");

        // Aimed at the playlist or autoplay: the end of the manual block.
        let mut q = base();
        splice_queued_at(&mut q, vec![song("d", None), song("e", None)], 6, None, false);
        assert_eq!(ids_of(&q.items), ["now", "n1", "n2", "x1", "d", "e", "p1", "r1"]);
        // Aimed at the playing row or the history: right after the playing track.
        let mut q = QueueState { current: 1, ..base() };
        q.items.insert(0, song("old", None));
        q.current = 1;
        splice_queued_at(&mut q, vec![auto_row("d")], 0, None, false);
        assert_eq!(ids_of(&q.items)[..3], ["old", "now", "d"]);
        assert!(q.items[2].queued && !q.items[2].autoplay, "dropped by hand, not autoplay");
    }

    /// A song already waiting in the manual block moves to the line instead of doubling, and the
    /// line's index is rebased past the row that left from in front of it.
    #[test]
    fn a_drop_moves_a_copy_already_in_next_in_queue() {
        let mut q = QueueState {
            items: vec![song("now", None), next_row("n1"), added_row("x1"), added_row("x2"), song("p1", None)],
            ..QueueState::default()
        };
        // Line in front of x2 (index 3); n1 leaves from in front of it.
        assert!(splice_queued_at(&mut q, vec![song("n1", None)], 3, None, false));
        assert_eq!(ids_of(&q.items), ["now", "x1", "n1", "x2", "p1"]);
        assert!(q.items[2].queued_end, "behind an Add to queue row");

        // "Prevent duplicates": the playlist's copy and a played copy go too; `current` follows.
        let mut q = QueueState {
            items: vec![song("d", None), song("now", None), song("d", None), song("p1", None)],
            current: 1,
            ..QueueState::default()
        };
        splice_queued_at(&mut q, vec![song("d", None)], 2, None, true);
        assert_eq!(q.current, 0);
        assert_eq!(ids_of(&q.items), ["now", "d", "p1"]);
    }

    /// Shuffled: a drop joins the un-shuffle snapshot, so turning shuffle off keeps it.
    #[test]
    fn a_drop_while_shuffled_survives_unshuffle() {
        let mut q = QueueState {
            items: vec![song("t0", None), song("t1", None), song("t2", None)],
            ..QueueState::default()
        };
        q.shuffle_orig = Some(q.items.clone());
        splice_queued_at(&mut q, vec![song("d", None)], 1, None, false);
        unshuffle_queue(&mut q);
        assert!(ids_of(&q.items).contains(&"d".to_string()));
    }

    /// Undo of Clear queue puts the cleared rows back right after the playing track, in order and
    /// with their markers, without doubling one queued again in the meantime.
    #[test]
    fn undo_clear_queue_restores_the_queued_rows() {
        let cleared = vec![next_row("n1"), added_row("x1"), song("odd", None)];
        let mut q = QueueState {
            items: vec![song("old", None), song("now", None), added_row("x1"), song("p1", None), auto_row("r1")],
            current: 1,
            ..QueueState::default()
        };
        assert_eq!(restore_queued_rows(&mut q, cleared.clone()), 2);
        assert_eq!(ids_of(&q.items), ["old", "now", "n1", "odd", "x1", "p1", "r1"]);
        assert!(q.items[2].queued, "Play next stays Play next");
        assert!(q.items[3].queued_end, "an unmarked row comes back as Add to queue");
        assert_eq!(q.current, 1, "the playing track doesn't move");

        let mut empty = QueueState::default();
        assert_eq!(restore_queued_rows(&mut empty, cleared), 0, "nothing playing: nothing to restore into");
    }
}
