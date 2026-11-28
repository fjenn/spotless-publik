// Observable app state over `CoreClient`: the iOS counterpart of the desktop's `player.svelte.ts`
// and `library.svelte.ts`. Views read these; every mutation goes through the core.
import SwiftUI
import Observation

/// Forwards core events (any thread) to the main-actor stores.
final class EventBridge: CoreEventListener, @unchecked Sendable {
    weak var app: AppModel?
    init(app: AppModel) { self.app = app }
    func onEvent(name: String, json: Data) {
        let ev = CoreEvent.decode(name: name, json: json)
        Task { @MainActor [weak app] in app?.apply(ev) }
    }
}

@MainActor @Observable
final class PlayerStore {
    let core: CoreClient
    var queue: QueueState = .empty
    var now: NowPlaying?
    var paused = true
    var position: Double = 0
    var duration: Double = 0
    /// Bumped on every track change; drives the mini player carousel direction.
    var changeSerial = 0
    var lastDirection: Int = 1
    /// The kind of the last context you started (Now Playing's "PLAYING FROM …" line).
    var contextKind: PlayContext.Kind?

    /// Told about every context you start (Home's shortcuts and Your Library's Recents move at once).
    @ObservationIgnored var onPlayContext: ((PlayContext, String?) -> Void)?
    /// Every user action goes through here: failures revert and toast (`FriendlyError.swift`).
    @ObservationIgnored let actions: CoreActions

    init(core: CoreClient) { self.core = core; actions = CoreActions(core: core) }

    var current: SongItem? { queue.items.indices.contains(queue.currentIndex) ? queue.items[queue.currentIndex] : nil }
    var previous: SongItem? { queue.items.indices.contains(queue.currentIndex - 1) ? queue.items[queue.currentIndex - 1] : nil }
    var next: SongItem? { queue.items.indices.contains(queue.currentIndex + 1) ? queue.items[queue.currentIndex + 1] : nil }
    var hasTrack: Bool { now != nil }
    /// stream: instant. The picked track is on screen and its audio is on the way (state.rs
    /// `show_loading`): the play buttons show their loading state.
    var loading: Bool { now?.loading == true }
    /// stream: instant. Tap → mini player and tap → audible, in ms, for the e2e probe's timing.
    @ObservationIgnored var tapAt: Date?
    @ObservationIgnored var tapId: String?
    @ObservationIgnored var tapUiMs: Int?
    @ObservationIgnored var tapAudioMs: Int?
    @ObservationIgnored var taps = 0
    func tapped(_ id: String?) { tapAt = Date(); tapId = id; tapUiMs = nil; tapAudioMs = nil; taps += 1 }
    func timeTap() {
        guard let t = tapAt, let id = tapId else { return }
        let ms = Int(Date().timeIntervalSince(t) * 1000)
        if tapUiMs == nil, now?.videoId == id, current?.videoId == id { tapUiMs = ms }
        if tapUiMs != nil, !loading, !paused, position > 0.05 { tapAudioMs = ms; tapAt = nil }
    }
    /// stream: pod-fixes. Now Playing offers lyrics for what's playing (the core says; episodes don't).
    var showsLyrics: Bool { now?.showsLyrics ?? true }
    var progress: Double { duration > 0 ? min(1, position / duration) : 0 }
    var shuffle: Bool { queue.shuffle ?? false }
    var repeatMode: RepeatMode { queue.repeat ?? .off }
    var upcoming: [(index: Int, song: SongItem)] {
        queue.items.enumerated().filter { $0.offset > queue.currentIndex }.map { ($0.offset, $0.element) }
    }

    /// The rows before the playing one: still in the queue (Previous walks them), shown folded in
    /// the queue sheet (issue #3).
    var earlier: [(index: Int, song: SongItem)] {
        queue.items.enumerated().filter { $0.offset < queue.currentIndex }.map { ($0.offset, $0.element) }
    }

    func refresh() async {
        if let q = try? await core.getQueue() { queue = q }
        if let p = try? await core.getPlayback() { now = p.now; paused = p.paused; position = p.position; duration = p.duration }
    }

    /// `startSecs` opens the row at `index` that far in (stream: pod-fixes).
    func play(_ items: [SongItem], at index: Int, context: PlayContext, shuffle: Bool = false, startSecs: Double? = nil) {
        onPlayContext?(context, items.first?.thumbnail)
        contextKind = context.kind
        tapped(items.indices.contains(index) ? items[index].videoId : nil)
        actions.run("play", "Couldn't play this") { [core] in
            try await core.playPlaylist(items: items, start: index, context: context, shuffle: shuffle, startSecs: startSecs)
        }
    }
    /// A single pick: Spotify plays only this song (the list it sat in is "what you might have
    /// meant", not a queue) and Autoplay carries on with similar songs.
    func playSingle(_ song: SongItem, from: String? = nil) {
        tapped(song.videoId)
        actions.run("play", "Couldn't play this song") { [core] in try await core.playSingle(song, from: from) }
    }
    func togglePause() {
        let was = paused
        paused.toggle()
        actions.run("pause", was ? "Couldn't play this" : "Couldn't pause", revert: { [weak self] in self?.paused = was }) { [core] in try await core.togglePause() }
    }
    func skipNext() { lastDirection = 1; actions.run("skip", "Couldn't skip this song") { [core] in try await core.nextTrack() } }
    func skipPrevious() { lastDirection = -1; actions.run("skip", "Couldn't go back") { [core] in try await core.prevTrack() } }
    func seek(_ t: Double) {
        let was = position
        position = t
        actions.run("seek", "Couldn't jump to that point", revert: { [weak self] in self?.position = was }) { [core] in try await core.seek(position: t) }
    }
    func toggleShuffle() { actions.run("shuffle", "Couldn't change shuffle") { [core] in try await core.toggleShuffle() } }
    func cycleRepeat() {
        let next: RepeatMode = switch repeatMode { case .off: .all; case .all: .one; case .one: .off }
        actions.run("repeat", "Couldn't change repeat") { [core] in try await core.setRepeat(next) }
    }
    func playIndex(_ i: Int) {
        lastDirection = i >= queue.currentIndex ? 1 : -1
        tapped(queue.items.indices.contains(i) ? queue.items[i].videoId : nil)
        actions.run("play", "Couldn't play this") { [core] in try await core.playIndex(i) }
    }
    /// Queue edits report back (true = done), so "Added to queue" only shows once it is.
    @discardableResult
    func addToQueue(_ s: [SongItem], from: String? = nil) -> Task<Bool, Never> {
        actions.run("queue", "Couldn't add to queue") { [core] in try await core.addToQueue(s, from: from) }
    }
    @discardableResult
    func playNext(_ s: [SongItem], from: String? = nil) -> Task<Bool, Never> {
        actions.run("queue", "Couldn't add to queue") { [core] in try await core.playNext(s, from: from) }
    }
    /// Spotify's "Clear queue": drops the songs you queued, keeps the context.
    func clearQueued() { actions.run("queue", "Couldn't clear the queue") { [core] in try await core.clearQueued() } }
    func remove(at i: Int) { actions.run("queue", "Couldn't remove that from the queue") { [core] in try await core.removeFromQueue(index: i) } }
    /// The list moved under your finger already: a refused move puts it back as the core has it.
    func move(from: Int, to: Int) {
        actions.run("queue", "Couldn't move that song", revert: { [weak self] in Task { await self?.refresh() } }) { [core] in
            try await core.moveInQueue(from: from, to: to)
        }
    }
    func radio(_ s: SongItem) { actions.run("radio", "Couldn't start radio") { [core] in try await core.startRadio(song: s) } }
}

@MainActor @Observable
final class LibraryStore {
    let core: CoreClient
    var snapshot: LibrarySnapshot = .empty
    var likedIds: Set<String> = []
    var recents: [RecentContext] = []
    /// Spotify's Recents also moves on a like (Liked Songs) and on starting a context, before the
    /// core has recorded the play: id → seconds. Kept across launches (likes have no play record).
    var touches: [String: Double] = [:]
    @ObservationIgnored let prefs: UserDefaults?
    static let touchesKey = "library_touches"

    @ObservationIgnored let actions: CoreActions

    init(core: CoreClient, prefs: UserDefaults? = nil) {
        self.core = core
        self.prefs = prefs
        actions = CoreActions(core: core)
        touches = (prefs?.dictionary(forKey: Self.touchesKey) as? [String: Double]) ?? [:]
    }

    /// Marks a library item as just used (Recents sort). Liked Songs' ids are folded to one.
    func touch(_ id: String, at t: Double = Date().timeIntervalSince1970) {
        let key = id == "liked" ? likedPlaylistId : id
        touches[key] = t
        // Only the newest 200 matter for a Recents sort.
        if touches.count > 200 {
            touches = Dictionary(uniqueKeysWithValues: touches.sorted { $0.value > $1.value }.prefix(200).map { ($0.key, $0.value) })
        }
        prefs?.set(touches, forKey: Self.touchesKey)
    }

    /// The Recents key for an item: the latest of the core's play and touch records, `base` (an own
    /// playlist's last edit) and a local touch. Seconds (milliseconds from the core are folded).
    func recentsKey(_ id: String, base: Double = 0) -> Double {
        func secs(_ v: Double) -> Double { v > 1e12 ? v / 1000 : v }
        return max(secs(snapshot.lastPlayed[id] ?? 0), secs(snapshot.lastTouched?[id] ?? 0), secs(base), touches[id] ?? 0)
    }

    func refresh() async {
        if let s = try? await core.librarySnapshot() { snapshot = s; likedIds = Set(s.likedIds) }
        if let r = try? await core.recentContexts(limit: 50) { recents = r }
    }

    func isLiked(_ id: String?) -> Bool { id.map(likedIds.contains) ?? false }
    func isHidden(_ id: String) -> Bool { snapshot.hiddenIds?.contains(id) ?? false }
    func isSaved(_ id: String) -> Bool { snapshot.saved.contains { $0.id == id } }

    /// Returns the new state. Optimistic, like the desktop: the heart flips at once and flips back
    /// (with a toast) if the core refuses.
    @discardableResult
    func toggleLike(_ song: SongItem) -> Bool {
        let on = !likedIds.contains(song.videoId)
        let revert = flipLike(song, on)
        actions.run("like", "Couldn't update Liked Songs", revert: revert) { [weak self, core] in
            try await core.setLiked(song, liked: on)
            await self?.refresh()
        }
        return on
    }

    /// The same without a toast, awaited: callers that batch several changes report once (Add to
    /// playlist). Returns the error, nil when it worked; the heart is back where it was on failure.
    func setLikedQuietly(_ song: SongItem, _ on: Bool) async -> Error? {
        guard likedIds.contains(song.videoId) != on else { return nil }
        let revert = flipLike(song, on)
        let e = await actions.attempt("like") { try await core.setLiked(song, liked: on) }
        if e != nil { revert() }
        return e
    }

    private func flipLike(_ song: SongItem, _ on: Bool) -> @MainActor () -> Void {
        let id = song.videoId
        let touched = touches[likedPlaylistId]
        if on { likedIds.insert(id); touch(likedPlaylistId) } else { likedIds.remove(id) }
        return { [weak self] in
            guard let self else { return }
            if on { self.likedIds.remove(id) } else { self.likedIds.insert(id) }
            if on { self.touches[likedPlaylistId] = touched; self.prefs?.set(self.touches, forKey: Self.touchesKey) }
        }
    }

    func setPinned(_ item: BrowseItem, _ on: Bool) {
        let before = snapshot.pinnedIds
        if on { snapshot.pinnedIds.insert(item.id, at: 0) } else { snapshot.pinnedIds.removeAll { $0 == item.id } }
        let kind = item.id == likedPlaylistId ? "liked" : item.kind.rawValue
        actions.run("pin", on ? "Couldn't pin this" : "Couldn't unpin this", revert: { [weak self] in self?.snapshot.pinnedIds = before }) { [weak self, core] in
            try await core.setPinned(id: item.id, kind: kind, pinned: on)
            await self?.refresh()
        }
    }

    /// Returns the new state. Your Library shows it once the core has it; a refusal toasts.
    @discardableResult
    func toggleSaved(_ item: BrowseItem) -> Bool {
        let on = !isSaved(item.id)
        actions.run("save", "Couldn't update Your Library") { [weak self, core] in
            try await core.setSaved(item, saved: on)
            await self?.refresh()
        }
        return on
    }
}

/// One toast at a time, above the mini player (Spotify: "Added to Liked Songs" / "Added to queue").
struct ToastMessage: Identifiable, Equatable {
    let id = UUID()
    var text: String
    var action: String?
    /// What tapping the action does ("Change" opens Add to playlist).
    var run: (@MainActor () -> Void)? = nil
    static func == (a: ToastMessage, b: ToastMessage) -> Bool { a.id == b.id }
}

enum LibrarySort: String, CaseIterable { case recents = "Recents", added = "Recently added", alphabetical = "Alphabetical", creator = "Creator", custom = "Custom order" }

/// What a bottom-sheet action menu is about.
enum SheetSubject: Identifiable, Equatable {
    case track(SongItem, context: PlayContext?)
    case collection(BrowseItem)
    case create
    case sort
    case episode(PodcastEpisode)
    case show(PodcastShow)
    case sleepTimer
    case speed
    /// The playing episode's chapters (Spotify's Chapters sheet).
    case chapters
    /// Music's Speed & pitch (tempo, pitch, reverb).
    case effects
    case aboutRecommendations
    case newFolder
    case newPlaylist
    /// Spotify's "Add to playlist" picker (also "Saved in" from a liked song's check button).
    case addToPlaylist([SongItem])
    /// Own playlists: Edit playlist, and Add to this playlist (id, name).
    case editPlaylist(String)
    case addSongs(String, String)
    var id: String {
        switch self {
        case .track(let s, _): "t:\(s.videoId)"
        case .collection(let c): "c:\(c.id)"
        case .create: "create"
        case .sort: "sort"
        case .episode(let e): "e:\(e.id)"
        case .show(let s): "s:\(s.id)"
        case .sleepTimer: "sleep"
        case .speed: "speed"
        case .chapters: "chapters"
        case .effects: "effects"
        case .aboutRecommendations: "aboutRecs"
        case .newFolder: "newFolder"
        case .newPlaylist: "newPlaylist"
        case .addToPlaylist(let s): "add:\(s.map(\.videoId).joined(separator: ","))"
        case .editPlaylist(let id): "edit:\(id)"
        case .addSongs(let id, _): "addSongs:\(id)"
        }
    }
}

/// A confirm dialog (Spotify uses system alerts for destructive actions).
struct ConfirmRequest: Identifiable {
    let id = UUID()
    var title: String
    var message: String?
    var action: String
    var run: () -> Void
}

@MainActor @Observable
final class AppModel {
    let core: CoreClient
    let player: PlayerStore
    let library: LibraryStore
    let podcasts: PodcastStore
    /// stream: audiobooks — saved books (Your Library, the book page's save button).
    let books: AudiobookStore
    let downloads: DownloadStore
    let settings: SettingsStore
    /// The optional YouTube account (stream ios-p2, Screens/YouTubeSignIn.swift).
    let account: AccountStore
    /// Device sync: Settings › Devices, the linking sheet, the welcome and the Home hint.
    let devices: DeviceSyncStore
    let router = Router()
    var toast: ToastMessage?
    /// Music's tempo, pitch and reverb for this session (Speed & pitch, `setEffects`).
    var effects = PlaybackEffects.original
    var sheet: SheetSubject?
    var confirm: ConfirmRequest?
    /// Spotify import onboarding (the optional first-run step; also from Settings).
    var importOpen = false
    /// Your Library's sort, one per chip (Spotify remembers each filter's own sort), kept across
    /// launches with the chip and the grid/list choice (`LibraryViewPrefs`).
    var librarySorts: [String: LibrarySort] = [:] { didSet { viewPrefs.save(sorts: librarySorts) } }
    var librarySort: LibrarySort {
        get {
            let s = librarySorts[libraryFilter ?? LibraryViewPrefs.allKey] ?? .recents
            // Custom order only exists for playlists.
            return s == .custom && libraryFilter != "Playlists" ? .recents : s
        }
        set { librarySorts[libraryFilter ?? LibraryViewPrefs.allKey] = newValue }
    }
    /// Your Library's chip (Playlists | Podcasts | Albums | Artists | Downloaded), its second-level
    /// chip, and the inline search (nil = closed). Deep link: encore://library?filter=&sub=&q=
    var libraryFilter: String? { didSet { saveLibraryFilter() } }
    var librarySubFilter: String? { didSet { saveLibraryFilter() } }
    var librarySearch: String?
    var libraryGrid = false { didSet { viewPrefs.save(grid: libraryGrid) } }
    /// Going offline switched Your Library to Downloaded: the chip to go back to once online
    /// (`.some(nil)` = no chip). Never saved: a relaunch opens on your own choice.
    @ObservationIgnored private var filterBeforeOffline: (filter: String?, sub: String?)?
    @ObservationIgnored let viewPrefs: LibraryViewPrefs
    /// Home's shelves (kept here so a shelf's "Show all" page and offline mode can read them).
    let home: HomeStore
    /// Bumped when Home should reload (`home-changed`, Made for you rebuilt): HomeView watches it.
    var homeSerial = 0
    @ObservationIgnored private var bridge: EventBridge?
    @ObservationIgnored private var toastTask: Task<Void, Never>?

    /// `prefs` keeps Your Library's view state and Recents touches; nil keeps them in memory only.
    /// By default the real core persists to the standard defaults and the mock (UI tests,
    /// snapshots) doesn't, so one test's grid or sort never leaks into the next.
    init(core: CoreClient, prefs: UserDefaults?? = nil) {
        self.core = core
        let store: UserDefaults? = prefs ?? AppModel.defaultPrefs(for: core)
        viewPrefs = LibraryViewPrefs(defaults: store)
        player = PlayerStore(core: core)
        library = LibraryStore(core: core, prefs: store)
        podcasts = PodcastStore(core: core)
        books = AudiobookStore(core: core)
        downloads = DownloadStore(core: core)
        settings = SettingsStore(core: core)
        account = AccountStore(core: core, settings: settings)
        devices = DeviceSyncStore(core: core, prefs: store)
        home = HomeStore(core: core)
        let saved = viewPrefs.load()
        librarySorts = saved.sorts
        libraryFilter = saved.filter
        librarySubFilter = saved.sub
        libraryGrid = saved.grid
        let b = EventBridge(app: self)
        bridge = b
        core.setListener(b)
        player.onPlayContext = { [weak self] ctx, thumb in self?.contextStarted(ctx, thumbnail: thumb) }
        for a in [player.actions, library.actions, podcasts.actions, books.actions, downloads.actions, settings.actions] {
            a.report = { [weak self] sentence in self?.fail(sentence) }
        }
        downloads.onOfflineChange = { [weak self] off in self?.offlineChanged(off) }
    }

    static func defaultPrefs(for core: CoreClient) -> UserDefaults? {
        if ProcessInfo.processInfo.arguments.contains("-persistViews") { return .standard }
        return core is MockCoreClient ? nil : .standard
    }

    /// Your own name for "Playlist • <you>" and the Creator sort: the signed-in YouTube account,
    /// else a name set in Settings (`profile_name`), else "You" (the desktop's `ownerName`).
    var localUser: String {
        for k in [SettingKey.youtubeAccount, "profile_name"] {
            if let n = settings.values[k]?.trimmingCharacters(in: .whitespaces), !n.isEmpty { return n }
        }
        return "You"
    }

    private func saveLibraryFilter() {
        // While offline forced Downloaded, keep the chip you chose on disk.
        if let before = filterBeforeOffline { viewPrefs.save(filter: before.filter, sub: before.sub) } else {
            viewPrefs.save(filter: libraryFilter, sub: librarySubFilter)
        }
    }

    /// A context started playing: it becomes Home's first shortcut and the newest Recents row at
    /// once (Spotify), and `home-changed` from the core revalidates Home afterwards.
    func contextStarted(_ ctx: PlayContext, thumbnail: String?) {
        guard let item = HomeStore.shortcut(for: ctx, thumbnail: thumbnail) else { return }
        home.touch(item)
        library.touch(item.id == "liked" ? likedPlaylistId : item.id)
    }

    /// Network flipped (or offline mode toggled): Library leans on Downloaded while offline and
    /// goes back to your chip once online; Home rebuilds from downloads, or reloads.
    func offlineChanged(_ offline: Bool) {
        if offline {
            if filterBeforeOffline == nil && libraryFilter != "Downloaded" {
                filterBeforeOffline = (libraryFilter, librarySubFilter)
                libraryFilter = "Downloaded"
                librarySubFilter = nil
            }
        } else if let before = filterBeforeOffline {
            filterBeforeOffline = nil
            // Only if you stayed on Downloaded: a chip you picked while offline wins.
            if libraryFilter == "Downloaded" { libraryFilter = before.filter; librarySubFilter = before.sub } else { saveLibraryFilter() }
        }
        homeSerial += 1
    }

    func start() async {
        await library.refresh()
        await player.refresh()
        await settings.refresh()
        // Spotify pins Liked Songs to the top of Your Library on a new account: once, on first run.
        if settings.values["ios_pins_seeded"] != "true" {
            if library.snapshot.pinnedIds.isEmpty {
                library.setPinned(BrowseItem(kind: .playlist, id: likedPlaylistId, title: "Liked Songs"), true)
            }
            try? await core.setSetting(key: "ios_pins_seeded", value: "true")
        }
        await podcasts.refresh()
        await books.refresh()
        await downloads.refresh()
        await account.start()
        await devices.refresh()
        StreamQuality.shared.attach(self)
    }

    /// First launch: offer the Spotify import. One rule with the Mac, in the core: an empty library
    /// and the offer never answered (`import_offer`).
    func shouldOfferImport() async -> Bool { (try? await core.importOffer()) ?? false }

    /// The first-run offer was answered (imported, or closed): never again, on either app.
    func importOfferAnswered() {
        settings.values[SettingKey.importDone] = "true"
        Task { try? await core.importOfferDone() }
    }

    func apply(_ ev: CoreEvent) {
        PlaybackBridge.shared.handle(ev)
        switch ev {
        case .queueChanged(let q): player.queue = q
        case .queueIndex(let i):
            // Same rows, new pointer (every advance and skip): keep the list, refresh the flags.
            player.queue.currentIndex = i.currentIndex
            if let s = i.shuffle { player.queue.shuffle = s }
            if let r = i.repeat { player.queue.repeat = r }
            if let n = i.sourceName { player.queue.sourceName = n }
            if let c = i.current, player.queue.items.indices.contains(i.currentIndex),
               player.queue.items[i.currentIndex].videoId == c.videoId { player.queue.items[i.currentIndex] = c }
        case .queueAppended(let a):
            player.queue.items += a.items
            player.queue.currentIndex = a.currentIndex
            if player.queue.items.count != a.len { Task { await player.refresh() } }
        case .playbackError(let message):
            if player.loading { player.now?.loading = false } // stream: instant: it gave up
            fail(friendlyError(message, "Couldn't play this song"))
        case .nowPlaying(let n):
            if n.videoId != player.now?.videoId {
                player.changeSerial += 1
                if let s = player.queue.items.first(where: { $0.videoId == n.videoId }) { home.touchSong(s) }
            }
            player.now = n // the core puts this item's own speed on (speeds.rs)
        case .position(let p): player.position = p
        case .duration(let d): player.duration = d
        case .playbackState(let paused): player.paused = paused
        case .rating(let id, let r):
            if r == .like {
                if !library.likedIds.contains(id) { library.touch(likedPlaylistId) }
                library.likedIds.insert(id)
            } else { library.likedIds.remove(id) }
        case .libraryChanged(let scope):
            Task {
                if scope == "podcasts" || scope == "episodes" { await podcasts.refresh() } else { await library.refresh() }
            }
        case .podcastProgress(let id, let p): podcasts.progress[id] = p
        case .downloadsChanged:
            Task { await downloads.refresh() }
            // Offline, Home is your downloads (the core's `offline_home`).
            if downloads.offline { homeSerial += 1 }
        case .downloadsProgress(let list):
            downloads.active = Dictionary(list.map { ($0.videoId, $0) }, uniquingKeysWith: { a, _ in a })
        case .downloadsNetwork(let n): downloads.setNetwork(n)
        case .sleepTimer(let s): settings.sleep = s
        case .smartShuffle(let s): settings.smartShuffle = s
        case .whatsNewChanged(let n): settings.whatsNewCount = n
        case .madeForYouChanged:
            homeSerial += 1
            // A failed rebuild read keeps the mixes you have rather than blanking the page.
            Task { if let c = try? await core.madeForYou() { settings.madeForYou = c } }
        case .homeChanged: homeSerial += 1
        case .importProgress: break // the import screen polls `importStatus` while open
        // Spotify keeps one speed for podcasts; music has its own session effects (`effects`).
        // Only an episode's rate is the remembered one.
        case .playbackParams(let e): if currentIsEpisode { settings.speed = e.speed }
        case .syncChanged(let s): devices.apply(s)
        case .unknown(let name): account.handle(event: name)
        }
        player.timeTap() // stream: instant
    }

    private var currentIsEpisode: Bool {
        guard let id = player.now?.videoId else { return false }
        return player.queue.items.first { $0.videoId == id }?.isEpisode == true
    }

    /// Speed & pitch (music): applies now unless an episode is playing, and stays for every song
    /// until changed back or the app restarts (the desktop's session effects).
    func setEffects(_ e: PlaybackEffects) {
        let before = effects
        effects = e
        // Always to the core, as the music's: it applies them now only if a song is playing.
        player.actions.run("effects", "Couldn't change the sound", revert: { [weak self] in self?.effects = before }) { [core] in
            try await core.setPlaybackParams(e)
        }
    }

    func showToast(_ text: String, action: String? = nil, run: (@MainActor () -> Void)? = nil) {
        withAnimation(Motion.spring) { toast = ToastMessage(text: text, action: action, run: run) }
        toastTask?.cancel()
        toastTask = Task {
            try? await Task.sleep(nanoseconds: run == nil ? 3_000_000_000 : 5_000_000_000)
            guard !Task.isCancelled else { return }
            withAnimation(Motion.quick) { toast = nil }
        }
    }

    /// A user action failed: the error buzz and the friendly sentence (`friendlyError`).
    func fail(_ sentence: String) {
        Haptics.error()
        showToast(sentence)
    }

    func toggleLike(_ song: SongItem) {
        let on = library.toggleLike(song)
        Haptics.impact(.light)
        if on {
            showToast("Added to Liked Songs", action: "Change") { [weak self] in self?.sheet = .addToPlaylist([song]) }
        } else {
            showToast("Removed from Liked Songs")
        }
    }

    /// The ⊕ / accent check button (Now Playing, mini player): ⊕ likes; the check opens the
    /// "Saved in" picker, where Liked Songs can be unticked or playlists added (Spotify iOS).
    func likeButton(_ song: SongItem) {
        if library.isLiked(song.videoId) {
            Haptics.impact(.light)
            sheet = .addToPlaylist([song])
        } else {
            toggleLike(song)
        }
    }

    func playNext(_ songs: [SongItem], from: String? = nil) {
        let t = player.playNext(songs, from: from)
        Haptics.impact(.medium)
        Task { if await t.value { showToast(songs.count == 1 ? "Playing next" : "Playing \(songs.count) songs next") } }
    }

    func queue(_ songs: [SongItem], from: String? = nil) {
        let t = player.addToQueue(songs, from: from)
        Task { if await t.value { showToast(songs.count == 1 ? "Added to queue" : "Added \(songs.count) songs to queue") } }
    }
}

enum Haptics {
    @MainActor static func impact(_ style: UIImpactFeedbackGenerator.FeedbackStyle) {
        UIImpactFeedbackGenerator(style: style).impactOccurred()
    }
    @MainActor static func selection() { UISelectionFeedbackGenerator().selectionChanged() }
    /// Spotify pairs a failure toast ("Can't play this right now") with the error buzz.
    @MainActor static func error() { UINotificationFeedbackGenerator().notificationOccurred(.error) }
}

/// Your Library's view state on disk: the chip, its second-level chip, grid or list, and one sort
/// per chip. A nil `defaults` keeps nothing (tests on the mock core).
struct LibraryViewPrefs {
    static let allKey = "All"
    let defaults: UserDefaults?
    private enum K { static let filter = "library_filter", sub = "library_sub", grid = "library_grid", sorts = "library_sorts" }

    func load() -> (filter: String?, sub: String?, grid: Bool, sorts: [String: LibrarySort]) {
        guard let d = defaults else { return (nil, nil, false, [:]) }
        let raw = d.dictionary(forKey: K.sorts) as? [String: String] ?? [:]
        return (d.string(forKey: K.filter), d.string(forKey: K.sub), d.bool(forKey: K.grid), raw.compactMapValues(LibrarySort.init(rawValue:)))
    }
    func save(filter: String?, sub: String?) {
        guard let d = defaults else { return }
        if let filter { d.set(filter, forKey: K.filter) } else { d.removeObject(forKey: K.filter) }
        if let sub { d.set(sub, forKey: K.sub) } else { d.removeObject(forKey: K.sub) }
    }
    func save(grid: Bool) { defaults?.set(grid, forKey: K.grid) }
    func save(sorts: [String: LibrarySort]) { defaults?.set(sorts.mapValues(\.rawValue), forKey: K.sorts) }
}
