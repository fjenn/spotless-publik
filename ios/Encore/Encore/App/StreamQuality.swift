// Stream ios-p2: Settings › Media quality and Data Saver, wired to the core. The core has one
// streaming setting, `quality` (the desktop's AUTO | LOW | NORMAL | HIGH | VERY_HIGH; it has no idea
// what network it's on). The phone does: Spotify's separate Wi-Fi and Cellular choices (and Data Saver) are kept
// as iOS settings, and this resolves them against the live network path into the core's `quality`
// whenever either one changes. A change drops the cached stream URLs and the primed next track
// (`quality_changed`), so the next song already plays at the new quality.
import Foundation
import Network
import Observation

@MainActor @Observable
final class StreamQuality {
    static let shared = StreamQuality()

    /// The desktop's five choices (Settings › Audio quality), for Wi-Fi and cellular alike. The
    /// core's `quality` gets the desktop's value for each (`LOW` | `NORMAL` | `HIGH` |
    /// `VERY_HIGH`; the core's `streaming_quality` plays Low and Normal at its low tier).
    static let choices: [(id: String, title: String, core: String)] = [
        ("auto", "Automatic", "AUTO"),
        ("low", "Low", "LOW"),
        ("normal", "Normal", "NORMAL"),
        ("high", "High", "HIGH"),
        ("very_high", "Very high", "VERY_HIGH"),
    ]

    @ObservationIgnored private weak var app: AppModel?
    @ObservationIgnored private var monitor: NWPathMonitor?
    @ObservationIgnored private(set) var onCellular = false
    /// The phone has a network path at all. Off, video is off (`VideoStore.allowed`) and a
    /// downloaded episode plays its sound alone.
    private(set) var online = true

    /// The stored choice as one of `choices`; anything else is Automatic.
    static func normalized(_ v: String?) -> String {
        choices.contains { $0.id == v } ? v! : "auto"
    }

    /// The core's `quality` for a network and the three settings. Automatic is the desktop's
    /// `AUTO`, except on cellular, where it saves data (`LOW`).
    static func coreQuality(cellular: Bool, wifi: String, cell: String, dataSaver: Bool) -> String {
        if dataSaver { return "LOW" }
        let pick = normalized(cellular ? cell : wifi)
        if pick == "auto" && cellular { return "LOW" }
        return choices.first { $0.id == pick }!.core
    }

    func attach(_ app: AppModel) {
        self.app = app
        apply()
        guard monitor == nil else { return }
        let m = NWPathMonitor()
        m.pathUpdateHandler = { path in
            let cell = path.isExpensive || path.usesInterfaceType(.cellular)
            let up = path.status == .satisfied
            Task { @MainActor in
                if StreamQuality.shared.online != up { StreamQuality.shared.online = up }
                guard StreamQuality.shared.onCellular != cell else { return }
                StreamQuality.shared.onCellular = cell
                StreamQuality.shared.apply()
            }
        }
        m.start(queue: DispatchQueue(label: "spotless.stream-quality"))
        monitor = m
    }

    /// Push the resolved quality to the core if it differs from what the core has.
    func apply() {
        guard let s = app?.settings else { return }
        let want = Self.coreQuality(cellular: onCellular,
                                    wifi: s.string(SettingKey.streamQuality, default: "auto"),
                                    cell: s.string(SettingKey.cellularQuality, default: "auto"),
                                    dataSaver: s.bool(SettingKey.dataSaver))
        if s.values["quality"] != want { s.set("quality", want) }
    }
}
