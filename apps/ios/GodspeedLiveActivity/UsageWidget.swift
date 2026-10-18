import SwiftUI
import UIKit
import WidgetKit

/// Home Screen (small, medium) and Lock Screen (circular, rectangular,
/// inline) widgets: the current agent login's 5-hour window and when it
/// resets. The app writes the reading to the App Group whenever it reads
/// usage from a Mac (`UsageFeed`); the timeline flips to "reset" when the
/// window reopens and to "stale" when the app hasn't looked in a while.
struct UsageWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: UsageWidgetStore.kind, provider: UsageProvider()) { entry in
            UsageWidgetView(entry: entry)
                .containerBackground(for: .widget) { Color.clear }
                .widgetURL(URL(string: "godspeed://usage"))
        }
        .configurationDisplayName("Agent Usage")
        .description("Your current agent's 5-hour usage and when it resets.")
        .supportedFamilies([.systemSmall, .systemMedium, .accessoryCircular, .accessoryRectangular, .accessoryInline])
    }
}

struct UsageEntry: TimelineEntry {
    var date: Date
    var timeline: UsageTimeline.Entry
}

struct UsageProvider: TimelineProvider {
    private static let sample = UsageWidgetSnapshot(
        agent: "Claude Code",
        harness: "claude-code",
        account: nil,
        window: .init(label: "5-hour", kind: .fiveHour, used: 0.64, resetsAtMs: Int64(Date().addingTimeInterval(8_040).timeIntervalSince1970 * 1000)),
        weekly: .init(label: "Weekly", kind: .weekly, used: 0.38, resetsAtMs: Int64(Date().addingTimeInterval(273_600).timeIntervalSince1970 * 1000)),
        fetchedAtMs: Int64(Date().timeIntervalSince1970 * 1000)
    )

    func placeholder(in context: Context) -> UsageEntry {
        UsageEntry(date: Date(), timeline: .init(date: Date(), state: .live, snapshot: Self.sample))
    }

    func getSnapshot(in context: Context, completion: @escaping (UsageEntry) -> Void) {
        let now = Date()
        let stored = UsageWidgetStore.load()
        let snapshot = stored ?? (context.isPreview ? Self.sample : nil)
        let entry = UsageTimeline.entries(snapshot, now: now).first ?? .init(date: now, state: .empty, snapshot: nil)
        completion(UsageEntry(date: now, timeline: entry))
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<UsageEntry>) -> Void) {
        let now = Date()
        let entries = UsageTimeline.entries(UsageWidgetStore.load(), now: now)
        completion(Timeline(
            entries: entries.map { UsageEntry(date: $0.date, timeline: $0) },
            policy: .after(UsageTimeline.refreshDate(entries, now: now))
        ))
    }
}

private enum UFace {
    static func sans(_ size: CGFloat, _ style: Font.TextStyle = .body) -> Font { .custom("Geist-Medium", size: size, relativeTo: style) }
    static func mono(_ size: CGFloat, _ style: Font.TextStyle = .body) -> Font { .custom("GeistMono-Medium", size: size, relativeTo: style) }
}

/// The app's palette (`Palette`, `UsageMeterView.color`).
private enum UTone {
    static func dynamic(_ light: UInt32, _ dark: UInt32) -> Color {
        Color(UIColor { $0.userInterfaceStyle == .dark ? UIColor(rgb: dark) : UIColor(rgb: light) })
    }
    static let accent = dynamic(0x5B43E8, 0x8B7CF6)
    static let warning = dynamic(0xA16207, 0xFACC15)
    static let danger = dynamic(0xDC2626, 0xF87171)

    static func level(_ used: Double) -> Color {
        switch UsageFormat.level(used) {
        case .normal: accent
        case .warn: warning
        case .critical: danger
        }
    }
}

private extension UIColor {
    convenience init(rgb: UInt32) {
        self.init(red: CGFloat(rgb >> 16 & 0xFF) / 255, green: CGFloat(rgb >> 8 & 0xFF) / 255, blue: CGFloat(rgb & 0xFF) / 255, alpha: 1)
    }
}

struct UsageWidgetView: View {
    @Environment(\.widgetFamily) private var family
    @Environment(\.widgetRenderingMode) private var renderingMode
    let entry: UsageEntry

    private var t: UsageTimeline.Entry { entry.timeline }
    private var used: Double { t.used }
    private var tint: Color { renderingMode == .fullColor ? UTone.level(used) : .primary }

    /// "resets in 2h 14m", or what the state says instead.
    private var resetLine: String {
        switch t.state {
        case .empty: return "Open Godspeed"
        case .reset: return "Reset · open to refresh"
        case .stale, .live:
            guard let ms = t.snapshot?.window.resetsAtMs else { return t.state == .stale ? "Open to refresh" : "" }
            return UsageFormat.resetsIn(ms: ms, now: entry.date)
        }
    }

    private var resetMoment: String? {
        guard t.state == .live || t.state == .stale, let ms = t.snapshot?.window.resetsAtMs else { return nil }
        return UsageFormat.moment(ms: ms, now: entry.date)
    }

    private var title: String {
        t.snapshot.map { "\($0.agent) · \($0.window.label)" } ?? "Agent usage"
    }

    var body: some View {
        switch family {
        case .accessoryCircular: circular
        case .accessoryRectangular: rectangular
        case .accessoryInline: inline
        case .systemMedium: medium
        default: small
        }
    }

    private var a11y: String {
        guard let s = t.snapshot, t.state != .empty else { return "Agent usage. Open Godspeed to load it." }
        return "\(s.agent) \(s.window.label) window, \(UsageFormat.percent(used)) used, \(resetLine)"
    }

    private var circular: some View {
        Gauge(value: used) {
            Image(systemName: "gauge.with.dots.needle.67percent")
        } currentValueLabel: {
            Text(t.state == .empty ? "–" : "\(Int((used * 100).rounded()))")
                .font(UFace.sans(14, .caption))
        }
        .gaugeStyle(.accessoryCircularCapacity)
        .tint(tint)
        .widgetAccentable()
        .accessibilityLabel(a11y)
    }

    private var rectangular: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(t.snapshot?.window.label ?? "Usage").font(UFace.sans(13, .headline)).widgetAccentable()
                Spacer(minLength: 4)
                Text(t.state == .empty ? "–" : UsageFormat.percent(used)).font(UFace.mono(13, .headline))
            }
            ProgressView(value: used).tint(tint).widgetAccentable()
            Text(resetLine).font(UFace.sans(11, .caption)).foregroundStyle(.secondary).lineLimit(1)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(a11y)
    }

    private var inline: some View {
        Text(t.state == .empty ? "Godspeed usage" : "\(UsageFormat.percent(used)) · \(resetLine)")
            .accessibilityLabel(a11y)
    }

    private var small: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
                .font(UFace.sans(12, .caption))
                .foregroundStyle(.secondary)
                .lineLimit(1)
            Text(t.state == .empty ? "–" : UsageFormat.percent(used))
                .font(UFace.sans(34, .largeTitle))
                .foregroundStyle(tint)
                .minimumScaleFactor(0.6)
            ProgressView(value: used).tint(tint)
            Spacer(minLength: 0)
            Text(resetLine)
                .font(UFace.sans(12, .caption))
                .foregroundStyle(.secondary)
                .lineLimit(2)
            if let at = resetMoment {
                Text(at).font(UFace.mono(11, .caption2)).foregroundStyle(.tertiary)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(a11y)
    }

    private var medium: some View {
        HStack(alignment: .top, spacing: 16) {
            small
            VStack(alignment: .leading, spacing: 6) {
                Text(t.snapshot?.account ?? " ")
                    .font(UFace.sans(12, .caption))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                if let weekly = t.snapshot?.weekly, t.state != .empty {
                    Text("Weekly").font(UFace.sans(12, .caption)).foregroundStyle(.secondary)
                    Text(UsageFormat.percent(weekly.used))
                        .font(UFace.sans(22, .title))
                        .foregroundStyle(renderingMode == .fullColor ? UTone.level(weekly.used) : .primary)
                    ProgressView(value: weekly.used).tint(renderingMode == .fullColor ? UTone.level(weekly.used) : .primary)
                    if let ms = weekly.resetsAtMs {
                        Text(UsageFormat.resetsIn(ms: ms, now: entry.date))
                            .font(UFace.sans(11, .caption))
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 0)
                if let s = t.snapshot {
                    Text("As of \(UsageFormat.moment(ms: s.fetchedAtMs, now: entry.date))")
                        .font(UFace.sans(10, .caption2))
                        .foregroundStyle(.tertiary)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .accessibilityElement(children: .combine)
        }
    }
}
