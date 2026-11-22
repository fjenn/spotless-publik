// Music videos on iPhone (stream ios-video): Spotify's "Switch to video" / "Switch to audio" in
// Now Playing, where the song has a music video.
//
// Engine: mpv draws the picture itself, into one CAMetalLayer (Vulkan through MoltenVK, VideoToolbox
// decode), from the same mpv instance that plays the sound (`crates/ffi/src/video.rs`). Picture and
// sound share one clock and the EQ chain stays in place, so Swift never syncs anything. Measured
// against the AVPlayer alternative in docs/streams/ios-video.md.
//
// The pieces:
// - `VideoEngine`: what the core offers (the layer, visibility, stats, the Song/Video switch).
// - `VideoStore.shared`: which side is picked, whether the picture may show (not offline, not on
//   Data Saver, app in the foreground, Now Playing open) and tells mpv; waits for the first frame.
// - `MPVLayerView.shared`: the one Metal layer for the whole process. mpv keeps its raw address, so
//   it is never freed. Its drawable has one fixed size, scaled aspect-fit into whichever box
//   `VideoHost` gives it. mpv's thread never touches it (`MPVMetalLayer`).
// - `OrientationLock`: Now Playing may turn to landscape (full-screen video) only while a video shows.
//   Full screen opens by turning the phone, or with the expand arrows in the video's controls
//   (which turn the screen itself).
import SwiftUI
import UIKit
import EncoreCore

// MARK: engine

/// mpv's picture, as the core exposes it. The real core and the mock both conform.
protocol VideoEngine: AnyObject {
    /// mpv draws into this `CAMetalLayer` (its address). Once per process.
    func setVideoLayer(_ layer: CAMetalLayer)
    /// Whether anyone can see the picture; off, mpv decodes no video at all.
    func setVideoVisible(_ on: Bool)
    func videoStats() -> VideoFrameStats
    /// The playing track's other version (its `counterpart`) from the same moment.
    func switchVersion() async throws
    /// stream: video-pods. Picture in Picture: mpv stops drawing through Metal and hands out frames.
    func setVideoPiP(_ on: Bool)
    /// The newest frame for PiP (BGRA), unless playback still sits at `unlessAt`. Any thread.
    func pipFrame(maxWidth: UInt32, unlessAt: Double?) -> PipFrameData?
}

struct VideoFrameStats: Equatable {
    var outputUp = false
    var width = 0
    var height = 0
    var avsync: Double?
    var dropped = 0
    var delayed = 0
    var fps: Double?
    var hwdec = ""
    var codec = ""
    /// The core dropped the playing file's picture because it stalled playback (a seek into a
    /// video stream it couldn't fetch); the sound plays on alone.
    var pictureDropped = false
    /// A decoded frame is on screen.
    var frameUp: Bool { outputUp && width > 0 }
}

extension RealCoreClient: VideoEngine {
    func setVideoLayer(_ layer: CAMetalLayer) {
        let address = UInt64(UInt(bitPattern: Unmanaged.passUnretained(layer).toOpaque()))
        do { try core.setVideoLayer(layer: address) } catch { coreLog.error("video layer refused: \(error.localizedDescription, privacy: .public)") }
    }
    func setVideoVisible(_ on: Bool) { core.setVideoVisible(on: on) }
    func videoStats() -> VideoFrameStats {
        let s = core.videoStats()
        return VideoFrameStats(outputUp: s.outputUp, width: Int(s.width), height: Int(s.height), avsync: s.avsync,
                               dropped: Int(s.framesDropped), delayed: Int(s.framesDelayed), fps: s.fps,
                               hwdec: s.hwdec, codec: s.codec, pictureDropped: s.dropped)
    }
    func switchVersion() async throws { try await core.switchVersion() }
    func setVideoPiP(_ on: Bool) { core.setVideoPip(on: on) }
    func pipFrame(maxWidth: UInt32, unlessAt: Double?) -> PipFrameData? {
        guard let f = core.pipFrame(maxWidth: maxWidth, unlessAt: unlessAt) else { return nil }
        return PipFrameData(pts: f.pts, width: Int(f.width), height: Int(f.height), bgra: f.bgra)
    }
}

/// The mock has no picture: it swaps versions like the core does, and never reports a frame, so
/// Now Playing keeps the video's poster frame (stable snapshots).
extension MockCoreClient: VideoEngine {
    func setVideoLayer(_ layer: CAMetalLayer) {}
    func setVideoVisible(_ on: Bool) {}
    func videoStats() -> VideoFrameStats { VideoFrameStats() }
    func setVideoPiP(_ on: Bool) {}
    func pipFrame(maxWidth: UInt32, unlessAt: Double?) -> PipFrameData? { nil }
    func switchVersion() async throws {
        let pos = withLock { () -> Double? in
            guard items.indices.contains(current), let other = items[current].counterpart else { return nil }
            var s = items[current]
            s.counterpart = s.videoId
            s.videoId = other
            s.isVideo = !(s.isVideo ?? false)
            if let a = s.thumbnail, let b = s.counterpartThumbnail { s.thumbnail = b; s.counterpartThumbnail = a }
            items[current] = s
            return position
        }
        guard let pos else { return }
        emit("queue-changed", withLock { queuePayload() })
        startCurrent(max(pos, 0.01))
    }
}

// MARK: state

@MainActor @Observable
final class VideoStore {
    static let shared = VideoStore()

    @ObservationIgnored private weak var app: AppModel?
    @ObservationIgnored private var engine: VideoEngine?
    /// "Switch to audio" on a video that has no song version hides the picture (the desktop's
    /// `video.want`). Session-sticky: whoever turns it off wants artwork on the next one too.
    var want = true
    /// The side just picked, held while the other version loads.
    var switching: Bool?
    /// A decoded frame of the current video is on screen (until then its poster frame shows).
    var frameUp = false
    var foreground = true
    /// Now Playing shows the full-screen video in landscape (it owns the layer then). Set through
    /// `enterFullscreen()` / `exitFullscreen()` and by turning the phone (`rotated(landscape:)`).
    private(set) var fullscreen = false
    /// Full screen came from a tap with the phone upright: the screen stays landscape (the phone
    /// isn't turned) until collapse, a swipe down, or the phone turning landscape and back.
    private(set) var fullscreenByTap = false
    /// Collapsed while the phone is still sideways: stay portrait until it is held upright again,
    /// else the screen would turn straight back into full screen.
    private var rotateBlocked = false
    /// mpv was last told this.
    private(set) var visible = false
    // ── stream: video-pods ──
    /// The playing episode's picture (the core's `podcasts::video`), and the episode it is for.
    var episodeVideo: EpisodeVideo?
    var episodeVideoFor: String?
    /// "Switch to audio" / "Switch to video" on an episode; nil follows the setting (`auto`).
    /// Session-sticky, like `want`.
    var episodeWant: Bool?
    /// Picture in Picture is up: the picture stays wanted with the app in the background.
    var pipActive = false
    /// The track (videoId) whose picture couldn't be played, so it fell back to audio on its own:
    /// the core dropped a picture that stalled playback, e.g. a scrub in a downloaded episode
    /// with no connection for the video. "Switch to video" clears it and tries again.
    var fallbackFor: String?
    @ObservationIgnored private var episodeKey: String?
    // ── end stream: video-pods ──
    @ObservationIgnored private var poll: Task<Void, Never>?
    @ObservationIgnored private var observers: [NSObjectProtocol] = []
    @ObservationIgnored private var lastAllowed: Bool?

    func attach(_ app: AppModel) {
        guard self.app !== app else { return }
        let first = self.app == nil
        self.app = app
        engine = app.core as? VideoEngine
        engine?.setVideoLayer(MPVLayerView.shared.metal)
        // Another AppModel (snapshot tests build their own): same store, its state starts over.
        want = true; switching = nil; frameUp = false; visible = false; lastAllowed = nil; fallbackFor = nil
        episodeVideo = nil; episodeVideoFor = nil; episodeWant = nil; episodeKey = nil
        fullscreen = false; fullscreenByTap = false; rotateBlocked = false
        VideoPiP.shared.attach(store: self, player: app.player, engine: engine) { [weak app] in
            app?.router.nowPlayingOpen = true
        }
        guard first else { return track() }
        let nc = NotificationCenter.default
        // Background or a locked phone: mpv stops decoding the picture and the sound carries on.
        observers.append(nc.addObserver(forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.foreground = false; self?.apply() }
        })
        observers.append(nc.addObserver(forName: UIApplication.willEnterForegroundNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.foreground = true }
        })
        UIDevice.current.beginGeneratingDeviceOrientationNotifications()
        observers.append(nc.addObserver(forName: UIDevice.orientationDidChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.deviceTurned(UIDevice.current.orientation) }
        })
        track()
        installTestHooks()
        VideoEngineProbe.startIfAsked(app: app)
        if let secs = UserDefaults.standard.object(forKey: "videoBench") as? Int ?? Int(UserDefaults.standard.string(forKey: "videoBench") ?? "") {
            VideoBench.start(seconds: secs, store: self)
        }
    }

    // What the view reads.

    private var current: SongItem? { app?.player.current }
    var isVideo: Bool { current?.isVideo == true }
    /// No video offline, in offline mode or on Data Saver (Spotify). Offline includes no network
    /// at all: a downloaded episode has its sound on the phone but never its picture.
    var allowed: Bool {
        guard let app else { return false }
        return !app.downloads.offline && !app.settings.bool(SettingKey.dataSaver) && StreamQuality.shared.online
    }
    /// stream: video-pods. The playing track is a podcast episode.
    var isEpisode: Bool { current?.isEpisode == true }
    /// The playing episode's picture, nil for a song or an episode without video.
    var episodeSrc: EpisodeVideo? { isEpisode && episodeVideoFor == current?.videoId ? episodeVideo : nil }
    /// There is a picture to show: a music video, or an episode with video.
    var hasPicture: Bool { isEpisode ? episodeSrc != nil : isVideo }
    /// The track has a music video to switch to, or is one; or it is an episode with video.
    var canSwitch: Bool {
        guard let s = current, allowed else { return false }
        if s.isEpisode == true { return episodeSrc != nil }
        return s.isVideo == true || s.counterpart != nil
    }
    /// Which side is picked: the user's choice, not what is on screen (the poster stands in). An
    /// episode starts on the side the setting picks.
    var side: Bool {
        if let f = fallbackFor, f == current?.videoId { return false }
        if isEpisode { return episodeSrc.map { episodeWant ?? $0.auto } ?? false }
        return switching ?? (isVideo && want)
    }
    /// Now Playing shows the video box instead of the cover.
    var showsVideo: Bool { allowed && side }
    /// Landscape turns Now Playing into the full-screen video.
    var landscapeAllowed: Bool {
        guard let app else { return false }
        return showsVideo && hasPicture && app.router.nowPlayingOpen && !app.router.lyricsOpen && !app.router.queueOpen && app.sheet == nil
    }
    /// stream: video-pods. With Now Playing closed, the mini player shows the live picture.
    var miniShowsVideo: Bool { showsVideo && hasPicture && app?.router.nowPlayingOpen == false }

    // Full screen (Spotify's video podcasts): the expand arrows, or turning the phone.

    /// The expand arrows on Now Playing's video: the screen turns to landscape, the video full screen.
    func enterFullscreen() {
        guard landscapeAllowed, !fullscreen else { return }
        Haptics.impact(.light)
        fullscreen = true
        fullscreenByTap = !UIDevice.current.orientation.isLandscape
        rotateBlocked = false
        apply()
        OrientationLock.rotate(to: .landscapeRight)
    }

    /// Collapse, a swipe down, or the video going away: back to Now Playing in portrait.
    func exitFullscreen() {
        guard fullscreen else { return }
        fullscreen = false
        fullscreenByTap = false
        rotateBlocked = UIDevice.current.orientation.isLandscape
        apply()
        OrientationLock.rotate(to: .portrait)
    }

    /// Now Playing's size turned: landscape opens full screen; upright closes one opened by turning.
    func rotated(landscape: Bool) {
        if landscape, !fullscreen, landscapeAllowed, !rotateBlocked {
            fullscreen = true
            fullscreenByTap = false
            apply()
        } else if !landscape, fullscreen, !fullscreenByTap {
            fullscreen = false
            apply()
        }
    }

    private func deviceTurned(_ o: UIDeviceOrientation) {
        if o == .portrait, rotateBlocked { rotateBlocked = false; apply() }
        // Tapped into full screen, then the phone turned sideways: turning it back now collapses.
        if o.isLandscape, fullscreen, fullscreenByTap { fullscreenByTap = false; apply() }
    }

    /// What the system may turn to: landscape only in a tapped full screen, any way up (but
    /// upside down) while a video shows in Now Playing, else portrait.
    private var orientationMask: UIInterfaceOrientationMask {
        if fullscreen { return fullscreenByTap ? .landscape : .allButUpsideDown }
        return landscapeAllowed && !rotateBlocked ? .allButUpsideDown : .portrait
    }

    /// Spotify's "Switch to video" / "Switch to audio".
    func pick(_ on: Bool) {
        if on, fallbackFor != nil { fallbackFor = nil }
        guard on != side, let s = current else { return }
        Haptics.impact(.light)
        // An episode only shows or hides its picture: the sound is the same either way.
        if s.isEpisode == true {
            episodeWant = on
            return
        }
        // No other version: show or hide the picture this track already is.
        if s.counterpart == nil || on == isVideo {
            want = on
            return
        }
        if on { want = true }
        switching = on
        Task {
            do { try await engine?.switchVersion() } catch { app?.showToast("Couldn't switch right now") }
            // The queue row flips when the core reports it; until then `switching` holds the side.
            for _ in 0..<40 where isVideo != on { try? await Task.sleep(nanoseconds: 100_000_000) }
            switching = nil
        }
    }

    // Telling mpv, whenever anything it depends on moves.

    private struct Wanted: Equatable {
        var visible: Bool; var allowed: Bool; var mask: UIInterfaceOrientationMask
        var pip: Bool; var paused: Bool; var episodeKey: String?
    }

    private func wanted() -> Wanted {
        let a = allowed
        let npOpen = app?.router.nowPlayingOpen == true
        let open = npOpen && !(app?.router.lyricsOpen ?? false) && !(app?.router.queueOpen ?? false)
        // Someone can see it: Now Playing's box, the mini player (Now Playing closed), or PiP.
        let seen = (foreground && (open || !npOpen)) || pipActive
        let visible = a && showsVideo && hasPicture && seen
        // Which episode's picture to ask the core for, and when to ask again: once it has loaded (an
        // RSS episode's YouTube twin is found while it resolves) and when a rule input moves.
        var key: String?
        if let s = current, s.isEpisode == true {
            let audioOnly = app?.settings.values[SettingKey.podcastAudioOnly] ?? ""
            key = "\(s.videoId)|\(app?.player.loading == true)|\(audioOnly)|\(a)"
        }
        return Wanted(visible: visible, allowed: a, mask: orientationMask,
                      pip: visible, paused: app?.player.paused ?? true, episodeKey: key)
    }

    /// Re-apply now (PiP closing in the background).
    func reapply() { apply() }

    /// Ask the core for the playing episode's picture (stream: video-pods).
    private func fetchEpisodeVideo(_ key: String?) {
        guard key != episodeKey else { return }
        episodeKey = key
        guard key != nil, let s = current, let core = app?.core else {
            if key == nil { episodeVideo = nil; episodeVideoFor = nil }
            return
        }
        let id = s.videoId
        let loading = app?.player.loading == true
        if loading && episodeVideoFor != id { episodeVideo = nil; episodeVideoFor = nil }
        if loading { return }
        Task {
            // A setting that just moved is written to the core asynchronously: let it land first.
            try? await Task.sleep(nanoseconds: 150_000_000)
            guard episodeKey == key else { return }
            let v = (try? await core.podcastEpisodeVideo(id: id)) ?? nil
            guard current?.videoId == id else { return }
            episodeVideo = v
            episodeVideoFor = id
        }
    }

    /// Re-applies on every change of what `wanted()` reads.
    private func track() {
        withObservationTracking { _ = wanted() } onChange: { [weak self] in
            Task { @MainActor in self?.track() }
        }
        apply()
    }

    /// The picture can't play, the sound can: show the cover and keep playing (Spotify), once.
    private func fallBackToAudio(offline: Bool) {
        guard let id = current?.videoId, fallbackFor != id else { return }
        fallbackFor = id
        app?.showToast(offline ? "Video unavailable offline. Playing audio." : "Video unavailable right now. Playing audio.")
    }

    private func apply() {
        // The video went away (Now Playing closed, a sheet, an episode without video): collapse.
        if fullscreen && !landscapeAllowed { return exitFullscreen() }
        let w = wanted()
        // The connection dropped with a picture on screen: the sound carries on from the phone
        // (a download) or the stream's buffer; say why the picture went.
        if lastAllowed == true, !w.allowed, !StreamQuality.shared.online, side, hasPicture,
           app?.downloads.offline == false {
            app?.showToast("Video unavailable offline. Playing audio.")
        }
        fetchEpisodeVideo(w.episodeKey)
        VideoPiP.shared.setEnabled(w.pip)
        VideoPiP.shared.playbackChanged()
        if w.allowed != lastAllowed {
            lastAllowed = w.allowed
            applySettings(w.allowed)
        }
        if w.visible != visible {
            visible = w.visible
            engine?.setVideoVisible(w.visible)
            if !w.visible { frameUp = false }
            startPolling(w.visible)
        }
        OrientationLock.set(w.mask)
    }

    /// The core resolves a video row's picture only with `music_videos` on, and Spotify plays the
    /// song version unless you switch: `videos_first` stays off (else the queue turns to videos).
    private func applySettings(_ allowed: Bool) {
        guard let core = app?.core else { return }
        Task {
            try? await core.setSetting(key: "videos_first", value: "false")
            try? await core.setSetting(key: "music_videos", value: allowed ? "true" : "false")
        }
    }

    /// While the picture is wanted, watch for its first frame (and again after each track change).
    private func startPolling(_ on: Bool) {
        poll?.cancel()
        guard on else { return }
        poll = Task { [weak self] in
            var lastId = self?.current?.videoId
            while !Task.isCancelled {
                guard let self else { return }
                let id = current?.videoId
                if id != lastId { lastId = id; frameUp = false }
                let stats = engine?.videoStats()
                if stats?.pictureDropped == true { fallBackToAudio(offline: !StreamQuality.shared.online) }
                let up = stats?.frameUp ?? false
                if up != frameUp { withAnimation(.easeOut(duration: 0.25)) { self.frameUp = up } }
                try? await Task.sleep(nanoseconds: 250_000_000)
            }
        }
    }

    func stats() -> VideoFrameStats? { engine?.videoStats() }

    /// Simulator checks without touching the screen: `-videoAuto 1` switches to the video as soon
    /// as the playing song has one; `notifyutil -p app.spotless.video.switch` flips the side and
    /// `app.spotless.video.state` prints it (VIDEO_STATE lines on stdout).
    private func installTestHooks() {
        // `-videoPick <videoId|albumId>`: play that song or video on its own (as a shared link
        // does), or an album from its first track with a music video. Then `-videoAuto`.
        let pick = UserDefaults.standard.string(forKey: "videoPick")
        let auto = UserDefaults.standard.bool(forKey: "videoAuto")
        // stream: video-pods. `-videoEpisode <id|first>`: play that episode (or the first one of the
        // first followed show) and open Now Playing.
        let episode = UserDefaults.standard.string(forKey: "videoEpisode")
        guard pick != nil || auto || episode != nil, let app else { return installSwitchNotifications() }
        Task {
            if let id = episode {
                var eps: [PodcastEpisode] = []
                if id == "first" {
                    if let show = (try? await app.core.podcastFollows())?.first,
                       let page = try? await app.core.podcastShow(id: show.id, sort: .newest) { eps = page.episodes }
                } else if let e = try? await app.core.podcastEpisode(id: id) { eps = [e] }
                guard let e = eps.first else { print("VIDEO_STATE episode failed \(id)"); return }
                app.player.play(eps.map(\.song), at: 0, context: PlayContext(kind: .song, id: nil, title: e.showTitle))
                for _ in 0..<40 where app.player.current?.videoId != e.id { try? await Task.sleep(nanoseconds: 250_000_000) }
                if UserDefaults.standard.string(forKey: "videoEpisodeOpen") != "0" { app.router.nowPlayingOpen = true }
                try? await Task.sleep(nanoseconds: 500_000_000)
            }
            if let id = pick {
                if id.hasPrefix("MPRE") {
                    guard let a = try? await app.core.getAlbum(id: id) else { print("VIDEO_STATE album failed \(id)"); return }
                    let at = a.items.firstIndex { $0.counterpart != nil } ?? 0
                    app.player.play(a.items, at: at, context: PlayContext(kind: .album, id: id, title: a.title ?? ""))
                    for _ in 0..<40 where app.player.current?.videoId != a.items[at].videoId { try? await Task.sleep(nanoseconds: 250_000_000) }
                } else {
                    guard let s = try? await app.core.song(videoId: id) else { print("VIDEO_STATE pick failed \(id)"); return }
                    app.player.playSingle(s)
                    for _ in 0..<40 where app.player.current?.videoId != id { try? await Task.sleep(nanoseconds: 250_000_000) }
                }
                app.router.nowPlayingOpen = true
                try? await Task.sleep(nanoseconds: 1_000_000_000)
            }
            guard auto else { return }
            for _ in 0..<600 {
                if canSwitch, !side { self.pick(true); return }
                try? await Task.sleep(nanoseconds: 250_000_000)
            }
        }
        installSwitchNotifications()
    }

    private func installSwitchNotifications() {
        let center = CFNotificationCenterGetDarwinNotifyCenter()
        for name in ["app.spotless.video.switch", "app.spotless.video.state", "app.spotless.video.pip", "app.spotless.video.fullscreen"] {
            CFNotificationCenterAddObserver(center, nil, { _, _, name, _, _ in
                let n = name?.rawValue as String? ?? ""
                Task { @MainActor in
                    let v = VideoStore.shared
                    if n.hasSuffix("switch") { v.pick(!v.side) }
                    if n.hasSuffix("pip") { VideoPiP.shared.toggleByHand() }
                    if n.hasSuffix("fullscreen") { if v.fullscreen { v.exitFullscreen() } else { v.enterFullscreen() } }
                    let s = v.stats() ?? VideoFrameStats()
                    let q = VideoStore.shared.app?.player.queue.items ?? []
                    print("VIDEO_STATE episode=\(v.isEpisode) src=\(v.episodeSrc?.videoId ?? v.episodeSrc?.url ?? "-") pip=\(v.pipActive) pipPossible=\(VideoPiP.shared.possible) id=\(v.current?.videoId ?? "-") twin=\(v.current?.counterpart ?? "-") paired=\(q.filter { $0.counterpart != nil }.count)/\(q.count) side=\(v.side) isVideo=\(v.isVideo) canSwitch=\(v.canSwitch) visible=\(v.visible) frameUp=\(v.frameUp) mpv=\(s.width)x\(s.height) hwdec=\(s.hwdec) codec=\(s.codec)")
                    fflush(stdout)
                }
            }, name as CFString, nil, .deliverImmediately)
        }
    }
}

// MARK: the layer

/// mpv's layer. mpv draws on its own thread ("vo"), and MoltenVK configures the layer from that
/// thread every time it builds a swapchain (device, pixel format, size, filters, colour space).
/// Any layer property set off the main thread opens an implicit CATransaction there. "vo" has no
/// run loop, so that transaction stays open until the thread exits (the picture closes: Switch to
/// audio, Now Playing closed, the app in the background, PiP). Then it commits, and the commit lays
/// out the whole window from "vo": UIKit's Auto Layout asserts (`_AssertAutoLayoutOnAllowedThreadsOnly`,
/// which crashed TestFlight builds 10 and 11).
///
/// So no write from another thread reaches the layer. A value the layer already has is dropped
/// (the main thread sets MoltenVK's values up front: `MPVLayerView.init`). A drawable setting that
/// differs goes to the main thread. A display setting (filters, gravity, opacity) is the app's,
/// and is ignored. `VideoThreadTests` checks this against real thread exits.
final class MPVMetalLayer: CAMetalLayer {
    /// Off-main writes that had to be passed to the main thread (MoltenVK wanted a value the main
    /// thread didn't set up front). Logged so a new MoltenVK shows up in the simulator run.
    nonisolated(unsafe) static var deferred: [String] = []

    private func write(_ key: String, same: Bool, display: Bool = false, _ apply: @escaping () -> Void) {
        if Thread.isMainThread { return apply() }
        guard !same, !display else { return }
        DispatchQueue.main.async {
            Self.deferred.append(key)
            print("VIDEO_LAYER \(key) set off the main thread: applied on main")
            fflush(stdout)
            apply()
        }
    }

    // What MoltenVK sets for a swapchain (MVKSwapchain::initCAMetalLayer).
    override var drawableSize: CGSize {
        get { super.drawableSize }
        set {
            // MoltenVK sometimes sets 1x1 to force a presentation, and the size can stick there
            // (flicker, then a 1-pixel picture). Ignored, as MPVKit's own demo does.
            guard Int(newValue.width) > 1, Int(newValue.height) > 1 else { return }
            write("drawableSize \(newValue) (was \(super.drawableSize), bounds \(bounds.size) x\(contentsScale))", same: newValue == super.drawableSize) { super.drawableSize = newValue }
        }
    }
    override var device: MTLDevice? {
        get { super.device }
        set { write("device", same: newValue === super.device) { super.device = newValue } }
    }
    override var pixelFormat: MTLPixelFormat {
        get { super.pixelFormat }
        set { write("pixelFormat \(newValue.rawValue)", same: newValue == super.pixelFormat) { super.pixelFormat = newValue } }
    }
    override var framebufferOnly: Bool {
        get { super.framebufferOnly }
        set { write("framebufferOnly", same: newValue == super.framebufferOnly) { super.framebufferOnly = newValue } }
    }
    override var maximumDrawableCount: Int {
        get { super.maximumDrawableCount }
        set { write("maximumDrawableCount \(newValue)", same: newValue == super.maximumDrawableCount) { super.maximumDrawableCount = newValue } }
    }
    override var colorspace: CGColorSpace? {
        get { super.colorspace }
        set { write("colorspace", same: newValue?.name == super.colorspace?.name) { super.colorspace = newValue } }
    }
    override var wantsExtendedDynamicRangeContent: Bool {
        get { super.wantsExtendedDynamicRangeContent }
        set { write("wantsEDR", same: newValue == super.wantsExtendedDynamicRangeContent) { super.wantsExtendedDynamicRangeContent = newValue } }
    }
    override var edrMetadata: CAEDRMetadata? {
        get { super.edrMetadata }
        set { write("edrMetadata", same: newValue == super.edrMetadata) { super.edrMetadata = newValue } }
    }
    override var presentsWithTransaction: Bool {
        get { super.presentsWithTransaction }
        set { write("presentsWithTransaction", same: newValue == super.presentsWithTransaction) { super.presentsWithTransaction = newValue } }
    }
    override var allowsNextDrawableTimeout: Bool {
        get { super.allowsNextDrawableTimeout }
        set { write("allowsNextDrawableTimeout", same: newValue == super.allowsNextDrawableTimeout) { super.allowsNextDrawableTimeout = newValue } }
    }
    // The app's: how the drawable sits in the box.
    override var magnificationFilter: CALayerContentsFilter {
        get { super.magnificationFilter }
        set { write("magnificationFilter", same: newValue == super.magnificationFilter, display: true) { super.magnificationFilter = newValue } }
    }
    override var minificationFilter: CALayerContentsFilter {
        get { super.minificationFilter }
        set { write("minificationFilter", same: newValue == super.minificationFilter, display: true) { super.minificationFilter = newValue } }
    }
    override var contentsGravity: CALayerContentsGravity {
        get { super.contentsGravity }
        set { write("contentsGravity", same: newValue == super.contentsGravity, display: true) { super.contentsGravity = newValue } }
    }
    override var isOpaque: Bool {
        get { super.isOpaque }
        set { write("opaque", same: newValue == super.isOpaque, display: true) { super.isOpaque = newValue } }
    }
    override var contentsScale: CGFloat {
        get { super.contentsScale }
        set { write("contentsScale", same: newValue == super.contentsScale, display: true) { super.contentsScale = newValue } }
    }
    // MoltenVK asks for a redisplay after some presents; a Metal layer's picture comes in drawables,
    // so from "vo" there is nothing to redraw.
    override func setNeedsDisplay() {
        if Thread.isMainThread { super.setNeedsDisplay() }
    }
    override func setNeedsDisplay(_ r: CGRect) {
        if Thread.isMainThread { super.setNeedsDisplay(r) }
    }
}

/// The one view mpv draws into, for the life of the process (mpv holds its layer's raw address).
///
/// Its size never changes: a 16:9 picture filling the screen's height in landscape. A box shows it
/// scaled by a transform, aspect-fit and centred (`VideoBox`). MoltenVK sizes the swapchain from the
/// layer's bounds times its scale, and a transform leaves both alone, so moving between boxes or
/// rotating never rebuilds anything. When the layer followed the box, every move and rotation
/// resized the drawable under mpv, and mpv and MoltenVK rebuilt the swapchain from "vo" a few
/// frames late. Meanwhile, old-size frames were stretched to the new box, and frames drawn at the
/// old size landed in the top-left corner of new-size drawables. A rebuild during a rotation could
/// also leave the output stuck until Switch to audio and back reopened it.
final class MPVLayerView: UIView {
    static let shared = MPVLayerView(frame: .zero)
    override class var layerClass: AnyClass { MPVMetalLayer.self }
    var metal: MPVMetalLayer { layer as! MPVMetalLayer }

    /// The fixed size, in points: 16:9 at the screen's landscape height (715 × 402 on an iPhone 17 Pro).
    static var fixedSize: CGSize {
        let s = UIScreen.main.bounds.size
        let h = min(s.width, s.height)
        return CGSize(width: (h * 16 / 9).rounded(), height: h)
    }
    /// The drawable, in pixels (2145 × 1206 on an iPhone 17 Pro).
    static var drawablePixels: CGSize {
        let f = fixedSize, s = UIScreen.main.nativeScale
        return CGSize(width: (f.width * s).rounded(), height: (f.height * s).rounded())
    }

    override init(frame: CGRect) {
        super.init(frame: CGRect(origin: .zero, size: Self.fixedSize))
        backgroundColor = .black
        isUserInteractionEnabled = false
        isOpaque = true
        contentMode = .scaleAspectFit
        contentScaleFactor = UIScreen.main.nativeScale
        // What MoltenVK sets from "vo" for libplacebo's swapchain (logged in the simulator): set here
        // first, its writes are no-ops.
        metal.device = MTLCreateSystemDefaultDevice()
        metal.framebufferOnly = false
        metal.pixelFormat = .bgr10a2Unorm
        metal.colorspace = CGColorSpace(name: CGColorSpace.sRGB)
        metal.magnificationFilter = .linear
        metal.minificationFilter = .linear
        metal.drawableSize = Self.drawablePixels
        accessibilityIdentifier = "np-video-layer"
    }
    required init?(coder: NSCoder) { fatalError() }

    /// Shown in `box`: the fixed size, centred, scaled to fit.
    func fit(in box: CGRect) {
        let f = Self.fixedSize
        let s = box.isEmpty ? 0.001 : min(box.width / f.width, box.height / f.height)
        if bounds.size != f { bounds = CGRect(origin: .zero, size: f) }
        center = CGPoint(x: box.midX, y: box.midY)
        transform = CGAffineTransform(scaleX: s, y: s)
    }
}

/// What a `VideoSurface` puts in the tree. The shared views (mpv's, and PiP's display layer under it)
/// live in the box `VideoHost` picks, and follow its size here.
final class VideoBox: UIView {
    var active = false {
        didSet { if active != oldValue { VideoHost.shared.changed(self) } }
    }
    /// Just moved in: take the box's size without animating (a box laid out inside the rotation's
    /// animation would grow the picture out of its top-left corner).
    fileprivate var snapNext = false

    override func didMoveToWindow() {
        super.didMoveToWindow()
        VideoHost.shared.changed(self)
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if snapNext {
            if !bounds.isEmpty { snapNext = false }
            UIView.performWithoutAnimation(place)
        } else {
            // Inside a rotation this rides the rotation's own animation: the picture scales smoothly.
            place()
        }
    }

    fileprivate func place() {
        for v in subviews {
            if let m = v as? MPVLayerView { m.fit(in: bounds) } else { v.frame = bounds }
        }
    }
}

/// Which box shows the picture: the most recently activated box that is in a window. SwiftUI
/// creates, updates and tears down the inline, full-screen and mini-player boxes in whatever order
/// it likes; here a late teardown of the old box can't take the picture from the new one, and a
/// box that joins the window after it was activated still gets it.
@MainActor final class VideoHost {
    static let shared = VideoHost()
    private struct Ref { weak var box: VideoBox? }
    private var active: [Ref] = []

    func changed(_ box: VideoBox) {
        active.removeAll { $0.box == nil || $0.box === box }
        if box.active { active.append(Ref(box: box)) }
        settle()
    }

    /// The box showing the picture now (tests).
    var current: VideoBox? { MPVLayerView.shared.superview as? VideoBox }

    private func settle() {
        guard let target = active.last(where: { $0.box?.window != nil })?.box else { return }
        // PiP's display layer first, under mpv's (VideoPiP.swift).
        let views: [UIView] = [SampleBufferView.shared, MPVLayerView.shared]
        guard views.contains(where: { $0.superview !== target }) else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        UIView.performWithoutAnimation {
            for v in views {
                v.autoresizingMask = []
                target.addSubview(v)
            }
            target.place()
        }
        CATransaction.commit()
        target.snapNext = true
        target.setNeedsLayout()
    }
}

/// Asks for the shared picture in this box while `active`. Only one box is active at a time (the Now
/// Playing slot, the full-screen view, or the mini player); `VideoHost` settles any overlap.
struct VideoSurface: UIViewRepresentable {
    var active = true
    func makeUIView(context: Context) -> VideoBox {
        let box = VideoBox()
        box.backgroundColor = .clear
        box.clipsToBounds = true
        box.isUserInteractionEnabled = false
        return box
    }
    func updateUIView(_ box: VideoBox, context: Context) { box.active = active }
    static func dismantleUIView(_ box: VideoBox, coordinator: ()) { box.active = false }
}

// MARK: orientation

/// Portrait everywhere, except Now Playing with a video showing, where turning the phone (or
/// tapping the video) gives the full-screen video (Spotify). The app delegate answers the system
/// with `mask` (`VideoStore.orientationMask`).
@MainActor enum OrientationLock {
    private(set) static var mask: UIInterfaceOrientationMask = .portrait

    private static var scenes: [UIWindowScene] {
        UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
    }

    static func set(_ m: UIInterfaceOrientationMask) {
        guard m != mask else { return }
        mask = m
        for scene in scenes {
            scene.keyWindow?.rootViewController?.setNeedsUpdateOfSupportedInterfaceOrientations()
            if m == .portrait, scene.interfaceOrientation.isLandscape {
                scene.requestGeometryUpdate(.iOS(interfaceOrientations: .portrait))
            }
        }
    }

    /// Turn the screen without the phone turning (full screen from a tap, and back). `mask` must
    /// allow it already.
    static func rotate(to target: UIInterfaceOrientationMask) {
        for scene in scenes {
            let there = target == .portrait ? scene.interfaceOrientation.isPortrait : scene.interfaceOrientation.isLandscape
            guard !there else { continue }
            scene.requestGeometryUpdate(.iOS(interfaceOrientations: target)) { error in
                coreLog.error("rotate refused: \(error.localizedDescription, privacy: .public)")
            }
        }
    }
}

final class OrientationDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication, supportedInterfaceOrientationsFor window: UIWindow?) -> UIInterfaceOrientationMask {
        MainActor.assumeIsolated { OrientationLock.mask }
    }
}

// MARK: measurements

/// `-videoBench <seconds>`: log the picture's sync and drops once a second (VIDEO_BENCH lines on
/// stdout), for the engine numbers in docs/streams/ios-video.md. CPU is read from outside (`ps`).
@MainActor enum VideoBench {
    static func start(seconds: Int, store: VideoStore) {
        Task {
            var t = 0
            var worst = 0.0
            var sum = 0.0
            var n = 0
            while t < seconds {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                t += 1
                guard let s = store.stats() else { continue }
                if let a = s.avsync, s.frameUp { worst = max(worst, abs(a)); sum += abs(a); n += 1 }
                print(String(format: "VIDEO_BENCH t=%d up=%d %dx%d avsync=%@ dropped=%d delayed=%d fps=%@ hwdec=%@ codec=%@",
                             t, s.frameUp ? 1 : 0, s.width, s.height, s.avsync.map { String(format: "%.4f", $0) } ?? "-",
                             s.dropped, s.delayed, s.fps.map { String(format: "%.2f", $0) } ?? "-", s.hwdec, s.codec))
                fflush(stdout)
            }
            print(String(format: "VIDEO_BENCH done samples=%d mean_abs_avsync=%.4f worst_abs_avsync=%.4f", n, n > 0 ? sum / Double(n) : 0, worst))
            fflush(stdout)
        }
    }
}
