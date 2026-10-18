import Foundation

/// Compiled into the app and the widget extension: the usage formatting both
/// render with (the desktop's Settings → Usage rules), the snapshot the app
/// writes to the App Group for the widgets, and the widgets' timeline.
enum UsageFormat {
    enum Level: Equatable {
        case normal
        /// 80% or more used.
        case warn
        /// 95% or more used.
        case critical
    }

    /// Desktop thresholds (`settings/accounts.rs`).
    static func level(_ used: Double) -> Level {
        used >= 0.95 ? .critical : used >= 0.8 ? .warn : .normal
    }

    /// Percent used, rounded: "64%".
    static func percent(_ used: Double) -> String {
        "\(Int((min(max(used, 0), 1) * 100).rounded()))%"
    }

    /// "2h 14m" until `ms` (minutes rounded up; days and hours past a day);
    /// nil once due.
    static func countdown(toMs ms: Int64, now: Date) -> String? {
        let seconds = max(0, (Double(ms) / 1000 - now.timeIntervalSince1970).rounded(.down))
        let minutes = Int64((seconds + 59) / 60)
        guard minutes > 0 else { return nil }
        let days = minutes / 1_440, hours = (minutes % 1_440) / 60, mins = minutes % 60
        if days > 0 { return hours > 0 ? "\(days)d \(hours)h" : "\(days)d" }
        if hours > 0 { return mins > 0 ? "\(hours)h \(mins)m" : "\(hours)h" }
        return "\(mins)m"
    }

    /// Desktop `resets_in`: "resets in 2h 14m", "resets now" once due.
    static func resetsIn(ms: Int64, now: Date) -> String {
        countdown(toMs: ms, now: now).map { "resets in \($0)" } ?? "resets now"
    }

    /// Desktop `local_moment_label`: "3:40 PM" today, "Tue 3:40 PM" within
    /// six days, else "Oct 21, 3:40 PM".
    static func moment(ms: Int64, now: Date, calendar: Calendar = .current, locale: Locale = .current) -> String {
        let date = Date(timeIntervalSince1970: Double(ms) / 1000)
        let f = DateFormatter()
        f.locale = locale
        f.timeZone = calendar.timeZone
        let time = DateFormatter.dateFormat(fromTemplate: "jmm", options: 0, locale: locale) ?? "h:mm a"
        if calendar.isDate(date, inSameDayAs: now) {
            f.dateFormat = time
        } else if let days = calendar.dateComponents([.day], from: calendar.startOfDay(for: now), to: calendar.startOfDay(for: date)).day,
                  (-6...6).contains(days) {
            f.dateFormat = "EEE " + time
        } else {
            f.dateFormat = (DateFormatter.dateFormat(fromTemplate: "MMMd", options: 0, locale: locale) ?? "MMM d") + ", " + time
        }
        return f.string(from: date)
    }

    /// "5-hour", "Weekly", or the engine's own label.
    static func windowLabel(_ label: String) -> String {
        switch windowKind(label) {
        case .fiveHour: "5-hour"
        case .weekly: "Weekly"
        case .other: label
        }
    }

    enum WindowKind: String, Codable, Equatable {
        case fiveHour, weekly, other
    }

    /// Mirrors the core's `window_kind` (and the desktop's).
    static func windowKind(_ label: String) -> WindowKind {
        let l = label.trimmingCharacters(in: .whitespaces).lowercased()
            .replacingOccurrences(of: "_", with: "-").replacingOccurrences(of: " ", with: "-")
        switch l {
        case "session", "5h", "5-hour", "five-hour", "5-hours": return .fiveHour
        case "week", "weekly", "7-day", "seven-day", "7d": return .weekly
        default: return .other
        }
    }

    /// `987`, `12.3K`, `4.5M`, `1.2B` (desktop `format_tokens`).
    static func tokens(_ n: UInt64) -> String {
        func scaled(_ v: Double, _ unit: String) -> String {
            var text = v < 10 ? String(format: "%.1f", v) : String(format: "%.0f", v)
            if text.hasSuffix(".0") { text.removeLast(2) }
            return text + unit
        }
        switch n {
        case ..<1_000: return String(n)
        case ..<999_500: return scaled(Double(n) / 1e3, "K")
        case ..<999_500_000: return scaled(Double(n) / 1e6, "M")
        default: return scaled(Double(n) / 1e9, "B")
        }
    }

    /// `$0`, `<$0.01`, `$12.34`, `$1,234` (desktop `format_usd`).
    static func usd(_ v: Double) -> String {
        if v <= 0 { return "$0" }
        if v < 0.005 { return "<$0.01" }
        if v < 1_000 { return String(format: "$%.2f", v) }
        let n = NumberFormatter()
        n.numberStyle = .decimal
        n.locale = Locale(identifier: "en_US")
        n.maximumFractionDigits = 0
        return "$" + (n.string(from: NSNumber(value: v.rounded())) ?? String(Int(v.rounded())))
    }

    /// "now", "4m ago", "2h ago", "3d ago".
    static func ago(ms: Int64, now: Date) -> String {
        let secs = max(0, Int64(now.timeIntervalSince1970) - ms / 1000)
        switch secs {
        case ..<60: return "now"
        case ..<3_600: return "\(secs / 60)m ago"
        case ..<86_400: return "\(secs / 3_600)h ago"
        default: return "\(secs / 86_400)d ago"
        }
    }

    /// "4m 12s" / "1h 02m" elapsed (Mission Control timers).
    static func elapsed(fromMs ms: Int64, now: Date) -> String {
        let secs = max(0, Int64(now.timeIntervalSince1970) - ms / 1000)
        if secs < 60 { return "\(secs)s" }
        if secs < 3_600 { return "\(secs / 60)m \(String(format: "%02d", secs % 60))s" }
        return "\(secs / 3_600)h \(String(format: "%02d", (secs % 3_600) / 60))m"
    }
}

/// What the widgets show: the current account's 5-hour window (or its
/// most-used window when it has none), written by the app to the App Group
/// whenever it reads usage.
struct UsageWidgetSnapshot: Codable, Equatable {
    struct Window: Codable, Equatable {
        var label: String
        var kind: UsageFormat.WindowKind
        var used: Double
        var resetsAtMs: Int64?
    }

    /// "Claude Code".
    var agent: String
    /// Wire harness id, for the brand tint.
    var harness: String
    /// Email or plan, when known.
    var account: String?
    var window: Window
    /// The weekly window, for the medium widget.
    var weekly: Window?
    /// When the app read it.
    var fetchedAtMs: Int64
}

/// The App Group the app writes and the widgets read.
enum UsageWidgetStore {
    static let group = "group.site.shrithan.godspeed.ios"
    static let key = "usage.widget.snapshot"
    static let kind = "GodspeedUsage"

    static var defaults: UserDefaults? { UserDefaults(suiteName: group) }

    static func load() -> UsageWidgetSnapshot? {
        guard let data = defaults?.data(forKey: key) else { return nil }
        return try? JSONDecoder().decode(UsageWidgetSnapshot.self, from: data)
    }

    /// True when the stored value changed.
    @discardableResult
    static func save(_ snapshot: UsageWidgetSnapshot?) -> Bool {
        guard let defaults else { return false }
        let old = defaults.data(forKey: key)
        let new = snapshot.flatMap { try? JSONEncoder().encode($0) }
        guard old != new else { return false }
        if let new { defaults.set(new, forKey: key) } else { defaults.removeObject(forKey: key) }
        return true
    }
}

/// The widgets' timeline: one entry now, one when the window resets (it
/// then reads 0% and "Open Godspeed to refresh"), and one when the reading
/// goes stale. WidgetKit is asked to reload at the last of them.
enum UsageTimeline {
    /// After this long without the app reading usage, the widget says so.
    static let staleAfter: TimeInterval = 6 * 3_600

    enum State: Equatable {
        /// No snapshot yet: open the app.
        case empty
        case live
        /// The window reset since the app last read it.
        case reset
        /// Read too long ago to trust.
        case stale
    }

    struct Entry: Equatable {
        var date: Date
        var state: State
        var snapshot: UsageWidgetSnapshot?

        /// What the gauge reads at `date`.
        var used: Double {
            guard let s = snapshot, state == .live || state == .stale else { return 0 }
            return s.window.used
        }
    }

    static func state(_ snapshot: UsageWidgetSnapshot?, at date: Date) -> State {
        guard let s = snapshot else { return .empty }
        if let reset = s.window.resetsAtMs, Double(reset) / 1000 <= date.timeIntervalSince1970 { return .reset }
        if date.timeIntervalSince1970 - Double(s.fetchedAtMs) / 1000 >= staleAfter { return .stale }
        return .live
    }

    static func entries(_ snapshot: UsageWidgetSnapshot?, now: Date) -> [Entry] {
        var dates = [now]
        if let s = snapshot {
            if let reset = s.window.resetsAtMs {
                let at = Date(timeIntervalSince1970: Double(reset) / 1000)
                if at > now { dates.append(at) }
            }
            let stale = Date(timeIntervalSince1970: Double(s.fetchedAtMs) / 1000 + staleAfter)
            if stale > now { dates.append(stale) }
        }
        dates.sort()
        var out: [Entry] = []
        for d in dates {
            let e = Entry(date: d, state: state(snapshot, at: d), snapshot: snapshot)
            if out.last?.state != e.state || out.isEmpty { out.append(e) }
        }
        return out
    }

    /// When WidgetKit should ask again: after the last transition, or in
    /// 30 minutes when nothing is scheduled (the app also reloads the
    /// timeline whenever it writes a new snapshot).
    static func refreshDate(_ entries: [Entry], now: Date) -> Date {
        let last = entries.map(\.date).max() ?? now
        return last > now ? last.addingTimeInterval(60) : now.addingTimeInterval(30 * 60)
    }
}
